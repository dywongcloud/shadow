//! Wire framing: `[u64 stream_id][u8 kind][u32 len][payload]`, big-endian.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// Gateway → instance: full request (meta + body) for `stream_id`.
    Request = 1,
    /// Instance → gateway: response status + headers.
    RespHead = 4,
    /// Instance → gateway: a chunk of response body.
    RespBody = 5,
    /// Instance → gateway: end of response.
    RespEnd = 6,
    /// Liveness (stream_id = 0).
    Ping = 7,
    Pong = 8,
    /// Instance → gateway: periodic in-band metrics (stream_id = 0).
    Metrics = 9,
    /// Instance → gateway: request rejected (overloaded) for `stream_id`.
    Nack = 10,
    /// Gateway → instance: the caller abandoned `stream_id` (client disconnect or head timeout),
    /// so stop working it and release its in-flight slot now. Without it a slot stays held for as
    /// long as the local hop takes, and abandoned requests ratchet `inflight` up to
    /// `max_concurrency` until every request is nacked.
    Cancel = 11,
}

impl FrameKind {
    fn from_u8(b: u8) -> Option<FrameKind> {
        Some(match b {
            1 => FrameKind::Request,
            4 => FrameKind::RespHead,
            5 => FrameKind::RespBody,
            6 => FrameKind::RespEnd,
            7 => FrameKind::Ping,
            8 => FrameKind::Pong,
            9 => FrameKind::Metrics,
            10 => FrameKind::Nack,
            11 => FrameKind::Cancel,
            _ => return None,
        })
    }
}

/// Largest payload `read_frame` will drain while skipping a kind this build does not know. Bounds
/// the bytes one unknown frame can cost; the frame header already caps every payload at 64 MiB, so
/// this only keeps a hostile peer from making a skip read unboundedly.
const MAX_SKIP_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Frame {
    pub stream_id: u64,
    pub kind: FrameKind,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(stream_id: u64, kind: FrameKind, payload: impl Into<Bytes>) -> Frame {
        Frame {
            stream_id,
            kind,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(13 + self.payload.len());
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.push(self.kind as u8);
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Read the next frame, SKIPPING kinds this build does not recognise instead of failing.
///
/// Gateway and cell agent ship together but not atomically: during a partial roll a newer peer
/// emits a kind the older side has never seen (today, [`FrameKind::Cancel`]). Failing there kills
/// the connection, which the gateway reads as an upstream failure and turns into a reroute — so a
/// version skew would look exactly like the outage Cancel exists to stop. Draining the payload and
/// continuing keeps a mixed-version fleet serving; the unknown frame carries no work either way.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<Frame> {
    let mut hdr = [0u8; 13];
    loop {
        r.read_exact(&mut hdr).await?;
        let stream_id = u64::from_be_bytes(hdr[0..8].try_into().unwrap());
        let len = u32::from_be_bytes(hdr[9..13].try_into().unwrap()) as usize;
        let known = FrameKind::from_u8(hdr[8]);
        if known.is_none() {
            anyhow::ensure!(len <= MAX_SKIP_BYTES, "unknown frame too large to skip: {len}");
            if len > 0 {
                let mut skip = vec![0u8; len];
                r.read_exact(&mut skip).await?;
            }
            continue;
        }
        let kind = known.expect("checked above");
        anyhow::ensure!(len <= 64 * 1024 * 1024, "frame too large: {len}");
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await?;
        return Ok(Frame {
            stream_id,
            kind,
            payload: Bytes::from(payload),
        });
    }
}
