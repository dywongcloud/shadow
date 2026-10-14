//! Instance-side tunnel: demux requests, proxy to the local function over HTTP concurrently, push
//! in-band metrics, nack when at capacity. Three serving modes, chosen per function:
//! [`TunnelServer::serve`] (HTTP-framed, multiplexed), [`TunnelServer::serve_raw`] (a bidirectional
//! byte splice for non-HTTP protocols) and [`TunnelServer::serve_maybe_raw`] (`serve`, except the
//! one connection whose first byte is [`TunnelServer::RAW_UPGRADE_MAGIC`] gets `serve_raw`: the
//! WebSocket-upgrade escape).

use crate::codec::{read_frame, Frame, FrameKind};
use crate::{Metrics, ReqMeta, RespMeta};
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};

/// Write-queue depth at or above which a backpressure event is counted; well clear of normal
/// streaming bursts.
const BACKPRESSURE_HWM: u64 = 256;

/// Concurrent local connections one upstream address may receive across every tunnel in this
/// process; litebox lowers it at init because its guest network stack re-arms a single listener
/// serially, so requests queue for the microseconds a busy guest needs instead of being accept-closed.
static LOCAL_CONNECT_PERMITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(64);

/// Set by the backend that owns this process's cells, once, at init.
pub fn set_local_connect_permits(permits: usize) {
    LOCAL_CONNECT_PERMITS.store(permits.max(1), Ordering::Relaxed);
}

/// Ceiling on completing a TCP connect to the local (guest) hop; 15 s matches the window a litebox
/// cold start gets to reach a listening state.
const LOCAL_CONNECT_DEADLINE: Duration = Duration::from_secs(15);

/// Ceiling on the two waits that can hold a guest's connect permit — queueing for it and reading
/// the response head — so a guest that accepts and never answers costs one retryable failure
/// instead of pinning the address's permit and an `inflight` slot until the OS gives up (~130 s).
/// Generous on purpose: shorter than forever, never shorter than real work.
const LOCAL_RESPONSE_DEADLINE: Duration = Duration::from_secs(60);

/// Consecutive requests that reached this function's address and produced NO response at all,
/// after which the tunnel declares the instance not answering and closes the connection — the only
/// failure signal that reaches the scheduler (`fluid-gateway` reads the close, calls
/// `Fluid::mark_dead`, the instance is replaced). Any response, the function's own 502 included,
/// resets the count.
const UNHEALTHY_FAILURE_STREAK: u32 = 5;

/// Releases one `inflight` slot on Drop, so an abandoned request still hands its slot back
/// instead of ratcheting `inflight` up to `max_concurrency` for good.
struct InflightSlot(Arc<AtomicU32>);

impl Drop for InflightSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Per-upstream-address connect gates keyed by the literal `local_http`; cleared if the map grows
/// past a bound so ephemeral-port churn cannot leak.
fn connect_gate(local_http: &str) -> Arc<tokio::sync::Semaphore> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
    > = std::sync::OnceLock::new();
    let gates = GATES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut map = gates
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if map.len() > 4096 {
        map.clear();
    }
    map.entry(local_http.to_string())
        .or_insert_with(|| {
            Arc::new(tokio::sync::Semaphore::new(
                LOCAL_CONNECT_PERMITS.load(Ordering::Relaxed),
            ))
        })
        .clone()
}

/// Byte + backpressure metering for one tunnel (#14). All counters are cumulative
/// except `queued`, which is the live write-queue depth (enqueued minus flushed).
#[derive(Default)]
struct TunnelMeter {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    queued: AtomicU64,
    backpressure_events: AtomicU64,
}

/// An `UnboundedSender<Frame>` that meters every enqueued frame: bumps the live write-queue depth
/// and trips the backpressure counter past the high-water mark.
#[derive(Clone)]
struct MeteredSender {
    tx: mpsc::UnboundedSender<Frame>,
    meter: Arc<TunnelMeter>,
}

impl MeteredSender {
    fn send(&self, f: Frame) -> Result<(), mpsc::error::SendError<Frame>> {
        let depth = self.meter.queued.fetch_add(1, Ordering::Relaxed) + 1;
        if depth >= BACKPRESSURE_HWM {
            self.meter
                .backpressure_events
                .fetch_add(1, Ordering::Relaxed);
        }
        if let Err(e) = self.tx.send(f) {
            self.meter.queued.fetch_sub(1, Ordering::Relaxed);
            return Err(e);
        }
        Ok(())
    }
}

pub struct TunnelServer;

impl TunnelServer {
    /// Serve one tunnel connection until it closes. `local_http` is the
    /// function server's `host:port`; `max_concurrency` bounds in-flight requests.
    pub async fn serve<S>(stream: S, local_http: String, max_concurrency: u32)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (raw_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
        let inflight = Arc::new(AtomicU32::new(0));
        let meter = Arc::new(TunnelMeter::default());
        let out_tx = MeteredSender {
            tx: raw_tx,
            meter: meter.clone(),
        };

        let writer = {
            let meter = meter.clone();
            tokio::spawn(async move {
                while let Some(frame) = out_rx.recv().await {
                    meter.queued.fetch_sub(1, Ordering::Relaxed);
                    let enc = frame.encode();
                    if wr.write_all(&enc).await.is_err() {
                        break;
                    }
                    meter
                        .bytes_out
                        .fetch_add(enc.len() as u64, Ordering::Relaxed);
                }
            })
        };

        {
            let out = out_tx.clone();
            let inflight = inflight.clone();
            let meter = meter.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let m = Metrics {
                        inflight: inflight.load(Ordering::Relaxed),
                        max_concurrency,
                        bytes_in: meter.bytes_in.load(Ordering::Relaxed),
                        bytes_out: meter.bytes_out.load(Ordering::Relaxed),
                        queue_depth: meter.queued.load(Ordering::Relaxed) as u32,
                        backpressure_events: meter.backpressure_events.load(Ordering::Relaxed),
                    };
                    let payload = serde_json::to_vec(&m).unwrap_or_default();
                    if out
                        .send(Frame::new(0, FrameKind::Metrics, payload))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        let failures = Arc::new(AtomicU32::new(0));
        let stuck = Arc::new(AtomicBool::new(false));
        let stuck_notify = Arc::new(Notify::new());
        loop {
            let notified = stuck_notify.notified();
            let frame = match tokio::select! {
                f = read_frame(&mut rd) => f,
                _ = notified => {
                    if stuck.load(Ordering::Relaxed) {
                        tracing::warn!(
                            local = %local_http,
                            failures = failures.load(Ordering::Relaxed),
                            "function answered none of the last requests; closing the tunnel \
                             so this instance is reaped"
                        );
                        break;
                    }
                    continue;
                }
            } {
                Ok(f) => f,
                Err(_) => break,
            };
            meter
                .bytes_in
                .fetch_add(frame.payload.len() as u64, Ordering::Relaxed);
            match frame.kind {
                FrameKind::Ping => {
                    let _ = out_tx.send(Frame::new(0, FrameKind::Pong, Bytes::new()));
                }
                FrameKind::Request => {
                    let id = frame.stream_id;
                    if inflight.load(Ordering::Relaxed) >= max_concurrency {
                        let _ = out_tx.send(Frame::new(id, FrameKind::Nack, Bytes::new()));
                        continue;
                    }
                    inflight.fetch_add(1, Ordering::Relaxed);
                    let out = out_tx.clone();
                    let local = local_http.clone();
                    let slot = InflightSlot(inflight.clone());
                    let failures = failures.clone();
                    let stuck = stuck.clone();
                    let stuck_notify = stuck_notify.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        if handle_request(id, frame.payload, &local, &out).await {
                            failures.store(0, Ordering::Relaxed);
                        } else {
                            let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
                            if n >= UNHEALTHY_FAILURE_STREAK && !stuck.swap(true, Ordering::Relaxed)
                            {
                                stuck_notify.notify_one();
                            }
                        }
                    });
                }
                _ => {}
            }
        }
        drop(out_tx);
        let _ = writer.await;
    }

    /// Serve one accepted connection in RAW byte-splice mode: connect a fresh TCP stream to
    /// `local_addr` and copy bytes bidirectionally with zero HTTP framing — the mode for non-HTTP
    /// protocols (`FunctionConfig::needs_raw_proxy()`). TCP only: a UDP service's local hop is a
    /// separate datagram relay in hive-backend's podman path, never here.
    pub async fn serve_raw<S>(stream: S, local_addr: String)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut backend = match tokio::time::timeout(
            LOCAL_CONNECT_DEADLINE,
            TcpStream::connect(&local_addr),
        )
        .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                tracing::warn!(local = %local_addr, error = %e, "raw splice: local connect failed");
                return;
            }
            Err(_) => {
                tracing::warn!(
                    local = %local_addr,
                    deadline = ?LOCAL_CONNECT_DEADLINE,
                    "raw splice: local connect timed out"
                );
                return;
            }
        };
        let mut stream = stream;
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut backend).await;
    }

    /// First byte that switches ONE connection on an HTTP-protocol listener to a raw splice (see
    /// [`serve_maybe_raw`](Self::serve_maybe_raw)); outside ASCII, so it can never collide with an
    /// HTTP method letter.
    pub const RAW_UPGRADE_MAGIC: u8 = 0xFE;

    /// Peek-and-dispatch entry for an HTTP-protocol listener: the one connection whose first byte is
    /// [`RAW_UPGRADE_MAGIC`](Self::RAW_UPGRADE_MAGIC) (consumed) gets a byte splice, every other
    /// gets [`serve`](Self::serve) unchanged.
    pub async fn serve_maybe_raw(stream: TcpStream, local_addr: String, max_concurrency: u32) {
        let mut peek = [0u8; 1];
        let is_raw =
            matches!(stream.peek(&mut peek).await, Ok(1) if peek[0] == Self::RAW_UPGRADE_MAGIC);
        if !is_raw {
            Self::serve(stream, local_addr, max_concurrency).await;
            return;
        }
        let mut stream = stream;
        let mut discard = [0u8; 1];
        if stream.read_exact(&mut discard).await.is_err() {
            return;
        }
        Self::serve_raw(stream, local_addr).await;
    }
}

/// Returns whether a response head reached the caller — the progress signal
/// [`UNHEALTHY_FAILURE_STREAK`] counts; a 502 the function sent is an answer, only "no response
/// at all" is a failure.
async fn handle_request(id: u64, payload: Bytes, local_http: &str, out: &MeteredSender) -> bool {
    let idempotent = payload
        .get(0..4)
        .and_then(|len| Some(u32::from_be_bytes(len.try_into().ok()?) as usize))
        .and_then(|meta_len| payload.get(4..4 + meta_len))
        .and_then(|meta| serde_json::from_slice::<ReqMeta>(meta).ok())
        .map(|meta| matches!(meta.method.as_str(), "GET" | "HEAD"))
        .unwrap_or(false);
    const MAX_ATTEMPTS: u32 = 5;
    let mut last_error = None;
    for attempt in 0..MAX_ATTEMPTS {
        let mut head_sent = false;
        match proxy_local(id, &payload, local_http, out, &mut head_sent).await {
            Ok(()) => return true,
            Err(error) => {
                if head_sent || !idempotent || attempt + 1 == MAX_ATTEMPTS {
                    if head_sent {
                        tracing::warn!(
                            id,
                            attempt,
                            error = %error,
                            "tunnel: upstream request failed after response head was already sent — truncating stream"
                        );
                        return true;
                    }
                    last_error = Some(error);
                    break;
                }
                tracing::debug!(
                    id,
                    attempt,
                    error = %error,
                    "tunnel: upstream request attempt failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)))
                    .await;
            }
        }
    }
    if let Some(e) = last_error {
        tracing::warn!(
            id,
            attempts = MAX_ATTEMPTS,
            idempotent,
            error = %e,
            "tunnel: upstream request failed after exhausting retries"
        );
        let meta = RespMeta {
            status: 502,
            headers: vec![],
            wait_until_ms: 0,
        };
        let _ = out.send(Frame::new(
            id,
            FrameKind::RespHead,
            serde_json::to_vec(&meta).unwrap_or_default(),
        ));
        let _ = out.send(Frame::new(
            id,
            FrameKind::RespBody,
            Bytes::from(format!("upstream error: {e}")),
        ));
        let _ = out.send(Frame::new(id, FrameKind::RespEnd, Bytes::new()));
    }
    false
}

/// Relay one framed request to the local hop over a fresh connection as a `RespHead` frame then
/// `RespBody` frames, marking `head_sent` as the head frame is sent. A chunked body is re-framed
/// chunk by chunk and its trailers are ignored.
async fn proxy_local(
    id: u64,
    payload: &[u8],
    local_http: &str,
    out: &MeteredSender,
    head_sent: &mut bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(payload.len() >= 4, "short request payload");
    let meta_len = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
    anyhow::ensure!(payload.len() >= 4 + meta_len, "truncated request meta");
    let meta: ReqMeta = serde_json::from_slice(&payload[4..4 + meta_len])?;
    let body = &payload[4 + meta_len..];

    let _connect_permit = tokio::time::timeout(
        LOCAL_RESPONSE_DEADLINE,
        connect_gate(local_http).acquire_owned(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out after {LOCAL_RESPONSE_DEADLINE:?} waiting for a local connect permit to {local_http}"
        )
    })??;
    let mut conn = tokio::time::timeout(LOCAL_CONNECT_DEADLINE, TcpStream::connect(local_http))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "local connect to {local_http} timed out after {LOCAL_CONNECT_DEADLINE:?}"
            )
        })??;

    let fwd_host = meta
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-host"))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    let mut req = format!("{} {} HTTP/1.1\r\n", meta.method, meta.path);
    let mut has_host = false;
    for (k, v) in &meta.headers {
        let kl = k.to_ascii_lowercase();
        if is_hop_by_hop(&kl) || kl == "content-length" {
            continue;
        }
        if kl == "host" {
            has_host = true;
            match fwd_host.as_deref() {
                Some(public) if is_loopback_authority(v.trim()) => {
                    req.push_str(&format!("host: {public}\r\n"));
                    continue;
                }
                _ => {}
            }
        }
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_host {
        match fwd_host.as_deref() {
            Some(public) => req.push_str(&format!("host: {public}\r\n")),
            None => req.push_str("host: fluid.internal\r\n"),
        }
    }
    req.push_str(&format!("content-length: {}\r\n", body.len()));
    req.push_str("connection: keep-alive\r\n\r\n");
    conn.write_all(req.as_bytes()).await?;
    conn.write_all(body).await?;
    conn.flush().await?;

    let mut raw = Vec::with_capacity(2048);
    let mut tmp = [0u8; 8192];
    let split;
    loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            split = pos;
            break;
        }
        let n = tokio::time::timeout(LOCAL_RESPONSE_DEADLINE, conn.read(&mut tmp))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "function accepted the request but sent no response head within {LOCAL_RESPONSE_DEADLINE:?}"
                )
            })??;
        anyhow::ensure!(n > 0, "function closed before headers");
        raw.extend_from_slice(&tmp[..n]);
        anyhow::ensure!(raw.len() < 1024 * 1024, "function headers too large");
    }
    let mut leftover = raw[split + 4..].to_vec();
    let head = std::str::from_utf8(&raw[..split])?;

    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(502);
    let mut headers = Vec::new();
    let mut wait_until_ms = 0u64;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            let kl = k.to_ascii_lowercase();
            if kl == "content-length" {
                content_length = v.parse().ok();
            }
            if kl == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
            if kl == "x-fluid-wait-until-ms" {
                wait_until_ms = v.parse().unwrap_or(0);
                continue;
            }
            if is_hop_by_hop(&kl) {
                continue;
            }
            headers.push((k.to_string(), v.to_string()));
        }
    }

    let bodiless = meta.method.eq_ignore_ascii_case("HEAD")
        || (100..200).contains(&status)
        || status == 204
        || status == 304;

    let meta = RespMeta {
        status,
        headers,
        wait_until_ms,
    };
    *head_sent = true;
    out.send(Frame::new(
        id,
        FrameKind::RespHead,
        serde_json::to_vec(&meta)?,
    ))?;
    if bodiless {
    } else if chunked {
        let mut buf = leftover;
        loop {
            let line_end = loop {
                if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                    break pos;
                }
                let r = conn.read(&mut tmp).await?;
                anyhow::ensure!(r > 0, "function closed before chunk size");
                buf.extend_from_slice(&tmp[..r]);
                anyhow::ensure!(buf.len() < 64 * 1024, "chunk size line too long");
            };
            let line = &buf[..line_end];
            let hex = line.split(|&b| b == b';').next().unwrap_or(line);
            let size = usize::from_str_radix(std::str::from_utf8(hex)?.trim(), 16)
                .map_err(|_| anyhow::anyhow!("invalid chunk size"))?;
            buf.drain(..line_end + 2);
            if size == 0 {
                break;
            }
            while buf.len() < size + 2 {
                let r = conn.read(&mut tmp).await?;
                anyhow::ensure!(r > 0, "function closed mid-chunk");
                buf.extend_from_slice(&tmp[..r]);
            }
            out.send(Frame::new(
                id,
                FrameKind::RespBody,
                Bytes::copy_from_slice(&buf[..size]),
            ))?;
            buf.drain(..size + 2);
        }
    } else {
        let mut sent = 0usize;
        if !leftover.is_empty() {
            let take = match content_length {
                Some(n) => leftover.len().min(n),
                None => leftover.len(),
            };
            leftover.truncate(take);
            sent += leftover.len();
            out.send(Frame::new(id, FrameKind::RespBody, Bytes::from(leftover)))?;
        }
        match content_length {
            Some(n) => {
                while sent < n {
                    let want = (n - sent).min(tmp.len());
                    let r = conn.read(&mut tmp[..want]).await?;
                    anyhow::ensure!(r > 0, "function closed mid-body");
                    out.send(Frame::new(
                        id,
                        FrameKind::RespBody,
                        Bytes::copy_from_slice(&tmp[..r]),
                    ))?;
                    sent += r;
                }
            }
            None => loop {
                let r = conn.read(&mut tmp).await?;
                if r == 0 {
                    break;
                }
                out.send(Frame::new(
                    id,
                    FrameKind::RespBody,
                    Bytes::copy_from_slice(&tmp[..r]),
                ))?;
            },
        }
    }
    out.send(Frame::new(id, FrameKind::RespEnd, Bytes::new()))?;
    Ok(())
}

fn is_hop_by_hop(h: &str) -> bool {
    matches!(
        h,
        "connection" | "keep-alive" | "transfer-encoding" | "upgrade" | "te" | "trailers"
    )
}

/// Is `authority` the cell's own loopback bind address (host or host:port)? Anything else routes
/// on its Host and is forwarded untouched.
fn is_loopback_authority(authority: &str) -> bool {
    let authority = authority.trim();
    let host = match authority.rfind(':') {
        Some(i) if !authority.starts_with('[') => &authority[..i],
        Some(_) if authority.ends_with(']') => authority,
        Some(i) => &authority[..i],
        None => authority,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.trim_end_matches('.');
    host == "localhost"
        || host == "::1"
        || host == "0.0.0.0"
        || host
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // #14: the metered sender tracks live write-queue depth and trips the
    // backpressure counter past the high-water mark when nothing drains the queue.
    #[test]
    fn metered_sender_tracks_queue_depth_and_backpressure() {
        let meter = Arc::new(TunnelMeter::default());
        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        let s = MeteredSender {
            tx,
            meter: meter.clone(),
        };

        // Enqueue past the high-water mark without draining.
        let n = BACKPRESSURE_HWM + 10;
        for _ in 0..n {
            s.send(Frame::new(1, FrameKind::RespBody, Bytes::from_static(b"x")))
                .unwrap();
        }
        assert_eq!(
            meter.queued.load(Ordering::Relaxed),
            n,
            "depth = all undrained frames"
        );
        // Events trip for every enqueue at/after the HWM: depths HWM..=n.
        assert_eq!(
            meter.backpressure_events.load(Ordering::Relaxed),
            n - BACKPRESSURE_HWM + 1
        );

        // Simulate the writer draining (it decrements on dequeue).
        for _ in 0..n {
            let _ = rx.try_recv();
            meter.queued.fetch_sub(1, Ordering::Relaxed);
        }
        assert_eq!(
            meter.queued.load(Ordering::Relaxed),
            0,
            "queue fully drained"
        );
    }

    // A send on a closed tunnel must not leak the depth gauge upward.
    #[test]
    fn metered_sender_undoes_depth_on_closed_channel() {
        let meter = Arc::new(TunnelMeter::default());
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        let s = MeteredSender {
            tx,
            meter: meter.clone(),
        };
        drop(rx); // close the receiver
        assert!(s
            .send(Frame::new(1, FrameKind::RespEnd, Bytes::new()))
            .is_err());
        assert_eq!(
            meter.queued.load(Ordering::Relaxed),
            0,
            "depth restored on send failure"
        );
    }
}
