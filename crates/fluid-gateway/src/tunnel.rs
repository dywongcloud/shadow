//! Persistent connection tunnels to function instances.
//!
//! Vercel's Functions router keeps **persistent TCP tunnels** to instances and
//! reuses them across requests (the `compute-resolver` routes a function's
//! requests back to a router that already has an open connection, giving >99%
//! reuse). Here we model that with a per-instance keep-alive connection pool:
//! a request reuses an idle connection to the chosen instance when one exists,
//! otherwise it opens a new one and returns it to the pool when done.
//!
//! Reuse requires knowing where a response ends without closing the socket, so
//! we only pool **content-length, keep-alive** responses. Streaming / explicitly
//! `Connection: close` responses are read to EOF and not reused.

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use hive_backend::{connect_endpoint, CellEndpoint, DuplexStream};
use hive_core::CellId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type Conn = Box<dyn DuplexStream>;

/// Per-instance idle-connection pool with reuse counters.
pub struct ConnPool {
    idle: Mutex<HashMap<CellId, Vec<Conn>>>,
    opened: AtomicU64,
    reused: AtomicU64,
    max_idle_per_cell: usize,
}

impl ConnPool {
    pub fn new() -> Self {
        ConnPool {
            idle: Mutex::new(HashMap::new()),
            opened: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            max_idle_per_cell: 64,
        }
    }

    /// Get a connection to `cell`: reuse an idle one if available, else dial.
    /// Returns `(conn, reused)`.
    pub async fn acquire(&self, cell: &CellId, ep: &CellEndpoint) -> anyhow::Result<(Conn, bool)> {
        let pooled = {
            let mut m = self.idle.lock();
            m.get_mut(cell).and_then(|v| v.pop())
        };
        if let Some(c) = pooled {
            self.reused.fetch_add(1, Ordering::Relaxed);
            return Ok((c, true));
        }
        let s = connect_endpoint(ep).await?;
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok((s, false))
    }

    /// Return a still-usable connection to the pool.
    pub fn release(&self, cell: &CellId, conn: Conn) {
        let mut m = self.idle.lock();
        let v = m.entry(cell.clone()).or_default();
        if v.len() < self.max_idle_per_cell {
            v.push(conn);
        }
    }

    /// Drop all pooled connections for an instance (it died).
    pub fn drop_cell(&self, cell: &CellId) {
        self.idle.lock().remove(cell);
    }

    /// (opened, reused, reuse_pct).
    pub fn reuse_stats(&self) -> (u64, u64, f64) {
        let o = self.opened.load(Ordering::Relaxed);
        let r = self.reused.load(Ordering::Relaxed);
        let total = o + r;
        let pct = if total > 0 { r as f64 / total as f64 } else { 0.0 };
        (o, r, pct)
    }
}

impl Default for ConnPool {
    fn default() -> Self {
        Self::new()
    }
}

/// How the response body is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// content-length: N. Reusable (we read exactly N bytes).
    Length(usize),
    /// Connection: close / chunked / unknown — read to EOF, do not reuse.
    Close,
}

pub struct Head {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub framing: Framing,
    /// True if the connection may be returned to the pool after the body.
    pub reusable: bool,
    /// Bytes already read past the header block (start of the body).
    pub leftover: Vec<u8>,
    /// `waitUntil` window declared by the function (x-fluid-wait-until-ms).
    pub wait_until_ms: u64,
}

/// Write an HTTP/1.1 keep-alive request to the instance.
pub async fn write_request(
    conn: &mut Conn,
    method: &str,
    path_q: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> anyhow::Result<()> {
    let mut req = format!("{method} {path_q} HTTP/1.1\r\n");
    let mut has_host = false;
    for (k, v) in headers.iter() {
        let kn = k.as_str();
        if is_hop_by_hop(kn) || kn == "content-length" {
            continue;
        }
        if kn == "host" {
            has_host = true;
        }
        if let Ok(vs) = v.to_str() {
            req.push_str(&format!("{kn}: {vs}\r\n"));
        }
    }
    if !has_host {
        req.push_str("host: fluid.internal\r\n");
    }
    req.push_str(&format!("content-length: {}\r\n", body.len()));
    req.push_str("connection: keep-alive\r\n\r\n");
    conn.write_all(req.as_bytes()).await?;
    conn.write_all(body).await?;
    conn.flush().await?;
    Ok(())
}

/// Read the response head and decide framing / reusability.
pub async fn read_head(conn: &mut Conn) -> anyhow::Result<Head> {
    let mut raw: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let split;
    loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            split = pos;
            break;
        }
        let n = conn.read(&mut tmp).await?;
        anyhow::ensure!(n > 0, "upstream closed before headers");
        raw.extend_from_slice(&tmp[..n]);
        anyhow::ensure!(raw.len() < 1024 * 1024, "response headers too large");
    }
    let leftover = raw[split + 4..].to_vec();
    let head = std::str::from_utf8(&raw[..split])?;

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("bad status line: {status_line}"))?;
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);

    let mut content_length: Option<usize> = None;
    let mut conn_close = false;
    let mut chunked = false;
    let mut wait_until_ms = 0u64;
    let mut headers = HeaderMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "content-length" => content_length = v.parse().ok(),
                "connection" => conn_close = v.eq_ignore_ascii_case("close"),
                "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
                "x-fluid-wait-until-ms" => wait_until_ms = v.parse().unwrap_or(0),
                _ => {}
            }
            if is_hop_by_hop(&k)
                || k == "content-length"
                || k == "transfer-encoding"
                || k == "x-fluid-wait-until-ms"
            {
                continue; // not forwarded verbatim
            }
            if let (Ok(name), Ok(val)) =
                (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v))
            {
                headers.append(name, val);
            }
        }
    }

    let (framing, reusable) = match content_length {
        Some(n) if !conn_close && !chunked => (Framing::Length(n), true),
        Some(n) => (Framing::Length(n), false), // length known but caller closes
        None => (Framing::Close, false),
    };

    Ok(Head {
        status,
        headers,
        framing,
        reusable,
        leftover,
        wait_until_ms,
    })
}

/// Read exactly `len` body bytes (some may already be in `leftover`).
pub async fn read_body_exact(conn: &mut Conn, mut have: Vec<u8>, len: usize) -> anyhow::Result<Vec<u8>> {
    have.truncate(len.min(have.len()));
    let mut buf = have;
    let mut tmp = [0u8; 16 * 1024];
    while buf.len() < len {
        let n = conn.read(&mut tmp).await?;
        anyhow::ensure!(n > 0, "upstream closed mid-body");
        let take = (len - buf.len()).min(n);
        buf.extend_from_slice(&tmp[..take]);
    }
    Ok(buf)
}

pub fn is_hop_by_hop(h: &str) -> bool {
    matches!(
        h,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}
