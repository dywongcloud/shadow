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
/// SIGNED control-plane gossip (web3 trustlessness): same request framing as
/// [`STREAM_GOSSIP`] followed by a trailer `[32B signer pubkey][8B ts_ms][64B sig]`.
/// The ed25519 signature covers a domain-separated preimage of the whole request
/// (`hive-gossip-v1 || method || path || body || ts`), so the receiver verifies the
/// MESSAGE cryptographically — not just the transport — and additionally binds the
/// signer to the QUIC connection's authenticated remote identity (signer == remote,
/// so a signed message can't be replayed by a third party from another channel).
const STREAM_GOSSIP_SIGNED: u8 = 0x03;
/// MESH JOIN (hot-join): a NOT-YET-TRUSTED node introduces itself. Framing:
/// `[u32 node_len][node_json][u32 proof_len][proof]` -> response `[u32 len][bytes]`
/// (empty = rejected). The caller's identity is the QUIC connection's
/// authenticated remote id (ed25519, unspoofable) — the join handler verifies a
/// shared-secret HMAC over THAT id, so possession of the fleet secret admits the
/// key without any per-node allowlist edit or restart. This is the ONLY stream
/// mode an untrusted connection may use when a trust set is enforced; every
/// other mode on an untrusted connection is dropped per-stream.
const STREAM_JOIN: u8 = 0x04;
/// RAW proxy to a NAMED local target (generic TCP/UDP mesh forwarding): unlike
/// [`STREAM_RAW`] — whose accept side is hard-wired to splice into the owner's
/// local HTTP gateway, which is why the WebSocket path must replay an HTTP
/// upgrade request over it — this mode opens with an explicit machine-readable
/// handshake naming WHICH deployment/function/container-port the opener wants,
/// so protocols with no HTTP request to replay (Postgres wire, Minecraft, DNS,
/// game UDP, …) can cross the mesh. Framing:
///   opener → owner:  `[u32 len][RawTarget JSON]` (see [`RawTarget`]), then payload
///   owner  → opener: `[1B status]` ([`RAW_TARGET_OK`] / error codes), then payload
/// The status byte is written BEFORE any spliced bytes, so the opener can fail
/// over to another candidate node without having consumed any client bytes
/// (same failover-safety property `edge::ws_proxy` relies on).
/// Payload after an OK status:
///   * `proto: tcp`  — an opaque byte splice both ways (copy_bidirectional).
///   * `proto: udp`  — length-prefixed datagrams `[u32 len][bytes]` both ways,
///     one frame per datagram (boundaries preserved), each ≤ [`RAW_MAX_DATAGRAM`].
/// MIXED-FLEET NOTE: an old receiver misparses this mode byte as a tunnel
/// session (the dispatcher's `_` arm) rather than rejecting it outright. This
/// is made SAFE (not just avoided-by-convention) by [`RAW_TARGET_MAGIC`]: the
/// admission response the real handler writes is unmistakable, so a stream
/// that lands on an old peer either times out (no unsolicited write arrives
/// within the firstbyte budget) or gets a magic-mismatch — either way
/// `open_raw_to_port` returns `Err` and the caller fails over to its next
/// candidate, never a garbage splice. Compatible peers are still preferred
/// where a capability signal is available (nearer/healthier candidates are
/// tried first), but correctness no longer depends on the caller having
/// perfect knowledge of every peer's version — unlike the coordination-only
/// staged-rollout rule [`STREAM_GOSSIP_SIGNED`] still relies on.
const STREAM_RAW_TARGET: u8 = 0x05;
const GOSSIP_METHOD_GET: u8 = 0;
const GOSSIP_METHOD_POST: u8 = 1;
/// Domain separator for gossip signatures (versioned; bump on format change).
const GOSSIP_SIG_DOMAIN: &[u8] = b"hive-gossip-v1";
/// Cap on a single gossip frame (request path/body or response) — gossip payloads
/// are small JSON rosters; this just bounds a malformed/hostile length prefix.
const GOSSIP_MAX_FRAME: usize = 16 * 1024 * 1024;

/// Serves one gossip request: `(method, path, body, verified_signer) -> response
/// body bytes`. The caller (hive-cloud) wires this to dispatch onto its local admin
/// handlers, so the exact same endpoints that answer HTTP gossip answer iroh gossip.
/// `verified_signer` is `Some(node_id)` when the request carried a VALID ed25519
/// signature bound to the connection's remote identity (see [`STREAM_GOSSIP_SIGNED`]),
/// `None` for legacy-unsigned requests (only admitted outside enforce mode).
pub type GossipHandler = Arc<
    dyn Fn(u8, String, Vec<u8>, Option<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Serves one MESH-JOIN request: `(remote_id, node_json, proof) -> response body`
/// (empty = rejected). `remote_id` is the QUIC connection's authenticated peer
/// identity — the handler verifies `proof` against IT (never against anything the
/// body claims), admits the id into the trust set on success, and returns the
/// current node roster so the joiner learns the whole mesh in one round trip.
pub type JoinHandler = Arc<
    dyn Fn(String, Vec<u8>, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Transport a [`RawTarget`] speaks. Kebab-case wire strings ("tcp"/"udp") so
/// the handshake JSON matches `fluid_core::ServiceProtocol`'s serde convention
/// (this crate deliberately does not depend on fluid-core; the mapping is done
/// by the resolver in hive-cloud).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawProto {
    Tcp,
    Udp,
}

/// The opening handshake of a [`STREAM_RAW_TARGET`] stream: which local service
/// the opener wants the owner node to splice it into. `deployment` may be empty
/// — the owner then resolves the project's CURRENT serving deployment locally
/// (fresher than anything the edge could pin across a redeploy); when set it
/// pins an exact deployment id.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RawTarget {
    pub project: String,
    pub function: String,
    #[serde(default)]
    pub deployment: String,
    /// The CONTAINER port of the target service (what the app listens on inside
    /// its container) — the stable identity of the port across nodes/redeploys.
    /// Never a host/public port: those are node-local allocations the opener
    /// cannot know.
    pub port: u16,
    pub proto: RawProto,
}

/// Fixed 4-byte preamble the owner writes BEFORE every [`STREAM_RAW_TARGET`]
/// admission status byte (`[magic][status]`, 5 bytes total). Exists so the
/// opener can tell a REAL raw-target response apart from an old,
/// un-upgraded peer's dispatcher misrouting the `0x05` mode byte to its
/// default tunnel-session arm (`fluid_tunnel::TunnelServer::serve`) — that
/// path can unsolicitedly write a Metrics frame whose wire encoding is
/// `[u64 stream_id=0 big-endian][kind][len][payload]`, i.e. its first byte
/// is `0x00`, which used to be read as a bare [`RAW_TARGET_OK`] and get a
/// live client spliced into tunnel-codec garbage instead of a clean
/// failover. Any realistic tunnel `stream_id` (a small counter) also has a
/// `0x00` leading byte in big-endian form, so a magic with a NON-ZERO first
/// byte defeats both the specific Metrics-frame collision and the general
/// class of it. This makes the admission self-certifying: even a stream
/// dialed against a peer with NO version/capability check at all fails
/// closed (bad magic ⇒ the opener bails and its caller fails over) rather
/// than needing the caller to have first verified peer compatibility.
pub const RAW_TARGET_MAGIC: [u8; 4] = [0xF1, 0x5C, 0x9E, 0xA2];
/// Status byte the owner writes on a [`STREAM_RAW_TARGET`] stream before any
/// payload: the target resolved and the local leg is connected — splice begins.
/// Always preceded on the wire by [`RAW_TARGET_MAGIC`].
pub const RAW_TARGET_OK: u8 = 0;
/// No local target for the handshake (unknown project/function/port, protocol
/// not locally forwardable yet, or a malformed handshake). The opener should
/// fail over to its next candidate node.
pub const RAW_TARGET_NOT_FOUND: u8 = 1;
/// The target resolved but the owner could not connect its local leg.
pub const RAW_TARGET_CONNECT_FAILED: u8 = 2;

/// Largest UDP payload one [`STREAM_RAW_TARGET`] datagram frame may carry —
/// the maximum UDP-over-IPv4 payload; anything larger could never have arrived
/// as a single datagram, so a bigger length prefix is a framing error.
pub const RAW_MAX_DATAGRAM: usize = 65_507;

/// What a [`RawTargetResolver`] hands back for an admitted target: the LOCAL
/// address of the leg to splice into, plus an opaque guard held for the
/// lifetime of the splice (e.g. a fluid-compute `Lease`, so instance inflight
/// accounting stays correct and the instance isn't idled out mid-connection).
pub struct RawTargetConn {
    /// `host:port` on the owner node — TCP-connected for `proto: tcp`, the
    /// datagram destination for `proto: udp`.
    pub addr: String,
    /// Dropped when the splice ends.
    pub guard: Option<Box<dyn std::any::Any + Send>>,
}

/// Resolves one [`RawTarget`] to its local leg on THIS node, or `None` when the
/// target isn't served here ([`RAW_TARGET_NOT_FOUND`] goes back to the opener).
/// Provided by hive-cloud, which owns the deployment→container mapping this
/// transport crate deliberately knows nothing about.
pub type RawTargetResolver = Arc<
    dyn Fn(RawTarget) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RawTargetConn>> + Send>>
        + Send
        + Sync,
>;

/// Gossip signature-verification mode (staged rollout so enforcement can't
/// partition a mixed-version fleet): `HIVE_GOSSIP_VERIFY` = `off` | `log` (default:
/// verify + count + warn, but still serve unsigned/invalid) | `enforce` (reject
/// anything but a valid signature from the connected peer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerifyMode {
    Off,
    Log,
    Enforce,
}

/// Whether outbound gossip is signed (`HIVE_GOSSIP_SIGN=1`). Default OFF until the
/// whole fleet runs a binary that understands [`STREAM_GOSSIP_SIGNED`].
pub fn gossip_sign_enabled() -> bool {
    std::env::var("HIVE_GOSSIP_SIGN").map(|v| v == "1" || v == "true").unwrap_or(false)
}

pub fn verify_mode() -> VerifyMode {
    match std::env::var("HIVE_GOSSIP_VERIFY").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "off" => VerifyMode::Off,
        "enforce" | "strict" | "1" => VerifyMode::Enforce,
        _ => VerifyMode::Log,
    }
}

/// Max allowed clock skew / age for a signed gossip message (replay guard),
/// env-tunable via `HIVE_GOSSIP_TS_WINDOW_SECS` (default 300).
fn gossip_ts_window_ms() -> u64 {
    std::env::var("HIVE_GOSSIP_TS_WINDOW_SECS").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(300) * 1000
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

/// The domain-separated byte string an ed25519 gossip signature covers. Pure, so
/// signer and verifier can't drift.
fn gossip_sig_preimage(method: u8, path: &str, body: &[u8], ts_ms: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(GOSSIP_SIG_DOMAIN.len() + 1 + 8 + 8 + path.len() + body.len() + 8);
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
pub fn sign_gossip(secret: &iroh::SecretKey, method: u8, path: &str, body: &[u8], ts_ms: u64) -> [u8; 104] {
    let sig = secret.sign(&gossip_sig_preimage(method, path, body, ts_ms));
    let mut out = [0u8; 104];
    out[..32].copy_from_slice(secret.public().as_bytes());
    out[32..40].copy_from_slice(&ts_ms.to_be_bytes());
    out[40..104].copy_from_slice(&sig.to_bytes());
    out
}

/// Verify a signed-gossip trailer. `remote_id` is the QUIC connection's
/// authenticated remote identity — the signer must BE that peer. Returns the
/// verified signer id string, or a &'static str reason (also counted).
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
    if signer.verify(&gossip_sig_preimage(method, path, body, ts), &sig).is_err() {
        VERIFY_STATS.bad_sig.fetch_add(1, Ordering::Relaxed);
        return Err("signature invalid");
    }
    // Replay guard: reject messages outside the freshness window (either direction —
    // covers clock skew AND recorded-replay).
    let window = gossip_ts_window_ms();
    if now_ms.abs_diff(ts) > window {
        VERIFY_STATS.stale_ts.fetch_add(1, Ordering::Relaxed);
        return Err("timestamp outside freshness window");
    }
    // Transport binding: the signer must be the connection's authenticated peer, so
    // a valid signed message lifted from one channel can't be injected via another.
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

/// Read one `[u32 len][bytes]` frame with an explicit size cap — the shared
/// primitive behind gossip/join frames (16 MiB cap) and raw-target datagram
/// frames ([`RAW_MAX_DATAGRAM`] cap).
async fn read_frame_max<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> std::io::Result<Vec<u8>> {
    let n = read_u32(r).await?;
    if n > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).await?;
    Ok(v)
}

/// Read one datagram frame off a raw-target UDP mesh stream: `Ok(Some(bytes))`
/// per datagram, `Ok(None)` on a clean end-of-stream. Exported so the edge-side
/// UDP relay speaks byte-identical framing to the owner-side pump in this crate.
pub async fn read_raw_datagram<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    match read_frame_max(r, RAW_MAX_DATAGRAM).await {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write one datagram frame (`[u32 len][bytes]`, boundary-preserving) onto a
/// raw-target UDP mesh stream. Oversized payloads are a framing error — they
/// could never have arrived as one datagram.
pub async fn write_raw_datagram<W: AsyncWrite + Unpin>(w: &mut W, datagram: &[u8]) -> std::io::Result<()> {
    if datagram.len() > RAW_MAX_DATAGRAM {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "datagram too large"));
    }
    w.write_all(&(datagram.len() as u32).to_be_bytes()).await?;
    w.write_all(datagram).await?;
    w.flush().await
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
    /// Per-peer, per-phase iroh timeout counters (#H4) — `p2p_timeout{phase,node_id}`.
    pub timeouts: Vec<PeerTimeout>,
}

/// One peer/phase timeout counter, surfaced via [`PeerPool::relay_stats`].
#[derive(Clone, Debug)]
pub struct PeerTimeout {
    pub node_id: String,
    pub phase: &'static str,
    pub count: u64,
}

// ---- H4: bounded iroh data plane ------------------------------------------------
//
// Every iroh phase gets a timeout budget so a holepunched-but-silent or
// accept-but-no-answer peer can't hang a request forever (and, because edge.rs
// walks candidates sequentially, block the whole queue). Budgets respect the
// pre-send vs post-send retry rule already encoded in `request_stream`.

/// Read a millisecond budget from env, falling back to `default_ms`. A value of 0
/// (or unparseable) falls back to the default — we never allow an unbounded await.
fn env_ms(key: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(key).ok().and_then(|s| s.parse::<u64>().ok()).filter(|&v| v > 0).unwrap_or(default_ms);
    Duration::from_millis(ms)
}
/// Connect (holepunch + handshake) budget — pre-send. Holepunch can be slow.
fn connect_budget() -> Duration { env_ms("HIVE_P2P_CONNECT_MS", 5_000) }
/// `open_bi` budget — pre-send. Cheap once the conn is live; a hang = half-dead trunk.
fn open_budget() -> Duration { env_ms("HIVE_P2P_OPEN_MS", 2_000) }
/// Discovery-fallback connect budget — used ONLY when a dial against the peer's
/// cached hint (its direct addrs / relay_url, as last gossiped — see `acquire`)
/// fails or times out. Deliberately a SEPARATE, smaller budget than
/// `connect_budget`: this second attempt strips the hint down to a bare
/// `EndpointId` so iroh's configured Discovery/AddressLookup (n0 pkarr/DNS, or
/// `HIVE_DISCOVERY_DNS`) gets a genuine shot at resolving a FRESH address. Per
/// iroh's own `connect()` semantics, a `RelayUrl` present in the hint — even a
/// stale/unreachable one — marks the address set "resolved" and Discovery is
/// NEVER consulted automatically, so without this fallback a stale cached hint
/// (typical for a peer only ever learned second/third-hand via gossip, never
/// dialed directly) can wedge every future dial to that peer forever once its
/// cross-cloud QUIC path flaps.
fn discovery_budget() -> Duration { env_ms("HIVE_P2P_DISCOVERY_MS", 4_000) }
/// Worst-case time `acquire()` needs to run the cached-hint attempt AND the
/// fresh-discovery fallback to completion: `connect_budget + discovery_budget`,
/// plus slack. A caller wrapping `join_request`/`gossip_request` in its OWN
/// `tokio::time::timeout` shorter than this drops the future mid-flight while
/// `dial_fresh` is still pending — the fallback `discovery_budget` exists to
/// recover a first-contact node's stale/private-IP cached hint (see
/// `discovery_budget`'s doc comment) never gets to run, so a first-contact dial
/// can never succeed no matter how long fresh discovery would have taken. Any
/// such outer timeout MUST be at least this long.
pub fn dial_fallback_ceiling() -> Duration { connect_budget() + discovery_budget() + Duration::from_secs(1) }
/// First-byte / response-headers budget — post-send. Generous: the cell may be cold.
fn firstbyte_budget() -> Duration { env_ms("HIVE_P2P_FIRSTBYTE_MS", 15_000) }
/// Body inter-chunk IDLE budget — post-send. Not a wall-clock cap; reset per chunk
/// so SSE / long streams survive, only killed on inactivity.
fn idle_budget() -> Duration { env_ms("HIVE_P2P_IDLE_MS", 45_000) }

const PHASE_CONNECT: usize = 0;
const PHASE_OPEN: usize = 1;
const PHASE_FIRSTBYTE: usize = 2;
const PHASE_IDLE: usize = 3;
const PHASE_NAMES: [&str; 4] = ["connect", "open", "firstbyte", "idle"];

/// Marker error: a **pre-send** iroh phase (connect or `open_bi`) timed out — the
/// strongest possible "this peer is dead" signal. The caller (edge gateway) should
/// mark the peer unhealthy so candidate ranking stops choosing it; downcast the
/// returned `anyhow::Error` to detect it.
#[derive(Clone, Debug)]
pub struct DeadPeerTimeout {
    pub node_id: String,
    pub phase: &'static str,
    pub budget_ms: u64,
}
impl std::fmt::Display for DeadPeerTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p2p {} timeout to {} after {}ms (peer presumed dead)", self.phase, self.node_id, self.budget_ms)
    }
}
impl std::error::Error for DeadPeerTimeout {}

/// Marker error: a **post-send** iroh phase (first byte or body idle) timed out.
/// NOT a dead-peer signal — the request was already on the wire, so the caller must
/// fail over (never silently retry a possibly-side-effecting call).
#[derive(Clone, Debug)]
pub struct PostSendTimeout {
    pub node_id: String,
    pub phase: &'static str,
    pub budget_ms: u64,
}
impl std::fmt::Display for PostSendTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p2p {} timeout to {} after {}ms (post-send; no retry)", self.phase, self.node_id, self.budget_ms)
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
        self.map.lock().await.entry(node_id.to_string()).or_insert([0; 4])[phase] += 1;
    }
    async fn snapshot(&self) -> Vec<PeerTimeout> {
        let m = self.map.lock().await;
        let mut out = Vec::new();
        for (node, counts) in m.iter() {
            for (i, &c) in counts.iter().enumerate() {
                if c > 0 {
                    out.push(PeerTimeout { node_id: node.clone(), phase: PHASE_NAMES[i], count: c });
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
    /// Next body chunk, or `None` at end-of-body — OR when the inter-chunk idle
    /// budget elapses (the peer went silent mid-stream). Each chunk resets the
    /// budget, so an active stream is never killed.
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
    timeouts: Arc<TimeoutCounters>,
}

impl PeerPool {
    /// Build a pool over a bound endpoint (cheap to clone — `Arc` inside).
    pub fn new(ep: Endpoint) -> Arc<PeerPool> {
        Arc::new(PeerPool {
            ep,
            trunks: Mutex::new(HashMap::new()),
            opened: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            timeouts: Arc::new(TimeoutCounters::default()),
        })
    }

    /// `(opened, reused)` connection counters — for diagnostics and tests.
    pub fn stats(&self) -> (u64, u64) {
        (self.opened.load(Ordering::Relaxed), self.reused.load(Ordering::Relaxed))
    }

    /// Proactively ensure a live trunk to `node_id` exists — dial (holepunch) if
    /// missing/dead, reuse if already warm — WITHOUT opening a request stream. Lets
    /// a background maintainer keep the whole mesh pre-trunked so a request-time
    /// dial/holepunch is the rare exception (peer just restarted), not the norm.
    /// Connect is bounded by the H4 budget, so a dead peer can't wedge the warmer.
    /// Returns whether a live trunk is now cached.
    pub async fn warm(&self, node_id: &str, addr_json: &str) -> bool {
        self.acquire(node_id, addr_json).await.is_ok()
    }

    /// Number of currently-cached (live-or-not) trunks — for diagnostics / tests.
    pub async fn trunk_count(&self) -> usize {
        self.trunks.lock().await.len()
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
        drop(map);
        s.timeouts = self.timeouts.snapshot().await;
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

        // Slow path: dial OUTSIDE the lock, BOUNDED by the connect budget (#H4).
        // A holepunch that never completes would otherwise hang here forever and —
        // since edge.rs walks candidates sequentially — block the whole queue.
        let addr: EndpointAddr = serde_json::from_str(addr_json)?;
        let id = addr.id;
        let budget = connect_budget();
        let conn = match tokio::time::timeout(budget, self.ep.connect(addr, HIVE_ALPN)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                // Hard connect error against the CACHED hint. This hint is very often a
                // stale, never-refreshed snapshot learned second/third-hand via gossip
                // (see `addr_json` — built once at the peer's own boot, no refresh setter
                // exists) rather than something this node ever dialed directly. Don't
                // give up on the peer yet: fall back to fresh discovery below. Log it —
                // previously this branch discarded the error with zero tracing, which is
                // exactly how the hk↔sj mesh-flap went unnoticed in production.
                self.evict(node_id).await;
                tracing::warn!(node_id, err = %e, "p2p connect error using cached hint; retrying via fresh discovery");
                self.dial_fresh(node_id, id).await?
            }
            Err(_) => {
                // Pre-send timeout against the cached hint: nothing was sent. Rather than
                // declaring the peer dead outright (the old behavior), give discovery a
                // real shot — the hint's direct addrs/relay may simply be stale, not the
                // peer itself.
                self.timeouts.bump(node_id, PHASE_CONNECT).await;
                self.evict(node_id).await; // stale trunk (if any) is definitely dead
                tracing::warn!(node_id, budget_ms = budget.as_millis() as u64, "p2p connect timeout using cached hint; retrying via fresh discovery");
                self.dial_fresh(node_id, id).await?
            }
        };

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

    /// Fallback dial using ONLY the peer's `EndpointId` — no cached direct addrs,
    /// no cached relay_url. This forces iroh's configured Discovery/AddressLookup
    /// (n0 pkarr/DNS, or the self-hosted `HIVE_DISCOVERY_DNS` resolver) to resolve
    /// the peer's CURRENT address rather than reusing whatever stale hint `acquire`
    /// already tried and failed on. Called from `acquire` only after the
    /// cached-hint attempt has already failed/timed out — see `discovery_budget`
    /// for why iroh would otherwise never consult Discovery on its own here.
    async fn dial_fresh(&self, node_id: &str, id: iroh::EndpointId) -> Result<Connection> {
        let budget = discovery_budget();
        match tokio::time::timeout(budget, self.ep.connect(EndpointAddr::new(id), HIVE_ALPN)).await {
            Ok(Ok(c)) => {
                tracing::info!(node_id, "p2p connect recovered via fresh discovery (cached hint was stale)");
                Ok(c)
            }
            Ok(Err(e)) => {
                self.evict(node_id).await;
                tracing::warn!(node_id, err = %e, "p2p connect error via fresh discovery (giving up)");
                Err(e.into())
            }
            Err(_) => {
                self.timeouts.bump(node_id, PHASE_CONNECT).await;
                self.evict(node_id).await;
                tracing::warn!(node_id, budget_ms = budget.as_millis() as u64, "p2p discovery-fallback connect timeout (giving up)");
                Err(anyhow::Error::new(DeadPeerTimeout {
                    node_id: node_id.to_string(),
                    phase: "connect",
                    budget_ms: budget.as_millis() as u64,
                }))
            }
        }
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
        // A mid-stream idle timeout (post-send) becomes an explicit error rather
        // than a silently-truncated body.
        if s.timed_out() {
            return Err(anyhow::Error::new(PostSendTimeout {
                node_id: node_id.to_string(),
                phase: "idle",
                budget_ms: idle_budget().as_millis() as u64,
            }));
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
            // ACQUIRE (connect is bounded inside `acquire`). A connect timeout /
            // failure is PRE-SEND → evict + retry once (safe even for POST).
            let conn = match self.acquire(node_id, addr_json).await {
                Ok(c) => c,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            // OPEN_BI — PRE-SEND, bounded by the open budget. An error OR a timeout
            // means no request bytes left this node → evict + retry once.
            let (mut send, recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_OPEN).await;
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(self.dead_peer(node_id, "open", open_budget()));
                }
            };
            // Select the multiplexed tunnel mode for the owner's dispatcher.
            send.write_all(&[STREAM_TUNNEL]).await?;
            send.flush().await?;
            // Past this point the request may be on the wire → do NOT retry in-call.
            let client = fluid_tunnel::TunnelClient::new(tokio::io::join(recv, send));
            // FIRST BYTE / response headers — POST-SEND, bounded by the firstbyte
            // budget (generous: the far cell may be cold-starting). On timeout we
            // return Err so edge.rs candidate failover decides — never retry here.
            // `to_vec` per attempt so a retry still owns the headers.
            let resp = match tokio::time::timeout(
                firstbyte_budget(),
                client.request(method, path, headers.to_vec(), body),
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
            // Move the client INTO the stream so it lives until the body is drained
            // (keeping the QUIC streams open); see `TunnelStream::_client`.
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

    /// Open a RAW bidirectional byte stream to a peer over its trunk, for upgraded
    /// connections (WebSocket) where HTTP request/response framing must be
    /// bypassed. The owner splices these bytes straight to its local target. Same
    /// pre-send retry-once semantics as [`request`].
    pub async fn open_raw(&self, node_id: &str, addr_json: &str) -> Result<P2pStream> {
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let conn = match self.acquire(node_id, addr_json).await {
                Ok(c) => c,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_OPEN).await;
                    self.evict(node_id).await;
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

    /// Open a RAW byte stream to a NAMED service on a peer — the mesh-forward
    /// primitive for generic (non-HTTP) TCP/UDP proxying. Unlike [`open_raw`],
    /// whose owner side splices into the owner's local HTTP gateway (so the
    /// caller must speak HTTP at it — the WebSocket upgrade-replay case), this
    /// sends a [`RawTarget`] handshake naming the deployment/function/container
    /// port to splice into, and waits for the owner's 1-byte admission status
    /// BEFORE returning — so a caller that hasn't yet consumed any client bytes
    /// can fail over to the next candidate node on any error (the same
    /// failover-safety `ws_proxy` gets from `open_raw` running pre-upgrade).
    ///
    /// On success the returned stream is the spliced connection:
    /// * `proto: tcp` — opaque bytes both ways (`copy_bidirectional` it).
    /// * `proto: udp` — one `[u32 len][bytes]` frame per datagram both ways
    ///   (use [`read_raw_datagram`]/[`write_raw_datagram`]).
    ///
    /// Pre-send phases (connect / `open_bi`) retry once, exactly as
    /// [`open_raw`]; once the handshake is on the wire a timeout/refusal is
    /// returned (bounded by the firstbyte budget) for the caller's candidate
    /// failover to decide.
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
            let conn = match self.acquire(node_id, addr_json).await {
                Ok(c) => c,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, mut recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_OPEN).await;
                    self.evict(node_id).await;
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
            // POST-SEND: await the owner's admission response, bounded by the
            // firstbyte budget (the owner may cold-start the target instance).
            // `[4B magic][1B status]`, not a bare status byte — see
            // `RAW_TARGET_MAGIC`'s doc for why: an un-upgraded peer's
            // dispatcher misrouting this stream to its default tunnel-session
            // handler can unsolicitedly write a frame whose first byte(s)
            // coincidentally equal a bare `RAW_TARGET_OK`; the magic makes a
            // real admission unmistakable so a misroute fails closed here
            // (bad/absent magic ⇒ Err, caller fails over) instead of splicing
            // the client into codec garbage.
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
                RAW_TARGET_OK => return Ok(tokio::io::join(recv, send)),
                RAW_TARGET_NOT_FOUND => anyhow::bail!(
                    "peer {node_id} has no local target for {}/{} port {} ({:?})",
                    target.project, target.function, target.port, target.proto
                ),
                RAW_TARGET_CONNECT_FAILED => anyhow::bail!(
                    "peer {node_id} could not connect its local leg for {}/{} port {}",
                    target.project, target.function, target.port
                ),
                other => anyhow::bail!("peer {node_id} sent unknown raw-target status {other}"),
            }
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
            let conn = match self.acquire(node_id, addr_json).await {
                Ok(c) => c,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, mut recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_OPEN).await;
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(self.dead_peer(node_id, "open", open_budget()));
                }
            };
            // SIGN outbound gossip (web3 trustlessness): receivers verify the
            // MESSAGE cryptographically, not just the QUIC transport. Env-gated
            // (`HIVE_GOSSIP_SIGN=1`) because an OLD receiver would misparse the new
            // stream mode as a tunnel — staged rollout is: (1) ship binary fleet-wide
            // (receivers understand both modes), (2) flip signing on everywhere,
            // (3) flip `HIVE_GOSSIP_VERIFY=enforce`. Each phase is mixed-fleet safe.
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
            // Response: [u32 len][bytes]. POST-SEND read bounded by the firstbyte
            // budget so a peer that accepts the stream but never answers can't hang.
            let resp = match tokio::time::timeout(firstbyte_budget(), read_frame(&mut recv)).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_FIRSTBYTE).await;
                    return Err(anyhow::Error::new(PostSendTimeout {
                        node_id: node_id.to_string(),
                        phase: "firstbyte",
                        budget_ms: firstbyte_budget().as_millis() as u64,
                    }));
                }
            };
            return Ok(resp);
        }
    }

    /// MESH JOIN (hot-join): introduce this node to `node_id` (a seed, dialed by
    /// KEY — iroh resolves the address via discovery/relay, no IP required).
    /// `node_json` is our own NodeInfo; `proof` is HMAC(fleet secret, OUR endpoint
    /// id). Returns the seed's node roster on admission (empty body = rejected).
    /// Same trunk reuse + redial-once semantics as [`gossip_request`].
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
            let conn = match self.acquire(node_id, addr_json).await {
                Ok(c) => c,
                Err(e) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let (mut send, mut recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_OPEN).await;
                    self.evict(node_id).await;
                    if attempt < 2 {
                        continue;
                    }
                    return Err(self.dead_peer(node_id, "open", open_budget()));
                }
            };
            send.write_all(&[STREAM_JOIN]).await?;
            send.write_all(&(node_json.len() as u32).to_be_bytes()).await?;
            send.write_all(node_json).await?;
            send.write_all(&(proof.len() as u32).to_be_bytes()).await?;
            send.write_all(proof.as_bytes()).await?;
            send.flush().await?;
            let _ = send.finish();
            let resp = match tokio::time::timeout(firstbyte_budget(), read_frame(&mut recv)).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_FIRSTBYTE).await;
                    return Err(anyhow::Error::new(PostSendTimeout {
                        node_id: node_id.to_string(),
                        phase: "firstbyte",
                        budget_ms: firstbyte_budget().as_millis() as u64,
                    }));
                }
            };
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

/// Return `addr_json` (a serialized `EndpointAddr`) with its relay transport
/// STEERED toward `relay_url` when given — used to hint a specific, freshly
/// gossiped relay (e.g. the target peer's own relay, or the nearest live peer's)
/// instead of dialing on whatever relay happened to be serialized into the
/// cached hint at gossip time (which may be stale or simply absent). Direct
/// socket addrs already present in `addr_json` are left untouched — only the
/// relay transport entry is replaced. `relay_url: None` returns `addr_json`
/// unchanged. Fails open: an unparseable `addr_json`/`relay_url` returns the
/// original string unchanged rather than erroring — a bad hint must never break
/// an otherwise-workable dial. Pure (no I/O), so it's directly unit-testable.
///
/// NOTE: this only STEERS which relay a per-connection dial prefers (via the
/// `EndpointAddr` passed to `connect()`); the relay itself must ALSO be a member
/// of the local endpoint's own live `RelayMap` (see [`RelaySet`]) for the hint to
/// have a real transport to route through — see this crate's module-level
/// research notes on `insert_relay`/`remove_relay` for why the two compose
/// rather than substitute for each other.
pub fn with_relay_hint(addr_json: &str, relay_url: Option<&str>) -> String {
    let Some(relay_url) = relay_url else { return addr_json.to_string() };
    let Ok(mut addr) = serde_json::from_str::<EndpointAddr>(addr_json) else {
        return addr_json.to_string();
    };
    let Ok(url) = relay_url.parse::<iroh::RelayUrl>() else {
        return addr_json.to_string();
    };
    addr.addrs.retain(|a| !matches!(a, iroh::TransportAddr::Relay(_)));
    addr.addrs.insert(iroh::TransportAddr::Relay(url));
    serde_json::to_string(&addr).unwrap_or_else(|_| addr_json.to_string())
}

/// Tracks the relay URL set THIS PROCESS has applied to a bound [`Endpoint`] via
/// [`Endpoint::insert_relay`]/[`Endpoint::remove_relay`], so a periodic refresh can
/// diff a newly-desired set against what's actually live and apply only the
/// delta. iroh's `Endpoint` exposes no live "read back the current `RelayMap`"
/// getter, so the applied set must be tracked by the caller — this is that
/// tracker. Scoped to ONLY the URLs this tracker itself has inserted: it never
/// touches whatever relay(s) the endpoint was bound with (env override / preset
/// default), so [`sync`](RelaySet::sync) composes safely on top of bind-time
/// config instead of fighting it.
///
/// Backed by [`Endpoint::insert_relay`]/`remove_relay` — a genuine LIVE update on
/// an already-bound, already-running endpoint (no rebind required); iroh's own
/// `test_endpoint_online_add_relay` proves a relay inserted this way is usable
/// within ~1s.
pub struct RelaySet {
    ep: Endpoint,
    applied: Mutex<std::collections::HashSet<iroh::RelayUrl>>,
}

impl RelaySet {
    /// Wrap a bound endpoint. The tracked "applied" set starts empty — this
    /// tracker only ever manages URLs it itself inserts via [`sync`](Self::sync),
    /// never the endpoint's bind-time relay config.
    pub fn new(ep: Endpoint) -> Arc<RelaySet> {
        Arc::new(RelaySet { ep, applied: Mutex::new(std::collections::HashSet::new()) })
    }

    /// The URLs currently applied by THIS tracker (for diagnostics/tests).
    pub async fn applied(&self) -> Vec<String> {
        self.applied.lock().await.iter().map(|u| u.to_string()).collect()
    }

    /// Reconcile the endpoint's live relay map to exactly `desired` (parsed from
    /// strings; unparseable entries are skipped, never panic on a bad gossiped
    /// URL): `insert_relay` every newly-desired URL, `remove_relay` every URL
    /// this tracker previously applied but no longer desires. URLs already
    /// applied AND still desired are left untouched (no redundant
    /// insert/network chatter). No-op (and doesn't touch the map at all) once
    /// the endpoint is closed — `insert_relay`/`remove_relay` themselves are
    /// no-ops on a closed endpoint, so this simply mirrors that.
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
    // Bound connect + open_bi (#H4) so a standalone dial can't hang unbounded.
    let conn = match tokio::time::timeout(connect_budget(), ep.connect(addr, HIVE_ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(anyhow::anyhow!("dial connect timeout after {}ms", connect_budget().as_millis())),
    };
    let (mut send, recv) = match tokio::time::timeout(open_budget(), conn.open_bi()).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(anyhow::anyhow!("dial open_bi timeout after {}ms", open_budget().as_millis())),
    };
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
    serve_tunnels_with_join(ep, local_http, max_concurrency, trust, gossip, None).await
}

/// [`serve_tunnels`] plus a MESH-JOIN surface (hot-join). Trust semantics:
/// * No trust set configured — identical to before, every mode permissionless.
/// * Trust set configured, TRUSTED peer — identical to before, every mode served.
/// * Trust set configured, UNTRUSTED peer — previously the whole connection was
///   dropped; now the connection stays open but EVERY stream except
///   [`STREAM_JOIN`] is dropped per-stream (fail-closed: tunnels/raw/gossip all
///   refused). A successful join inserts the peer into the shared trust set, so
///   the SAME connection's subsequent streams are served (trust is re-read per
///   stream) — the joiner never has to redial.
/// * No `join` handler — untrusted connections are dropped exactly as before.
pub async fn serve_tunnels_with_join(
    ep: Endpoint,
    local_http: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
    join: Option<JoinHandler>,
) {
    serve_tunnels_full(ep, local_http, max_concurrency, trust, gossip, join, None).await
}

/// [`serve_tunnels_with_join`] plus the generic raw-target surface: when a
/// [`RawTargetResolver`] is provided, [`STREAM_RAW_TARGET`] streams are served
/// (parse the [`RawTarget`] handshake, resolve it to a local leg, splice — see
/// [`serve_raw_target`]); without one they are answered [`RAW_TARGET_NOT_FOUND`]
/// so an opener fails over instead of hanging. Trust semantics are identical to
/// every other non-JOIN mode: an untrusted peer's raw-target streams are dropped.
pub async fn serve_tunnels_full(
    ep: Endpoint,
    local_http: String,
    max_concurrency: u32,
    trust: Option<TrustSet>,
    gossip: Option<GossipHandler>,
    join: Option<JoinHandler>,
    raw_resolver: Option<RawTargetResolver>,
) {
    while let Some(incoming) = ep.accept().await {
        let local = local_http.clone();
        let trust = trust.clone();
        let gossip = gossip.clone();
        let join = join.clone();
        let raw_resolver = raw_resolver.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            // The connection's cryptographic remote identity (ed25519, from the QUIC
            // TLS handshake — unspoofable). Used for the optional #20 allowlist AND
            // to bind signed-gossip messages to their sender (web3 verification).
            let remote_id = conn.remote_id().to_string();
            // #20 peer trust/attestation: when an allowlist is configured, only
            // admit endpoints whose cryptographic iroh identity is trusted. With no
            // join surface, dropping `conn` closes the QUIC connection (original
            // behavior); with one, the connection survives so the peer can present
            // its join proof — but every non-JOIN stream is refused until it does.
            if let Some(trust) = &trust {
                if !peer_trusted(trust, &remote_id) && join.is_none() {
                    tracing::warn!(peer = %remote_id, "rejected untrusted P2P peer (#20 peer trust)");
                    return;
                }
            }
            // One inbound connection → many bi streams, each dispatched by mode.
            // This already handles a pooled peer that multiplexes many requests
            // over one connection (each request is a new stream here).
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let local = local.clone();
                let gossip = gossip.clone();
                let join = join.clone();
                let raw_resolver = raw_resolver.clone();
                let rid = remote_id.clone();
                let trust = trust.clone();
                tokio::spawn(async move {
                    // Read the 1-byte mode selector that prefixes every stream.
                    let mut mode = [0u8; 1];
                    if recv.read_exact(&mut mode).await.is_err() {
                        return;
                    }
                    // Trust is re-read PER STREAM (not cached from accept time) so a
                    // connection that joins mid-life is served immediately after.
                    let trusted = trust.as_ref().map(|t| peer_trusted(t, &rid)).unwrap_or(true);
                    if mode[0] == STREAM_JOIN {
                        if let Some(j) = join {
                            serve_join(recv, send, j, rid).await;
                        }
                        return;
                    }
                    if !trusted {
                        tracing::warn!(peer = %rid, mode = mode[0], "dropped stream from untrusted peer (join required first)");
                        return;
                    }
                    match mode[0] {
                        STREAM_GOSSIP | STREAM_GOSSIP_SIGNED => {
                            if let Some(h) = gossip {
                                serve_gossip(recv, send, h, mode[0] == STREAM_GOSSIP_SIGNED, rid, trust).await;
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
        });
    }
}

/// Server side of a [`STREAM_JOIN`] stream: read `(node_json, proof)`, hand them to
/// the join handler with the connection's authenticated remote identity, frame the
/// handler's response back (empty = rejected). The mode byte is already consumed.
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
}

/// Test/diagnostic helper (#H4): accept P2P connections + bi streams but NEVER
/// write a response — the "accept-but-silent" peer. Holds both stream halves open
/// without answering, so a caller's first-byte timeout must fire. Not used in
/// production; exposed so the timeout tests can stand up a real silent owner.
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

/// Server side of a [`STREAM_GOSSIP`]/[`STREAM_GOSSIP_SIGNED`] stream: read the
/// framed `(method, path, body)` request (+ signature trailer when signed), VERIFY
/// it per the configured [`VerifyMode`], run the caller-provided handler, and frame
/// the response back. The mode byte has already been consumed. `remote_id` is the
/// QUIC connection's authenticated peer identity.
async fn serve_gossip<R, W>(
    mut recv: R,
    mut send: W,
    handler: GossipHandler,
    signed: bool,
    remote_id: String,
    trust: Option<TrustSet>,
) where
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
    let mode = verify_mode();
    // Verify: a signed request must check out; an unsigned one is only admitted
    // below enforce. `verified_signer` flows to the handler for signer-based authz.
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
                // Signature validity proves the sender POSSESSES the private
                // key for `signer` — it does NOT prove `signer` is a real
                // fleet member (anyone can generate a fresh ed25519 keypair
                // for free and self-sign with it). When a trust set is
                // actually configured, additionally require `signer` to be a
                // member of it before treating the message as authoritative;
                // an unlisted key is handled exactly like an invalid
                // signature (rejected in Enforce, logged in Log mode). No
                // trust set configured => unchanged behavior (today's
                // default, tracked as a separate infra-level decision).
                let trusted = trust.as_ref().map(|t| peer_trusted(t, &signer)).unwrap_or(true);
                if trusted {
                    verified_signer = Some(signer);
                } else if mode == VerifyMode::Enforce {
                    VERIFY_STATS.rejected.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(peer = %remote_id, %path, %signer, "REJECTED gossip (signature valid but signer is not a trusted fleet member)");
                    let _ = send.write_all(&0u32.to_be_bytes()).await;
                    let _ = send.flush().await;
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
                    // Explicit empty response: the peer sees a clean failure, not a hang.
                    let _ = send.write_all(&0u32.to_be_bytes()).await;
                    let _ = send.flush().await;
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
            return;
        }
    }
    let resp = handler(m[0], path, body, verified_signer).await;
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

/// Server side of a [`STREAM_RAW_TARGET`] stream (the owner-node accept handler
/// for generic raw TCP/UDP mesh forwarding): read the [`RawTarget`] handshake,
/// resolve it to a local leg via the caller-provided [`RawTargetResolver`]
/// (hive-cloud owns the deployment→container-port mapping), answer the 1-byte
/// admission status, then move payload:
/// * `tcp` — `copy_bidirectional` between the mesh stream and a fresh TCP
///   connection to the resolved local address.
/// * `udp` — pump length-prefixed datagram frames ↔ a connected local UDP
///   socket, preserving datagram boundaries. Session lifetime is owned by the
///   OPENER (the edge relay closes the mesh stream to end it) — mirroring how a
///   raw TCP splice lives until either side closes.
///
/// Every refusal (bad handshake, no resolver, unresolvable target, connect
/// failure) is an explicit status byte, never a silent close, so the opener can
/// fail over to another candidate node without a timeout.
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
    // Handshake frame: a target descriptor is tiny — cap well under the gossip
    // frame limit so a hostile length prefix can't balloon the read. A
    // malformed/unreadable handshake still gets an explicit refusal (not a
    // bare close) so the opener fails fast instead of burning its firstbyte
    // budget waiting on a peer that already gave up.
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
    // Held for the whole splice — e.g. the fluid-compute lease keeping the
    // instance's inflight accounting correct for this live connection.
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
            // A connected loopback socket: send() targets the container's
            // published local UDP port; recv() only accepts its replies.
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
            // Inbound (mesh → container) in its own task: `read_raw_datagram`
            // is not cancellation-safe mid-frame, so it must never sit in a
            // select arm. Ends on stream EOF/error — the opener closing the
            // mesh stream IS the end-of-session signal.
            let inbound_sock = sock.clone();
            let mut inbound = tokio::spawn(async move {
                while let Ok(Some(d)) = read_raw_datagram(&mut recv).await {
                    if inbound_sock.send(&d).await.is_err() {
                        break;
                    }
                }
            });
            // Outbound (container → mesh) here; `UdpSocket::recv` is
            // cancel-safe, so selecting it against the inbound task's exit is
            // sound. Either side ending tears the whole session down.
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
