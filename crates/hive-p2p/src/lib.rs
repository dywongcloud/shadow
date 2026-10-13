//! Distribute the infra over a peer-to-peer QUIC mesh with iroh: the transport-agnostic Fluid
//! tunnel protocol ([`fluid_tunnel`]) rides an iroh connection, so an instance on node B is
//! reachable from node A's gateway by node id with NAT traversal / relay fallback. [`bind`],
//! [`serve_tunnels`] (instance side), [`dial`] (gateway side).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::{
    endpoint::presets::N0, endpoint::Connection, endpoint::QuicTransportConfig, EndpointAddr,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

pub use iroh::Endpoint;

/// Public mainline-DHT address lookup (`bind_full` registers it; `--dht-probe` and
/// `GET /v1/mesh/discovery` read it); see its module docs for what becomes publicly resolvable.
pub mod dht;
pub mod establish;
pub mod private_path;

pub use establish::{establish_stats, ClassCounts, ClassLimits, EstablishStats, PeerBudget};
use establish::{ConnClass, ConnectFailure, Event as EstablishEvent};

/// Connection-level QUIC idle timeout for trunked connections. No keep-alive override:
/// `QuicTransportConfig` already installs iroh's tuned 5 s keep-alives against a 15 s path idle
/// timeout.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent inbound bi-streams one peer connection may hold open (`HIVE_P2P_MAX_STREAMS`): stated
/// explicitly, equal to the `max_concurrency` request bound since one request rides one stream, and
/// enforced by QUIC flow control so a peer at the ceiling is back-pressured, never dropped.
fn max_streams() -> u32 {
    std::env::var("HIVE_P2P_MAX_STREAMS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(256)
}

/// Concurrently-served inbound FLEET connections per node — a blast-radius backstop far above any
/// legitimate steady state; `0` disables the cap.
fn max_inbound_conns() -> usize {
    std::env::var("HIVE_P2P_MAX_CONNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(512)
}

/// ALPN identifying the Hive function-tunnel protocol over iroh.
pub const HIVE_ALPN: &[u8] = b"hive/tunnel/0";

/// ALPN for browser-tab peers — a low-trust surface disjoint from [`HIVE_ALPN`]'s mode-byte dispatch
/// (`serve_browser_conn` has no gossip/join/raw arms); re-exported from `hive-browser-proto` so the
/// two halves cannot drift.
pub use hive_browser_proto::BROWSER_ALPN;

use hive_browser_proto::{
    check_len_for, encode_invoke, encode_request, reset as browser_reset, Op,
    BROWSER_MAX_CRR_FRAME, BROWSER_MAX_ECHO, BROWSER_MAX_FRAME, FUNCTION_DIGEST_LEN,
};

/// Concurrently-served `hive/browser/0` connections — a budget SEPARATE from [`max_inbound_conns`]
/// so a flooding browser peer cannot exhaust the fleet-trunk pool; lower, since browsers are
/// numerous, low-trust and cheap to refuse.
fn max_browser_conns() -> usize {
    std::env::var("HIVE_P2P_BROWSER_MAX_CONNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(128)
}

/// Concurrent inbound HANDSHAKES whose ClientHello could not be classified from the first Initial
/// (every hybrid X25519MLKEM768 dial): held here only across the deadline-bounded handshake, then
/// the negotiated ALPN decides the budget. `0` = uncapped.
fn max_pending_conns() -> usize {
    std::env::var("HIVE_P2P_PENDING_MAX_CONNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(256)
}

/// FLEET connections one remote endpoint may hold, checked when a NEW connection is admitted
/// (after the handshake); over the cap the new connection is closed, an established trunk is
/// never evicted. `0` = uncapped.
fn max_conns_per_endpoint() -> usize {
    std::env::var("HIVE_P2P_MAX_CONNS_PER_ENDPOINT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(16)
}

/// Wall-clock bound on ONE inbound accept (the noq handshake plus iroh's unbounded
/// `register_connection`): default [`IDLE_TIMEOUT`] + 10 s, deliberately ABOVE the idle timeout so
/// a handshake the remote abandoned ends as a handshake error and is never counted `accept_stuck`.
pub(crate) fn accept_deadline() -> Duration {
    env_ms(
        "HIVE_P2P_ACCEPT_DEADLINE_MS",
        (IDLE_TIMEOUT + Duration::from_secs(10)).as_millis() as u64,
    )
}

/// Application close code for a fleet connection refused AFTER its handshake (class budget or
/// per-endpoint cap full); same number as the browser protocol's `OVERLOADED`, and a dialer
/// treats it like a pre-handshake refusal.
const FLEET_CLOSE_OVERLOADED: u32 = browser_reset::OVERLOADED;

/// Whether a fleet connection's close is this crate's post-handshake refusal.
fn is_overload_close(error: &iroh::endpoint::ConnectionError) -> bool {
    matches!(
        error,
        iroh::endpoint::ConnectionError::ApplicationClosed(close)
            if close.error_code == iroh::endpoint::VarInt::from_u32(FLEET_CLOSE_OVERLOADED)
    )
}

const BROWSER_MAX_ACTIVE_STREAMS: usize = 32;
const BROWSER_MAX_STREAMS_PER_ENDPOINT: usize = 8;
const BROWSER_MAX_STREAMS_PER_CONNECTION: usize = 8;
const BROWSER_MAX_CONNECTIONS_PER_ENDPOINT: usize = 4;
const BROWSER_MAX_INFLIGHT_BYTES: usize = 32 << 20;
const BROWSER_MAX_OUTBOUND_TRUNKS: usize = 128;
const BROWSER_MAX_OUTBOUND_WAITERS: usize = 256;
const BROWSER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BROWSER_STREAM_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const BROWSER_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

fn max_browser_outbound_trunks() -> usize {
    std::env::var("HIVE_P2P_BROWSER_OUTBOUND_TRUNKS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(BROWSER_MAX_OUTBOUND_TRUNKS)
}

type BrowserCounts = Arc<std::sync::Mutex<HashMap<String, usize>>>;

struct BrowserCountGuard {
    key: String,
    counts: BrowserCounts,
}

impl BrowserCountGuard {
    fn acquire(counts: BrowserCounts, key: String, limit: usize) -> Option<Self> {
        let mut current = counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = current.entry(key.clone()).or_default();
        if *count >= limit {
            return None;
        }
        *count += 1;
        drop(current);
        Some(Self { key, counts })
    }
}

impl Drop for BrowserCountGuard {
    fn drop(&mut self) {
        let mut current = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = current.get_mut(&self.key).is_some_and(|count| {
            *count -= 1;
            *count == 0
        });
        if remove {
            current.remove(&self.key);
        }
    }
}

#[derive(Clone)]
struct BrowserInboundResources {
    streams: Arc<tokio::sync::Semaphore>,
    bytes: Arc<tokio::sync::Semaphore>,
    peer_streams: BrowserCounts,
    peer_connections: BrowserCounts,
}

impl BrowserInboundResources {
    fn new() -> Self {
        Self {
            streams: Arc::new(tokio::sync::Semaphore::new(BROWSER_MAX_ACTIVE_STREAMS)),
            bytes: Arc::new(tokio::sync::Semaphore::new(BROWSER_MAX_INFLIGHT_BYTES)),
            peer_streams: Arc::new(std::sync::Mutex::new(HashMap::new())),
            peer_connections: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

struct BrowserConnectionActivity {
    active: usize,
    idle_since: Option<Instant>,
}

struct BrowserConnectionStreamGuard {
    activity: Arc<std::sync::Mutex<BrowserConnectionActivity>>,
}

impl BrowserConnectionStreamGuard {
    fn acquire(activity: Arc<std::sync::Mutex<BrowserConnectionActivity>>) -> Self {
        let mut state = activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active += 1;
        state.idle_since = None;
        drop(state);
        Self { activity }
    }
}

impl Drop for BrowserConnectionStreamGuard {
    fn drop(&mut self) {
        let mut state = self
            .activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            state.idle_since = Some(Instant::now());
        }
    }
}

/// First byte on every hive-p2p bi stream selects how the owner handles it; the `fluid-tunnel`
/// wire protocol rides unchanged after this byte on a `STREAM_TUNNEL` stream.
const STREAM_TUNNEL: u8 = 0x00;
const STREAM_RAW: u8 = 0x01;
/// Control-plane GOSSIP over the mesh: `[u8 method][u32 path_len][path][u32 body_len][body]`
/// tunneled to the peer's local admin, response `[u32 len][bytes]`; method 0=GET, 1=POST.
const STREAM_GOSSIP: u8 = 0x02;
/// SIGNED gossip: [`STREAM_GOSSIP`]'s framing plus a trailer `[32B signer pubkey][8B ts_ms][64B sig]`
/// — an ed25519 signature over `hive-gossip-v1 || method || path || body || ts`, with signer ==
/// the connection's authenticated remote identity.
const STREAM_GOSSIP_SIGNED: u8 = 0x03;
/// MESH JOIN (hot-join): a not-yet-trusted node introduces itself,
/// `[u32 node_len][node_json][u32 proof_len][proof]` → `[u32 len][bytes]` (empty = rejected); the
/// proof is a fleet-secret HMAC over the QUIC-authenticated remote id. The ONLY stream mode an
/// untrusted connection may use.
const STREAM_JOIN: u8 = 0x04;
/// RAW proxy to a NAMED local target (generic TCP/UDP mesh forwarding): opener → owner
/// `[u32 len][RawTarget JSON]`, owner → opener `[RAW_TARGET_MAGIC][1B status]` BEFORE any payload (so
/// the opener can fail over without having consumed client bytes), then an opaque splice for `tcp`
/// or `[u32 len][bytes]` datagram frames (≤ [`RAW_MAX_DATAGRAM`]) for `udp`.
const STREAM_RAW_TARGET: u8 = 0x05;
const GOSSIP_METHOD_GET: u8 = 0;
const GOSSIP_METHOD_POST: u8 = 1;
/// Domain separator for gossip signatures (versioned; bump on format change).
const GOSSIP_SIG_DOMAIN: &[u8] = b"hive-gossip-v1";
/// Cap on a single gossip frame (request path/body or response) — gossip payloads
/// are small JSON rosters; this just bounds a malformed/hostile length prefix.
const GOSSIP_MAX_FRAME: usize = 16 * 1024 * 1024;
/// Read size of a streamed response body (`PeerPool::read_response_frame`);
/// each read is bounded by the idle budget.
const RESPONSE_CHUNK: usize = 64 * 1024;
/// Largest up-front reservation for a streamed response body.
const RESPONSE_PREALLOC: usize = 1024 * 1024;

/// Serves one gossip request `(method, path, body, verified_signer) -> response body`; hive-cloud
/// wires it to the same admin handlers that answer HTTP gossip. `verified_signer` is `Some` only
/// for a VALID signature bound to the connection's remote identity.
pub type GossipHandler = Arc<
    dyn Fn(
            u8,
            String,
            Vec<u8>,
            Option<String>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Serves one MESH-JOIN request `(remote_id, node_json, proof) -> response body` (empty =
/// rejected); the proof is verified against the QUIC-authenticated `remote_id`, never against
/// anything the body claims.
pub type JoinHandler = Arc<
    dyn Fn(
            String,
            Vec<u8>,
            String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Re-checks a browser endpoint identity against the admission store before any `hive/browser/0`
/// stream is accepted; `false` closes the connection without exposing an op.
pub type BrowserAdmissionHandler = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Serves one `Op::CrrSync` request against the fleet's replica `(remote_id, request) -> reply`;
/// `Err(code)` is the stream reset code, so protocol faults stay distinct from sync-domain
/// refusals inside an Ok reply.
pub type BrowserCrrHandler = Arc<
    dyn Fn(
            String,
            Vec<u8>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, u32>> + Send>>
        + Send
        + Sync,
>;

/// Receives a browser connection's FRAMED byte totals `(endpoint_id, inbound, outbound)` per
/// completed op stream — exactly the wire contract's bytes (u32 LE prefix, op byte, payload),
/// reported at the stage they provably moved. Synchronous: pure in-memory arithmetic, never
/// blocks the stream task.
pub type BrowserMeterHandler = Arc<dyn Fn(String, u64, u64) + Send + Sync>;

/// The installed browser byte meter, process-global so both metering boundaries (inbound
/// `serve_browser_conn`, outbound `BrowserPool`) reach it; `None` is a compiled-in no-op.
static BROWSER_METER: std::sync::RwLock<Option<BrowserMeterHandler>> = std::sync::RwLock::new(None);

/// Install (or clear) the process-global [`BrowserMeterHandler`]; not a trust boundary — it
/// observes byte counts and can never influence whether a stream is served.
pub fn set_browser_meter(meter: Option<BrowserMeterHandler>) {
    if let Ok(mut slot) = BROWSER_METER.write() {
        *slot = meter;
    }
}

/// Report one completed stage of a browser op stream to the installed meter, if any; attribution
/// is the handler's job.
fn meter_browser_bytes(endpoint_id: &str, inbound: u64, outbound: u64) {
    let meter = BROWSER_METER
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    if let Some(meter) = meter {
        meter(endpoint_id.to_string(), inbound, outbound);
    }
}

/// Transport a [`RawTarget`] speaks; kebab-case wire strings so the handshake JSON matches
/// `fluid_core::ServiceProtocol` (mapped by hive-cloud's resolver — this crate does not depend on
/// fluid-core).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawProto {
    Tcp,
    Udp,
}

/// The opening handshake of a [`STREAM_RAW_TARGET`] stream: which local service the owner should
/// splice into. An empty `deployment` resolves the project's CURRENT serving deployment on the
/// owner.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RawTarget {
    pub project: String,
    pub function: String,
    #[serde(default)]
    pub deployment: String,
    /// The CONTAINER port of the target service — its stable identity across nodes/redeploys; never
    /// a host/public port.
    pub port: u16,
    pub proto: RawProto,
}

/// Fixed 4-byte preamble the owner writes BEFORE every [`STREAM_RAW_TARGET`] status byte, so a
/// real admission is unmistakable: an un-upgraded peer's dispatcher routes the `0x05` mode byte
/// to its tunnel-session arm, whose unsolicited Metrics frame starts with `0x00` — a bare
/// [`RAW_TARGET_OK`]. A non-zero first byte makes a misroute fail closed.
pub const RAW_TARGET_MAGIC: [u8; 4] = [0xF1, 0x5C, 0x9E, 0xA2];
/// Status byte: the target resolved and the local leg is connected — splice begins. Always
/// preceded by [`RAW_TARGET_MAGIC`].
pub const RAW_TARGET_OK: u8 = 0;
/// No local target for the handshake (unknown project/function/port, protocol not forwardable,
/// malformed handshake); the opener fails over to its next candidate.
pub const RAW_TARGET_NOT_FOUND: u8 = 1;
/// The target resolved but the owner could not connect its local leg.
pub const RAW_TARGET_CONNECT_FAILED: u8 = 2;

/// Largest UDP payload one [`STREAM_RAW_TARGET`] datagram frame may carry (the IPv4 maximum); a
/// bigger length prefix is a framing error.
pub const RAW_MAX_DATAGRAM: usize = 65_507;

/// What a [`RawTargetResolver`] hands back for an admitted target: the LOCAL address to splice
/// into plus an opaque guard held for the splice's lifetime (e.g. a fluid-compute `Lease`).
pub struct RawTargetConn {
    /// `host:port` on the owner node — TCP-connected for `proto: tcp`, the
    /// datagram destination for `proto: udp`.
    pub addr: String,
    /// Dropped when the splice ends.
    pub guard: Option<Box<dyn std::any::Any + Send>>,
}

/// Resolves one [`RawTarget`] to its local leg on THIS node, or `None` ([`RAW_TARGET_NOT_FOUND`]);
/// provided by hive-cloud, which owns the deployment→container mapping.
pub type RawTargetResolver = Arc<
    dyn Fn(
            RawTarget,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RawTargetConn>> + Send>>
        + Send
        + Sync,
>;

/// Gossip signature-verification mode, `HIVE_GOSSIP_VERIFY` = `off` | `log` (default: verify,
/// count, warn, still serve) | `enforce` (only a valid signature from the connected peer); staged
/// so enforcement cannot partition a mixed-version fleet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerifyMode {
    Off,
    Log,
    Enforce,
}

/// Whether outbound gossip is signed (`HIVE_GOSSIP_SIGN=1`). Default OFF until the
/// whole fleet runs a binary that understands [`STREAM_GOSSIP_SIGNED`].
pub fn gossip_sign_enabled() -> bool {
    std::env::var("HIVE_GOSSIP_SIGN")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

pub fn verify_mode() -> VerifyMode {
    match std::env::var("HIVE_GOSSIP_VERIFY")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" => VerifyMode::Off,
        "enforce" | "strict" | "1" => VerifyMode::Enforce,
        _ => VerifyMode::Log,
    }
}

/// Max allowed clock skew / age for a signed gossip message (replay guard),
/// env-tunable via `HIVE_GOSSIP_TS_WINDOW_SECS` (default 300).
fn gossip_ts_window_ms() -> u64 {
    std::env::var("HIVE_GOSSIP_TS_WINDOW_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300)
        * 1000
}

/// Mesh-wide message-verification counters (surfaced via `/v1/relay` for
/// observability during the staged log→enforce rollout).
#[derive(Default)]
pub struct VerifyStats {
    pub signed_ok: AtomicU64,
    pub unsigned: AtomicU64,
    pub bad_sig: AtomicU64,
    pub stale_ts: AtomicU64,
    pub signer_mismatch: AtomicU64,
    pub rejected: AtomicU64,
}

static VERIFY_STATS: VerifyStats = VerifyStats {
    signed_ok: AtomicU64::new(0),
    unsigned: AtomicU64::new(0),
    bad_sig: AtomicU64::new(0),
    stale_ts: AtomicU64::new(0),
    signer_mismatch: AtomicU64::new(0),
    rejected: AtomicU64::new(0),
};

/// Snapshot of the verification counters:
/// `(signed_ok, unsigned, bad_sig, stale_ts, signer_mismatch, rejected)`.
pub fn verify_stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        VERIFY_STATS.signed_ok.load(Ordering::Relaxed),
        VERIFY_STATS.unsigned.load(Ordering::Relaxed),
        VERIFY_STATS.bad_sig.load(Ordering::Relaxed),
        VERIFY_STATS.stale_ts.load(Ordering::Relaxed),
        VERIFY_STATS.signer_mismatch.load(Ordering::Relaxed),
        VERIFY_STATS.rejected.load(Ordering::Relaxed),
    )
}

/// Per-connection post-quantum key-exchange counters (`/v1/relay`) — the SOLE source of truth for
/// whether a trunk negotiated hybrid PQ, read off the ACTUAL completed handshake
/// (`negotiated_key_exchange_group`) on the accept path and on every fresh `PeerPool` dial; never
/// derived from config, since an old binary or the `tls-ring`-only browser build falls back to
/// classical silently.
#[derive(Default)]
pub struct PqKexStats {
    /// A hybrid PQ group (X25519MLKEM768 or another ML-KEM combination) was negotiated — a
    /// KEY-EXCHANGE property only; identity stays classical Ed25519.
    pub hybrid_pq: AtomicU64,
    /// A classical-only group (X25519/SECP256R1/SECP384R1) was negotiated —
    /// the peer connected fine, just without PQ protection on this leg.
    pub classical_fallback: AtomicU64,
    /// The handshake completed but no recognized group could be read back — reported as unknown,
    /// never folded into either bucket.
    pub unknown: AtomicU64,
}

static PQ_KEX_STATS: PqKexStats = PqKexStats {
    hybrid_pq: AtomicU64::new(0),
    classical_fallback: AtomicU64::new(0),
    unknown: AtomicU64::new(0),
};

/// Snapshot of the PQ key-exchange counters: `(hybrid_pq, classical_fallback, unknown)`.
pub fn pq_kex_stats() -> (u64, u64, u64) {
    (
        PQ_KEX_STATS.hybrid_pq.load(Ordering::Relaxed),
        PQ_KEX_STATS.classical_fallback.load(Ordering::Relaxed),
        PQ_KEX_STATS.unknown.load(Ordering::Relaxed),
    )
}

/// Read the REAL negotiated key-exchange group off a just-established connection and bucket it
/// (synchronous: the handshake is complete once a `Connection` exists); `class` names the budget
/// it was admitted under.
fn record_kex_telemetry(conn: &Connection, class: &'static str) {
    let group = conn
        .handshake_data()
        .and_then(|data| data.downcast::<noq::crypto::rustls::HandshakeData>().ok())
        .and_then(|hd| hd.negotiated_key_exchange_group);
    tracing::debug!(
        class,
        group = ?group,
        remote = %conn.remote_id().fmt_short(),
        "p2p handshake key exchange"
    );
    match group {
        Some(
            rustls::NamedGroup::X25519MLKEM768
            | rustls::NamedGroup::secp256r1MLKEM768
            | rustls::NamedGroup::MLKEM768,
        ) => {
            PQ_KEX_STATS.hybrid_pq.fetch_add(1, Ordering::Relaxed);
        }
        Some(_) => {
            PQ_KEX_STATS
                .classical_fallback
                .fetch_add(1, Ordering::Relaxed);
        }
        None => {
            PQ_KEX_STATS.unknown.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The domain-separated byte string an ed25519 gossip signature covers. Pure, so
/// signer and verifier can't drift.
fn gossip_sig_preimage(method: u8, path: &str, body: &[u8], ts_ms: u64) -> Vec<u8> {
    let mut m =
        Vec::with_capacity(GOSSIP_SIG_DOMAIN.len() + 1 + 8 + 8 + path.len() + body.len() + 8);
    m.extend_from_slice(GOSSIP_SIG_DOMAIN);
    m.push(method);
    m.extend_from_slice(&(path.len() as u32).to_be_bytes());
    m.extend_from_slice(path.as_bytes());
    m.extend_from_slice(&(body.len() as u32).to_be_bytes());
    m.extend_from_slice(body);
    m.extend_from_slice(&ts_ms.to_be_bytes());
    m
}

/// Sign a gossip request with this node's iroh identity key. Returns the 104-byte
/// trailer `[32B signer pubkey][8B ts_ms][64B sig]` appended to the framed request.
pub fn sign_gossip(
    secret: &iroh::SecretKey,
    method: u8,
    path: &str,
    body: &[u8],
    ts_ms: u64,
) -> [u8; 104] {
    let sig = secret.sign(&gossip_sig_preimage(method, path, body, ts_ms));
    let mut out = [0u8; 104];
    out[..32].copy_from_slice(secret.public().as_bytes());
    out[32..40].copy_from_slice(&ts_ms.to_be_bytes());
    out[40..104].copy_from_slice(&sig.to_bytes());
    out
}

/// Verify a signed-gossip trailer: the timestamp must be inside `gossip_ts_window_ms` in either
/// direction and the signer must BE `remote_id`. Returns the verified signer id or a counted
/// reason.
pub fn verify_gossip(
    trailer: &[u8],
    method: u8,
    path: &str,
    body: &[u8],
    remote_id: &str,
    now_ms: u64,
) -> Result<String, &'static str> {
    if trailer.len() != 104 {
        VERIFY_STATS.bad_sig.fetch_add(1, Ordering::Relaxed);
        return Err("malformed signature trailer");
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&trailer[..32]);
    let mut tsb = [0u8; 8];
    tsb.copy_from_slice(&trailer[32..40]);
    let ts = u64::from_be_bytes(tsb);
    let mut sigb = [0u8; 64];
    sigb.copy_from_slice(&trailer[40..104]);
    let Ok(signer) = iroh::PublicKey::from_bytes(&pk) else {
        VERIFY_STATS.bad_sig.fetch_add(1, Ordering::Relaxed);
        return Err("invalid signer key");
    };
    let sig = iroh::Signature::from_bytes(&sigb);
    if signer
        .verify(&gossip_sig_preimage(method, path, body, ts), &sig)
        .is_err()
    {
        VERIFY_STATS.bad_sig.fetch_add(1, Ordering::Relaxed);
        return Err("signature invalid");
    }
    let window = gossip_ts_window_ms();
    if now_ms.abs_diff(ts) > window {
        VERIFY_STATS.stale_ts.fetch_add(1, Ordering::Relaxed);
        return Err("timestamp outside freshness window");
    }
    let signer_s = signer.to_string();
    if !remote_id.is_empty() && signer_s != remote_id {
        VERIFY_STATS.signer_mismatch.fetch_add(1, Ordering::Relaxed);
        return Err("signer does not match connection identity");
    }
    VERIFY_STATS.signed_ok.fetch_add(1, Ordering::Relaxed);
    Ok(signer_s)
}

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
    read_frame_max(r, GOSSIP_MAX_FRAME).await
}

/// Read one `[u32 len][bytes]` frame with an explicit size cap — the shared primitive behind
/// gossip/join frames (16 MiB) and raw-target datagram frames ([`RAW_MAX_DATAGRAM`]).
async fn read_frame_max<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> std::io::Result<Vec<u8>> {
    let n = read_u32(r).await?;
    if n > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).await?;
    Ok(v)
}

/// Read one datagram frame off a raw-target UDP mesh stream: `Ok(Some(bytes))` per datagram,
/// `Ok(None)` ONLY on a clean end-of-stream at a frame boundary; a peer that dies mid-frame is
/// `Err(InvalidData "truncated datagram frame")`. Not cancellation-safe mid-frame: never poll it
/// inside a `select!` arm. Exported so the edge-side relay speaks byte-identical framing.
pub async fn read_raw_datagram<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut prefix[got..]).await {
            Ok(0) => {
                if got == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated datagram frame: EOF mid length-prefix",
                ));
            }
            Ok(n) => got += n,
            Err(e) => return Err(e),
        }
    }
    let n = u32::from_be_bytes(prefix) as usize;
    if n > RAW_MAX_DATAGRAM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated datagram frame: EOF mid payload",
            )
        } else {
            e
        }
    })?;
    Ok(Some(v))
}

/// Write one `[u32 len][bytes]` datagram frame; an oversized payload is a framing error.
pub async fn write_raw_datagram<W: AsyncWrite + Unpin>(
    w: &mut W,
    datagram: &[u8],
) -> std::io::Result<()> {
    if datagram.len() > RAW_MAX_DATAGRAM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "datagram too large",
        ));
    }
    w.write_all(&(datagram.len() as u32).to_be_bytes()).await?;
    w.write_all(datagram).await?;
    w.flush().await
}

/// Serialize this endpoint's dialable address (direct addrs + relay) to JSON for gossip,
/// AUGMENTED with the node's publicly-routable address when configured
/// ([`configured_public_addr`]): iroh alone discovers only the private interface address behind
/// 1:1 NAT, which pinned the whole mesh to the relay fallback. Discovered entries are kept for
/// same-VPC peers.
pub fn addr_json(ep: &Endpoint) -> Option<String> {
    let mut addr = ep.addr();
    if let Some(sa) = configured_public_addr() {
        addr.addrs.insert(iroh::TransportAddr::Ip(sa));
    }
    serde_json::to_string(&addr).ok()
}

/// This node's publicly-dialable QUIC address from `HIVE_PUBLIC_IP` + `HIVE_IROH_PORT` (both
/// required: a VM cannot read its external IP, and an ephemeral port goes stale on restart);
/// loopback/unspecified are rejected.
fn configured_public_addr() -> Option<std::net::SocketAddr> {
    let ip: std::net::IpAddr = std::env::var("HIVE_PUBLIC_IP").ok()?.trim().parse().ok()?;
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    let port: u16 = std::env::var("HIVE_IROH_PORT")
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|p| *p != 0)?;
    Some(std::net::SocketAddr::new(ip, port))
}

/// Extract the iroh `EndpointId` from an `addr_json` blob; used to build the peer-trust allowlist
/// from the roster.
pub fn endpoint_id_from_addr_json(addr_json: &str) -> Option<String> {
    serde_json::from_str::<EndpointAddr>(addr_json)
        .ok()
        .map(|a| a.id.to_string())
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
    /// Per-peer, per-phase iroh timeout counters (#H4) — `p2p_timeout{phase,node_id}`.
    pub timeouts: Vec<PeerTimeout>,
    /// Every live trunk with its per-path QUIC transport state: `cwnd`, `lost_packets` and the
    /// SELECTED path tell a relayed path, a loss-collapsed window and an app-limited sender apart,
    /// which byte totals cannot.
    pub trunks: Vec<TrunkStats>,
}

/// One live trunk's transport state, surfaced via [`PeerPool::relay_stats`].
#[derive(Clone, Debug, Default)]
pub struct TrunkStats {
    pub endpoint_id: String,
    pub incarnation: u64,
    /// Any IP path exists (the trunk has holepunched at least once).
    pub has_direct: bool,
    /// The path iroh currently sends application data on is the relay.
    pub selected_is_relay: bool,
    pub udp_tx_bytes: u64,
    pub udp_rx_bytes: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub paths: Vec<TrunkPathStats>,
}

/// One QUIC path of a trunk, straight from noq's `PathStats`.
#[derive(Clone, Debug, Default)]
pub struct TrunkPathStats {
    pub remote: String,
    pub selected: bool,
    pub is_ip: bool,
    pub is_relay: bool,
    pub rtt_ms: f64,
    /// Congestion window in bytes; near 12 000 on a busy trunk means the sender never left the
    /// initial window.
    pub cwnd: u64,
    pub congestion_events: u64,
    pub spurious_congestion_events: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub udp_tx_bytes: u64,
    pub udp_rx_bytes: u64,
    pub udp_tx_datagrams: u64,
    pub udp_rx_datagrams: u64,
    pub current_mtu: u16,
    pub black_holes_detected: u64,
    pub sent_plpmtud_probes: u64,
    pub lost_plpmtud_probes: u64,
}

/// One peer/phase timeout counter, surfaced via [`PeerPool::relay_stats`].
#[derive(Clone, Debug)]
pub struct PeerTimeout {
    pub node_id: String,
    pub phase: &'static str,
    pub count: u64,
}

/// Read a millisecond budget from env, falling back to `default_ms`. A value of 0
/// (or unparseable) falls back to the default — we never allow an unbounded await.
fn env_ms(key: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}
/// Connect (holepunch + handshake) budget — pre-send. Holepunch can be slow.
fn connect_budget() -> Duration {
    env_ms("HIVE_P2P_CONNECT_MS", 5_000)
}
/// `open_bi` budget — pre-send. Cheap once the conn is live; a hang = half-dead trunk.
fn open_budget() -> Duration {
    env_ms("HIVE_P2P_OPEN_MS", 2_000)
}
/// Discovery-fallback connect budget (`HIVE_P2P_DISCOVERY_MS`, default 4000), used only when the
/// cached-hint dial fails: a `RelayUrl` in the hint marks the address "resolved" so iroh never
/// consults Discovery on its own. The mainline DHT needs 2.0–4.6 s plus a cold warm-up, so fleet
/// deploys set 8000 (ansible `hive_p2p_discovery_ms`).
fn discovery_budget() -> Duration {
    env_ms("HIVE_P2P_DISCOVERY_MS", 4_000)
}

/// The live discovery-fallback budget, for `GET /v1/mesh/discovery` — the same function the dial
/// path uses.
pub fn dial_discovery_budget() -> Duration {
    discovery_budget()
}
/// Worst-case time `acquire()` needs for the cached-hint attempt AND the fresh-discovery fallback
/// (`connect_budget + discovery_budget` + slack); any outer timeout around
/// `join_request`/`gossip_request` MUST be at least this long.
pub fn dial_fallback_ceiling() -> Duration {
    connect_budget() + discovery_budget() + Duration::from_secs(1)
}
/// First-byte / response-headers budget — post-send. Generous: the cell may be cold.
fn firstbyte_budget() -> Duration {
    env_ms("HIVE_P2P_FIRSTBYTE_MS", 15_000)
}
/// Body inter-chunk IDLE budget — post-send. Not a wall-clock cap; reset per chunk
/// so SSE / long streams survive, only killed on inactivity.
fn idle_budget() -> Duration {
    env_ms("HIVE_P2P_IDLE_MS", 45_000)
}

const PHASE_CONNECT: usize = 0;
const PHASE_OPEN: usize = 1;
const PHASE_FIRSTBYTE: usize = 2;
const PHASE_IDLE: usize = 3;
const PHASE_NAMES: [&str; 4] = ["connect", "open", "firstbyte", "idle"];

/// Marker error: a **pre-send** phase (connect or `open_bi`) timed out — the strongest "this peer
/// is dead" signal; the caller marks the peer unhealthy. Downcast to detect.
#[derive(Clone, Debug)]
pub struct DeadPeerTimeout {
    pub node_id: String,
    pub phase: &'static str,
    pub budget_ms: u64,
}
impl std::fmt::Display for DeadPeerTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "p2p {} timeout to {} after {}ms (peer presumed dead)",
            self.phase, self.node_id, self.budget_ms
        )
    }
}
impl std::error::Error for DeadPeerTimeout {}

/// Marker error: a **post-send** phase (first byte or body idle) timed out — NOT a dead-peer
/// signal; the request was on the wire, so the caller fails over, never retries.
#[derive(Clone, Debug)]
pub struct PostSendTimeout {
    pub node_id: String,
    pub phase: &'static str,
    pub budget_ms: u64,
}
impl std::fmt::Display for PostSendTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "p2p {} timeout to {} after {}ms (post-send; no retry)",
            self.phase, self.node_id, self.budget_ms
        )
    }
}
impl std::error::Error for PostSendTimeout {}

/// Per-peer, per-phase timeout counters (#H4 observability).
#[derive(Default)]
struct TimeoutCounters {
    map: Mutex<HashMap<String, [u64; 4]>>,
}
impl TimeoutCounters {
    async fn bump(&self, node_id: &str, phase: usize) {
        self.map
            .lock()
            .await
            .entry(node_id.to_string())
            .or_insert([0; 4])[phase] += 1;
    }
    async fn snapshot(&self) -> Vec<PeerTimeout> {
        let m = self.map.lock().await;
        let mut out = Vec::new();
        for (node, counts) in m.iter() {
            for (i, &c) in counts.iter().enumerate() {
                if c > 0 {
                    out.push(PeerTimeout {
                        node_id: node.clone(),
                        phase: PHASE_NAMES[i],
                        count: c,
                    });
                }
            }
        }
        out
    }
}

/// A response collected from a single P2P tunnel request (gateway side).
pub struct TunnelResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A streamed P2P response: head immediately, body chunks via [`recv`](TunnelStream::recv) as
/// they arrive, so SSE / chunked bodies forward incrementally.
pub struct TunnelStream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    body: tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>,
    /// Kept alive so the tunnel's QUIC streams stay open until the body is fully consumed; dropping
    /// the client early resets the send stream and truncates the response.
    _client: fluid_tunnel::TunnelClient,
    /// Inter-chunk idle budget (#H4) — reset per chunk, so a long-lived but active
    /// stream (SSE) survives and only an idle/wedged one is killed.
    idle: Duration,
    node_id: String,
    timeouts: Arc<TimeoutCounters>,
    /// Set once the idle budget elapses, so the buffered [`PeerPool::request`]
    /// drain can distinguish "killed on inactivity" from a clean EOF.
    idle_timed_out: bool,
}

impl TunnelStream {
    /// Next body chunk, or `None` at end-of-body — or when the inter-chunk idle budget elapses; each
    /// chunk resets the budget.
    pub async fn recv(&mut self) -> Option<bytes::Bytes> {
        if self.idle_timed_out {
            return None;
        }
        match tokio::time::timeout(self.idle, self.body.recv()).await {
            Ok(opt) => opt,
            Err(_) => {
                self.idle_timed_out = true;
                self.timeouts.bump(&self.node_id, PHASE_IDLE).await;
                tracing::warn!(node_id = %self.node_id, idle_ms = self.idle.as_millis() as u64, "p2p body idle timeout");
                None
            }
        }
    }

    /// Whether the stream ended because of an idle timeout (vs a clean EOF). Lets a
    /// buffered caller turn a mid-stream stall into an explicit error.
    pub fn timed_out(&self) -> bool {
        self.idle_timed_out
    }
}

/// A cached, reusable QUIC connection to one peer. The pool caches the `Connection`, never a
/// `TunnelClient` (one byte stream, head-of-line blocking); each request opens its own bi stream.
struct Trunk {
    incarnation: u64,
    conn: Connection,
}

/// Browser invocation failed before or after a complete request frame entered QUIC; `sent ==
/// false` is safe to fall back from, `sent == true` means the browser may already have executed.
#[derive(Clone, Debug)]
pub struct BrowserInvokeError {
    pub sent: bool,
    pub message: String,
}

impl BrowserInvokeError {
    fn new(sent: bool, error: impl std::fmt::Display) -> Self {
        Self {
            sent,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for BrowserInvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "browser invoke failed (sent={}): {}",
            self.sent, self.message
        )
    }
}

impl std::error::Error for BrowserInvokeError {}

struct BrowserSlot {
    epoch: u64,
    state: BrowserSlotState,
    last_used: Instant,
}

#[derive(Clone)]
enum BrowserSlotState {
    Vacant,
    Dialing(Arc<BrowserDial>),
    Ready(Arc<BrowserTrunk>),
}

struct BrowserDial {
    state: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
}

impl BrowserDial {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::atomic::AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    async fn wait(&self) -> bool {
        loop {
            let notified = self.notify.notified();
            let state = self.state.load(Ordering::Acquire);
            if state != 0 {
                return state == 2;
            }
            notified.await;
        }
    }

    fn finish(&self, fenced: bool) {
        self.state
            .store(if fenced { 2 } else { 1 }, Ordering::Release);
        self.notify.notify_waiters();
    }
}

struct BrowserTrunk {
    conn: Connection,
    streams: Arc<tokio::sync::Semaphore>,
    active: std::sync::atomic::AtomicUsize,
    last_used: std::sync::Mutex<Instant>,
}

impl BrowserTrunk {
    fn new(conn: Connection) -> Arc<Self> {
        Arc::new(Self {
            conn,
            streams: Arc::new(tokio::sync::Semaphore::new(
                BROWSER_MAX_STREAMS_PER_CONNECTION,
            )),
            active: std::sync::atomic::AtomicUsize::new(0),
            last_used: std::sync::Mutex::new(Instant::now()),
        })
    }

    fn last_used(&self) -> Instant {
        *self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn release(&self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        *self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    fn close(&self, code: u32, reason: &'static [u8]) {
        self.streams.close();
        self.conn.close(code.into(), reason);
    }
}

enum BrowserAcquireError {
    Failed(BrowserInvokeError),
    Retryable {
        error: BrowserInvokeError,
        epoch: u64,
    },
    Fenced,
}

struct BrowserAcquired {
    epoch: u64,
    trunk: Arc<BrowserTrunk>,
}

enum BrowserAcquireAction {
    Ready {
        epoch: u64,
        trunk: Arc<BrowserTrunk>,
    },
    Wait(Arc<BrowserDial>),
    Dial {
        epoch: u64,
        dial: Arc<BrowserDial>,
    },
}

struct BrowserStreamLease {
    trunk: Arc<BrowserTrunk>,
    _stream: tokio::sync::OwnedSemaphorePermit,
    _global: tokio::sync::OwnedSemaphorePermit,
}

impl BrowserStreamLease {
    async fn acquire(
        trunk: Arc<BrowserTrunk>,
        global: Arc<tokio::sync::Semaphore>,
    ) -> std::result::Result<Self, BrowserInvokeError> {
        let stream = tokio::time::timeout(
            BROWSER_STREAM_WAIT_TIMEOUT,
            trunk.streams.clone().acquire_owned(),
        )
        .await
        .map_err(|_| BrowserInvokeError::new(false, "browser trunk stream wait timed out"))?
        .map_err(|_| BrowserInvokeError::new(false, "browser trunk was closed"))?;
        trunk.active.fetch_add(1, Ordering::AcqRel);
        *trunk
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
        let global =
            match tokio::time::timeout(BROWSER_STREAM_WAIT_TIMEOUT, global.acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => {
                    trunk.release();
                    return Err(BrowserInvokeError::new(
                        false,
                        "browser global stream pool was closed",
                    ));
                }
                Err(_) => {
                    trunk.release();
                    return Err(BrowserInvokeError::new(
                        false,
                        "browser global stream wait timed out",
                    ));
                }
            };
        Ok(Self {
            trunk,
            _stream: stream,
            _global: global,
        })
    }
}

impl Drop for BrowserStreamLease {
    fn drop(&mut self) {
        self.trunk.release();
    }
}

struct BrowserRequestGuard {
    send: Option<iroh::endpoint::SendStream>,
    recv: Option<iroh::endpoint::RecvStream>,
    _lease: BrowserStreamLease,
    armed: bool,
}

impl BrowserRequestGuard {
    fn new(lease: BrowserStreamLease) -> Self {
        Self {
            send: None,
            recv: None,
            _lease: lease,
            armed: true,
        }
    }

    fn attach(&mut self, send: iroh::endpoint::SendStream, recv: iroh::endpoint::RecvStream) {
        self.send = Some(send);
        self.recv = Some(recv);
    }

    fn close(&mut self, code: u32) {
        if !self.armed {
            return;
        }
        if let Some(send) = self.send.as_mut() {
            let _ = send.reset(code.into());
        }
        if let Some(recv) = self.recv.as_mut() {
            let _ = recv.stop(code.into());
        }
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BrowserRequestGuard {
    fn drop(&mut self) {
        self.close(browser_reset::DEADLINE_EXCEEDED);
    }
}

/// Bounded, tenant-free observability for [`BrowserPool`]: global aggregates only (never
/// per-endpoint), latency as a running sum/count.
#[derive(Default)]
struct BrowserPoolCounterCells {
    dial_attempts_total: std::sync::atomic::AtomicU64,
    dial_failures_total: std::sync::atomic::AtomicU64,
    dial_latency_ms_sum: std::sync::atomic::AtomicU64,
    dial_latency_samples: std::sync::atomic::AtomicU64,
    invoke_attempts_total: std::sync::atomic::AtomicU64,
    invoke_pre_send_failures_total: std::sync::atomic::AtomicU64,
    invoke_post_send_failures_total: std::sync::atomic::AtomicU64,
    invoke_successes_total: std::sync::atomic::AtomicU64,
    invoke_latency_ms_sum: std::sync::atomic::AtomicU64,
    invoke_latency_samples: std::sync::atomic::AtomicU64,
    bytes_sent_total: std::sync::atomic::AtomicU64,
    bytes_received_total: std::sync::atomic::AtomicU64,
    /// A trunk closed and removed from the pool (explicit revocation or `invoke`'s
    /// redial-on-failure) — the available proxy for "the underlying connection had to be
    /// re-established".
    trunk_evictions_total: std::sync::atomic::AtomicU64,
    invoke_redials_total: std::sync::atomic::AtomicU64,
    /// CRR sync rounds, counted SEPARATELY from function invokes so database replication stays
    /// visible; tenant-free aggregates.
    crr_attempts_total: std::sync::atomic::AtomicU64,
    crr_successes_total: std::sync::atomic::AtomicU64,
    crr_failures_total: std::sync::atomic::AtomicU64,
}

#[derive(Default, serde::Serialize)]
pub struct BrowserPoolCounters {
    pub dial_attempts_total: u64,
    pub dial_failures_total: u64,
    pub dial_avg_latency_ms: f64,
    pub invoke_attempts_total: u64,
    pub invoke_pre_send_failures_total: u64,
    pub invoke_post_send_failures_total: u64,
    pub invoke_successes_total: u64,
    pub invoke_avg_latency_ms: f64,
    pub bytes_sent_total: u64,
    pub bytes_received_total: u64,
    pub trunk_evictions_total: u64,
    pub invoke_redials_total: u64,
    pub crr_attempts_total: u64,
    pub crr_successes_total: u64,
    pub crr_failures_total: u64,
}

fn avg_ms(sum: &std::sync::atomic::AtomicU64, samples: &std::sync::atomic::AtomicU64) -> f64 {
    use std::sync::atomic::Ordering::Relaxed;
    let n = samples.load(Relaxed);
    if n == 0 {
        0.0
    } else {
        sum.load(Relaxed) as f64 / n as f64
    }
}

/// Native client for `hive/browser/0`; separate from [`PeerPool`] because ALPN is negotiated per
/// QUIC connection, so a fleet trunk cannot carry browser streams.
pub struct BrowserPool {
    ep: Endpoint,
    trunks: Mutex<HashMap<String, BrowserSlot>>,
    global_streams: Arc<tokio::sync::Semaphore>,
    waiters: Arc<tokio::sync::Semaphore>,
    next_epoch: AtomicU64,
    counters: BrowserPoolCounterCells,
}

impl BrowserPool {
    pub fn new(ep: Endpoint) -> Arc<Self> {
        Arc::new(Self {
            ep,
            trunks: Mutex::new(HashMap::new()),
            global_streams: Arc::new(tokio::sync::Semaphore::new(BROWSER_MAX_ACTIVE_STREAMS)),
            waiters: Arc::new(tokio::sync::Semaphore::new(BROWSER_MAX_OUTBOUND_WAITERS)),
            next_epoch: AtomicU64::new(1),
            counters: BrowserPoolCounterCells::default(),
        })
    }

    pub fn stats(&self) -> BrowserPoolCounters {
        use std::sync::atomic::Ordering::Relaxed;
        let c = &self.counters;
        BrowserPoolCounters {
            dial_attempts_total: c.dial_attempts_total.load(Relaxed),
            dial_failures_total: c.dial_failures_total.load(Relaxed),
            dial_avg_latency_ms: avg_ms(&c.dial_latency_ms_sum, &c.dial_latency_samples),
            invoke_attempts_total: c.invoke_attempts_total.load(Relaxed),
            invoke_pre_send_failures_total: c.invoke_pre_send_failures_total.load(Relaxed),
            invoke_post_send_failures_total: c.invoke_post_send_failures_total.load(Relaxed),
            invoke_successes_total: c.invoke_successes_total.load(Relaxed),
            invoke_avg_latency_ms: avg_ms(&c.invoke_latency_ms_sum, &c.invoke_latency_samples),
            bytes_sent_total: c.bytes_sent_total.load(Relaxed),
            bytes_received_total: c.bytes_received_total.load(Relaxed),
            trunk_evictions_total: c.trunk_evictions_total.load(Relaxed),
            invoke_redials_total: c.invoke_redials_total.load(Relaxed),
            crr_attempts_total: c.crr_attempts_total.load(Relaxed),
            crr_successes_total: c.crr_successes_total.load(Relaxed),
            crr_failures_total: c.crr_failures_total.load(Relaxed),
        }
    }

    pub async fn trunk_count(&self) -> usize {
        self.trunks
            .lock()
            .await
            .values()
            .filter(|slot| matches!(&slot.state, BrowserSlotState::Ready(_)))
            .count()
    }

    async fn acquire(
        &self,
        endpoint_id: &str,
        addr_json: &str,
        expected_epoch: Option<u64>,
    ) -> std::result::Result<BrowserAcquired, BrowserAcquireError> {
        let addr: EndpointAddr = serde_json::from_str(addr_json)
            .map_err(|error| BrowserAcquireError::Failed(BrowserInvokeError::new(false, error)))?;
        let key = addr.id.to_string();
        if key != endpoint_id {
            return Err(BrowserAcquireError::Failed(BrowserInvokeError::new(
                false,
                "browser address identity does not match serving target",
            )));
        }
        let mut epoch_guard = expected_epoch;
        loop {
            let action = {
                let mut trunks = self.trunks.lock().await;
                let existing = trunks
                    .get(&key)
                    .map(|slot| (slot.epoch, slot.state.clone()));
                if epoch_guard.is_none() {
                    epoch_guard = existing.as_ref().map(|(epoch, _)| *epoch);
                }
                if let Some(expected) = epoch_guard {
                    if existing.as_ref().map(|(epoch, _)| *epoch) != Some(expected) {
                        return Err(BrowserAcquireError::Fenced);
                    }
                }
                match existing {
                    Some((epoch, BrowserSlotState::Ready(trunk)))
                        if trunk.conn.close_reason().is_none() =>
                    {
                        BrowserAcquireAction::Ready { epoch, trunk }
                    }
                    Some((_, BrowserSlotState::Dialing(dial))) => BrowserAcquireAction::Wait(dial),
                    Some((epoch, BrowserSlotState::Ready(trunk))) => {
                        trunk.close(0, b"dead browser trunk replaced");
                        self.counters
                            .trunk_evictions_total
                            .fetch_add(1, Ordering::Relaxed);
                        let dial = BrowserDial::new();
                        let slot = trunks.get_mut(&key).expect("browser slot is present");
                        slot.state = BrowserSlotState::Dialing(dial.clone());
                        slot.last_used = Instant::now();
                        BrowserAcquireAction::Dial { epoch, dial }
                    }
                    Some((epoch, BrowserSlotState::Vacant)) => {
                        let dial = BrowserDial::new();
                        let slot = trunks.get_mut(&key).expect("browser slot is present");
                        slot.state = BrowserSlotState::Dialing(dial.clone());
                        slot.last_used = Instant::now();
                        BrowserAcquireAction::Dial { epoch, dial }
                    }
                    None => {
                        if trunks.len() >= max_browser_outbound_trunks() {
                            let victim = trunks
                                .iter()
                                .filter_map(|(victim_key, slot)| match &slot.state {
                                    BrowserSlotState::Vacant => {
                                        Some((slot.last_used, victim_key.clone()))
                                    }
                                    BrowserSlotState::Ready(trunk)
                                        if trunk.active.load(Ordering::Acquire) == 0 =>
                                    {
                                        Some((trunk.last_used(), victim_key.clone()))
                                    }
                                    _ => None,
                                })
                                .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
                                .map(|(_, victim_key)| victim_key);
                            let Some(victim) = victim else {
                                return Err(BrowserAcquireError::Failed(BrowserInvokeError::new(
                                    false,
                                    "browser trunk pool is at active capacity",
                                )));
                            };
                            if let Some(slot) = trunks.remove(&victim) {
                                if let BrowserSlotState::Ready(trunk) = slot.state {
                                    trunk.close(0, b"browser trunk capacity eviction");
                                    self.counters
                                        .trunk_evictions_total
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        let epoch = self.next_epoch.fetch_add(1, Ordering::AcqRel);
                        epoch_guard = Some(epoch);
                        let dial = BrowserDial::new();
                        trunks.insert(
                            key.clone(),
                            BrowserSlot {
                                epoch,
                                state: BrowserSlotState::Dialing(dial.clone()),
                                last_used: Instant::now(),
                            },
                        );
                        BrowserAcquireAction::Dial { epoch, dial }
                    }
                }
            };
            match action {
                BrowserAcquireAction::Ready { epoch, trunk } => {
                    return Ok(BrowserAcquired { epoch, trunk });
                }
                BrowserAcquireAction::Wait(dial) => {
                    let _waiter = self.waiters.clone().try_acquire_owned().map_err(|_| {
                        BrowserAcquireError::Failed(BrowserInvokeError::new(
                            false,
                            "browser dial waiter pool is full",
                        ))
                    })?;
                    let fenced = tokio::time::timeout(
                        connect_budget() + Duration::from_secs(1),
                        dial.wait(),
                    )
                    .await
                    .map_err(|_| {
                        BrowserAcquireError::Failed(BrowserInvokeError::new(
                            false,
                            "browser dial wait timed out",
                        ))
                    })?;
                    if fenced {
                        return Err(BrowserAcquireError::Fenced);
                    }
                }
                BrowserAcquireAction::Dial { epoch, dial } => {
                    self.counters
                        .dial_attempts_total
                        .fetch_add(1, Ordering::Relaxed);
                    let dial_t0 = Instant::now();
                    let connected = match tokio::time::timeout(
                        connect_budget(),
                        self.ep.connect(addr.clone(), BROWSER_ALPN),
                    )
                    .await
                    {
                        Ok(Ok(conn)) => Ok(conn),
                        Ok(Err(error)) => Err(BrowserInvokeError::new(false, error)),
                        Err(_) => Err(BrowserInvokeError::new(false, "browser connect timed out")),
                    };
                    match connected {
                        Ok(conn) => {
                            self.counters
                                .dial_latency_ms_sum
                                .fetch_add(dial_t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                            self.counters
                                .dial_latency_samples
                                .fetch_add(1, Ordering::Relaxed);
                            let trunk = BrowserTrunk::new(conn.clone());
                            let mut trunks = self.trunks.lock().await;
                            let current = trunks.get(&key).is_some_and(|slot| {
                                slot.epoch == epoch
                                    && matches!(
                                        &slot.state,
                                        BrowserSlotState::Dialing(current)
                                            if Arc::ptr_eq(current, &dial)
                                    )
                            });
                            if !current {
                                drop(trunks);
                                conn.close(browser_reset::FORBIDDEN.into(), b"browser dial fenced");
                                dial.finish(true);
                                return Err(BrowserAcquireError::Fenced);
                            }
                            let slot = trunks.get_mut(&key).expect("browser slot is present");
                            slot.state = BrowserSlotState::Ready(trunk.clone());
                            slot.last_used = Instant::now();
                            drop(trunks);
                            dial.finish(false);
                            return Ok(BrowserAcquired { epoch, trunk });
                        }
                        Err(error) => {
                            self.counters
                                .dial_failures_total
                                .fetch_add(1, Ordering::Relaxed);
                            let mut trunks = self.trunks.lock().await;
                            let current = trunks.get(&key).is_some_and(|slot| {
                                slot.epoch == epoch
                                    && matches!(
                                        &slot.state,
                                        BrowserSlotState::Dialing(current)
                                            if Arc::ptr_eq(current, &dial)
                                    )
                            });
                            if current {
                                let slot = trunks.get_mut(&key).expect("browser slot is present");
                                slot.state = BrowserSlotState::Vacant;
                                slot.last_used = Instant::now();
                            }
                            drop(trunks);
                            dial.finish(!current);
                            if !current {
                                return Err(BrowserAcquireError::Fenced);
                            }
                            return Err(BrowserAcquireError::Retryable { error, epoch });
                        }
                    }
                }
            }
        }
    }

    async fn epoch_current(&self, endpoint_id: &str, epoch: u64) -> bool {
        self.trunks
            .lock()
            .await
            .get(endpoint_id)
            .is_some_and(|slot| slot.epoch == epoch)
    }

    async fn evict_if(&self, endpoint_id: &str, expected: &Arc<BrowserTrunk>) {
        let mut trunks = self.trunks.lock().await;
        let current = trunks.get(endpoint_id).is_some_and(|slot| {
            matches!(
                &slot.state,
                BrowserSlotState::Ready(trunk) if Arc::ptr_eq(trunk, expected)
            )
        });
        let removed = if current {
            let slot = trunks
                .get_mut(endpoint_id)
                .expect("browser slot is present");
            slot.last_used = Instant::now();
            match std::mem::replace(&mut slot.state, BrowserSlotState::Vacant) {
                BrowserSlotState::Ready(trunk) => Some(trunk),
                _ => None,
            }
        } else {
            None
        };
        drop(trunks);
        if let Some(trunk) = removed {
            trunk.close(0, b"browser trunk evicted");
            self.counters
                .trunk_evictions_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Close a revoked browser's pooled connection immediately. Future invokes
    /// cannot reuse a capability-bearing connection after admission removal.
    pub async fn close_endpoint(&self, endpoint_id: &str) {
        let state = {
            let mut trunks = self.trunks.lock().await;
            let Some(slot) = trunks.get_mut(endpoint_id) else {
                return;
            };
            slot.epoch = self.next_epoch.fetch_add(1, Ordering::AcqRel);
            slot.last_used = Instant::now();
            std::mem::replace(&mut slot.state, BrowserSlotState::Vacant)
        };
        match state {
            BrowserSlotState::Ready(trunk) => {
                trunk.close(browser_reset::FORBIDDEN, b"browser endpoint revoked");
                self.counters
                    .trunk_evictions_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            BrowserSlotState::Dialing(dial) => dial.finish(true),
            BrowserSlotState::Vacant => {}
        }
    }

    /// Invoke a digest-pinned browser artifact; the wire carries only `[digest][request JSON]`, every
    /// await is bounded and every reply length is checked before allocation.
    pub async fn invoke(
        &self,
        endpoint_id: &str,
        addr_json: &str,
        digest: &str,
        request_json: &str,
    ) -> std::result::Result<Vec<u8>, BrowserInvokeError> {
        use std::sync::atomic::Ordering::Relaxed;
        self.counters.invoke_attempts_total.fetch_add(1, Relaxed);
        let invoke_t0 = std::time::Instant::now();
        let result = self
            .invoke_inner(endpoint_id, addr_json, digest, request_json)
            .await;
        self.counters
            .invoke_latency_ms_sum
            .fetch_add(invoke_t0.elapsed().as_millis() as u64, Relaxed);
        self.counters.invoke_latency_samples.fetch_add(1, Relaxed);
        match &result {
            Ok(reply) => {
                self.counters.invoke_successes_total.fetch_add(1, Relaxed);
                self.counters
                    .bytes_received_total
                    .fetch_add(reply.len() as u64, Relaxed);
            }
            Err(error) if error.sent => {
                self.counters
                    .invoke_post_send_failures_total
                    .fetch_add(1, Relaxed);
            }
            Err(_) => {
                self.counters
                    .invoke_pre_send_failures_total
                    .fetch_add(1, Relaxed);
            }
        }
        result
    }

    async fn invoke_inner(
        &self,
        endpoint_id: &str,
        addr_json: &str,
        digest: &str,
        request_json: &str,
    ) -> std::result::Result<Vec<u8>, BrowserInvokeError> {
        if request_json.len() > BROWSER_MAX_FRAME - 1 - FUNCTION_DIGEST_LEN {
            return Err(BrowserInvokeError::new(
                false,
                "browser request frame too large",
            ));
        }
        let payload =
            encode_invoke(digest, request_json).map_err(|e| BrowserInvokeError::new(false, e))?;
        let frame = encode_request(Op::Invoke, &payload);
        self.request_op(endpoint_id, addr_json, Op::Invoke, frame)
            .await
    }

    /// One fleet-INITIATED CRR anti-entropy round against an admitted browser's replica; `payload` is
    /// a verbatim `encode_crr_sync_request` frame built by hive-cloud, which owns watermarks and
    /// batches.
    pub async fn crr_sync(
        &self,
        endpoint_id: &str,
        addr_json: &str,
        payload: &[u8],
    ) -> std::result::Result<Vec<u8>, BrowserInvokeError> {
        use std::sync::atomic::Ordering::Relaxed;
        self.counters.crr_attempts_total.fetch_add(1, Relaxed);
        if payload.len() > BROWSER_MAX_CRR_FRAME - 1 {
            self.counters.crr_failures_total.fetch_add(1, Relaxed);
            return Err(BrowserInvokeError::new(
                false,
                "browser crr sync frame too large",
            ));
        }
        let frame = encode_request(Op::CrrSync, payload);
        let result = self
            .request_op(endpoint_id, addr_json, Op::CrrSync, frame)
            .await;
        match &result {
            Ok(_) => {
                self.counters.crr_successes_total.fetch_add(1, Relaxed);
            }
            Err(_) => {
                self.counters.crr_failures_total.fetch_add(1, Relaxed);
            }
        }
        result
    }

    /// Shared plumbing for every outbound op on a browser trunk: acquire (one redial), one bounded bi
    /// stream, one framed request, one framed reply checked against the op's own cap
    /// ([`check_len_for`]) before allocation.
    async fn request_op(
        &self,
        endpoint_id: &str,
        addr_json: &str,
        op: Op,
        frame: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, BrowserInvokeError> {
        let mut epoch = None;
        let mut attempt = 0u8;
        let mut io = loop {
            attempt += 1;
            if attempt > 1 {
                self.counters
                    .invoke_redials_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            let acquired = match self.acquire(endpoint_id, addr_json, epoch).await {
                Ok(acquired) => acquired,
                Err(BrowserAcquireError::Fenced) => {
                    return Err(BrowserInvokeError::new(
                        false,
                        "browser endpoint closed during dial",
                    ))
                }
                Err(BrowserAcquireError::Retryable {
                    epoch: failed_epoch,
                    ..
                }) if attempt < 2 => {
                    epoch = Some(failed_epoch);
                    continue;
                }
                Err(BrowserAcquireError::Retryable { error, .. })
                | Err(BrowserAcquireError::Failed(error)) => return Err(error),
            };
            epoch = Some(acquired.epoch);
            let trunk = acquired.trunk;
            let lease =
                BrowserStreamLease::acquire(trunk.clone(), self.global_streams.clone()).await?;
            if !self.epoch_current(endpoint_id, acquired.epoch).await {
                drop(lease);
                return Err(BrowserInvokeError::new(
                    false,
                    "browser endpoint closed before stream open",
                ));
            }
            let mut request = BrowserRequestGuard::new(lease);
            match tokio::time::timeout(open_budget(), trunk.conn.open_bi()).await {
                Ok(Ok((send, recv))) => {
                    request.attach(send, recv);
                    break request;
                }
                Ok(Err(error)) => {
                    let current = self.epoch_current(endpoint_id, acquired.epoch).await;
                    if attempt < 2 && current {
                        request.close(browser_reset::HANDLER_FAILED);
                        drop(request);
                        self.evict_if(endpoint_id, &trunk).await;
                        tracing::debug!(endpoint_id, %error, "browser stream open failed; redialing");
                        continue;
                    }
                    request.close(browser_reset::HANDLER_FAILED);
                    if !current {
                        return Err(BrowserInvokeError::new(
                            false,
                            "browser endpoint closed during stream open",
                        ));
                    }
                    return Err(BrowserInvokeError::new(false, error));
                }
                Err(_) => {
                    let current = self.epoch_current(endpoint_id, acquired.epoch).await;
                    if attempt < 2 && current {
                        request.close(browser_reset::DEADLINE_EXCEEDED);
                        drop(request);
                        self.evict_if(endpoint_id, &trunk).await;
                        continue;
                    }
                    request.close(browser_reset::DEADLINE_EXCEEDED);
                    if !current {
                        return Err(BrowserInvokeError::new(
                            false,
                            "browser endpoint closed during stream open",
                        ));
                    }
                    return Err(BrowserInvokeError::new(
                        false,
                        "browser stream open timed out",
                    ));
                }
            }
        };

        let write = tokio::time::timeout(
            BROWSER_READ_TIMEOUT,
            io.send
                .as_mut()
                .expect("browser send stream is present")
                .write_all(&frame),
        )
        .await;
        match write {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                io.close(browser_reset::HANDLER_FAILED);
                return Err(BrowserInvokeError::new(true, error));
            }
            Err(_) => {
                io.close(browser_reset::DEADLINE_EXCEEDED);
                return Err(BrowserInvokeError::new(
                    true,
                    "browser request write timed out",
                ));
            }
        }
        self.counters
            .bytes_sent_total
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        meter_browser_bytes(endpoint_id, 0, frame.len() as u64);
        if let Err(error) = io
            .send
            .as_mut()
            .expect("browser send stream is present")
            .finish()
        {
            io.close(browser_reset::HANDLER_FAILED);
            return Err(BrowserInvokeError::new(true, error));
        }
        let stopped = tokio::time::timeout(
            BROWSER_READ_TIMEOUT,
            io.send
                .as_mut()
                .expect("browser send stream is present")
                .stopped(),
        )
        .await;
        match stopped {
            Ok(Ok(None)) => {}
            Ok(Ok(Some(code))) => {
                let reset =
                    u32::try_from(code.into_inner()).unwrap_or(browser_reset::HANDLER_FAILED);
                io.close(reset);
                return Err(BrowserInvokeError::new(
                    true,
                    format!("browser peer stopped request with code {reset}"),
                ));
            }
            Ok(Err(error)) => {
                io.close(browser_reset::HANDLER_FAILED);
                return Err(BrowserInvokeError::new(true, error));
            }
            Err(_) => {
                io.close(browser_reset::DEADLINE_EXCEEDED);
                return Err(BrowserInvokeError::new(
                    true,
                    "browser request acknowledgement timed out",
                ));
            }
        }

        let mut len = [0u8; 4];
        let prefix = tokio::time::timeout(
            firstbyte_budget(),
            io.recv
                .as_mut()
                .expect("browser receive stream is present")
                .read_exact(&mut len),
        )
        .await;
        match prefix {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                io.close(browser_reset::MALFORMED_PAYLOAD);
                return Err(BrowserInvokeError::new(true, error));
            }
            Err(_) => {
                io.close(browser_reset::DEADLINE_EXCEEDED);
                return Err(BrowserInvokeError::new(true, "browser reply timed out"));
            }
        }
        let n = match check_len_for(op, len) {
            Ok(n) => n,
            Err(error) => {
                io.close(error.reset_code());
                return Err(BrowserInvokeError::new(true, error));
            }
        };
        let mut reply = vec![0u8; n];
        let body = tokio::time::timeout(
            idle_budget(),
            io.recv
                .as_mut()
                .expect("browser receive stream is present")
                .read_exact(&mut reply),
        )
        .await;
        match body {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                io.close(browser_reset::MALFORMED_PAYLOAD);
                return Err(BrowserInvokeError::new(true, error));
            }
            Err(_) => {
                io.close(browser_reset::DEADLINE_EXCEEDED);
                return Err(BrowserInvokeError::new(
                    true,
                    "browser reply body timed out",
                ));
            }
        }
        let mut trailing = [0u8; 1];
        let eof = tokio::time::timeout(
            idle_budget(),
            io.recv
                .as_mut()
                .expect("browser receive stream is present")
                .read(&mut trailing),
        )
        .await;
        match eof {
            Ok(Ok(None)) => {}
            Ok(Ok(Some(_))) => {
                io.close(browser_reset::MALFORMED_PAYLOAD);
                return Err(BrowserInvokeError::new(
                    true,
                    "browser reply contains trailing bytes",
                ));
            }
            Ok(Err(error)) => {
                io.close(browser_reset::MALFORMED_PAYLOAD);
                return Err(BrowserInvokeError::new(true, error));
            }
            Err(_) => {
                io.close(browser_reset::DEADLINE_EXCEEDED);
                return Err(BrowserInvokeError::new(true, "browser reply EOF timed out"));
            }
        }
        io.disarm();
        meter_browser_bytes(endpoint_id, 4 + reply.len() as u64, 0);
        Ok(reply)
    }
}

/// ms since UNIX epoch — local helper (this crate deliberately has no
/// hive-core dependency).
fn pool_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ceiling of the refused-dial backoff (`PeerPool::refused`) — below the
/// negative-discovery memo's 180 s cap by design.
const REFUSED_BACKOFF_CAP_MS: u64 = 30_000;
/// How long a cached-hint address proven to answer as another identity stays
/// out of that peer's dial set (`PeerPool::hint_distrust`).
const HINT_DISTRUST_TTL_MS: u64 = 600_000;

/// A dial skipped inside the peer's refused-dial backoff window; deliberately NOT a
/// [`DeadPeerTimeout`] — the peer answered "not now", so it must not be read as a death.
#[derive(Clone, Debug)]
pub struct PeerRefused {
    pub node_id: String,
    pub retry_in_ms: u64,
}
impl std::fmt::Display for PeerRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "p2p peer {} is refusing new connections; not dialing for another {}ms",
            self.node_id, self.retry_in_ms
        )
    }
}
impl std::error::Error for PeerRefused {}

/// A relay entry routes by endpoint id, so what answers through it is the dialed identity —
/// unlike a direct address, which is whoever holds it now.
fn is_relay_addr(addr: &iroh::TransportAddr) -> bool {
    matches!(addr, iroh::TransportAddr::Relay(_))
}

/// How one bounded connect attempt ended (`PeerPool::connect_once`).
enum ConnectOutcome {
    Connected(Connection),
    Failed(ConnectFailure, anyhow::Error),
    TimedOut,
}

/// What one `acquire` dial's attempts showed about THIS endpoint (`DialEvidenceTimes::last_timeout`);
/// atomics so the dial future stays `Send`.
#[derive(Default)]
struct DialTrace {
    /// An attempt ran out its connect budget.
    timed_out: std::sync::atomic::AtomicBool,
    /// An attempt got an answer (a refusal, or a handshake with another identity): this endpoint's
    /// connection setup works, so the dial's timeouts were the path's.
    answered: std::sync::atomic::AtomicBool,
}

impl DialTrace {
    fn note(&self, outcome: &ConnectOutcome) {
        match outcome {
            ConnectOutcome::TimedOut => self.timed_out.store(true, Ordering::Relaxed),
            ConnectOutcome::Failed(ConnectFailure::Refused | ConnectFailure::IdentityMismatch, _) => {
                self.answered.store(true, Ordering::Relaxed)
            }
            _ => {}
        }
    }

    /// The dial failed because this side's connects timed out unanswered.
    fn our_timeout(&self) -> bool {
        self.timed_out.load(Ordering::Relaxed) && !self.answered.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct AcquiredPeer {
    key: String,
    generation: u64,
    trunk_incarnation: u64,
    conn: Connection,
}

struct UnpublishedConnection(Option<Connection>);

impl UnpublishedConnection {
    fn new(conn: Connection) -> Self {
        Self(Some(conn))
    }

    fn publish(mut self) -> Connection {
        self.0.take().expect("unpublished connection is present")
    }
}

impl Drop for UnpublishedConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.0.take() {
            conn.close(0u32.into(), b"dial canceled before publication");
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DialLifecycleError {
    Canceled,
    Superseded,
}

impl std::fmt::Display for DialLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Canceled => "leader dial dropped before completing",
            Self::Superseded => "dial superseded by peer lifecycle change",
        })
    }
}

impl std::error::Error for DialLifecycleError {}

#[derive(Clone, Debug)]
enum SharedDialError {
    DeadPeer(DeadPeerTimeout),
    Refused(PeerRefused),
    Failed(String),
    Lifecycle(DialLifecycleError),
}

impl SharedDialError {
    fn from_anyhow(error: &anyhow::Error) -> Self {
        if let Some(refused) = error.downcast_ref::<PeerRefused>() {
            return Self::Refused(refused.clone());
        }
        error
            .downcast_ref::<DeadPeerTimeout>()
            .cloned()
            .map(Self::DeadPeer)
            .unwrap_or_else(|| Self::Failed(error.to_string()))
    }

    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::DeadPeer(error) => anyhow::Error::new(error),
            Self::Refused(error) => anyhow::Error::new(error),
            Self::Failed(message) => anyhow::anyhow!(message),
            Self::Lifecycle(error) => anyhow::Error::new(error),
        }
    }
}

type DialOutcome = Option<std::result::Result<AcquiredPeer, SharedDialError>>;

struct DialFlight {
    generation: u64,
    signal: tokio::sync::watch::Sender<DialOutcome>,
}

#[derive(Default)]
struct DialEvidenceTimes {
    last_success: Option<Instant>,
    last_failure: Option<Instant>,
    /// The last failed dial whose connect attempts ran out their budget with NONE answered — narrower
    /// than `last_failure`; refusals, identity mismatches and negative-discovery short-circuits are
    /// not establishment-wedge evidence.
    last_timeout: Option<Instant>,
}

#[derive(Default)]
struct PeerPoolState {
    trunks: HashMap<String, Trunk>,
    aliases: HashMap<String, String>,
    inflight: HashMap<String, DialFlight>,
    generations: HashMap<String, u64>,
    dial_evidence: HashMap<String, DialEvidenceTimes>,
    next_trunk_incarnation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerDialEvidence {
    pub endpoint_id: String,
    pub last_success_ago: Option<Duration>,
    pub last_failure_ago: Option<Duration>,
    /// Age of the last failed dial whose connect attempts TIMED OUT on this
    /// side, none answered (see `DialEvidenceTimes::last_timeout`).
    pub last_timeout_ago: Option<Duration>,
}

struct DialLeaderGuard<'a> {
    state: &'a StdMutex<PeerPoolState>,
    key: String,
    signal: tokio::sync::watch::Sender<DialOutcome>,
    armed: bool,
}

impl DialLeaderGuard<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DialLeaderGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .inflight
            .get(&self.key)
            .is_some_and(|flight| flight.signal.same_channel(&self.signal))
        {
            state.inflight.remove(&self.key);
            let _ = self
                .signal
                .send_replace(Some(Err(SharedDialError::Lifecycle(
                    DialLifecycleError::Canceled,
                ))));
        }
    }
}

/// Connection pool + multiplexer for the cross-node mesh path: ONE persistent iroh QUIC
/// connection per peer, a NEW bi stream per request, so no request pays a handshake/holepunch.
pub struct PeerPool {
    ep: Endpoint,
    state: StdMutex<PeerPoolState>,
    opened: AtomicU64,
    reused: AtomicU64,
    timeouts: Arc<TimeoutCounters>,
    /// Per-peer WARM backoff (key → next_attempt_ms, cur_delay_ms), consulted only by
    /// [`PeerPool::warm`] — a request-driven `acquire` always dials; grows ×1.5 + dither to a 5-minute
    /// cap, cleared on a successful warm.
    warm_backoff: Mutex<HashMap<String, (u64, u64)>>,
    /// Negative-discovery memo (node_id → until_ms, cur_delay_ms): a peer whose fresh-discovery dial
    /// just failed is not re-discovered for 30 s → 180 s; the cap stays LOW because starving the dial
    /// path of addresses partitions the fleet.
    neg_discovery: Mutex<HashMap<String, (u64, u64)>>,
    /// Refused-dial backoff (endpoint id → until_ms, cur_delay_ms): set when a peer REFUSES through an
    /// identity-routed path (a direct-address refusal is confirmed first — it names no identity);
    /// inside the window `acquire` fails at once without dialing. 1 s growing to a 30 s cap, below
    /// the negative-discovery cap, cleared once a request round-trips.
    refused: StdMutex<HashMap<String, (u64, u64)>>,
    /// Suspended cached-hint IP addresses (endpoint id → until_ms, addresses): a handshake that
    /// fails `UnknownIssuer` reached someone else's endpoint, and the error names no address, so EVERY
    /// direct address of that hint is suspended for [`HINT_DISTRUST_TTL_MS`] — only when a relay
    /// entry remains to dial, so the set is never emptied.
    hint_distrust:
        StdMutex<HashMap<String, (u64, std::collections::BTreeSet<iroh::TransportAddr>)>>,
    /// Tencent-CCN private-path candidates (endpoint id → VPC-private address), populated ONLY by
    /// this node's own trusted gossip merge (`set_private_candidate`); `acquire` adds the candidate
    /// to the dial set for exactly the peers that proved eligibility — one extra candidate per
    /// authenticated peer, never a bulk address set.
    private_candidates: StdMutex<HashMap<String, std::net::SocketAddr>>,
}

impl PeerPool {
    /// Build a pool over a bound endpoint (cheap to clone — `Arc` inside).
    pub fn new(ep: Endpoint) -> Arc<PeerPool> {
        Arc::new(PeerPool {
            ep,
            state: StdMutex::new(PeerPoolState::default()),
            opened: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            timeouts: Arc::new(TimeoutCounters::default()),
            warm_backoff: Mutex::new(HashMap::new()),
            neg_discovery: Mutex::new(HashMap::new()),
            refused: StdMutex::new(HashMap::new()),
            hint_distrust: StdMutex::new(HashMap::new()),
            private_candidates: StdMutex::new(HashMap::new()),
        })
    }

    /// Remaining refused-dial backoff for `key`, if inside a window.
    fn refused_backoff_remaining(&self, key: &str) -> Option<u64> {
        let now = pool_now_ms();
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refused
            .get(key)
            .map(|(until, _)| until.saturating_sub(now))
            .filter(|remaining| *remaining > 0)
    }

    /// The peer refused a dial: open (or grow) its backoff window and return its length; one WARN
    /// per refusal, rate-bounded by the window itself.
    fn note_refused(&self, node_id: &str, key: &str, how: &'static str) -> u64 {
        establish::note(EstablishEvent::OutboundRefused);
        let now = pool_now_ms();
        let backoff = {
            let mut refused = self
                .refused
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let delay = refused
                .get(key)
                .filter(|(until, _)| now <= until + 2 * REFUSED_BACKOFF_CAP_MS)
                .map(|(_, delay)| *delay)
                .unwrap_or(0);
            let grown = if delay == 0 {
                1_000
            } else {
                delay + delay / 2 + (now % (delay / 2 + 1))
            };
            let capped = grown.min(REFUSED_BACKOFF_CAP_MS);
            refused.insert(key.to_string(), (now + capped, capped));
            capped
        };
        tracing::warn!(
            node_id,
            key = %key,
            how,
            backoff_ms = backoff,
            "p2p peer refused our connection; backing off before the next dial"
        );
        backoff
    }

    fn clear_refused(&self, key: &str) {
        self.refused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key);
    }

    /// Strip suspended direct addresses (`hint_distrust`) from `addr` — ONLY when a relay entry
    /// survives the strip; a set left without a relay is dialed as is, since a probably-wrong address
    /// beats no address.
    fn apply_hint_distrust(&self, key: &str, addr: &mut EndpointAddr) {
        let now = pool_now_ms();
        let mut distrust = self
            .hint_distrust
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        distrust.retain(|_, (until, _)| *until > now);
        let Some((_, wrong)) = distrust.get(key) else {
            return;
        };
        let mut stripped = addr.clone();
        stripped.addrs.retain(|a| !wrong.contains(a));
        if stripped.addrs.iter().any(is_relay_addr) {
            *addr = stripped;
        } else if addr.addrs.iter().any(|a| wrong.contains(a)) {
            tracing::debug!(
                key = %key,
                "p2p hint has suspended direct addresses but no relay; dialing its full set"
            );
        }
    }

    /// A dial through `addr` completed a handshake with a DIFFERENT identity: suspend EVERY direct
    /// address of the hint for this endpoint id, but only while the hint carries a relay to dial
    /// instead. Returns whether anything was suspended.
    fn distrust_hint(&self, node_id: &str, key: &str, addr: &EndpointAddr) -> bool {
        let wrong: std::collections::BTreeSet<iroh::TransportAddr> = addr
            .addrs
            .iter()
            .filter(|a| matches!(a, iroh::TransportAddr::Ip(_)))
            .cloned()
            .collect();
        if wrong.is_empty() || !addr.addrs.iter().any(is_relay_addr) {
            tracing::warn!(
                node_id,
                key = %key,
                "p2p cached hint answered with a different identity (UnknownIssuer) and has no \
                 relay entry; suspending nothing, retrying via fresh discovery"
            );
            return false;
        }
        tracing::warn!(
            node_id,
            key = %key,
            addrs = ?wrong,
            ttl_ms = HINT_DISTRUST_TTL_MS,
            "p2p cached hint answered with a different identity (UnknownIssuer); suspending \
             every direct address of the hint for this peer and dialing via its relay"
        );
        let mut distrust = self
            .hint_distrust
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = distrust
            .entry(key.to_string())
            .or_insert_with(|| (0, Default::default()));
        entry.0 = pool_now_ms() + HINT_DISTRUST_TTL_MS;
        entry.1.extend(wrong);
        true
    }

    /// One real outbound connect attempt, counted as an `OutboundAttempt`,
    /// bounded by `budget` and recorded in `trace`.
    async fn connect_once(
        &self,
        addr: EndpointAddr,
        budget: Duration,
        trace: &DialTrace,
    ) -> ConnectOutcome {
        establish::note(EstablishEvent::OutboundAttempt);
        let outcome = match tokio::time::timeout(budget, self.ep.connect(addr, HIVE_ALPN)).await {
            Ok(Ok(conn)) => ConnectOutcome::Connected(conn),
            Ok(Err(error)) => {
                ConnectOutcome::Failed(establish::classify_connect_error(&error), error.into())
            }
            Err(_) => ConnectOutcome::TimedOut,
        };
        trace.note(&outcome);
        outcome
    }

    /// The typed error for a refusal, opening (or growing) the backoff window.
    fn refused_error(&self, node_id: &str, key: &str, how: &'static str) -> anyhow::Error {
        let retry_in_ms = self.note_refused(node_id, key, how);
        anyhow::Error::new(PeerRefused {
            node_id: node_id.to_string(),
            retry_in_ms,
        })
    }

    fn lock_private_candidates(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, std::net::SocketAddr>> {
        self.private_candidates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register `endpoint_id`'s Tencent-CCN private-VPC address as a dial candidate; re-validated with
    /// `is_safe_private_candidate` as a second gate and silently ignored when unsafe.
    pub fn set_private_candidate(&self, endpoint_id: &str, addr: std::net::SocketAddr) {
        if !private_path::is_safe_private_candidate(addr.ip()) {
            tracing::warn!(
                endpoint_id,
                addr = %addr,
                "refused to register unsafe private-path candidate"
            );
            return;
        }
        self.lock_private_candidates()
            .insert(endpoint_id.to_string(), addr);
    }

    /// Drop a peer's private-path candidate (no longer CCN-eligible or aged out); absence is always
    /// safe.
    pub fn clear_private_candidate(&self, endpoint_id: &str) {
        self.lock_private_candidates().remove(endpoint_id);
    }

    /// The private candidate currently registered for `endpoint_id`, if any.
    pub fn private_candidate(&self, endpoint_id: &str) -> Option<std::net::SocketAddr> {
        self.lock_private_candidates().get(endpoint_id).copied()
    }

    /// Reap private-candidate entries for peers absent from `keep` (the round's full gossiped fleet),
    /// called once per gossip round, so a peer that stops being gossiped at all cannot leave an entry
    /// behind forever.
    pub fn retain_private_candidates(&self, keep: &std::collections::HashSet<String>) {
        self.lock_private_candidates()
            .retain(|eid, _| keep.contains(eid));
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PeerPoolState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn canonical_key_locked(state: &PeerPoolState, label: &str) -> String {
        state
            .aliases
            .get(label)
            .cloned()
            .unwrap_or_else(|| label.to_string())
    }

    fn fence_identity_locked(state: &mut PeerPoolState, key: &str, close_reason: &'static [u8]) {
        let generation = state.generations.entry(key.to_string()).or_insert(0);
        *generation = generation.wrapping_add(1);
        if let Some(flight) = state.inflight.remove(key) {
            let _ = flight
                .signal
                .send_replace(Some(Err(SharedDialError::Lifecycle(
                    DialLifecycleError::Superseded,
                ))));
        }
        if let Some(trunk) = state.trunks.remove(key) {
            trunk.conn.close(0u32.into(), close_reason);
        }
    }

    fn install_alias_locked(state: &mut PeerPoolState, label: &str, key: &str) {
        if let Some(previous) = state.aliases.get(label).filter(|old| *old != key).cloned() {
            Self::fence_identity_locked(state, &previous, b"peer label remapped");
        }
        state.aliases.insert(label.to_string(), key.to_string());
    }

    fn allocate_trunk_incarnation_locked(state: &mut PeerPoolState) -> u64 {
        state.next_trunk_incarnation = state.next_trunk_incarnation.wrapping_add(1);
        if state.next_trunk_incarnation == 0 {
            state.next_trunk_incarnation = 1;
        }
        state.next_trunk_incarnation
    }

    /// `(opened, reused)` connection counters — for diagnostics and tests.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.opened.load(Ordering::Relaxed),
            self.reused.load(Ordering::Relaxed),
        )
    }

    pub fn dial_evidence_snapshot(&self) -> Vec<PeerDialEvidence> {
        let state = self.lock_state();
        let now = Instant::now();
        let mut snapshot = state
            .dial_evidence
            .iter()
            .map(|(endpoint_id, evidence)| PeerDialEvidence {
                endpoint_id: endpoint_id.clone(),
                last_success_ago: evidence
                    .last_success
                    .map(|at| now.saturating_duration_since(at)),
                last_failure_ago: evidence
                    .last_failure
                    .map(|at| now.saturating_duration_since(at)),
                last_timeout_ago: evidence
                    .last_timeout
                    .map(|at| now.saturating_duration_since(at)),
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
        snapshot
    }

    fn record_success(&self, acquired: &AcquiredPeer) {
        let mut state = self.lock_state();
        let generation_current = state
            .generations
            .get(&acquired.key)
            .copied()
            .unwrap_or_default()
            == acquired.generation;
        let trunk_current = state
            .trunks
            .get(&acquired.key)
            .is_some_and(|trunk| trunk.incarnation == acquired.trunk_incarnation);
        if !generation_current || !trunk_current {
            return;
        }
        state
            .dial_evidence
            .entry(acquired.key.clone())
            .or_default()
            .last_success = Some(Instant::now());
        drop(state);
        self.clear_refused(&acquired.key);
    }

    /// Proactively ensure a live trunk to `node_id` exists (dial if missing/dead, reuse if warm)
    /// without opening a request stream, so a request-time dial is the rare exception; connect is
    /// bounded. Returns whether a live trunk is cached.
    pub async fn warm(&self, node_id: &str, addr_json: &str) -> bool {
        let addr: EndpointAddr = match serde_json::from_str(addr_json) {
            Ok(addr) => addr,
            Err(_) => return false,
        };
        let key = addr.id.to_string();
        let now = pool_now_ms();
        if let Some((next, _)) = self.warm_backoff.lock().await.get(&key) {
            if now < *next {
                return false;
            }
        }
        match self.acquire(node_id, addr_json).await {
            Ok(_) => {
                self.warm_backoff.lock().await.remove(&key);
                true
            }
            Err(_) => {
                let mut b = self.warm_backoff.lock().await;
                let (_, delay) = b.get(&key).copied().unwrap_or((0, 0));
                let grown = if delay == 0 {
                    5_000
                } else {
                    delay + delay / 2 + (now % (delay / 2 + 1))
                };
                let capped = grown.min(300_000);
                b.insert(key, (now + capped, capped));
                false
            }
        }
    }

    /// Number of currently-cached (live-or-not) trunks — for diagnostics / tests.
    pub async fn trunk_count(&self) -> usize {
        self.lock_state().trunks.len()
    }

    /// Relay cost accounting: classify each live trunk as relay vs direct via iroh's `remote_addr()`
    /// and sum its QUIC byte counters; relayed bytes transit a relay server (a real cost and a SPOF
    /// signal).
    pub async fn relay_stats(&self) -> RelayStats {
        let mut s = RelayStats::default();
        {
            let state = self.lock_state();
            for (endpoint_id, t) in state.trunks.iter() {
                let cs = t.conn.stats();
                let (tx, rx) = (cs.udp_tx.bytes, cs.udp_rx.bytes);
                let paths = t.conn.paths();
                let has_direct = paths.iter().any(|p| p.is_ip());
                let mut ts = TrunkStats {
                    endpoint_id: endpoint_id.clone(),
                    incarnation: t.incarnation,
                    has_direct,
                    selected_is_relay: paths.iter().any(|p| p.is_selected() && p.is_relay()),
                    udp_tx_bytes: tx,
                    udp_rx_bytes: rx,
                    lost_packets: cs.lost_packets,
                    lost_bytes: cs.lost_bytes,
                    paths: Vec::new(),
                };
                for p in paths.iter() {
                    let ps = p.stats();
                    ts.paths.push(TrunkPathStats {
                        remote: format!("{:?}", p.remote_addr()),
                        selected: p.is_selected(),
                        is_ip: p.is_ip(),
                        is_relay: p.is_relay(),
                        rtt_ms: ps.rtt.as_secs_f64() * 1000.0,
                        cwnd: ps.cwnd,
                        congestion_events: ps.congestion_events,
                        spurious_congestion_events: ps.spurious_congestion_events,
                        lost_packets: ps.lost_packets,
                        lost_bytes: ps.lost_bytes,
                        udp_tx_bytes: ps.udp_tx.bytes,
                        udp_rx_bytes: ps.udp_rx.bytes,
                        udp_tx_datagrams: ps.udp_tx.datagrams,
                        udp_rx_datagrams: ps.udp_rx.datagrams,
                        current_mtu: ps.current_mtu,
                        black_holes_detected: ps.black_holes_detected,
                        sent_plpmtud_probes: ps.sent_plpmtud_probes,
                        lost_plpmtud_probes: ps.lost_plpmtud_probes,
                    });
                }
                s.trunks.push(ts);
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
        }
        s.timeouts = self.timeouts.snapshot().await;
        s
    }

    /// Get a live connection to `node_id`: reuse the warm trunk, else dial. The map lock is taken
    /// ONLY to look up / insert, never across the (possibly slow) `connect`.
    async fn acquire(&self, node_id: &str, addr_json: &str) -> Result<AcquiredPeer> {
        let mut addr: EndpointAddr = serde_json::from_str(addr_json)?;
        let id = addr.id;
        let key = id.to_string();

        if let Some(private) = self.private_candidate(&key) {
            let inserted = addr.addrs.insert(iroh::TransportAddr::Ip(private));
            if inserted {
                tracing::debug!(
                    node_id,
                    key = %key,
                    private_addr = %private,
                    "added Tencent CCN private-path candidate to dial set"
                );
            }
        }

        let (signal, generation, leader) = {
            let mut state = self.lock_state();
            if let Some((trunk_incarnation, conn)) = state
                .trunks
                .get(&key)
                .filter(|trunk| trunk.conn.close_reason().is_none())
                .map(|trunk| (trunk.incarnation, trunk.conn.clone()))
            {
                let generation = *state.generations.entry(key.clone()).or_insert(0);
                Self::install_alias_locked(&mut state, node_id, &key);
                drop(state);
                self.reused.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(node_id, key = %key, "trunk reused");
                return Ok(AcquiredPeer {
                    key,
                    generation,
                    trunk_incarnation,
                    conn,
                });
            }
            if let Some(dead) = state.trunks.remove(&key) {
                let overloaded = dead
                    .conn
                    .close_reason()
                    .is_some_and(|reason| is_overload_close(&reason));
                dead.conn.close(0u32.into(), b"dead trunk removed by pool");
                if overloaded {
                    self.note_refused(node_id, &key, "closed as overloaded after the handshake");
                }
            }
            if let Some(retry_in_ms) = self.refused_backoff_remaining(&key) {
                drop(state);
                tracing::debug!(node_id, key = %key, retry_in_ms, "p2p dial skipped: refused-dial backoff");
                return Err(anyhow::Error::new(PeerRefused {
                    node_id: node_id.to_string(),
                    retry_in_ms,
                }));
            }

            let generation = *state.generations.entry(key.clone()).or_insert(0);
            let existing = state
                .inflight
                .get(&key)
                .map(|flight| (flight.generation, flight.signal.clone()));
            match existing {
                Some((flight_generation, signal)) if flight_generation == generation => {
                    (signal, generation, false)
                }
                stale => {
                    if let Some((_, stale_signal)) = stale {
                        state.inflight.remove(&key);
                        let _ = stale_signal.send_replace(Some(Err(SharedDialError::Lifecycle(
                            DialLifecycleError::Superseded,
                        ))));
                    }
                    let (signal, _) = tokio::sync::watch::channel(None);
                    state.inflight.insert(
                        key.clone(),
                        DialFlight {
                            generation,
                            signal: signal.clone(),
                        },
                    );
                    (signal, generation, true)
                }
            }
        };

        if !leader {
            let mut receiver = signal.subscribe();
            return match receiver.wait_for(|outcome| outcome.is_some()).await {
                Ok(outcome) => match (*outcome).clone() {
                    Some(Ok(acquired)) => Ok(acquired),
                    Some(Err(error)) => Err(error.into_anyhow()),
                    None => {
                        Err(SharedDialError::Lifecycle(DialLifecycleError::Canceled).into_anyhow())
                    }
                },
                Err(_) => {
                    Err(SharedDialError::Lifecycle(DialLifecycleError::Canceled).into_anyhow())
                }
            };
        }

        let mut leader_guard = DialLeaderGuard {
            state: &self.state,
            key: key.clone(),
            signal: signal.clone(),
            armed: true,
        };
        let trace = DialTrace::default();
        let dialed: Result<Connection> = async {
            let budget = connect_budget();
            self.apply_hint_distrust(&key, &mut addr);
            let hinted = addr.clone();
            let mut routed = hinted.clone();
            routed.addrs.retain(is_relay_addr);
            let has_direct = hinted.addrs.iter().any(|a| matches!(a, iroh::TransportAddr::Ip(_)));
            match self.connect_once(addr, budget, &trace).await {
                ConnectOutcome::Connected(conn) => Ok(conn),
                ConnectOutcome::Failed(ConnectFailure::Refused, _) if !has_direct => {
                    Err(self.refused_error(node_id, &key, "connection refused"))
                }
                ConnectOutcome::Failed(ConnectFailure::Refused, _) if !routed.addrs.is_empty() => {
                    match self.connect_once(routed, budget, &trace).await {
                        ConnectOutcome::Connected(conn) => Ok(conn),
                        ConnectOutcome::Failed(ConnectFailure::Refused, _) => Err(self.refused_error(
                            node_id,
                            &key,
                            "connection refused (confirmed via the relay)",
                        )),
                        _ => self.dial_fresh(node_id, id, &trace).await,
                    }
                }
                ConnectOutcome::Failed(ConnectFailure::Refused, _) => {
                    self.dial_fresh(node_id, id, &trace).await
                }
                ConnectOutcome::Failed(ConnectFailure::IdentityMismatch, _) => {
                    if !self.distrust_hint(node_id, &key, &hinted) {
                        return self.dial_fresh(node_id, id, &trace).await;
                    }
                    match self.connect_once(routed, budget, &trace).await {
                        ConnectOutcome::Connected(conn) => Ok(conn),
                        ConnectOutcome::Failed(ConnectFailure::Refused, _) => Err(self.refused_error(
                            node_id,
                            &key,
                            "connection refused (via the relay)",
                        )),
                        _ => self.dial_fresh(node_id, id, &trace).await,
                    }
                }
                ConnectOutcome::Failed(ConnectFailure::Other, error) => {
                    tracing::warn!(node_id, err = %error, "p2p connect error using cached hint; retrying via fresh discovery");
                    self.dial_fresh(node_id, id, &trace).await
                }
                ConnectOutcome::TimedOut => {
                    self.timeouts.bump(node_id, PHASE_CONNECT).await;
                    tracing::warn!(
                        node_id,
                        budget_ms = budget.as_millis() as u64,
                        "p2p connect timeout using cached hint; retrying via fresh discovery"
                    );
                    self.dial_fresh(node_id, id, &trace).await
                }
            }
        }
        .await;
        if dialed.is_ok() {
            establish::note(EstablishEvent::OutboundEstablished);
        }

        match dialed {
            Ok(conn) => {
                let mut state = self.lock_state();
                let same_channel = state.inflight.get(&key).is_some_and(|flight| {
                    flight.generation == generation && flight.signal.same_channel(&signal)
                });
                let current =
                    same_channel && state.generations.get(&key).copied().unwrap_or(0) == generation;
                if current {
                    Self::install_alias_locked(&mut state, node_id, &key);
                    let trunk_incarnation = Self::allocate_trunk_incarnation_locked(&mut state);
                    let acquired = AcquiredPeer {
                        key: key.clone(),
                        generation,
                        trunk_incarnation,
                        conn: conn.clone(),
                    };
                    if let Some(old) = state.trunks.insert(
                        key.clone(),
                        Trunk {
                            incarnation: trunk_incarnation,
                            conn: conn.clone(),
                        },
                    ) {
                        old.conn.close(0u32.into(), b"replaced by fresh trunk");
                    }
                    state.inflight.remove(&key);
                    state
                        .dial_evidence
                        .entry(key.clone())
                        .or_default()
                        .last_success = Some(Instant::now());
                    let _ = signal.send_replace(Some(Ok(acquired.clone())));
                    drop(state);
                    leader_guard.disarm();
                    self.opened.fetch_add(1, Ordering::Relaxed);
                    record_kex_telemetry(&conn, "outbound");
                    let private_hint = self.private_candidate(&key).map(|sa| sa.ip());
                    let path = conn
                        .paths()
                        .iter()
                        .find(|p| p.is_selected())
                        .map(|p| match p.remote_addr() {
                            iroh::TransportAddr::Ip(sa) => {
                                private_path::classify_remote_addr(sa.ip(), private_hint).as_str()
                            }
                            iroh::TransportAddr::Relay(_) => private_path::PathKind::Relay.as_str(),
                            _ => "custom",
                        })
                        .unwrap_or("unknown");
                    tracing::info!(node_id, key = %key, path, "trunk opened");
                    Ok(acquired)
                } else {
                    if same_channel {
                        state.inflight.remove(&key);
                        let _ = signal.send_replace(Some(Err(SharedDialError::Lifecycle(
                            DialLifecycleError::Superseded,
                        ))));
                    }
                    conn.close(0u32.into(), b"dial superseded by peer lifecycle change");
                    drop(state);
                    leader_guard.disarm();
                    tracing::info!(node_id, key = %key, "dial superseded; fresh connection closed");
                    Err(anyhow::Error::new(DialLifecycleError::Superseded))
                }
            }
            Err(error) => {
                let shared = SharedDialError::from_anyhow(&error);
                let mut state = self.lock_state();
                let same_channel = state.inflight.get(&key).is_some_and(|flight| {
                    flight.generation == generation && flight.signal.same_channel(&signal)
                });
                let current =
                    same_channel && state.generations.get(&key).copied().unwrap_or(0) == generation;
                if current {
                    state.inflight.remove(&key);
                    let evidence = state.dial_evidence.entry(key.clone()).or_default();
                    let now = Instant::now();
                    evidence.last_failure = Some(now);
                    if trace.our_timeout() {
                        evidence.last_timeout = Some(now);
                    }
                    let _ = signal.send_replace(Some(Err(shared)));
                } else if same_channel {
                    state.inflight.remove(&key);
                    let _ = signal.send_replace(Some(Err(SharedDialError::Lifecycle(
                        DialLifecycleError::Superseded,
                    ))));
                }
                drop(state);
                leader_guard.disarm();
                if current {
                    Err(error)
                } else {
                    Err(anyhow::Error::new(DialLifecycleError::Superseded))
                }
            }
        }
    }

    /// Fallback dial using ONLY the peer's `EndpointId` — no cached addrs or relay — forcing iroh's
    /// Discovery/AddressLookup to resolve a CURRENT address; called from `acquire` only after the
    /// cached-hint attempt failed. Recorded in `trace` (the memo short-circuit sends nothing).
    async fn dial_fresh(
        &self,
        node_id: &str,
        id: iroh::EndpointId,
        trace: &DialTrace,
    ) -> Result<Connection> {
        let now = pool_now_ms();
        let memo_key = id.to_string();
        if let Some((until, _)) = self.neg_discovery.lock().await.get(&memo_key) {
            if now < *until {
                return Err(anyhow::Error::new(DeadPeerTimeout {
                    node_id: node_id.to_string(),
                    phase: "connect",
                    budget_ms: 0,
                }));
            }
        }
        let bump_memo = || async {
            let mut m = self.neg_discovery.lock().await;
            let (_, delay) = m.get(&memo_key).copied().unwrap_or((0, 0));
            let grown = if delay == 0 {
                30_000
            } else {
                delay + delay / 2 + (now % (delay / 2 + 1))
            };
            let capped = grown.min(180_000);
            m.insert(memo_key.clone(), (now + capped, capped));
        };
        let budget = discovery_budget();
        match self.connect_once(EndpointAddr::new(id), budget, trace).await {
            ConnectOutcome::Connected(c) => {
                let c = UnpublishedConnection::new(c);
                self.neg_discovery.lock().await.remove(&memo_key);
                tracing::info!(
                    node_id,
                    "p2p connect recovered via fresh discovery (cached hint was stale)"
                );
                Ok(c.publish())
            }
            ConnectOutcome::Failed(ConnectFailure::Refused, _) => {
                Err(self.refused_error(node_id, &memo_key, "connection refused"))
            }
            ConnectOutcome::Failed(_, e) => {
                bump_memo().await;
                tracing::warn!(node_id, err = %e, "p2p connect error via fresh discovery (giving up)");
                Err(e)
            }
            ConnectOutcome::TimedOut => {
                bump_memo().await;
                self.timeouts.bump(node_id, PHASE_CONNECT).await;
                tracing::warn!(
                    node_id,
                    budget_ms = budget.as_millis() as u64,
                    "p2p discovery-fallback connect timeout (giving up)"
                );
                Err(anyhow::Error::new(DeadPeerTimeout {
                    node_id: node_id.to_string(),
                    phase: "connect",
                    budget_ms: budget.as_millis() as u64,
                }))
            }
        }
    }

    /// Close one failed acquired trunk and remove it only if it is still the published trunk for
    /// this peer, so a delayed failure never evicts a healthy replacement.
    async fn evict_acquired(&self, acquired: &AcquiredPeer) {
        let mut state = self.lock_state();
        let generation_current = state
            .generations
            .get(&acquired.key)
            .copied()
            .unwrap_or_default()
            == acquired.generation;
        let trunk_current = state
            .trunks
            .get(&acquired.key)
            .is_some_and(|trunk| trunk.incarnation == acquired.trunk_incarnation);
        if generation_current && trunk_current {
            state.trunks.remove(&acquired.key);
            state
                .dial_evidence
                .entry(acquired.key.clone())
                .or_default()
                .last_failure = Some(Instant::now());
        }
        drop(state);
        if acquired
            .conn
            .close_reason()
            .is_some_and(|reason| is_overload_close(&reason))
        {
            let node_id = acquired.conn.remote_id().to_string();
            self.note_refused(
                &node_id,
                &acquired.key,
                "closed as overloaded after the handshake",
            );
        }
        acquired
            .conn
            .close(0u32.into(), b"failed acquired trunk evicted by pool");
    }

    /// Cross-node gateway-side call: ONE HTTP request over a NEW bi stream on the peer's reused trunk,
    /// returning the full response. Retries ONCE on a pre-send failure (`open_bi` error or dead trunk
    /// — no bytes left this node); a failure after the request is written is returned for the
    /// caller's candidate failover, never re-executed.
    pub async fn request(
        &self,
        node_id: &str,
        addr_json: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<TunnelResp> {
        let mut s = self
            .request_stream(node_id, addr_json, method, path, headers, body)
            .await?;
        let mut buf = Vec::new();
        while let Some(chunk) = s.recv().await {
            buf.extend_from_slice(&chunk);
        }
        if s.timed_out() {
            return Err(anyhow::Error::new(PostSendTimeout {
                node_id: node_id.to_string(),
                phase: "idle",
                budget_ms: idle_budget().as_millis() as u64,
            }));
        }
        Ok(TunnelResp {
            status: s.status,
            headers: s.headers,
            body: buf,
        })
    }

    /// Streaming variant of [`request`]: the response head plus a receiver yielding body chunks as
    /// the owner produces them; pre-send failures retry once, a failure after the request is on the
    /// wire is returned.
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
            let acquired = match self.acquire(node_id, addr_json).await {
                Ok(acquired) => acquired,
                Err(e) => {
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, recv) =
                match tokio::time::timeout(open_budget(), acquired.conn.open_bi()).await {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.timeouts.bump(node_id, PHASE_OPEN).await;
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(self.dead_peer(node_id, "open", open_budget()));
                    }
                };
            send.write_all(&[STREAM_TUNNEL]).await?;
            send.flush().await?;
            let client = fluid_tunnel::TunnelClient::new(tokio::io::join(recv, send));
            let resp = match tokio::time::timeout(
                firstbyte_budget(),
                client.request(method, path, headers.to_vec(), body, firstbyte_budget()),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_FIRSTBYTE).await;
                    return Err(anyhow::Error::new(PostSendTimeout {
                        node_id: node_id.to_string(),
                        phase: "firstbyte",
                        budget_ms: firstbyte_budget().as_millis() as u64,
                    }));
                }
            };
            self.record_success(&acquired);
            return Ok(TunnelStream {
                status: resp.status,
                headers: resp.headers,
                body: resp.body,
                _client: client,
                idle: idle_budget(),
                node_id: node_id.to_string(),
                timeouts: self.timeouts.clone(),
                idle_timed_out: false,
            });
        }
    }

    /// Build a [`DeadPeerTimeout`] anyhow error for a pre-send phase.
    fn dead_peer(&self, node_id: &str, phase: &'static str, budget: Duration) -> anyhow::Error {
        anyhow::Error::new(DeadPeerTimeout {
            node_id: node_id.to_string(),
            phase,
            budget_ms: budget.as_millis() as u64,
        })
    }

    /// Open a RAW bidirectional byte stream to a peer over its trunk for upgraded (WebSocket)
    /// connections; the owner splices it to its local target. Same pre-send retry-once semantics as
    /// [`request`].
    pub async fn open_raw(&self, node_id: &str, addr_json: &str) -> Result<P2pStream> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let acquired = match self.acquire(node_id, addr_json).await {
                Ok(acquired) => acquired,
                Err(e) => {
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, recv) =
                match tokio::time::timeout(open_budget(), acquired.conn.open_bi()).await {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.timeouts.bump(node_id, PHASE_OPEN).await;
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(self.dead_peer(node_id, "open", open_budget()));
                    }
                };
            send.write_all(&[STREAM_RAW]).await?;
            send.flush().await?;
            return Ok(tokio::io::join(recv, send));
        }
    }

    /// Open a RAW byte stream to a NAMED service on a peer (the mesh-forward primitive for non-HTTP
    /// TCP/UDP): sends a [`RawTarget`] handshake and waits for the owner's admission status BEFORE
    /// returning, so a caller that has consumed no client bytes can fail over. `tcp` streams are
    /// opaque bytes; `udp` streams carry `[u32 len][bytes]` frames
    /// ([`read_raw_datagram`]/[`write_raw_datagram`]).
    pub async fn open_raw_to_port(
        &self,
        node_id: &str,
        addr_json: &str,
        target: &RawTarget,
    ) -> Result<P2pStream> {
        let hs = serde_json::to_vec(target)?;
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let acquired = match self.acquire(node_id, addr_json).await {
                Ok(acquired) => acquired,
                Err(e) => {
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, mut recv) =
                match tokio::time::timeout(open_budget(), acquired.conn.open_bi()).await {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.timeouts.bump(node_id, PHASE_OPEN).await;
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(self.dead_peer(node_id, "open", open_budget()));
                    }
                };
            send.write_all(&[STREAM_RAW_TARGET]).await?;
            send.write_all(&(hs.len() as u32).to_be_bytes()).await?;
            send.write_all(&hs).await?;
            send.flush().await?;
            let mut resp = [0u8; 5];
            match tokio::time::timeout(firstbyte_budget(), recv.read_exact(&mut resp)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_FIRSTBYTE).await;
                    return Err(anyhow::Error::new(PostSendTimeout {
                        node_id: node_id.to_string(),
                        phase: "firstbyte",
                        budget_ms: firstbyte_budget().as_millis() as u64,
                    }));
                }
            }
            if resp[..4] != RAW_TARGET_MAGIC {
                anyhow::bail!(
                    "peer {node_id} sent a non-raw-target response (magic mismatch — likely an un-upgraded peer misrouting this stream to its tunnel handler); treating as refused"
                );
            }
            match resp[4] {
                RAW_TARGET_OK => {
                    self.record_success(&acquired);
                    return Ok(tokio::io::join(recv, send));
                }
                RAW_TARGET_NOT_FOUND => anyhow::bail!(
                    "peer {node_id} has no local target for {}/{} port {} ({:?})",
                    target.project,
                    target.function,
                    target.port,
                    target.proto
                ),
                RAW_TARGET_CONNECT_FAILED => anyhow::bail!(
                    "peer {node_id} could not connect its local leg for {}/{} port {}",
                    target.project,
                    target.function,
                    target.port
                ),
                other => anyhow::bail!("peer {node_id} sent unknown raw-target status {other}"),
            }
        }
    }

    /// Control-plane gossip over the mesh: tunnel an HTTP-shaped request (`method` =
    /// [`GOSSIP_GET`]/[`GOSSIP_POST`]) to the peer's local admin over a new bi stream on the reused
    /// trunk, returning the response body; re-dials once if the trunk is dead.
    pub async fn gossip_request(
        &self,
        node_id: &str,
        addr_json: &str,
        method: u8,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        self.gossip_request_with_response_cap(
            node_id,
            addr_json,
            method,
            path,
            body,
            GOSSIP_MAX_FRAME,
        )
        .await
    }

    /// Gossip request with a caller-selected response bound; ordinary control-plane calls keep the
    /// strict default cap and cannot be made to allocate more by their peer.
    pub async fn gossip_request_with_response_cap(
        &self,
        node_id: &str,
        addr_json: &str,
        method: u8,
        path: &str,
        body: &[u8],
        response_cap: usize,
    ) -> Result<Vec<u8>> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let acquired = match self.acquire(node_id, addr_json).await {
                Ok(acquired) => acquired,
                Err(e) => {
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, mut recv) =
                match tokio::time::timeout(open_budget(), acquired.conn.open_bi()).await {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.timeouts.bump(node_id, PHASE_OPEN).await;
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(self.dead_peer(node_id, "open", open_budget()));
                    }
                };
            if gossip_sign_enabled() {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let trailer = sign_gossip(self.ep.secret_key(), method, path, body, ts);
                send.write_all(&[STREAM_GOSSIP_SIGNED, method]).await?;
                send.write_all(&(path.len() as u32).to_be_bytes()).await?;
                send.write_all(path.as_bytes()).await?;
                send.write_all(&(body.len() as u32).to_be_bytes()).await?;
                send.write_all(body).await?;
                send.write_all(&trailer).await?;
            } else {
                send.write_all(&[STREAM_GOSSIP, method]).await?;
                send.write_all(&(path.len() as u32).to_be_bytes()).await?;
                send.write_all(path.as_bytes()).await?;
                send.write_all(&(body.len() as u32).to_be_bytes()).await?;
                send.write_all(body).await?;
            }
            send.flush().await?;
            let _ = send.finish();
            let resp = self
                .read_response_frame(&mut recv, response_cap, node_id)
                .await?;
            self.record_success(&acquired);
            return Ok(resp);
        }
    }

    /// MESH JOIN: introduce this node to seed `node_id` (dialed by KEY); `proof` is HMAC(fleet secret,
    /// OUR endpoint id). Returns the seed's roster on admission (empty = rejected).
    pub async fn join_request(
        &self,
        node_id: &str,
        addr_json: &str,
        node_json: &[u8],
        proof: &str,
    ) -> Result<Vec<u8>> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let acquired = match self.acquire(node_id, addr_json).await {
                Ok(acquired) => acquired,
                Err(e) => {
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, mut recv) =
                match tokio::time::timeout(open_budget(), acquired.conn.open_bi()).await {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.timeouts.bump(node_id, PHASE_OPEN).await;
                        self.evict_acquired(&acquired).await;
                        if attempt < 2 {
                            continue;
                        }
                        return Err(self.dead_peer(node_id, "open", open_budget()));
                    }
                };
            send.write_all(&[STREAM_JOIN]).await?;
            send.write_all(&(node_json.len() as u32).to_be_bytes())
                .await?;
            send.write_all(node_json).await?;
            send.write_all(&(proof.len() as u32).to_be_bytes()).await?;
            send.write_all(proof.as_bytes()).await?;
            send.flush().await?;
            let _ = send.finish();
            let resp = self
                .read_response_frame(&mut recv, GOSSIP_MAX_FRAME, node_id)
                .await?;
            self.record_success(&acquired);
            return Ok(resp);
        }
    }

    /// Read one `[u32 len][bytes]` response frame: the length prefix under the first-byte budget, then
    /// the body in [`RESPONSE_CHUNK`] reads each bounded by the IDLE budget — a deadline on progress,
    /// not on size, so a multi-MB snapshot can arrive at the ~170 KB/s a trunk sustains. Shared by
    /// every request/response mode.
    async fn read_response_frame<R: AsyncRead + Unpin>(
        &self,
        recv: &mut R,
        cap: usize,
        node_id: &str,
    ) -> Result<Vec<u8>> {
        let mut prefix = [0u8; 4];
        match tokio::time::timeout(
            firstbyte_budget(),
            AsyncReadExt::read_exact(recv, &mut prefix),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                self.timeouts.bump(node_id, PHASE_FIRSTBYTE).await;
                return Err(anyhow::Error::new(PostSendTimeout {
                    node_id: node_id.to_string(),
                    phase: "firstbyte",
                    budget_ms: firstbyte_budget().as_millis() as u64,
                }));
            }
        }
        let len = u32::from_be_bytes(prefix) as usize;
        if len > cap {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large").into(),
            );
        }
        let idle = idle_budget();
        let mut body = Vec::with_capacity(len.min(RESPONSE_PREALLOC));
        let mut chunk = vec![0u8; RESPONSE_CHUNK.min(len.max(1))];
        while body.len() < len {
            let want = (len - body.len()).min(chunk.len());
            match tokio::time::timeout(idle, AsyncReadExt::read(recv, &mut chunk[..want])).await {
                Ok(Ok(0)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("response truncated after {} of {len} bytes", body.len()),
                    )
                    .into())
                }
                Ok(Ok(n)) => body.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_IDLE).await;
                    tracing::warn!(
                        node_id,
                        received = body.len(),
                        expected = len,
                        idle_ms = idle.as_millis() as u64,
                        "p2p response body idle timeout"
                    );
                    return Err(anyhow::Error::new(PostSendTimeout {
                        node_id: node_id.to_string(),
                        phase: "idle",
                        budget_ms: idle.as_millis() as u64,
                    }));
                }
            }
        }
        Ok(body)
    }

    pub async fn close_peer(&self, node_id: &str) {
        let mut state = self.lock_state();
        let key = Self::canonical_key_locked(&state, node_id);
        Self::fence_identity_locked(&mut state, &key, b"closed by pool");
    }

    /// Test/diagnostic helper: forcibly close a peer's cached connection IN PLACE so the next request
    /// must detect the dead trunk and re-dial. Returns whether a trunk was cached.
    pub async fn sever_peer(&self, node_id: &str) -> bool {
        let state = self.lock_state();
        let key = Self::canonical_key_locked(&state, node_id);
        match state.trunks.get(&key) {
            Some(t) => {
                t.conn.close(0u32.into(), b"severed by test");
                t.conn.close_reason().is_some()
            }
            None => false,
        }
    }
}

/// A bootstrap seed peer: a stable PUBLIC node a fresh/wiped node can rendezvous with over iroh
/// with zero prior state — the fixed-identity public FC nodes, never NAT'd Macs. `node_id` is the
/// hex `EndpointId`, `addr_json` a serialized `EndpointAddr`.
#[derive(Clone, Debug)]
pub struct SeedPeer {
    pub node_id: String,
    pub addr_json: String,
}

/// Parse one `HIVE_BOOTSTRAP_PEERS` entry: `<nodeid>`, `<nodeid>@<ip:port>[+<ip:port>…]`,
/// optionally `…|<relay_url>`; the id alone suffices when discovery can resolve it, addrs/relay
/// are hints for a faster/offline dial.
fn parse_seed_addr(entry: &str) -> Option<EndpointAddr> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    let (left, relay) = entry
        .split_once('|')
        .map(|(l, r)| (l, Some(r.trim())))
        .unwrap_or((entry, None));
    let (id_str, addrs_str) = left
        .split_once('@')
        .map(|(i, a)| (i.trim(), Some(a.trim())))
        .unwrap_or((left.trim(), None));
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

/// Bind an iroh endpoint with a PERSISTENT identity from `key_path` (32 raw secret-key bytes,
/// generated and saved 0600 when absent/corrupt) so the `EndpointId` survives restarts; `seeds`
/// are registered with a static address-lookup provider for the cold-start rendezvous. `None` ⇒
/// ephemeral identity.
pub async fn bind_with_key(
    key_path: Option<std::path::PathBuf>,
    seeds: &[SeedPeer],
) -> Result<Endpoint> {
    bind_full(key_path, seeds, &[], true).await
}

/// Like [`bind_with_key`] but also registers self-hosted discovery (Seer): per `discovery_urls`
/// entry a pkarr PUBLISHER and RESOLVER. `n0_discovery` keeps (`true`, additive) or drops
/// (`false`, `Minimal` preset) n0's public pkarr/DNS; the n0 relay stays either way.
pub async fn bind_full(
    key_path: Option<std::path::PathBuf>,
    seeds: &[SeedPeer],
    discovery_urls: &[String],
    n0_discovery: bool,
) -> Result<Endpoint> {
    let tc = QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            IDLE_TIMEOUT
                .try_into()
                .expect("30s is a valid QUIC idle timeout"),
        ))
        .max_concurrent_bidi_streams(iroh::endpoint::VarInt::from_u32(max_streams()))
        .build();
    let mut builder = if n0_discovery {
        Endpoint::builder(N0)
    } else {
        Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::endpoint::default_relay_mode())
    }
    .alpns(vec![HIVE_ALPN.to_vec(), BROWSER_ALPN.to_vec()])
    .transport_config(tc);
    if let Some(map) = relay_map_from_env() {
        let n = map.len();
        builder = builder.relay_mode(iroh::RelayMode::Custom(map));
        tracing::info!(
            relays = n,
            "using self-hosted iroh relays (HIVE_RELAY_URLS)"
        );
    }
    if let Some(port) = std::env::var("HIVE_IROH_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
    {
        let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
        let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        builder = builder
            .clear_ip_transports()
            .bind_addr(v4)
            .and_then(|b| b.bind_addr(v6))
            .expect("UNSPECIFIED v4/v6 with a parsed u16 port is always a bindable socket addr");
        tracing::info!(port, "pinned iroh QUIC bind port (HIVE_IROH_PORT)");
    }
    let secret = key_path.map(|path| load_or_create_secret(&path));
    if let Some(sk) = &secret {
        builder = builder.secret_key(sk.clone());
    }
    let allow_private = std::env::var("HIVE_SEED_ALLOW_PRIVATE")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let mut dropped_addrs = 0usize;
    let seed_addrs: Vec<EndpointAddr> = seeds
        .iter()
        .filter_map(|s| serde_json::from_str::<EndpointAddr>(&s.addr_json).ok())
        .map(|mut a| {
            if !allow_private {
                let before = a.addrs.len();
                a.addrs.retain(|t| match t {
                    iroh::TransportAddr::Relay(_) => true,
                    iroh::TransportAddr::Ip(sa) => crate::dht::is_publicly_routable(sa.ip()),
                    _ => false,
                });
                dropped_addrs += before.saturating_sub(a.addrs.len());
            }
            a
        })
        .filter(|a| !a.addrs.is_empty())
        .collect();
    if dropped_addrs > 0 {
        tracing::info!(
            dropped_addrs,
            seeds = seed_addrs.len(),
            "filtered unroutable seed addresses out of the dial candidate set"
        );
    }
    let seed_count = seed_addrs.len();
    if !seed_addrs.is_empty() {
        builder = builder.address_lookup(iroh::address_lookup::MemoryLookup::from_endpoint_info(
            seed_addrs,
        ));
    }
    let mut pkarr_count = 0usize;
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
                pkarr_count += 1;
            }
            Err(e) => {
                tracing::warn!(url = raw, error = %e, "invalid HIVE_DISCOVERY_DNS entry; skipped")
            }
        }
    }
    dht::record_providers(seed_count, pkarr_count, n0_discovery);
    if let Some(lookup) = dht::lookup_from_env(secret.as_ref()).await {
        builder = builder.address_lookup(lookup);
    }
    builder = builder.crypto_provider(pq_hybrid_crypto_provider());
    let ep = builder.bind().await?;
    Ok(ep)
}

/// The mesh transport's TLS crypto provider: aws-lc-rs with `X25519MLKEM768` FIRST, built as an
/// explicit `kx_groups` list. Mandatory: the `N0`/`Minimal` presets prefer plain `ring` (zero PQ)
/// whenever both TLS backends are compiled in, and `default_provider()`'s order depends on feature
/// unification. A peer without ML-KEM falls back to classical X25519 automatically; what was
/// negotiated is only known from `record_kex_telemetry`.
fn pq_hybrid_crypto_provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = vec![
        rustls::crypto::aws_lc_rs::kx_group::X25519MLKEM768,
        rustls::crypto::aws_lc_rs::kx_group::X25519,
        rustls::crypto::aws_lc_rs::kx_group::SECP256R1,
        rustls::crypto::aws_lc_rs::kx_group::SECP384R1,
    ];
    std::sync::Arc::new(provider)
}

/// Build a self-hosted relay map from `HIVE_RELAY_URLS` (comma-separated); `None` when unset so
/// callers keep the n0 relay default; bad entries are skipped.
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

/// Return `addr_json` with its relay transport entry STEERED to `relay_url` (direct addrs
/// untouched; `None` returns it unchanged). Fails open on unparseable input. This only steers
/// which relay a dial prefers — the relay must also be in the endpoint's live `RelayMap`
/// ([`RelaySet`]) to be usable.
pub fn with_relay_hint(addr_json: &str, relay_url: Option<&str>) -> String {
    let Some(relay_url) = relay_url else {
        return addr_json.to_string();
    };
    let Ok(mut addr) = serde_json::from_str::<EndpointAddr>(addr_json) else {
        return addr_json.to_string();
    };
    let Ok(url) = relay_url.parse::<iroh::RelayUrl>() else {
        return addr_json.to_string();
    };
    addr.addrs
        .retain(|a| !matches!(a, iroh::TransportAddr::Relay(_)));
    addr.addrs.insert(iroh::TransportAddr::Relay(url));
    serde_json::to_string(&addr).unwrap_or_else(|_| addr_json.to_string())
}

/// Tracks the relay URL set THIS PROCESS applied via `insert_relay`/`remove_relay` (iroh exposes
/// no read-back of the live `RelayMap`), so a refresh applies only the delta; scoped to URLs it
/// inserted itself, never the bind-time relays.
pub struct RelaySet {
    ep: Endpoint,
    applied: Mutex<std::collections::HashSet<iroh::RelayUrl>>,
}

impl RelaySet {
    /// Wrap a bound endpoint; the applied set starts empty and only ever holds URLs
    /// [`sync`](Self::sync) inserted.
    pub fn new(ep: Endpoint) -> Arc<RelaySet> {
        Arc::new(RelaySet {
            ep,
            applied: Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// The URLs currently applied by THIS tracker (for diagnostics/tests).
    pub async fn applied(&self) -> Vec<String> {
        self.applied
            .lock()
            .await
            .iter()
            .map(|u| u.to_string())
            .collect()
    }

    /// Reconcile the live relay map to exactly `desired` (unparseable entries skipped): insert the
    /// new, remove what this tracker applied but no longer wants, leave the rest untouched; a no-op
    /// on a closed endpoint.
    pub async fn sync(&self, desired: impl IntoIterator<Item = String>) {
        let desired: std::collections::HashSet<iroh::RelayUrl> = desired
            .into_iter()
            .filter_map(|s| match s.parse::<iroh::RelayUrl>() {
                Ok(u) => Some(u),
                Err(e) => {
                    tracing::warn!(url = %s, error = %e, "invalid relay URL in live relay set; skipped");
                    None
                }
            })
            .collect();
        let mut applied = self.applied.lock().await;
        let to_add: Vec<iroh::RelayUrl> = desired.difference(&applied).cloned().collect();
        let to_remove: Vec<iroh::RelayUrl> = applied.difference(&desired).cloned().collect();
        for url in to_add {
            let cfg = Arc::new(iroh::RelayConfig::from(url.clone()));
            self.ep.insert_relay(url.clone(), cfg).await;
            applied.insert(url);
        }
        for url in to_remove {
            self.ep.remove_relay(&url).await;
            applied.remove(&url);
        }
    }
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
        tracing::warn!(
            ?path,
            "could not persist iroh key; identity will be ephemeral"
        );
    }
    sk
}

/// The combined send+recv halves of a P2P stream, usable as one duplex stream.
pub type P2pStream = tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>;

/// Dial a remote endpoint and open one tunnel stream, writing the `STREAM_TUNNEL` mode byte; the
/// caller wraps the stream in a `TunnelClient`.
pub async fn dial(ep: &Endpoint, addr: impl Into<EndpointAddr>) -> Result<P2pStream> {
    let conn = match tokio::time::timeout(connect_budget(), ep.connect(addr, HIVE_ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            return Err(anyhow::anyhow!(
                "dial connect timeout after {}ms",
                connect_budget().as_millis()
            ))
        }
    };
    let (mut send, recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            return Err(anyhow::anyhow!(
                "dial open_bi timeout after {}ms",
                open_budget().as_millis()
            ))
        }
    };
    send.write_all(&[STREAM_TUNNEL]).await?;
    send.flush().await?;
    Ok(tokio::io::join(recv, send))
}

/// Accept P2P connections forever and serve each bi stream by its leading mode byte:
/// `STREAM_TUNNEL` → a `fluid-tunnel` session proxied to `local_http`, `STREAM_RAW` → a raw splice
/// to it. The instance (owner) side.
pub async fn serve_tunnels(
    ep: Endpoint,
    local_http: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
) {
    serve_tunnels_with_join(ep, local_http, max_concurrency, trust, gossip, None).await
}

/// [`serve_tunnels`] plus the MESH-JOIN surface. With a trust set configured, an UNTRUSTED peer's
/// connection stays open but every stream except [`STREAM_JOIN`] is dropped per-stream; a
/// successful join inserts the peer so the SAME connection's later streams are served. Without a
/// `join` handler untrusted connections are dropped.
pub async fn serve_tunnels_with_join(
    ep: Endpoint,
    local_http: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
    join: Option<JoinHandler>,
) {
    serve_tunnels_full(
        ep,
        local_http,
        max_concurrency,
        trust,
        gossip,
        join,
        None,
        None,
        None,
        &[],
    )
    .await
}

/// [`serve_tunnels_with_join`] plus the raw-target surface (a [`RawTargetResolver`] serves
/// [`STREAM_RAW_TARGET`], else [`RAW_TARGET_NOT_FOUND`]) and the gossip request-path prefixes whose
/// bulk responses are never deprioritized ([`BulkExemptPrefixes`]).
pub async fn serve_tunnels_full(
    ep: Endpoint,
    local_http: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
    join: Option<JoinHandler>,
    raw_resolver: Option<RawTargetResolver>,
    browser_admission: Option<BrowserAdmissionHandler>,
    browser_crr: Option<BrowserCrrHandler>,
    bulk_exempt: BulkExemptPrefixes,
) {
    let browser_resources = BrowserInboundResources::new();
    let budgets = establish::InboundBudgets {
        fleet: establish::Budget::new(max_inbound_conns()),
        browser: establish::Budget::new(max_browser_conns()),
        pending: establish::Budget::new(max_pending_conns()),
        fleet_peers: Arc::new(std::sync::Mutex::new(HashMap::new())),
        browser_peers: browser_resources.peer_connections.clone(),
    };
    establish::register_inbound(&budgets);
    while let Some(incoming) = ep.accept().await {
        establish::note(EstablishEvent::InboundInitial);
        let class = incoming
            .decrypt()
            .and_then(|d| d.alpns())
            .map(|alpns| {
                if alpns
                    .filter_map(Result::ok)
                    .any(|protocol| protocol.as_ref() == BROWSER_ALPN)
                {
                    ConnClass::Browser
                } else {
                    ConnClass::Fleet
                }
            })
            .unwrap_or(ConnClass::Pending);
        let permit = match budgets.for_class(class).try_acquire() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                establish::note(EstablishEvent::Refused);
                warn_budget_refusal(class, &format!("{:?}", incoming.remote_addr()));
                continue;
            }
            Err(tokio::sync::TryAcquireError::Closed) => return,
        };
        let local = local_http.clone();
        let trust = trust.clone();
        let gossip = gossip.clone();
        let join = join.clone();
        let raw_resolver = raw_resolver.clone();
        let browser_admission = browser_admission.clone();
        let browser_crr = browser_crr.clone();
        let browser_resources = browser_resources.clone();
        let budgets = budgets.clone();
        tokio::spawn(async move {
            let inflight = establish::InflightAccept::start();
            let pending = match incoming.accept() {
                Ok(accepting) => accepting.into_0rtt(),
                Err(_) => return,
            };
            let accepted =
                tokio::time::timeout(accept_deadline(), pending.handshake_completed()).await;
            drop(inflight);
            let conn = match accepted {
                Ok(Ok(conn)) => conn,
                Ok(Err(_)) => return,
                Err(_) => {
                    pending.close(FLEET_CLOSE_OVERLOADED.into(), b"accept deadline exceeded");
                    establish::note(EstablishEvent::AcceptStuck);
                    warn_accept_stuck(class);
                    return;
                }
            };
            drop(pending);
            establish::note(EstablishEvent::InboundEstablished);
            let actual = if conn.alpn() == BROWSER_ALPN {
                ConnClass::Browser
            } else if conn.alpn() == HIVE_ALPN {
                ConnClass::Fleet
            } else {
                return;
            };
            let _permit = if actual == class {
                permit
            } else {
                match budgets.for_class(actual).try_acquire() {
                    Ok(real) => {
                        drop(permit);
                        real
                    }
                    Err(_) => {
                        drop(permit);
                        refuse_admitted(&conn, actual, "budget exhausted");
                        warn_budget_refusal(actual, &conn.remote_id().fmt_short().to_string());
                        return;
                    }
                }
            };
            record_kex_telemetry(&conn, actual.as_str());
            if actual == ConnClass::Browser {
                let remote_id = conn.remote_id().to_string();
                let admitted = match browser_admission {
                    Some(check) => check(remote_id.clone()).await,
                    None => true,
                };
                if !admitted {
                    tracing::warn!(peer = %remote_id, "rejected unadmitted browser peer");
                    conn.close(
                        browser_reset::FORBIDDEN.into(),
                        b"browser admission required",
                    );
                    return;
                }
                let Some(_peer_connection) = BrowserCountGuard::acquire(
                    browser_resources.peer_connections.clone(),
                    remote_id.clone(),
                    BROWSER_MAX_CONNECTIONS_PER_ENDPOINT,
                ) else {
                    conn.close(
                        browser_reset::OVERLOADED.into(),
                        b"browser peer connection limit reached",
                    );
                    return;
                };
                serve_browser_conn(conn, remote_id, browser_resources, browser_crr).await;
                return;
            }
            let remote_id = conn.remote_id().to_string();
            let _peer_connection = match max_conns_per_endpoint() {
                0 => None,
                cap => {
                    match BrowserCountGuard::acquire(budgets.fleet_peers.clone(), remote_id, cap) {
                        Some(guard) => Some(guard),
                        None => {
                            refuse_admitted(&conn, actual, "per-endpoint connection cap reached");
                            warn_endpoint_cap(&conn.remote_id().fmt_short().to_string(), cap);
                            return;
                        }
                    }
                }
            };
            serve_fleet_conn(
                conn,
                local,
                max_concurrency,
                trust,
                gossip,
                join,
                raw_resolver,
                bulk_exempt,
            )
            .await;
        });
    }
}

/// Close an ADMITTED (handshake-complete) connection this node has no budget
/// for, with the overload code a dialer backs off on, and count it.
fn refuse_admitted(conn: &Connection, class: ConnClass, why: &'static str) {
    establish::note(EstablishEvent::Refused);
    let code = match class {
        ConnClass::Browser => browser_reset::OVERLOADED,
        _ => FLEET_CLOSE_OVERLOADED,
    };
    conn.close(code.into(), why.as_bytes());
}

/// The budget-exhausted WARN, at most one line per class per 10 s, carrying occupancy, refusal
/// count and the remote.
fn warn_budget_refusal(class: ConnClass, remote: &str) {
    static GATES: [establish::WarnGate; 3] = [
        establish::WarnGate::new(),
        establish::WarnGate::new(),
        establish::WarnGate::new(),
    ];
    let gate = &GATES[class as usize];
    if let Some(suppressed) = gate.admit(Duration::from_secs(10)) {
        let stats = establish_stats(60);
        let (in_use, limit) = match class {
            ConnClass::Fleet => (stats.budget_in_use.fleet, stats.budget_limit.fleet),
            ConnClass::Browser => (stats.budget_in_use.browser, stats.budget_limit.browser),
            ConnClass::Pending => (stats.budget_in_use.pending, stats.budget_limit.pending),
        };
        tracing::warn!(
            class = class.as_str(),
            remote,
            in_use,
            limit = ?limit,
            refused_since_last_line = suppressed + 1,
            refused_last_60s = stats.refused_window,
            accept_stuck = stats.accept_stuck,
            accept_inflight = stats.accept_inflight,
            "P2P connection budget exhausted; rejecting connection"
        );
    }
}

/// The per-endpoint admission-cap WARN, at most one line per 10 s (an ungated line per refusal is
/// a journal flood any remote can drive).
fn warn_endpoint_cap(peer: &str, cap: usize) {
    static GATE: establish::WarnGate = establish::WarnGate::new();
    if let Some(suppressed) = GATE.admit(Duration::from_secs(10)) {
        tracing::warn!(
            peer,
            cap,
            refused_since_last_line = suppressed + 1,
            "P2P per-endpoint connection cap reached; closing the NEW connection \
             (established trunks are never evicted)"
        );
    }
}

/// The accept-deadline WARN, at most one line per 10 s.
fn warn_accept_stuck(class: ConnClass) {
    static GATE: establish::WarnGate = establish::WarnGate::new();
    if let Some(suppressed) = GATE.admit(Duration::from_secs(10)) {
        let stats = establish_stats(60);
        tracing::warn!(
            class = class.as_str(),
            deadline_ms = accept_deadline().as_millis() as u64,
            stuck_since_last_line = suppressed + 1,
            accept_stuck_total = stats.accept_stuck,
            accept_inflight = stats.accept_inflight,
            accept_inflight_oldest_ms = ?stats.accept_inflight_oldest_ms,
            "P2P accept exceeded its deadline (iroh connection registration hung, or the remote \
             kept its handshake alive past it); closed it and released its budget slot"
        );
    }
}

async fn serve_fleet_conn(
    conn: Connection,
    local: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
    join: Option<JoinHandler>,
    raw_resolver: Option<RawTargetResolver>,
    bulk_exempt: BulkExemptPrefixes,
) {
    let remote_id = conn.remote_id().to_string();
    if let Some(trust) = &trust {
        if !peer_trusted(trust, &remote_id) && join.is_none() {
            tracing::warn!(peer = %remote_id, "rejected untrusted P2P peer (#20 peer trust)");
            return;
        }
    }
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        let local = local.clone();
        let gossip = gossip.clone();
        let join = join.clone();
        let raw_resolver = raw_resolver.clone();
        let rid = remote_id.clone();
        let trust = trust.clone();
        tokio::spawn(async move {
            let mut mode = [0u8; 1];
            if recv.read_exact(&mut mode).await.is_err() {
                return;
            }
            let trusted = trust
                .as_ref()
                .map(|trust| peer_trusted(trust, &rid))
                .unwrap_or(true);
            if mode[0] == STREAM_JOIN {
                if let Some(join) = join {
                    serve_join(recv, send, join, rid).await;
                }
                return;
            }
            if !trusted {
                tracing::warn!(peer = %rid, mode = mode[0], "dropped stream from untrusted peer (join required first)");
                return;
            }
            match mode[0] {
                STREAM_GOSSIP | STREAM_GOSSIP_SIGNED => {
                    if let Some(gossip) = gossip {
                        serve_gossip(
                            recv,
                            send,
                            gossip,
                            mode[0] == STREAM_GOSSIP_SIGNED,
                            rid,
                            trust,
                            bulk_exempt,
                        )
                        .await;
                    }
                }
                STREAM_RAW => raw_splice(tokio::io::join(recv, send), &local).await,
                STREAM_RAW_TARGET => serve_raw_target(recv, send, raw_resolver).await,
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
}

/// Refuse one `hive/browser/0` stream with a `hive-browser-proto` reset code.
fn reject_browser_stream(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    code: u32,
) {
    let _ = send.reset(code.into());
    let _ = recv.stop(code.into());
}

async fn write_browser_reply(
    send: &mut iroh::endpoint::SendStream,
    payload: &[u8],
) -> std::result::Result<(), ()> {
    send.write_all(&(payload.len() as u32).to_le_bytes())
        .await
        .map_err(|_| ())?;
    send.write_all(payload).await.map_err(|_| ())
}

async fn serve_browser_conn(
    conn: Connection,
    remote_id: String,
    resources: BrowserInboundResources,
    crr_handler: Option<BrowserCrrHandler>,
) {
    let connection_streams = Arc::new(tokio::sync::Semaphore::new(
        BROWSER_MAX_STREAMS_PER_CONNECTION,
    ));
    let connection_activity = Arc::new(std::sync::Mutex::new(BrowserConnectionActivity {
        active: 0,
        idle_since: Some(Instant::now()),
    }));
    loop {
        let wait = {
            let activity = connection_activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if activity.active > 0 {
                BROWSER_CONNECTION_IDLE_TIMEOUT
            } else {
                BROWSER_CONNECTION_IDLE_TIMEOUT.saturating_sub(
                    activity
                        .idle_since
                        .map(|since| since.elapsed())
                        .unwrap_or_default(),
                )
            }
        };
        let (mut send, mut recv) = match tokio::time::timeout(wait, conn.accept_bi()).await {
            Ok(Ok(streams)) => streams,
            Ok(Err(_)) => break,
            Err(_) => {
                let idle = {
                    let activity = connection_activity
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    activity.active == 0
                        && activity
                            .idle_since
                            .is_some_and(|since| since.elapsed() >= BROWSER_CONNECTION_IDLE_TIMEOUT)
                };
                if idle {
                    conn.close(
                        browser_reset::DEADLINE_EXCEEDED.into(),
                        b"browser connection idle",
                    );
                    break;
                }
                continue;
            }
        };
        let peer_stream = BrowserCountGuard::acquire(
            resources.peer_streams.clone(),
            remote_id.clone(),
            BROWSER_MAX_STREAMS_PER_ENDPOINT,
        );
        let permits = resources
            .streams
            .clone()
            .try_acquire_owned()
            .ok()
            .zip(connection_streams.clone().try_acquire_owned().ok())
            .zip(peer_stream);
        let Some(((stream_permit, connection_permit), peer_permit)) = permits else {
            reject_browser_stream(&mut send, &mut recv, browser_reset::OVERLOADED);
            continue;
        };
        let connection_activity =
            BrowserConnectionStreamGuard::acquire(connection_activity.clone());
        let resources = resources.clone();
        let crr_handler = crr_handler.clone();
        let remote_id = remote_id.clone();
        tokio::spawn(async move {
            let _stream_permit = stream_permit;
            let _connection_permit = connection_permit;
            let _peer_permit = peer_permit;
            let _connection_activity = connection_activity;
            let request = tokio::time::timeout(BROWSER_READ_TIMEOUT, async {
                let mut lenb = [0u8; 4];
                recv.read_exact(&mut lenb)
                    .await
                    .map_err(|_| browser_reset::MALFORMED_PAYLOAD)?;
                let declared = u32::from_le_bytes(lenb) as usize;
                if declared == 0 {
                    return Err(browser_reset::MALFORMED_PAYLOAD);
                }
                let mut opb = [0u8; 1];
                recv.read_exact(&mut opb)
                    .await
                    .map_err(|_| browser_reset::MALFORMED_PAYLOAD)?;
                let op = Op::from_byte(opb[0]).map_err(|error| error.reset_code())?;
                let len = check_len_for(op, lenb).map_err(|error| error.reset_code())?;
                if len == 0 {
                    return Err(browser_reset::MALFORMED_PAYLOAD);
                }
                match op {
                    Op::Echo if len - 1 > BROWSER_MAX_ECHO => {
                        return Err(browser_reset::FRAME_TOO_LARGE);
                    }
                    Op::Echo | Op::CrrSync => {}
                    Op::Invoke | Op::AssetGet => {
                        return Err(browser_reset::NO_HANDLER);
                    }
                }
                let bytes = u32::try_from(len - 1).map_err(|_| browser_reset::FRAME_TOO_LARGE)?;
                let byte_permit = resources
                    .bytes
                    .clone()
                    .try_acquire_many_owned(bytes)
                    .map_err(|_| browser_reset::OVERLOADED)?;
                let mut payload = vec![0u8; len - 1];
                recv.read_exact(&mut payload)
                    .await
                    .map_err(|_| browser_reset::MALFORMED_PAYLOAD)?;
                let mut trailing = [0u8; 1];
                match recv.read(&mut trailing).await {
                    Ok(None) => {}
                    Ok(Some(_)) | Err(_) => return Err(browser_reset::MALFORMED_PAYLOAD),
                }
                meter_browser_bytes(&remote_id, 5 + payload.len() as u64, 0);
                Ok::<_, u32>((op, payload, byte_permit))
            })
            .await;
            let (op, payload, _byte_permit) = match request {
                Ok(Ok(request)) => request,
                Ok(Err(code)) => {
                    reject_browser_stream(&mut send, &mut recv, code);
                    return;
                }
                Err(_) => {
                    reject_browser_stream(&mut send, &mut recv, browser_reset::DEADLINE_EXCEEDED);
                    return;
                }
            };
            let reply = match op {
                Op::Echo => payload,
                Op::CrrSync => {
                    let Some(handler) = crr_handler else {
                        reject_browser_stream(&mut send, &mut recv, browser_reset::NO_HANDLER);
                        return;
                    };
                    match tokio::time::timeout(
                        Duration::from_secs(30),
                        handler(remote_id.clone(), payload),
                    )
                    .await
                    {
                        Ok(Ok(reply)) => reply,
                        Ok(Err(code)) => {
                            reject_browser_stream(&mut send, &mut recv, code);
                            return;
                        }
                        Err(_) => {
                            reject_browser_stream(
                                &mut send,
                                &mut recv,
                                browser_reset::DEADLINE_EXCEEDED,
                            );
                            return;
                        }
                    }
                }
                Op::Invoke | Op::AssetGet => unreachable!("filtered above"),
            };
            if reply.len() > op.frame_cap() {
                reject_browser_stream(&mut send, &mut recv, browser_reset::HANDLER_FAILED);
                return;
            }
            match tokio::time::timeout(BROWSER_READ_TIMEOUT, write_browser_reply(&mut send, &reply))
                .await
            {
                Ok(Ok(())) => {
                    meter_browser_bytes(&remote_id, 0, 4 + reply.len() as u64);
                    if send.finish().is_err() {
                        reject_browser_stream(&mut send, &mut recv, browser_reset::HANDLER_FAILED);
                    }
                }
                Ok(Err(())) => {
                    reject_browser_stream(&mut send, &mut recv, browser_reset::HANDLER_FAILED)
                }
                Err(_) => {
                    reject_browser_stream(&mut send, &mut recv, browser_reset::DEADLINE_EXCEEDED)
                }
            }
        });
    }
}

/// Server side of a [`STREAM_JOIN`] stream: read `(node_json, proof)`, hand them to the join
/// handler with the connection's authenticated remote identity, frame the response back (empty =
/// rejected).
async fn serve_join<R, W>(mut recv: R, mut send: W, handler: JoinHandler, remote_id: String)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let node_json = match read_frame(&mut recv).await {
        Ok(b) => b,
        Err(_) => return,
    };
    let proof = match read_frame(&mut recv).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return,
    };
    let resp = handler(remote_id, node_json, proof).await;
    let _ = send.write_all(&(resp.len() as u32).to_be_bytes()).await;
    let _ = send.write_all(&resp).await;
    let _ = send.flush().await;
    let _ = send.shutdown().await;
}

/// Test/diagnostic helper: accept connections + bi streams but NEVER answer (the
/// "accept-but-silent" peer) so a caller's first-byte timeout must fire; used by the live
/// witnesses, never production.
#[doc(hidden)]
pub async fn serve_silent(ep: Endpoint) {
    while let Some(incoming) = ep.accept().await {
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            while let Ok((send, recv)) = conn.accept_bi().await {
                tokio::spawn(async move {
                    let _keep = (send, recv); // hold open, never respond
                    std::future::pending::<()>().await;
                });
            }
        });
    }
}

/// A gossip response at least this large is BULK and is sent below the default stream priority
/// unless its path is exempt.
const BULK_RESPONSE_BYTES: usize = 256 << 10;

/// Request-path prefixes whose responses are EXEMPT from bulk deprioritization. quinn's scheduler
/// is STRICT priority: a deprioritized reply makes no progress while any default-priority stream
/// has data, so a replication pull the requester is blocked on must share the round-robin
/// instead.
type BulkExemptPrefixes = &'static [&'static str];

/// A response sink whose transmit priority can be lowered.
trait ResponsePriority {
    /// Let every default-priority stream on the same connection go first.
    fn deprioritize(&self);
}

impl ResponsePriority for iroh::endpoint::SendStream {
    fn deprioritize(&self) {
        let _ = self.set_priority(-1);
    }
}

async fn serve_gossip<R, W>(
    mut recv: R,
    mut send: W,
    handler: GossipHandler,
    signed: bool,
    remote_id: String,
    trust: Option<TrustSet>,
    bulk_exempt: BulkExemptPrefixes,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + ResponsePriority,
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
    let mode = verify_mode();
    let mut verified_signer: Option<String> = None;
    if signed {
        let mut trailer = [0u8; 104];
        if recv.read_exact(&mut trailer).await.is_err() {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        match verify_gossip(&trailer, m[0], &path, &body, &remote_id, now) {
            Ok(signer) => {
                let trusted = trust
                    .as_ref()
                    .map(|t| peer_trusted(t, &signer))
                    .unwrap_or(true);
                if trusted {
                    verified_signer = Some(signer);
                } else if mode == VerifyMode::Enforce {
                    VERIFY_STATS.rejected.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(peer = %remote_id, %path, %signer, "REJECTED gossip (signature valid but signer is not a trusted fleet member)");
                    let _ = send.write_all(&0u32.to_be_bytes()).await;
                    let _ = send.flush().await;
                    let _ = send.shutdown().await;
                    return;
                } else {
                    if mode == VerifyMode::Log {
                        tracing::warn!(peer = %remote_id, %path, %signer, "gossip signer NOT in trust set (log mode — serving anyway)");
                    }
                    verified_signer = Some(signer);
                }
            }
            Err(reason) => {
                if mode == VerifyMode::Enforce {
                    VERIFY_STATS.rejected.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(peer = %remote_id, %path, %reason, "REJECTED gossip (signature verification failed, enforce mode)");
                    let _ = send.write_all(&0u32.to_be_bytes()).await;
                    let _ = send.flush().await;
                    let _ = send.shutdown().await;
                    return;
                }
                if mode == VerifyMode::Log {
                    tracing::warn!(peer = %remote_id, %path, %reason, "gossip signature INVALID (log mode — serving anyway)");
                }
            }
        }
    } else {
        VERIFY_STATS.unsigned.fetch_add(1, Ordering::Relaxed);
        if mode == VerifyMode::Enforce {
            VERIFY_STATS.rejected.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(peer = %remote_id, %path, "REJECTED unsigned gossip (enforce mode)");
            let _ = send.write_all(&0u32.to_be_bytes()).await;
            let _ = send.flush().await;
            let _ = send.shutdown().await;
            return;
        }
    }
    let bulk_exempt_hit = bulk_exempt.iter().any(|prefix| path.starts_with(prefix));
    let resp = handler(m[0], path, body, verified_signer).await;
    if resp.len() >= BULK_RESPONSE_BYTES && !bulk_exempt_hit {
        send.deprioritize();
    }
    let len = (resp.len() as u32).to_be_bytes();
    let _ = send.write_all(&len).await;
    let _ = send.write_all(&resp).await;
    let _ = send.flush().await;
    let _ = send.shutdown().await;
}

/// Splice a raw P2P stream to a fresh TCP connection to `local_http` for upgraded (WebSocket)
/// connections.
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

/// Server side of a [`STREAM_RAW_TARGET`] stream: read the [`RawTarget`] handshake, resolve it via
/// the [`RawTargetResolver`], answer the admission status, then splice (`tcp`:
/// `copy_bidirectional`; `udp`: datagram frames ↔ a connected local UDP socket, session owned by
/// the OPENER). Every refusal is an explicit status byte, never a silent close.
async fn serve_raw_target<R, W>(mut recv: R, mut send: W, resolver: Option<RawTargetResolver>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let refuse = |mut send: W, code: u8| async move {
        let mut buf = [0u8; 5];
        buf[..4].copy_from_slice(&RAW_TARGET_MAGIC);
        buf[4] = code;
        let _ = send.write_all(&buf).await;
        let _ = send.flush().await;
    };
    let raw = match read_frame_max(&mut recv, 4096).await {
        Ok(b) => b,
        Err(_) => {
            refuse(send, RAW_TARGET_NOT_FOUND).await;
            return;
        }
    };
    let Ok(target) = serde_json::from_slice::<RawTarget>(&raw) else {
        refuse(send, RAW_TARGET_NOT_FOUND).await;
        return;
    };
    let Some(resolver) = resolver else {
        refuse(send, RAW_TARGET_NOT_FOUND).await;
        return;
    };
    let Some(conn) = resolver(target.clone()).await else {
        tracing::debug!(project = %target.project, function = %target.function, port = target.port, proto = ?target.proto, "raw target: no local leg resolved");
        refuse(send, RAW_TARGET_NOT_FOUND).await;
        return;
    };
    let _guard = conn.guard;
    match target.proto {
        RawProto::Tcp => {
            let mut tcp = match tokio::net::TcpStream::connect(&conn.addr).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!(local = %conn.addr, error = %e, "raw target: local TCP connect failed");
                    refuse(send, RAW_TARGET_CONNECT_FAILED).await;
                    return;
                }
            };
            let mut ok = [0u8; 5];
            ok[..4].copy_from_slice(&RAW_TARGET_MAGIC);
            ok[4] = RAW_TARGET_OK;
            if send.write_all(&ok).await.is_err() || send.flush().await.is_err() {
                return;
            }
            let mut stream = tokio::io::join(recv, send);
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
        }
        RawProto::Udp => {
            let sock = match tokio::net::UdpSocket::bind("127.0.0.1:0").await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "raw target: local UDP bind failed");
                    refuse(send, RAW_TARGET_CONNECT_FAILED).await;
                    return;
                }
            };
            if let Err(e) = sock.connect(&conn.addr).await {
                tracing::debug!(local = %conn.addr, error = %e, "raw target: local UDP connect failed");
                refuse(send, RAW_TARGET_CONNECT_FAILED).await;
                return;
            }
            let mut ok = [0u8; 5];
            ok[..4].copy_from_slice(&RAW_TARGET_MAGIC);
            ok[4] = RAW_TARGET_OK;
            if send.write_all(&ok).await.is_err() || send.flush().await.is_err() {
                return;
            }
            let sock = Arc::new(sock);
            let inbound_sock = sock.clone();
            let mut inbound = tokio::spawn(async move {
                while let Ok(Some(d)) = read_raw_datagram(&mut recv).await {
                    if inbound_sock.send(&d).await.is_err() {
                        break;
                    }
                }
            });
            let mut buf = vec![0u8; RAW_MAX_DATAGRAM];
            loop {
                tokio::select! {
                    _ = &mut inbound => break,
                    r = sock.recv(&mut buf) => match r {
                        Ok(n) => {
                            if write_raw_datagram(&mut send, &buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                }
            }
            inbound.abort();
        }
    }
}

/// Assert at compile time that a `P2pStream` satisfies the tunnel transport bound.
fn _assert_duplex<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>() {}
#[allow(dead_code)]
fn _check() {
    _assert_duplex::<P2pStream>();
}
