//! One persistent, multiplexed tunnel per instance: a single connection carries many concurrent
//! requests tagged by `stream_id`, plus in-band metrics and nack signals. [`TunnelClient`] is the
//! gateway side, [`TunnelServer`] the instance side; the transport is any `AsyncRead + AsyncWrite`
//! byte stream (TCP, vsock, an iroh stream).

mod client;
mod codec;
mod server;

pub use client::{TunnelClient, TunnelResponse};
pub use codec::{Frame, FrameKind};
pub use server::{set_local_connect_permits, TunnelServer};

use serde::{Deserialize, Serialize};

/// Request metadata (the body follows in the same [`FrameKind::Request`] frame payload).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReqMeta {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body_len: u32,
}

/// Response metadata, sent in a [`FrameKind::RespHead`] frame; the body follows in
/// [`FrameKind::RespBody`] frames.
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
    /// Current write-queue depth: frames enqueued for the router but not yet flushed; sustained
    /// non-zero means downstream backpressure.
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
