//! Instance-side tunnel: demux requests, proxy to the local function over HTTP
//! concurrently, push in-band metrics, and nack when at capacity.
//!
//! Two serving modes, chosen per-function by the backend that fronts it:
//! * [`TunnelServer::serve`] — the HTTP-framed multiplexed path (the default).
//! * [`TunnelServer::serve_raw`] — a plain bidirectional byte splice for
//!   non-HTTP protocols (`needs_raw_proxy()`), with no framing whatsoever.
//!
//! A THIRD mode exists for an HTTP-protocol function that ALSO needs an
//! occasional raw escape on ONE connection — a WebSocket upgrade, which
//! cannot ride the multiplexed frame format (it needs the whole connection,
//! unframed, for the life of the upgraded session):
//! * [`TunnelServer::serve_maybe_raw`] — peeks the connection's first byte
//!   (non-consuming, `TcpStream::peek`); [`RAW_UPGRADE_MAGIC`] switches this
//!   ONE connection to [`serve_raw`], anything else (every real HTTP request
//!   line starts with an uppercase ASCII method letter) is byte-identical
//!   `serve`. The accept loop that fronts an HTTP-protocol function uses this
//!   instead of bare `serve` so a caller that knows to send the magic byte
//!   first (`fluid_gateway`'s local WS splice) can open a raw connection to
//!   the SAME listener ordinary framed traffic uses, with zero effect on
//!   every other connection.

use crate::codec::{read_frame, Frame, FrameKind};
use crate::{Metrics, ReqMeta, RespMeta};
use bytes::Bytes;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Write-queue depth (frames waiting to be flushed to the router) at or above
/// which we count a backpressure event (#14). Tuned to be well clear of normal
/// streaming bursts so the counter signals a genuinely slow/stalled consumer.
const BACKPRESSURE_HWM: u64 = 256;

/// Byte + backpressure metering for one tunnel (#14). All counters are cumulative
/// except `queued`, which is the live write-queue depth (enqueued minus flushed).
#[derive(Default)]
struct TunnelMeter {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    queued: AtomicU64,
    backpressure_events: AtomicU64,
}

/// An `UnboundedSender<Frame>` that meters every frame it enqueues (#14): it bumps
/// the live write-queue depth and trips the backpressure counter when the depth
/// crosses the high-water mark. Keeps call sites as plain `out.send(frame)`.
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
            // Send failed (tunnel closed) — undo the depth bump so the gauge
            // doesn't drift upward on a dead tunnel.
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

        // Writer task: flushes frames and accounts bytes_out + drains the queue gauge.
        let writer = {
            let meter = meter.clone();
            tokio::spawn(async move {
                while let Some(frame) = out_rx.recv().await {
                    // Dequeued: drop the live write-queue depth before the (awaitable)
                    // socket write, so the gauge reflects "waiting to flush", not
                    // "currently flushing".
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

        // Metrics ticker (in-band health + #14 byte/backpressure metering).
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

        // Reader loop: demux requests.
        loop {
            let frame = match read_frame(&mut rd).await {
                Ok(f) => f,
                Err(_) => break,
            };
            // Account inbound bytes (#14): request payloads dominate ingress.
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
                    let inflight = inflight.clone();
                    tokio::spawn(async move {
                        handle_request(id, frame.payload, &local, &out).await;
                        inflight.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                _ => {}
            }
        }
        drop(out_tx);
        let _ = writer.await;
    }

    /// Serve one accepted connection in RAW byte-splice mode: connect a fresh
    /// TCP stream to `local_addr` (the function/container's published loopback
    /// port) and copy bytes bidirectionally until either side closes — ZERO
    /// HTTP framing, parsing, or tunnel-frame codec. This is the serving mode
    /// for non-HTTP application protocols (`FunctionConfig::needs_raw_proxy()`:
    /// gRPC / raw TCP wire protocols like Postgres or Minecraft), whose bytes
    /// the HTTP-framed [`TunnelServer::serve`] path would corrupt by writing an
    /// HTTP request line at them and chunk-decoding their responses. Same
    /// proven splice pattern as hive-cloud's `db_gateway` and `edge::ws_proxy`.
    ///
    /// TCP-ONLY by design: `copy_bidirectional` is a byte-stream concept and
    /// does not apply to UDP datagrams. A UDP service's local hop needs a
    /// separate datagram relay (host UDP socket <-> the container's published
    /// loopback UDP port, preserving datagram boundaries) — that relay plugs in
    /// beside this function at the accept-loop branch in hive-backend's podman
    /// path, NOT here.
    pub async fn serve_raw<S>(stream: S, local_addr: String)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut backend = match TcpStream::connect(&local_addr).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(local = %local_addr, error = %e, "raw splice: local connect failed");
                return;
            }
        };
        let mut stream = stream;
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut backend).await;
    }

    /// First byte a caller writes to switch ONE connection on an
    /// HTTP-protocol function's listener (normally exclusively `serve`, the
    /// framed multiplexed path) to a raw splice for that connection alone —
    /// see [`serve_maybe_raw`](Self::serve_maybe_raw). Chosen outside the
    /// ASCII range: every real HTTP request line starts with an uppercase
    /// method letter (`G`/`P`/`H`/…, all < `0x80`), so this can never
    /// collide with genuine framed traffic.
    pub const RAW_UPGRADE_MAGIC: u8 = 0xFE;

    /// Peek-and-dispatch entry point for an HTTP-protocol function's
    /// listener: a plain byte-stream splice for the ONE connection whose
    /// first byte is [`RAW_UPGRADE_MAGIC`] (magic consumed, never spliced),
    /// [`serve`](Self::serve) unchanged for every other connection (the
    /// peek does not consume anything a real request needs). Used by the
    /// accept loop in place of bare `serve` so a caller that knows the magic
    /// (`fluid_gateway`'s local WebSocket-upgrade splice) can open a raw
    /// connection to the SAME listener ordinary framed traffic uses.
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
        // The peek above only inspected the byte; actually consume it here
        // (a real read, not another peek) so it never leaks into the splice.
        if stream.read_exact(&mut discard).await.is_err() {
            return;
        }
        Self::serve_raw(stream, local_addr).await;
    }
}

async fn handle_request(id: u64, payload: Bytes, local_http: &str, out: &MeteredSender) {
    if let Err(e) = proxy_local(id, &payload, local_http, out).await {
        // Make sure the caller gets *something* terminal.
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
}

async fn proxy_local(
    id: u64,
    payload: &[u8],
    local_http: &str,
    out: &MeteredSender,
) -> anyhow::Result<()> {
    // Split payload: [u32 meta_len][meta json][body].
    anyhow::ensure!(payload.len() >= 4, "short request payload");
    let meta_len = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
    anyhow::ensure!(payload.len() >= 4 + meta_len, "truncated request meta");
    let meta: ReqMeta = serde_json::from_slice(&payload[4..4 + meta_len])?;
    let body = &payload[4 + meta_len..];

    let mut conn = TcpStream::connect(local_http).await?;

    // Write HTTP/1.1 request, Connection: close so EOF delimits the response.
    let mut req = format!("{} {} HTTP/1.1\r\n", meta.method, meta.path);
    let mut has_host = false;
    for (k, v) in &meta.headers {
        let kl = k.to_ascii_lowercase();
        if is_hop_by_hop(&kl) || kl == "content-length" {
            continue;
        }
        if kl == "host" {
            has_host = true;
        }
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_host {
        req.push_str("host: fluid.internal\r\n");
    }
    req.push_str(&format!("content-length: {}\r\n", body.len()));
    req.push_str("connection: close\r\n\r\n");
    conn.write_all(req.as_bytes()).await?;
    conn.write_all(body).await?;
    conn.flush().await?;

    // Read response head.
    let mut raw = Vec::with_capacity(2048);
    let mut tmp = [0u8; 8192];
    let split;
    loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            split = pos;
            break;
        }
        let n = conn.read(&mut tmp).await?;
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
            // The function may stream its response with chunked transfer-encoding
            // (Next.js does this, especially for gzipped responses). We must DECODE
            // the chunk framing here — `transfer-encoding` is hop-by-hop and is
            // dropped, so the framing bytes would otherwise be forwarded as part of
            // the body and corrupt it (e.g. break a gzip Content-Encoding).
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

    // Send response head, then stream the body.
    let meta = RespMeta {
        status,
        headers,
        wait_until_ms,
    };
    out.send(Frame::new(
        id,
        FrameKind::RespHead,
        serde_json::to_vec(&meta)?,
    ))?;
    // Stream the body.
    if chunked {
        // Decode HTTP/1.1 chunked framing and forward ONLY the payload bytes, so
        // any Content-Encoding (gzip/br) stays valid for the client. Each chunk is
        // "<hex-size>[;ext]\r\n<data>\r\n", terminated by a zero-size chunk.
        let mut buf = leftover;
        loop {
            // Read until we have a full chunk-size line.
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
            buf.drain(..line_end + 2); // consume "<size>\r\n"
            if size == 0 {
                break; // last chunk (any trailers are ignored)
            }
            // Ensure the full chunk data + its trailing CRLF are buffered.
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
            buf.drain(..size + 2); // consume data + trailing CRLF
        }
    } else {
        // Prefer content-length (read EXACTLY n bytes — never wait for EOF, which
        // can hang if the function keeps the socket open); otherwise read to close.
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
