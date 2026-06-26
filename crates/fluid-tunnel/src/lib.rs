//! `fluid-tunnel` — one persistent, multiplexed tunnel per instance.
//!
//! This is the deeper version of Vercel's "persistent TCP tunnel": instead of a
//! pool of one-request-at-a-time keep-alive connections, a **single** connection
//! to an instance carries **many concurrent requests**, each tagged with a
//! `stream_id`, plus in-band **metrics** and **nack** (overload) signals.
//!
//! * [`TunnelClient`] — gateway side. `request()` multiplexes a request onto the
//!   shared connection and returns a streamed response; a reader task demuxes
//!   responses back to the right caller and tracks instance [`Health`].
//! * [`TunnelServer`] — instance side (the cell agent, or the mock backend's
//!   in-process bridge). Demuxes requests, proxies each to the local function
//!   over HTTP concurrently, streams responses back, emits metrics, and nacks
//!   when at capacity.
//!
//! The transport is any `AsyncRead + AsyncWrite` byte stream — TCP, vsock, or an
//! iroh P2P stream — so the same protocol distributes infra locally or globally.

mod client;
mod codec;
mod server;

pub use client::{TunnelClient, TunnelResponse};
pub use codec::{Frame, FrameKind};
pub use server::TunnelServer;

use serde::{Deserialize, Serialize};

/// Request metadata (the body follows in the same REQUEST frame payload).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReqMeta {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body_len: u32,
}

/// Response metadata, sent in a RESP_HEAD frame; body follows in RESP_BODY.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RespMeta {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// `waitUntil` window the function declared (x-fluid-wait-until-ms).
    pub wait_until_ms: u64,
}

/// In-band health/metrics the instance pushes to the router (gap #3, #14).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub inflight: u32,
    pub max_concurrency: u32,
    /// Cumulative request bytes received from the router over this tunnel (#14).
    #[serde(default)]
    pub bytes_in: u64,
    /// Cumulative response bytes written back to the router over this tunnel (#14).
    #[serde(default)]
    pub bytes_out: u64,
    /// Current write-queue depth: frames enqueued for the router but not yet
    /// flushed to the socket (#14). A sustained non-zero depth means the router /
    /// network can't drain responses as fast as the function produces them — i.e.
    /// downstream backpressure.
    #[serde(default)]
    pub queue_depth: u32,
    /// Times the write queue crossed the backpressure high-water mark (#14).
    #[serde(default)]
    pub backpressure_events: u64,
}

/// Snapshot of an instance's health as last reported over the tunnel.
#[derive(Clone, Copy, Debug, Default)]
pub struct Health {
    pub inflight: u32,
    pub max_concurrency: u32,
    pub last_update_ms: u64,
    pub alive: bool,
    /// Tunnel byte/backpressure metering (#14), last reported.
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub queue_depth: u32,
    pub backpressure_events: u64,
}
