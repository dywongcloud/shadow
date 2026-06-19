//! Instance-side tunnel: demux requests, proxy to the local function over HTTP
//! concurrently, push in-band metrics, and nack when at capacity.

use crate::codec::{read_frame, Frame, FrameKind};
use crate::{Metrics, ReqMeta, RespMeta};
use bytes::Bytes;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub struct TunnelServer;

impl TunnelServer {
    /// Serve one tunnel connection until it closes. `local_http` is the
    /// function server's `host:port`; `max_concurrency` bounds in-flight requests.
    pub async fn serve<S>(stream: S, local_http: String, max_concurrency: u32)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
        let inflight = Arc::new(AtomicU32::new(0));

        // Writer task.
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if wr.write_all(&frame.encode()).await.is_err() {
                    break;
                }
            }
        });

        // Metrics ticker (in-band health).
        {
            let out = out_tx.clone();
            let inflight = inflight.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let m = Metrics {
                        inflight: inflight.load(Ordering::Relaxed),
                        max_concurrency,
                    };
                    let payload = serde_json::to_vec(&m).unwrap_or_default();
                    if out.send(Frame::new(0, FrameKind::Metrics, payload)).is_err() {
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
}

async fn handle_request(
    id: u64,
    payload: Bytes,
    local_http: &str,
    out: &mpsc::UnboundedSender<Frame>,
) {
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
    out: &mpsc::UnboundedSender<Frame>,
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
            out.send(Frame::new(id, FrameKind::RespBody, Bytes::copy_from_slice(&buf[..size])))?;
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
                    out.send(Frame::new(id, FrameKind::RespBody, Bytes::copy_from_slice(&tmp[..r])))?;
                    sent += r;
                }
            }
            None => loop {
                let r = conn.read(&mut tmp).await?;
                if r == 0 {
                    break;
                }
                out.send(Frame::new(id, FrameKind::RespBody, Bytes::copy_from_slice(&tmp[..r])))?;
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
