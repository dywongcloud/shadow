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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

// Re-export the endpoint type so callers (hive-cloud) don't depend on iroh directly.
pub use iroh::Endpoint;

/// QUIC keep-alive for trunked connections — keeps an idle-but-warm connection
/// from being reaped (and re-dialed) under bursty load. Below iroh's idle timeout.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// ALPN identifying the Hive function-tunnel protocol over iroh.
pub const HIVE_ALPN: &[u8] = b"hive/tunnel/0";

/// First byte on every hive-p2p bi stream selects how the owner handles it:
/// a multiplexed `fluid-tunnel` session (HTTP request/response) or a raw byte
/// splice for upgraded connections (WebSocket). This 1-byte mode lives at the
/// hive-p2p framing layer — the `fluid-tunnel` wire protocol is unchanged, it
/// simply rides AFTER this byte on a `STREAM_TUNNEL` stream.
const STREAM_TUNNEL: u8 = 0x00;
const STREAM_RAW: u8 = 0x01;
/// Control-plane GOSSIP over the same iroh mesh: an HTTP-shaped request
/// (`[u8 method][u32 path_len][path][u32 body_len][body]`) tunneled to the peer's
/// local admin, response framed back as `[u32 len][bytes]`. Lets the control plane
/// run over authenticated QUIC instead of HTTP-over-SSH (the trust gate on the
/// connection already authenticates the peer's identity). Method: 0=GET, 1=POST.
const STREAM_GOSSIP: u8 = 0x02;
const GOSSIP_METHOD_GET: u8 = 0;
const GOSSIP_METHOD_POST: u8 = 1;
/// Cap on a single gossip frame (request path/body or response) — gossip payloads
/// are small JSON rosters; this just bounds a malformed/hostile length prefix.
const GOSSIP_MAX_FRAME: usize = 16 * 1024 * 1024;

/// Serves one gossip request: `(method, path, body) -> response body bytes`. The
/// caller (hive-cloud) wires this to dispatch onto its local admin handlers, so the
/// exact same endpoints that answer HTTP gossip answer iroh gossip.
pub type GossipHandler = Arc<
    dyn Fn(u8, String, Vec<u8>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Method code for a GET gossip request.
pub const GOSSIP_GET: u8 = GOSSIP_METHOD_GET;
/// Method code for a POST gossip request.
pub const GOSSIP_POST: u8 = GOSSIP_METHOD_POST;

async fn read_u32<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<usize> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).await?;
    Ok(u32::from_be_bytes(b) as usize)
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let n = read_u32(r).await?;
    if n > GOSSIP_MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "gossip frame too large"));
    }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).await?;
    Ok(v)
}

/// Serialize this endpoint's dialable address (direct socket addrs + relay url) to
/// JSON, so peers can learn it via gossip and dial directly — no DNS/relay
/// discovery round-trip required.
pub fn addr_json(ep: &Endpoint) -> Option<String> {
    serde_json::to_string(&ep.addr()).ok()
}

/// Extract the iroh `EndpointId` (cryptographic node identity, as a string) from an
/// `addr_json` blob (a serialized `EndpointAddr` learned via gossip). Used to build
/// the peer-trust allowlist (#20) from the fleet roster the node already knows.
pub fn endpoint_id_from_addr_json(addr_json: &str) -> Option<String> {
    serde_json::from_str::<EndpointAddr>(addr_json).ok().map(|a| a.id.to_string())
}

/// Shared, gossip-updated set of trusted peer `EndpointId`s for P2P admission (#20).
pub type TrustSet = Arc<std::sync::RwLock<std::collections::HashSet<String>>>;

/// Whether `id` is admitted by the trust set. A connection is allowed iff its
/// remote endpoint id is present. Pure for testability.
pub fn peer_trusted(trust: &TrustSet, id: &str) -> bool {
    trust.read().map(|s| s.contains(id)).unwrap_or(false)
}

/// Relay-vs-direct byte/connection accounting for the mesh trunks (#23).
#[derive(Default, Clone, Debug)]
pub struct RelayStats {
    pub relayed_conns: usize,
    pub direct_conns: usize,
    pub relayed_bytes_tx: u64,
    pub relayed_bytes_rx: u64,
    pub direct_bytes_tx: u64,
    pub direct_bytes_rx: u64,
}

/// A response collected from a single P2P tunnel request (gateway side).
pub struct TunnelResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A streamed P2P response: head available immediately, body delivered as chunks
/// via [`recv`](TunnelStream::recv) as they arrive (gateway side). Completes when
/// the owner finishes the response — letting the caller forward an SSE / chunked
/// body incrementally instead of buffering it whole.
pub struct TunnelStream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    body: tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>,
    /// Kept alive so the tunnel's QUIC streams stay open until the body is fully
    /// consumed — dropping the client early would reset the send stream and can
    /// truncate the response. The owning [`TunnelStream`] outlives the body drain.
    _client: fluid_tunnel::TunnelClient,
}

impl TunnelStream {
    /// Next body chunk, or `None` at the end of the response body.
    pub async fn recv(&mut self) -> Option<bytes::Bytes> {
        self.body.recv().await
    }
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

    /// Relay cost accounting (#23): classify each live trunk as relay vs direct via
    /// iroh's `remote_addr()` and sum its QUIC byte counters (`udp_tx`/`udp_rx`).
    /// RELAYED bytes transit a relay server — a real $ cost and a latency/SPOF
    /// signal — so surfacing them shows how much mesh traffic isn't going direct
    /// peer-to-peer (and whether holepunching is succeeding).
    pub async fn relay_stats(&self) -> RelayStats {
        let mut s = RelayStats::default();
        let map = self.trunks.lock().await;
        for t in map.values() {
            let cs = t.conn.stats();
            let (tx, rx) = (cs.udp_tx.bytes, cs.udp_rx.bytes);
            // A connection with ANY direct (IP) path has holepunched — its traffic
            // goes peer-to-peer. A relay-only connection (no IP path) is costing
            // relay bandwidth for all its bytes.
            let has_direct = t.conn.paths().iter().any(|p| p.is_ip());
            if has_direct {
                s.direct_conns += 1;
                s.direct_bytes_tx += tx;
                s.direct_bytes_rx += rx;
            } else {
                s.relayed_conns += 1;
                s.relayed_bytes_tx += tx;
                s.relayed_bytes_rx += rx;
            }
        }
        s
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
        // Buffered helper for callers that need the whole body in memory: drive the
        // streaming path and drain its chunk channel.
        let mut s = self
            .request_stream(node_id, addr_json, method, path, headers, body)
            .await?;
        let mut buf = Vec::new();
        while let Some(chunk) = s.recv().await {
            buf.extend_from_slice(&chunk);
        }
        Ok(TunnelResp { status: s.status, headers: s.headers, body: buf })
    }

    /// Streaming variant of [`request`]: returns the response head plus a receiver
    /// that yields body chunks as the owner produces them — no buffering. The
    /// caller (e.g. the edge gateway) can wrap the receiver in an
    /// `axum::body::Body::from_stream` so SSE / chunked responses arrive
    /// incrementally cross-node. Pre-send (`open_bi`) failures retry once exactly
    /// as [`request`] did; a failure after the request is on the wire is returned.
    pub async fn request_stream(
        &self,
        node_id: &str,
        addr_json: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<TunnelStream> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let conn = self.acquire(node_id, addr_json).await?;
            // `open_bi` is PRE-SEND: an error means no request bytes left this node.
            let (mut send, recv) = match conn.open_bi().await {
                Ok(pair) => pair,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue; // re-dial + resend exactly once
                    }
                    return Err(e.into());
                }
            };
            // Select the multiplexed tunnel mode for the owner's dispatcher.
            send.write_all(&[STREAM_TUNNEL]).await?;
            send.flush().await?;
            // Past this point the request may be on the wire → do NOT retry in-call.
            let client = fluid_tunnel::TunnelClient::new(tokio::io::join(recv, send));
            // `to_vec` per attempt so a retry still owns the headers.
            let resp = client.request(method, path, headers.to_vec(), body).await?;
            // Move the client INTO the stream so it lives until the body is drained
            // (keeping the QUIC streams open); see `TunnelStream::_client`.
            return Ok(TunnelStream {
                status: resp.status,
                headers: resp.headers,
                body: resp.body,
                _client: client,
            });
        }
    }

    /// Open a RAW bidirectional byte stream to a peer over its trunk, for upgraded
    /// connections (WebSocket) where HTTP request/response framing must be
    /// bypassed. The owner splices these bytes straight to its local target. Same
    /// pre-send retry-once semantics as [`request`].
    pub async fn open_raw(&self, node_id: &str, addr_json: &str) -> Result<P2pStream> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let conn = self.acquire(node_id, addr_json).await?;
            let (mut send, recv) = match conn.open_bi().await {
                Ok(pair) => pair,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
            };
            send.write_all(&[STREAM_RAW]).await?;
            send.flush().await?;
            return Ok(tokio::io::join(recv, send));
        }
    }

    /// Control-plane gossip over the mesh (#unify): tunnel an HTTP-shaped request to
    /// the peer's local admin over a NEW bi stream on the reused trunk, and return
    /// the response body bytes. `method` is [`GOSSIP_GET`]/[`GOSSIP_POST`]. Re-dials
    /// once if the cached trunk is dead.
    pub async fn gossip_request(
        &self,
        node_id: &str,
        addr_json: &str,
        method: u8,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let conn = self.acquire(node_id, addr_json).await?;
            let (mut send, mut recv) = match conn.open_bi().await {
                Ok(pair) => pair,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
            };
            send.write_all(&[STREAM_GOSSIP, method]).await?;
            send.write_all(&(path.len() as u32).to_be_bytes()).await?;
            send.write_all(path.as_bytes()).await?;
            send.write_all(&(body.len() as u32).to_be_bytes()).await?;
            send.write_all(body).await?;
            send.flush().await?;
            let _ = send.finish();
            // Response: [u32 len][bytes].
            let resp = read_frame(&mut recv).await?;
            return Ok(resp);
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
/// A bootstrap seed peer: a stable PUBLIC node a fresh/wiped node can rendezvous
/// with over iroh with zero prior state and no SSH. `node_id` is the iroh
/// `EndpointId` (hex); `addr_json` is a serialized `EndpointAddr` (id + optional
/// direct addrs + relay). Seeds should be the fixed-identity public FC nodes, never
/// NAT'd Macs. Strings (not iroh types) so callers stay iroh-free.
#[derive(Clone, Debug)]
pub struct SeedPeer {
    pub node_id: String,
    pub addr_json: String,
}

/// Parse one `HIVE_BOOTSTRAP_PEERS` entry into an `EndpointAddr`. Forms:
///   `<nodeid>`                         — NodeId only (address resolved via discovery)
///   `<nodeid>@<ip:port>[+<ip:port>…]`  — NodeId + direct address hint(s)
///   `…|<relay_url>`                    — optional home-relay hint
/// The NodeId alone is sufficient (the seed self-publishes via the n0 pkarr/DNS
/// discovery the N0 preset enables); addrs/relay are hints for faster/offline dial.
fn parse_seed_addr(entry: &str) -> Option<EndpointAddr> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    let (left, relay) = entry.split_once('|').map(|(l, r)| (l, Some(r.trim()))).unwrap_or((entry, None));
    let (id_str, addrs_str) =
        left.split_once('@').map(|(i, a)| (i.trim(), Some(a.trim()))).unwrap_or((left.trim(), None));
    let id: iroh::EndpointId = id_str.parse().ok()?;
    let mut taddrs: Vec<iroh::TransportAddr> = Vec::new();
    if let Some(addrs) = addrs_str {
        for a in addrs.split('+').map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(sa) = a.parse::<std::net::SocketAddr>() {
                taddrs.push(iroh::TransportAddr::Ip(sa));
            }
        }
    }
    if let Some(r) = relay {
        if let Ok(url) = r.parse::<iroh::RelayUrl>() {
            taddrs.push(iroh::TransportAddr::Relay(url));
        }
    }
    Some(EndpointAddr::from_parts(id, taddrs))
}

/// Parse a comma-separated `HIVE_BOOTSTRAP_PEERS` list into [`SeedPeer`]s (skipping
/// unparseable entries).
pub fn parse_bootstrap_seeds(csv: &str) -> Vec<SeedPeer> {
    csv.split(',')
        .filter_map(|e| {
            let addr = parse_seed_addr(e)?;
            let node_id = addr.id.to_string();
            let addr_json = serde_json::to_string(&addr).ok()?;
            Some(SeedPeer { node_id, addr_json })
        })
        .collect()
}

pub async fn bind() -> Result<Endpoint> {
    bind_with_key(None, &[]).await
}

/// Bind an iroh endpoint with a PERSISTENT identity loaded from `key_path` (32 raw
/// secret-key bytes). If the file is absent/corrupt, a new key is generated and
/// saved (0600). A stable secret key ⇒ a stable `EndpointId` across process
/// restarts, so peers' cached addresses stay valid and the mesh re-rendezvouses
/// over iroh without re-bootstrapping. `None` ⇒ ephemeral identity (tests/dev).
///
/// `seeds` are registered with a static address-lookup provider so the endpoint can
/// dial a seed BY NodeId even with no cached/learned address — the cold-start
/// rendezvous path. Dynamic resolution + self-publish come from the n0 pkarr/DNS
/// discovery the `N0` preset already enables (forward-compat: that discovery's
/// server is the n0 default today and can later point at the platform's own DNS via
/// a custom `DnsAddressLookup`/`PkarrPublisher` at this same `address_lookup()` hook).
pub async fn bind_with_key(key_path: Option<std::path::PathBuf>, seeds: &[SeedPeer]) -> Result<Endpoint> {
    bind_full(key_path, seeds, &[], true).await
}

/// Like [`bind_with_key`] but also registers self-hosted discovery (Seer): for each
/// URL in `discovery_urls`, add a pkarr PUBLISHER (self-publish our address keyed by
/// NodeId) and a pkarr RESOLVER (resolve peers' NodeIds) pointed at that Seer relay.
///
/// `n0_discovery` controls n0's public pkarr/DNS:
///   * `true` (default) — keep n0 discovery; Seer is ADDED alongside it (additive,
///     no regression — the mesh works if Seer is down).
///   * `false` — drop n0 discovery (use the `Minimal` preset), relying on Seer for
///     NodeId↔address resolution. The n0 RELAY is still kept (`default_relay_mode`)
///     so NAT'd nodes stay reachable; wiring a self-hosted relay is a separate step.
pub async fn bind_full(
    key_path: Option<std::path::PathBuf>,
    seeds: &[SeedPeer],
    discovery_urls: &[String],
    n0_discovery: bool,
) -> Result<Endpoint> {
    let tc = QuicTransportConfig::builder()
        .keep_alive_interval(KEEPALIVE)
        .build();
    let mut builder = if n0_discovery {
        // n0 discovery (pkarr/DNS) + n0's relays (unless HIVE_RELAY_URLS overrides below).
        Endpoint::builder(N0)
    } else {
        // No n0 pkarr/DNS; keep a relay so NAT'd nodes remain reachable.
        Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::endpoint::default_relay_mode())
    }
    .alpns(vec![HIVE_ALPN.to_vec()])
    .transport_config(tc);
    // Self-hosted relays (HIVE_RELAY_URLS): when set, NAT-traversal + relayed data
    // paths transit OUR iroh-relay infra instead of n0's — applied in BOTH branches,
    // overriding the preset's relay map. Direct hole-punching is unchanged (relays stay
    // fallback-only). Unset → keep prior n0-relay behavior so existing deploys don't break.
    if let Some(map) = relay_map_from_env() {
        let n = map.len();
        builder = builder.relay_mode(iroh::RelayMode::Custom(map));
        tracing::info!(relays = n, "using self-hosted iroh relays (HIVE_RELAY_URLS)");
    }
    if let Some(path) = key_path {
        builder = builder.secret_key(load_or_create_secret(&path));
    }
    let seed_addrs: Vec<EndpointAddr> =
        seeds.iter().filter_map(|s| serde_json::from_str::<EndpointAddr>(&s.addr_json).ok()).collect();
    if !seed_addrs.is_empty() {
        builder = builder.address_lookup(iroh::address_lookup::MemoryLookup::from_endpoint_info(seed_addrs));
    }
    for raw in discovery_urls {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match url::Url::parse(raw) {
            Ok(u) => {
                builder = builder
                    .address_lookup(iroh::address_lookup::PkarrPublisher::builder(u.clone()))
                    .address_lookup(iroh::address_lookup::PkarrResolver::builder(u));
            }
            Err(e) => tracing::warn!(url = raw, error = %e, "invalid HIVE_DISCOVERY_DNS entry; skipped"),
        }
    }
    let ep = builder.bind().await?;
    Ok(ep)
}

/// Build a self-hosted relay map from `HIVE_RELAY_URLS` (comma-separated relay URLs,
/// e.g. `https://relay-us.example.com,https://relay-ap.example.com`). Returns `None`
/// when unset/empty so callers keep the default (n0) relay behavior. Reuses iroh's
/// `RelayUrl` parsing (same as seed-addr relay handling); bad entries are skipped.
fn relay_map_from_env() -> Option<iroh::RelayMap> {
    let raw = std::env::var("HIVE_RELAY_URLS").ok()?;
    let urls: Vec<iroh::RelayUrl> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|u| match u.parse::<iroh::RelayUrl>() {
            Ok(url) => Some(url),
            Err(e) => {
                tracing::warn!(url = u, error = %e, "invalid HIVE_RELAY_URLS entry; skipped");
                None
            }
        })
        .collect();
    if urls.is_empty() {
        return None;
    }
    Some(iroh::RelayMap::from_iter(urls))
}

/// Load a persistent iroh secret key from `path`, or generate + save one (0600).
fn load_or_create_secret(path: &std::path::Path) -> iroh::SecretKey {
    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return iroh::SecretKey::from_bytes(&arr);
        }
        tracing::warn!(?path, "iroh key file malformed; regenerating");
    }
    let sk = iroh::SecretKey::generate();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(path, sk.to_bytes()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(?path, "generated + persisted iroh identity");
    } else {
        tracing::warn!(?path, "could not persist iroh key; identity will be ephemeral");
    }
    sk
}

/// The combined send+recv halves of a P2P stream, usable as one duplex stream.
pub type P2pStream = tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>;

/// Dial a remote endpoint and open a bidirectional stream for one tunnel. Writes
/// the `STREAM_TUNNEL` mode byte so the owner's dispatcher treats it as a
/// `fluid-tunnel` session (the caller wraps the returned stream in a `TunnelClient`).
pub async fn dial(ep: &Endpoint, addr: impl Into<EndpointAddr>) -> Result<P2pStream> {
    let conn = ep.connect(addr, HIVE_ALPN).await?;
    let (mut send, recv) = conn.open_bi().await?;
    send.write_all(&[STREAM_TUNNEL]).await?;
    send.flush().await?;
    Ok(tokio::io::join(recv, send))
}

/// Accept P2P connections forever; serve every bidirectional stream according to
/// its leading mode byte: `STREAM_TUNNEL` → a `fluid-tunnel` session proxied to
/// the local server at `local_http`; `STREAM_RAW` → a raw byte splice to a fresh
/// connection to `local_http` (for upgraded/WebSocket connections). This is the
/// instance (owner) side.
pub async fn serve_tunnels(
    ep: Endpoint,
    local_http: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
) {
    while let Some(incoming) = ep.accept().await {
        let local = local_http.clone();
        let trust = trust.clone();
        let gossip = gossip.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            // #20 peer trust/attestation: when an allowlist is configured, only
            // admit endpoints whose cryptographic iroh identity (from their TLS
            // cert, unspoofable) is trusted. Dropping `conn` closes the QUIC
            // connection, so an untrusted peer can open no streams.
            if let Some(trust) = &trust {
                let rid = conn.remote_id().to_string();
                if !peer_trusted(trust, &rid) {
                    tracing::warn!(peer = %rid, "rejected untrusted P2P peer (#20 peer trust)");
                    return;
                }
            }
            // One inbound connection → many bi streams, each dispatched by mode.
            // This already handles a pooled peer that multiplexes many requests
            // over one connection (each request is a new stream here).
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let local = local.clone();
                let gossip = gossip.clone();
                tokio::spawn(async move {
                    // Read the 1-byte mode selector that prefixes every stream.
                    let mut mode = [0u8; 1];
                    if recv.read_exact(&mut mode).await.is_err() {
                        return;
                    }
                    match mode[0] {
                        STREAM_GOSSIP => {
                            if let Some(h) = gossip {
                                serve_gossip(recv, send, h).await;
                            }
                        }
                        STREAM_RAW => raw_splice(tokio::io::join(recv, send), &local).await,
                        _ => {
                            fluid_tunnel::TunnelServer::serve(
                                tokio::io::join(recv, send),
                                local,
                                max_concurrency,
                            )
                            .await
                        }
                    }
                });
            }
        });
    }
}

/// Server side of a [`STREAM_GOSSIP`] stream: read the framed `(method, path,
/// body)` request, run the caller-provided handler (which dispatches onto the local
/// admin), and frame the response back. The mode byte has already been consumed.
async fn serve_gossip<R, W>(mut recv: R, mut send: W, handler: GossipHandler)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut m = [0u8; 1];
    if recv.read_exact(&mut m).await.is_err() {
        return;
    }
    let path = match read_frame(&mut recv).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return,
    };
    let body = match read_frame(&mut recv).await {
        Ok(b) => b,
        Err(_) => return,
    };
    let resp = handler(m[0], path, body).await;
    let len = (resp.len() as u32).to_be_bytes();
    let _ = send.write_all(&len).await;
    let _ = send.write_all(&resp).await;
    let _ = send.flush().await;
}

/// Splice a raw P2P stream to a fresh TCP connection to `local_http`, copying
/// bytes both ways until either side closes. Used for upgraded (WebSocket)
/// connections, which carry their own framing and must bypass HTTP parsing.
async fn raw_splice(mut stream: P2pStream, local_http: &str) {
    let mut tcp = match tokio::net::TcpStream::connect(local_http).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(local = %local_http, error = %e, "raw splice: local connect failed");
            return;
        }
    };
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
}

/// Assert at compile time that a `P2pStream` satisfies the tunnel transport bound.
fn _assert_duplex<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>() {}
#[allow(dead_code)]
fn _check() {
    _assert_duplex::<P2pStream>();
}

#[cfg(test)]
mod trust_tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::RwLock;

    #[test]
    fn peer_trust_allowlist_admits_only_known_ids() {
        let set: TrustSet = Arc::new(RwLock::new(HashSet::new()));
        // Empty allowlist → nothing is trusted (fail-closed when enforcing).
        assert!(!peer_trusted(&set, "node-abc"));
        set.write().unwrap().insert("node-abc".to_string());
        assert!(peer_trusted(&set, "node-abc"), "listed id admitted");
        assert!(!peer_trusted(&set, "node-xyz"), "unlisted id rejected");
    }

    #[test]
    fn endpoint_id_from_garbage_is_none() {
        assert_eq!(endpoint_id_from_addr_json("not json"), None);
        assert_eq!(endpoint_id_from_addr_json("{}"), None);
    }

    #[test]
    fn parse_bootstrap_seeds_forms() {
        // A valid ed25519 public key (iroh EndpointId), 64 hex chars.
        let id = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";
        // NodeId only, NodeId@addr(+addr), with relay, plus garbage that must drop.
        let csv = format!(
            "{id} , {id}@1.2.3.4:9000+5.6.7.8:9001 , {id}|https://relay.example/ , not-a-key , ",
        );
        let seeds = super::parse_bootstrap_seeds(&csv);
        assert_eq!(seeds.len(), 3, "3 valid entries, garbage+empty dropped");
        for s in &seeds {
            assert_eq!(s.node_id, id, "node_id extracted");
            // addr_json round-trips to an EndpointAddr with the right id.
            assert_eq!(super::endpoint_id_from_addr_json(&s.addr_json).as_deref(), Some(id));
        }
        // The @addr entry carries the direct addresses; the bare entry doesn't.
        let with_addrs = &seeds[1].addr_json;
        assert!(with_addrs.contains("1.2.3.4") && with_addrs.contains("5.6.7.8"), "direct addrs preserved: {with_addrs}");
        assert!(super::parse_bootstrap_seeds("").is_empty());
    }

    #[test]
    fn persistent_secret_is_stable_across_loads() {
        let dir = std::env::temp_dir().join(format!("iroh-key-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("iroh_secret.key");
        // First call generates + persists.
        let k1 = super::load_or_create_secret(&path).public();
        assert!(path.exists(), "key file written");
        // Second call loads the SAME key (stable identity).
        let k2 = super::load_or_create_secret(&path).public();
        assert_eq!(k1, k2, "persisted identity must be stable across loads");
        // Malformed file → regenerate (different key), no panic.
        std::fs::write(&path, b"short").unwrap();
        let k3 = super::load_or_create_secret(&path).public();
        assert_ne!(k3, k1, "malformed key file is regenerated");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
