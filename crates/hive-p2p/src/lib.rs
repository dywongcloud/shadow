//! `hive-p2p` — distribute the infra over a peer-to-peer QUIC mesh with iroh.
//!
//! The Fluid tunnel protocol ([`fluid_tunnel`]) is transport-agnostic, so we can
//! carry it over an iroh P2P connection: an instance on node B is reachable from
//! node A's gateway by **node id** (a public key), with NAT traversal / relay
//! fallback handled by iroh. This turns the single-machine platform into a
//! distributed one — boxes and instances can live anywhere.
//!
//! * [`bind`] — start an iroh endpoint speaking the Hive ALPN.
//! * [`serve_tunnels`] — accept P2P connections and serve each as a tunnel to a
//!   local function (the instance side).
//! * [`dial`] — open a P2P connection to a remote instance and return a duplex
//!   byte stream a [`fluid_tunnel::TunnelClient`] can drive (the gateway side).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use iroh::{endpoint::presets::N0, endpoint::Connection, endpoint::QuicTransportConfig, EndpointAddr};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

// Re-export the endpoint type so callers (hive-cloud) don't depend on iroh directly.
pub use iroh::Endpoint;

/// QUIC keep-alive for trunked connections — keeps an idle-but-warm connection
/// from being reaped (and re-dialed) under bursty load. Below iroh's idle timeout.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// ALPN identifying the Hive function-tunnel protocol over iroh.
pub const HIVE_ALPN: &[u8] = b"hive/tunnel/0";

/// Serialize this endpoint's dialable address (direct socket addrs + relay url) to
/// JSON, so peers can learn it via gossip and dial directly — no DNS/relay
/// discovery round-trip required.
pub fn addr_json(ep: &Endpoint) -> Option<String> {
    serde_json::to_string(&ep.addr()).ok()
}

/// A response collected from a single P2P tunnel request (gateway side).
pub struct TunnelResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A cached, reusable QUIC connection to one peer — the "trunk". The pool OWNS the
/// `Connection`; dropping it closes the QUIC connection. We cache the connection,
/// **never a `TunnelClient`**: a `TunnelClient` is one byte stream, so sharing it
/// would funnel every request through a single QUIC stream and head-of-line-block
/// them on a lossy WAN. Each request opens its own bi stream over this connection.
struct Trunk {
    conn: Connection,
}

/// Connection pool + multiplexer for the cross-node mesh path. Keeps ONE persistent
/// iroh QUIC connection per peer (`node_id`) and opens a NEW bi STREAM per request,
/// instead of dialing a fresh connection (and paying a handshake / holepunch) each
/// time. Directed dial + gossip discovery are unchanged; only the connection
/// lifecycle is pooled.
pub struct PeerPool {
    ep: Endpoint,
    trunks: Mutex<HashMap<String, Trunk>>,
    opened: AtomicU64,
    reused: AtomicU64,
}

impl PeerPool {
    /// Build a pool over a bound endpoint (cheap to clone — `Arc` inside).
    pub fn new(ep: Endpoint) -> Arc<PeerPool> {
        Arc::new(PeerPool {
            ep,
            trunks: Mutex::new(HashMap::new()),
            opened: AtomicU64::new(0),
            reused: AtomicU64::new(0),
        })
    }

    /// `(opened, reused)` connection counters — for diagnostics and tests.
    pub fn stats(&self) -> (u64, u64) {
        (self.opened.load(Ordering::Relaxed), self.reused.load(Ordering::Relaxed))
    }

    /// Get a live connection to `node_id`: reuse the warm trunk, else dial a new one.
    /// The map lock is taken ONLY to look up / insert — **never** held across the
    /// (possibly slow, holepunching) `connect`, so a slow first-contact to one peer
    /// can't serialize requests to the others.
    async fn acquire(&self, node_id: &str, addr_json: &str) -> Result<Connection> {
        // Fast path: reuse a still-live trunk.
        {
            let map = self.trunks.lock().await;
            if let Some(t) = map.get(node_id) {
                if t.conn.close_reason().is_none() {
                    let conn = t.conn.clone();
                    drop(map);
                    self.reused.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(node_id, "trunk reused");
                    return Ok(conn);
                }
            }
        } // lock dropped: trunk missing or dead → dial below

        // Slow path: dial OUTSIDE the lock.
        let addr: EndpointAddr = serde_json::from_str(addr_json)?;
        let conn = self.ep.connect(addr, HIVE_ALPN).await?;

        // Re-lock to publish the trunk. A concurrent first-contact may double-dial;
        // that's fine — last insert wins and the extra connection drops (closes).
        {
            let mut map = self.trunks.lock().await;
            map.insert(node_id.to_string(), Trunk { conn: conn.clone() });
        }
        self.opened.fetch_add(1, Ordering::Relaxed);
        tracing::info!(node_id, "trunk opened");
        Ok(conn)
    }

    /// Drop the cached trunk for a peer so the next request re-dials.
    async fn evict(&self, node_id: &str) {
        self.trunks.lock().await.remove(node_id);
    }

    /// Cross-node gateway-side call: send ONE HTTP request over a NEW bi stream on
    /// the peer's REUSED trunk, and return the full response.
    ///
    /// Retries ONCE on a **pre-send** failure (`open_bi` error, or a dead trunk) —
    /// no request bytes left this node, so a redial + resend is safe even for a
    /// non-idempotent method. A failure **after** the request is written is NOT
    /// retried here: it's returned so the caller's candidate failover decides,
    /// rather than silently re-executing a POST. Liveness is judged on the
    /// `Connection` (`close_reason()`), with `open_bi()` failure authoritative.
    pub async fn request(
        &self,
        node_id: &str,
        addr_json: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<TunnelResp> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let conn = self.acquire(node_id, addr_json).await?;
            // `open_bi` is PRE-SEND: an error means no request bytes left this node.
            let (send, recv) = match conn.open_bi().await {
                Ok(pair) => pair,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue; // re-dial + resend exactly once
                    }
                    return Err(e.into());
                }
            };
            // Past this point the request may be on the wire → do NOT retry in-call.
            let client = fluid_tunnel::TunnelClient::new(tokio::io::join(recv, send));
            // `to_vec` per attempt so a retry still owns the headers.
            let resp = client.request(method, path, headers.to_vec(), body).await?;
            let mut buf = Vec::new();
            let mut rx = resp.body;
            while let Some(chunk) = rx.recv().await {
                buf.extend_from_slice(&chunk);
            }
            return Ok(TunnelResp { status: resp.status, headers: resp.headers, body: buf });
        }
    }

    /// Test/diagnostic helper: close + drop a peer's trunk so the next request
    /// re-dials a fresh connection.
    pub async fn close_peer(&self, node_id: &str) {
        if let Some(t) = self.trunks.lock().await.remove(node_id) {
            t.conn.close(0u32.into(), b"closed by pool");
        }
    }

    /// Test/diagnostic helper: forcibly close a peer's cached connection IN PLACE
    /// (the trunk stays in the map). This severs the real QUIC connection without
    /// evicting it, so the next request must DETECT the dead trunk — via
    /// `close_reason()` or an `open_bi()` failure — and re-dial. Returns whether a
    /// trunk was cached, and (for assertions) whether that handle now reports closed.
    pub async fn sever_peer(&self, node_id: &str) -> bool {
        let map = self.trunks.lock().await;
        match map.get(node_id) {
            Some(t) => {
                t.conn.close(0u32.into(), b"severed by test");
                // Same Arc-backed connection state, so the cached clone observes it.
                t.conn.close_reason().is_some()
            }
            None => false,
        }
    }
}

/// Bind an iroh endpoint that can accept Hive tunnels (N0 preset = relay + DNS
/// discovery so peers are reachable by endpoint id from anywhere). A QUIC
/// keep-alive is set so pooled (trunked) connections stay warm between requests.
pub async fn bind() -> Result<Endpoint> {
    let tc = QuicTransportConfig::builder()
        .keep_alive_interval(KEEPALIVE)
        .build();
    let ep = Endpoint::builder(N0)
        .alpns(vec![HIVE_ALPN.to_vec()])
        .transport_config(tc)
        .bind()
        .await?;
    Ok(ep)
}

/// The combined send+recv halves of a P2P stream, usable as one duplex stream.
pub type P2pStream = tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>;

/// Dial a remote endpoint and open a bidirectional stream for one tunnel.
pub async fn dial(ep: &Endpoint, addr: impl Into<EndpointAddr>) -> Result<P2pStream> {
    let conn = ep.connect(addr, HIVE_ALPN).await?;
    let (send, recv) = conn.open_bi().await?;
    Ok(tokio::io::join(recv, send))
}

/// Accept P2P connections forever; serve every bidirectional stream as a tunnel
/// to the local function server at `local_http`. This is the instance side.
pub async fn serve_tunnels(ep: Endpoint, local_http: String, max_concurrency: u32) {
    while let Some(incoming) = ep.accept().await {
        let local = local_http.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            // One inbound connection → many bi streams, each served as its own
            // tunnel. This already handles a pooled peer that multiplexes many
            // requests over one connection (each request is a new stream here).
            while let Ok((send, recv)) = conn.accept_bi().await {
                let stream = tokio::io::join(recv, send);
                let local = local.clone();
                tokio::spawn(async move {
                    fluid_tunnel::TunnelServer::serve(stream, local, max_concurrency).await;
                });
            }
        });
    }
}

/// Assert at compile time that a `P2pStream` satisfies the tunnel transport bound.
fn _assert_duplex<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>() {}
#[allow(dead_code)]
fn _check() {
    _assert_duplex::<P2pStream>();
}
