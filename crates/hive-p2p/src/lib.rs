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
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::{
    endpoint::presets::N0, endpoint::Connection, endpoint::QuicTransportConfig, EndpointAddr,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

// Re-export the endpoint type so callers (hive-cloud) don't depend on iroh directly.
pub use iroh::Endpoint;

/// Public mainline-DHT address lookup (`bind_full` registers it; `--dht-probe`
/// and `GET /v1/mesh/discovery` read it). See the module docs for what becomes
/// publicly resolvable and every env flag that gates it.
pub mod dht;

/// Connection-level QUIC idle timeout for trunked connections.
///
/// This deliberately does NOT set a keep-alive interval. `QuicTransportConfig`'s
/// builder already installs iroh's own tuned values — a 5s `keep_alive_interval`
/// AND a 5s `default_path_keep_alive_interval` against a 15s path idle timeout —
/// which are what actually hold a multipath connection's paths open. The previous
/// code here set `keep_alive_interval(15s)`, described as "keeps an idle-but-warm
/// connection from being reaped … below iroh's idle timeout". Both halves of that
/// were wrong: it did not ADD a keep-alive (one already existed), it TRIPLED
/// iroh's interval to exactly the path idle timeout, and "iroh's idle timeout"
/// conflated the 30s connection timeout with the 15s PATH timeout. It stayed
/// harmless only because the separate per-path keep-alive was left untouched and
/// was silently doing the real work. Overriding a transport parameter the
/// upstream tuned for its own hole-punching needs a measured reason; there was
/// none, so the override is gone and iroh's defaults stand.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent inbound bi-streams a single peer connection may hold open.
///
/// Stated EXPLICITLY rather than inherited. The accept loop spawns one task per
/// accepted stream, so this number is the real per-connection task ceiling — and
/// it was previously whatever quinn happened to default to (100), a value this
/// code never chose and no comment ever acknowledged. iroh's QUIC guide is
/// direct about not accepting unbounded concurrent streams without resource
/// limits; inheriting a library default silently is how you end up unable to say
/// what your own limit is.
///
/// 256 to AGREE WITH `max_concurrency` (the per-session request bound passed
/// into `serve_tunnels_full`, 256 in `main.rs`). One request rides one stream
/// here, so the two are bounds on the same resource; leaving them at different
/// values (100 vs 256) meant the request bound could never actually be reached
/// and the effective limit was the accidental one. Enforced by QUIC flow control,
/// so a peer at the ceiling is BACK-PRESSURED — it waits for a slot rather than
/// having streams silently dropped.
fn max_streams() -> u32 {
    std::env::var("HIVE_P2P_MAX_STREAMS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(256)
}

/// Concurrently-served inbound CONNECTIONS per node.
///
/// The genuinely unbounded resource before this: `serve_tunnels_full` spawned a
/// task per accepted connection with no ceiling at all, while streams at least
/// had quinn's implicit per-connection cap. A 14-node fleet needs a handful of
/// connections; 512 is far above any legitimate steady state, so this is a
/// blast-radius backstop against connection floods, not a working limit anyone
/// should reach. `0` disables the cap entirely for an operator who needs the old
/// unbounded behaviour back.
fn max_inbound_conns() -> usize {
    std::env::var("HIVE_P2P_MAX_CONNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(512)
}

/// ALPN identifying the Hive function-tunnel protocol over iroh.
pub const HIVE_ALPN: &[u8] = b"hive/tunnel/0";

/// ALPN for browser-tab peers (`crates/hive-browser`) — a dedicated, low-trust
/// surface, structurally disjoint from [`HIVE_ALPN`]'s gossip/join/raw modes.
/// Connections on this ALPN never reach the mode-byte dispatch below — see
/// `serve_browser_conn`, which has no gossip/join/raw arms at all.
///
/// Re-exported from `hive-browser-proto` rather than declared here: the browser
/// half of this protocol is a separate crate on a separate target, and the two
/// used to hold identical constants in sync by comment alone. They no longer
/// can drift. Kept `pub` at this path so existing `hive_p2p::BROWSER_ALPN`
/// callers are unaffected.
pub use hive_browser_proto::BROWSER_ALPN;

use hive_browser_proto::{
    check_len_for, encode_invoke, encode_request, reset as browser_reset, Op,
    BROWSER_MAX_CRR_FRAME, BROWSER_MAX_ECHO, BROWSER_MAX_FRAME, FUNCTION_DIGEST_LEN,
};

/// Concurrently-served `hive/browser/0` connections — SEPARATE from
/// [`max_inbound_conns`]'s fleet-trunk budget on purpose (gap-fleet-accept-loop-not-router):
/// an admitted-but-flooding browser peer must not exhaust the pool fleet-trunk
/// (`HIVE_ALPN`) connections rely on. Lower default than the trunk budget —
/// browsers are numerous, low-trust, and each one is cheap to refuse.
fn max_browser_conns() -> usize {
    std::env::var("HIVE_P2P_BROWSER_MAX_CONNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(128)
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
    dyn Fn(
            u8,
            String,
            Vec<u8>,
            Option<String>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Serves one MESH-JOIN request: `(remote_id, node_json, proof) -> response body`
/// (empty = rejected). `remote_id` is the QUIC connection's authenticated peer
/// identity — the handler verifies `proof` against IT (never against anything the
/// body claims), admits the id into the trust set on success, and returns the
/// current node roster so the joiner learns the whole mesh in one round trip.
pub type JoinHandler = Arc<
    dyn Fn(
            String,
            Vec<u8>,
            String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>
        + Send
        + Sync,
>;

/// Re-checks a browser endpoint identity against the control-plane admission
/// store before any hive/browser/0 stream is accepted. The handler may perform
/// one leader fallback; false closes the connection without exposing an op.
pub type BrowserAdmissionHandler = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Serves one `Op::CrrSync` request against the fleet's replica of the
/// caller's browser-replicated database (bn-browser-fleet-crr-exchange):
/// `(remote_id, request payload) -> reply payload`, where `remote_id` is the
/// QUIC connection's authenticated browser identity. hive-cloud installs the
/// real implementation (grant re-check against its own replicated admission
/// view, replica open, capped apply/export); `Err(code)` is the stream reset
/// code to refuse with, so protocol faults stay distinct from sync-domain
/// refusals (which travel inside an Ok reply's status byte).
pub type BrowserCrrHandler = Arc<
    dyn Fn(
            String,
            Vec<u8>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, u32>> + Send>>
        + Send
        + Sync,
>;

/// Receives a browser connection's FRAMED byte totals as each op stream
/// completes: `(endpoint_id, inbound_bytes, outbound_bytes)`, where
/// `endpoint_id` is the QUIC-authenticated browser identity (hive-cloud maps
/// it to the admission's tenant — the crate itself stays tenant-free).
/// "Framed" means exactly the bytes of this ALPN's wire contract: the u32 LE
/// length prefix, the op byte, and the payload — never transport overhead.
/// Bytes are reported at the stage they provably moved: a fully-read request
/// counts inbound even when the stream is then refused, a fully-written
/// request counts outbound even when the reply never arrives. Synchronous by
/// design: recording is pure in-memory arithmetic and must never block (or be
/// dropped mid-await by) the stream task that calls it.
pub type BrowserMeterHandler = Arc<dyn Fn(String, u64, u64) + Send + Sync>;

/// The installed browser byte meter (bn-impl-relay-byte-metering). Process-
/// global like `VERIFY_COUNTERS`: both metering boundaries — the inbound
/// `serve_browser_conn` accept path and the outbound `BrowserPool` op path —
/// reach it without growing `serve_tunnels_full`'s parameter list again.
/// `None` (the default) makes metering a compiled-in no-op, so witness
/// harnesses and embedders that never install a handler pay one RwLock read.
static BROWSER_METER: std::sync::RwLock<Option<BrowserMeterHandler>> = std::sync::RwLock::new(None);

/// Install (or clear) the process-global [`BrowserMeterHandler`]. hive-cloud
/// installs its per-tenant recorder once at boot, next to the admission
/// handler registration. NOT part of any trust boundary: the handler observes
/// byte counts, it can never influence whether a stream is served.
pub fn set_browser_meter(meter: Option<BrowserMeterHandler>) {
    if let Ok(mut slot) = BROWSER_METER.write() {
        *slot = meter;
    }
}

/// Report one completed stage of a browser op stream to the installed meter
/// (if any). Never fails, never blocks the caller meaningfully, and never
/// attributes: attribution is the handler's job (it owns the endpoint→tenant
/// join against the admission store).
fn meter_browser_bytes(endpoint_id: &str, inbound: u64, outbound: u64) {
    let meter = BROWSER_METER
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    if let Some(meter) = meter {
        meter(endpoint_id.to_string(), inbound, outbound);
    }
}

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
    dyn Fn(
            RawTarget,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RawTargetConn>> + Send>>
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
    if signer
        .verify(&gossip_sig_preimage(method, path, body, ts), &sig)
        .is_err()
    {
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).await?;
    Ok(v)
}

/// Read one datagram frame off a raw-target UDP mesh stream: `Ok(Some(bytes))`
/// per datagram, `Ok(None)` ONLY on a clean end-of-stream at a frame boundary
/// (zero bytes of the length prefix read). A peer that dies mid-frame — EOF
/// after 1–3 prefix bytes or mid-payload — is `Err(InvalidData "truncated
/// datagram frame")`, never a silent clean close: the previous shape mapped
/// every `UnexpectedEof` to `Ok(None)`, making a mid-write peer death
/// indistinguishable from a graceful end (p2p-raw-datagram-truncation).
/// Exported so the edge-side UDP relay speaks byte-identical framing to the
/// owner-side pump in this crate.
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

/// Write one datagram frame (`[u32 len][bytes]`, boundary-preserving) onto a
/// raw-target UDP mesh stream. Oversized payloads are a framing error — they
/// could never have arrived as one datagram.
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

/// Serialize this endpoint's dialable address (direct socket addrs + relay url) to
/// JSON, so peers can learn it via gossip and dial directly — no DNS/relay
/// discovery round-trip required.
///
/// The advertised set is AUGMENTED with this node's publicly-routable address when
/// one is configured (see [`configured_public_addr`]). Without that augmentation the
/// blob carries only what iroh discovered from its own sockets, which on a cloud VM
/// behind 1:1 NAT is the PRIVATE interface address — `10.0.0.x:11204`, unroutable
/// from any other region. Measured on the live fleet: the leader's peer book held 72
/// private `10.x` QUIC addresses against 12 public ones (and those 12 were the relay
/// port, not the QUIC port), so EVERY direct dial had an unreachable target and the
/// whole mesh was pinned to the relay fallback. Roughly half of all node pairs then
/// failed both probe and gossip, and the health loop withdrew live nodes from client
/// DNS and placement.
///
/// This does not REPLACE the discovered addresses, it adds to them: the private
/// entries stay valid for same-VPC peers, and the relay entry stays as the fallback
/// for nodes with no inbound reachability at all.
pub fn addr_json(ep: &Endpoint) -> Option<String> {
    let mut addr = ep.addr();
    if let Some(sa) = configured_public_addr() {
        addr.addrs.insert(iroh::TransportAddr::Ip(sa));
    }
    serde_json::to_string(&addr).ok()
}

/// This node's publicly-dialable QUIC address, from `HIVE_PUBLIC_IP` +
/// `HIVE_IROH_PORT`, or `None` when either is unset/unusable.
///
/// BOTH are required and neither is inferable. `HIVE_PUBLIC_IP` is the address
/// operators already configure for client DNS (a cloud VM cannot read its own
/// external IP off an interface), and the port must be the PINNED one — an
/// ephemeral bind port changes on every restart, so publishing it would advertise
/// an address that goes stale the moment the process restarts, which is worse than
/// publishing nothing. Loopback/unspecified addresses are rejected: advertising
/// those tells peers to dial themselves.
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

/// Extract the iroh `EndpointId` (cryptographic node identity, as a string) from an
/// `addr_json` blob (a serialized `EndpointAddr` learned via gossip). Used to build
/// the peer-trust allowlist (#20) from the fleet roster the node already knows.
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
/// NOTE: this budget is now load-bearing for TWO mechanisms, not one. The
/// public mainline-DHT provider (`dht`) resolves in a measured 2.0–4.6s steady
/// state, on top of a 2–6s cold-routing-table warm-up, so at the 4000ms default
/// `dial_fresh` cancels the DHT before it can answer on most first dials and
/// the provider ships inert. Fleet deploys set `HIVE_P2P_DISCOVERY_MS=8000`
/// (ansible `hive_p2p_discovery_ms`); the default is left at 4000 so this
/// change alters nothing for a caller that has not opted into the DHT-friendly
/// budget. `dial_fallback_ceiling()` tracks it automatically and all three of
/// its callers already floor their own timeouts at that ceiling.
fn discovery_budget() -> Duration {
    env_ms("HIVE_P2P_DISCOVERY_MS", 4_000)
}

/// The live discovery-fallback budget, for operator observability
/// (`GET /v1/mesh/discovery`). Same function the dial path uses — asking the
/// question with a second implementation is how a diagnostic and the decision
/// it describes quietly diverge.
pub fn dial_discovery_budget() -> Duration {
    discovery_budget()
}
/// Worst-case time `acquire()` needs to run the cached-hint attempt AND the
/// fresh-discovery fallback to completion: `connect_budget + discovery_budget`,
/// plus slack. A caller wrapping `join_request`/`gossip_request` in its OWN
/// `tokio::time::timeout` shorter than this drops the future mid-flight while
/// `dial_fresh` is still pending — the fallback `discovery_budget` exists to
/// recover a first-contact node's stale/private-IP cached hint (see
/// `discovery_budget`'s doc comment) never gets to run, so a first-contact dial
/// can never succeed no matter how long fresh discovery would have taken. Any
/// such outer timeout MUST be at least this long.
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
        write!(
            f,
            "p2p {} timeout to {} after {}ms (peer presumed dead)",
            self.phase, self.node_id, self.budget_ms
        )
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
    incarnation: u64,
    conn: Connection,
}

/// Browser invocation failed before or after a complete request frame entered
/// QUIC. Callers may safely fall back after `sent == false`; `sent == true`
/// means the browser may already have executed the function.
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

/// Bounded, tenant-free observability for [`BrowserPool`] (bn-p2p-observability)
/// — global aggregates only, same posture as hive-cloud's browser admission/
/// presence counters (never per-endpoint, which would leak which specific
/// browser peers are active). Latency is a running sum/count (avg on read)
/// rather than a full histogram — proportionate to the existing simple-counter
/// style elsewhere in this codebase, not a new dependency for percentiles.
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
    /// A trunk closed and removed from the pool, whether by explicit
    /// revocation (`close_endpoint`) or by `invoke`'s own redial-on-failure
    /// path — the same proxy this codebase already uses elsewhere for "the
    /// underlying connection had to be re-established" (relay switch, NAT
    /// rebinding, a genuinely dead peer). iroh does not expose a distinct
    /// path-migration event to hook here without new plumbing, so this is the
    /// honest, already-available signal, not a stand-in pretending to be more.
    trunk_evictions_total: std::sync::atomic::AtomicU64,
    invoke_redials_total: std::sync::atomic::AtomicU64,
    /// CRR sync rounds (bn-browser-fleet-crr-exchange) counted SEPARATELY from
    /// function invokes: same trunk/request plumbing, different op, and an
    /// operator must be able to see database replication without it
    /// disappearing into invoke traffic. Tenant-free global aggregates, same
    /// posture as every counter above.
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

/// Native client for `hive/browser/0`. Browser trunks are kept separate from
/// [`PeerPool`]: ALPN is negotiated per QUIC connection, so a fleet
/// `hive/tunnel/0` trunk cannot carry browser streams.
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

    /// Invoke a digest-pinned browser artifact. Executable source is never sent:
    /// the wire carries only `[digest][request JSON]` inside the shared protocol
    /// frame. Every await is bounded and every reply length is checked before
    /// allocation.
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

    /// One CRR anti-entropy round against an admitted browser's replica
    /// (bn-browser-fleet-crr-exchange) — the fleet-INITIATED direction of the
    /// exchange. The browser serves these via its own inbound `Op::CrrSync`
    /// handler, the same op it uses when it initiates; `payload` is a verbatim
    /// `hive_browser_proto::encode_crr_sync_request` frame built by the caller
    /// (hive-cloud owns the watermarks/batches; this pool only knows frames).
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

    /// Shared request/response plumbing for every outbound op on a browser
    /// trunk: acquire (with one redial), one bounded bi stream, one framed
    /// request, then one framed reply whose declared length is checked against
    /// the OP's own cap ([`check_len_for`]) before any allocation.
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
        // Metered at the stage the bytes provably moved: the full request
        // frame is on the wire even if the reply below never arrives.
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
        // The reply's framed total: u32 LE prefix + body, fully read.
        meter_browser_bytes(endpoint_id, 4 + reply.len() as u64, 0);
        Ok(reply)
    }
}

/// Connection pool + multiplexer for the cross-node mesh path. Keeps ONE persistent
/// iroh QUIC connection per peer (`node_id`) and opens a NEW bi STREAM per request,
/// instead of dialing a fresh connection (and paying a handshake / holepunch) each
/// time. Directed dial + gossip discovery are unchanged; only the connection
/// lifecycle is pooled.

/// ms since UNIX epoch — local helper (this crate deliberately has no
/// hive-core dependency).
fn pool_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    Failed(String),
    Lifecycle(DialLifecycleError),
}

impl SharedDialError {
    fn from_anyhow(error: &anyhow::Error) -> Self {
        error
            .downcast_ref::<DeadPeerTimeout>()
            .cloned()
            .map(Self::DeadPeer)
            .unwrap_or_else(|| Self::Failed(error.to_string()))
    }

    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::DeadPeer(error) => anyhow::Error::new(error),
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

pub struct PeerPool {
    ep: Endpoint,
    state: StdMutex<PeerPoolState>,
    opened: AtomicU64,
    reused: AtomicU64,
    timeouts: Arc<TimeoutCounters>,
    /// Per-peer WARM backoff: canonical key → (next_attempt_ms, cur_delay_ms).
    /// Consulted ONLY by [`PeerPool::warm`] — a request-driven `acquire` always
    /// dials (a real caller's demand outranks the backoff; tau's peering loop
    /// draws the same line). Cleared on a successful warm; grows
    /// delay + delay/2 + dither, capped at 5 minutes. A stale entry left
    /// behind after a REQUEST-driven recovery is harmless: warm's skip is a
    /// no-op while the trunk is live, and the entry stops BLOCKING within one
    /// cap (it is removed on the next successful warm, not by a timer).
    warm_backoff: Mutex<HashMap<String, (u64, u64)>>,
    /// Negative-discovery memo: node_id → (until_ms, cur_delay_ms). A peer
    /// whose FRESH-DISCOVERY dial just failed is not re-discovered for a
    /// bounded window (30s → 180s cap), so dead peers stop burning the
    /// multi-second discovery budget healthy peers need every warmer tick.
    /// The cap is deliberately LOW — the retain_dialable partition taught this
    /// fleet that starving the dial path of addresses partitions it, so a
    /// dead-looking peer (bootstrap seeds included) is always re-tried within
    /// three minutes, and any success clears the memo.
    neg_discovery: Mutex<HashMap<String, (u64, u64)>>,
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
        })
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
    }

    /// Proactively ensure a live trunk to `node_id` exists — dial (holepunch) if
    /// missing/dead, reuse if already warm — WITHOUT opening a request stream. Lets
    /// a background maintainer keep the whole mesh pre-trunked so a request-time
    /// dial/holepunch is the rare exception (peer just restarted), not the norm.
    /// Connect is bounded by the H4 budget, so a dead peer can't wedge the warmer.
    /// Returns whether a live trunk is now cached.
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

    /// Relay cost accounting (#23): classify each live trunk as relay vs direct via
    /// iroh's `remote_addr()` and sum its QUIC byte counters (`udp_tx`/`udp_rx`).
    /// RELAYED bytes transit a relay server — a real $ cost and a latency/SPOF
    /// signal — so surfacing them shows how much mesh traffic isn't going direct
    /// peer-to-peer (and whether holepunching is succeeding).
    pub async fn relay_stats(&self) -> RelayStats {
        let mut s = RelayStats::default();
        {
            let state = self.lock_state();
            for t in state.trunks.values() {
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
        }
        s.timeouts = self.timeouts.snapshot().await;
        s
    }

    /// Get a live connection to `node_id`: reuse the warm trunk, else dial a new one.
    /// The map lock is taken ONLY to look up / insert — **never** held across the
    /// (possibly slow, holepunching) `connect`, so a slow first-contact to one peer
    /// can't serialize requests to the others.
    async fn acquire(&self, node_id: &str, addr_json: &str) -> Result<AcquiredPeer> {
        let addr: EndpointAddr = serde_json::from_str(addr_json)?;
        let id = addr.id;
        let key = id.to_string();

        let (signal, generation, leader) = {
            let mut state = self.lock_state();
            if let Some((trunk_incarnation, conn)) = state
                .trunks
                .get(&key)
                .filter(|trunk| trunk.conn.close_reason().is_none())
                .map(|trunk| (trunk.incarnation, trunk.conn.clone()))
            {
                let generation = *state.generations.entry(key.clone()).or_insert(0);
                // The label→eid alias installs ONLY against a proven live
                // trunk: installed eagerly (as the first cut did), a STALE
                // cached addr fences the label's previous — healthy — eid,
                // closing a good trunk and failing its in-flight dial on
                // every warm-backoff cadence (refutation finding F3: the
                // trunk-instability class canonical-endpoint keying exists
                // to prevent, reintroduced through the alias fence). A label
                // remap must be proven by a completed dial, never asserted
                // by cached input.
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
                dead.conn.close(0u32.into(), b"dead trunk removed by pool");
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
        let dialed: Result<Connection> = async {
            let budget = connect_budget();
            Ok(match tokio::time::timeout(budget, self.ep.connect(addr, HIVE_ALPN)).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(error)) => {
                    tracing::warn!(node_id, err = %error, "p2p connect error using cached hint; retrying via fresh discovery");
                    self.dial_fresh(node_id, id).await?
                }
                Err(_) => {
                    self.timeouts.bump(node_id, PHASE_CONNECT).await;
                    tracing::warn!(
                        node_id,
                        budget_ms = budget.as_millis() as u64,
                        "p2p connect timeout using cached hint; retrying via fresh discovery"
                    );
                    self.dial_fresh(node_id, id).await?
                }
            })
        }
        .await;

        match dialed {
            Ok(conn) => {
                let mut state = self.lock_state();
                let same_channel = state.inflight.get(&key).is_some_and(|flight| {
                    flight.generation == generation && flight.signal.same_channel(&signal)
                });
                let current =
                    same_channel && state.generations.get(&key).copied().unwrap_or(0) == generation;
                if current {
                    // Same proof-gated alias install as the reuse path: the
                    // dial completed, so remapping the label — and fencing
                    // the previous identity's trunk — is earned (F3).
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
                    tracing::info!(node_id, key = %key, "trunk opened");
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
                    state
                        .dial_evidence
                        .entry(key.clone())
                        .or_default()
                        .last_failure = Some(Instant::now());
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

    /// Fallback dial using ONLY the peer's `EndpointId` — no cached direct addrs,
    /// no cached relay_url. This forces iroh's configured Discovery/AddressLookup
    /// (n0 pkarr/DNS, or the self-hosted `HIVE_DISCOVERY_DNS` resolver) to resolve
    /// the peer's CURRENT address rather than reusing whatever stale hint `acquire`
    /// already tried and failed on. Called from `acquire` only after the
    /// cached-hint attempt has already failed/timed out — see `discovery_budget`
    /// for why iroh would otherwise never consult Discovery on its own here.
    async fn dial_fresh(&self, node_id: &str, id: iroh::EndpointId) -> Result<Connection> {
        let now = pool_now_ms();
        // Memo keyed on the CANONICAL endpoint id (available here by
        // construction), never the caller's label — the two planes pass
        // different labels for the same peer, and a label-keyed memo would
        // give each caller its own window instead of one per peer.
        let memo_key = id.to_string();
        if let Some((until, _)) = self.neg_discovery.lock().await.get(&memo_key) {
            if now < *until {
                // Fresh discovery against this peer failed moments ago; do not
                // burn the multi-second budget again inside the memo window.
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
        match tokio::time::timeout(budget, self.ep.connect(EndpointAddr::new(id), HIVE_ALPN)).await
        {
            Ok(Ok(c)) => {
                let c = UnpublishedConnection::new(c);
                self.neg_discovery.lock().await.remove(&memo_key);
                tracing::info!(
                    node_id,
                    "p2p connect recovered via fresh discovery (cached hint was stale)"
                );
                Ok(c.publish())
            }
            Ok(Err(e)) => {
                bump_memo().await;
                tracing::warn!(node_id, err = %e, "p2p connect error via fresh discovery (giving up)");
                Err(e.into())
            }
            Err(_) => {
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

    /// Close one failed acquired trunk and remove it only if it is still the
    /// published trunk for this peer. A delayed failure from an older connection
    /// must never evict a healthy replacement published under the same peer
    /// generation.
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
        acquired
            .conn
            .close(0u32.into(), b"failed acquired trunk evicted by pool");
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
        Ok(TunnelResp {
            status: s.status,
            headers: s.headers,
            body: buf,
        })
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
            let acquired = match self.acquire(node_id, addr_json).await {
                Ok(acquired) => acquired,
                Err(e) => {
                    if attempt < 2 {
                        continue;
                    }
                    return Err(e);
                }
            };
            // OPEN_BI — PRE-SEND, bounded by the open budget. An error OR a timeout
            // means no request bytes left this node → evict + retry once.
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
            self.record_success(&acquired);
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
            self.record_success(&acquired);
            return Ok(resp);
        }
    }

    pub async fn close_peer(&self, node_id: &str) {
        let mut state = self.lock_state();
        let key = Self::canonical_key_locked(&state, node_id);
        Self::fence_identity_locked(&mut state, &key, b"closed by pool");
    }

    /// Test/diagnostic helper: forcibly close a peer's cached connection IN PLACE
    /// (the trunk stays in the map). This severs the real QUIC connection without
    /// evicting it, so the next request must DETECT the dead trunk — via
    /// `close_reason()` or an `open_bi()` failure — and re-dial. Returns whether a
    /// trunk was cached, and (for assertions) whether that handle now reports closed.
    pub async fn sever_peer(&self, node_id: &str) -> bool {
        let state = self.lock_state();
        let key = Self::canonical_key_locked(&state, node_id);
        match state.trunks.get(&key) {
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
pub async fn bind_with_key(
    key_path: Option<std::path::PathBuf>,
    seeds: &[SeedPeer],
) -> Result<Endpoint> {
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
    // Only the connection-level idle timeout and the concurrent-stream ceiling
    // are set; keep-alive intervals are left at iroh's tuned defaults. See
    // `IDLE_TIMEOUT`'s doc comment for why the previous keep-alive override was
    // removed rather than adjusted, and `max_streams()` for why the stream cap
    // is stated explicitly rather than inherited.
    let tc = QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            IDLE_TIMEOUT
                .try_into()
                .expect("30s is a valid QUIC idle timeout"),
        ))
        .max_concurrent_bidi_streams(iroh::endpoint::VarInt::from_u32(max_streams()))
        .build();
    let mut builder = if n0_discovery {
        // n0 discovery (pkarr/DNS) + n0's relays (unless HIVE_RELAY_URLS overrides below).
        Endpoint::builder(N0)
    } else {
        // No n0 pkarr/DNS; keep a relay so NAT'd nodes remain reachable.
        Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::endpoint::default_relay_mode())
    }
    // BROWSER_ALPN is accepted alongside the fleet trunk ALPN so a browser tab
    // can dial in directly; `serve_tunnels_full` dispatches per-ALPN with its
    // OWN connection budget (see `max_browser_conns`) before either one is
    // ever handed a mode-byte stream.
    .alpns(vec![HIVE_ALPN.to_vec(), BROWSER_ALPN.to_vec()])
    .transport_config(tc);
    // Self-hosted relays (HIVE_RELAY_URLS): when set, NAT-traversal + relayed data
    // paths transit OUR iroh-relay infra instead of n0's — applied in BOTH branches,
    // overriding the preset's relay map. Direct hole-punching is unchanged (relays stay
    // fallback-only). Unset → keep prior n0-relay behavior so existing deploys don't break.
    if let Some(map) = relay_map_from_env() {
        let n = map.len();
        builder = builder.relay_mode(iroh::RelayMode::Custom(map));
        tracing::info!(
            relays = n,
            "using self-hosted iroh relays (HIVE_RELAY_URLS)"
        );
    }
    // Pin the QUIC bind port when HIVE_IROH_PORT is set. By default iroh binds an
    // EPHEMERAL UDP port (witnessed live: 37095/60316/43633/51891 on one node,
    // different every restart). That is fine when every path is relayed, but it
    // makes a cloud security-group rule impossible to write — you cannot open a
    // port whose number changes on each boot, which is exactly why the CVM/GPU
    // hosts (inbound 22 only) could never accept a DIRECT connection and were
    // relay-only. Pinning the port is the PREREQUISITE for allowing hole-punched
    // direct p2p on those hosts; relays are unchanged and remain the fallback
    // whenever the direct path or discovery is unavailable. Unset ⇒ ephemeral,
    // exactly as before, so nothing changes for hosts that don't need it.
    if let Some(port) = std::env::var("HIVE_IROH_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
    {
        // One `bind_addr` per address family (iroh's own contract: calling it
        // twice for the SAME family is undefined routing), preceded by
        // `clear_ip_transports` so our explicit pair replaces the preset's
        // default sockets rather than racing them — this mirrors iroh's
        // documented example. Both calls are infallible in practice: the
        // addresses are `UNSPECIFIED` + a parsed `u16`, and the only error
        // arms are a duplicate user-defined default (impossible right after
        // clearing) or an invalid prefix length (not reachable with default
        // opts), so a failure here is a programming error and must be loud
        // rather than silently reverting to an ephemeral port the operator
        // then cannot open in a firewall.
        let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
        let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        builder = builder
            .clear_ip_transports()
            .bind_addr(v4)
            .and_then(|b| b.bind_addr(v6))
            .expect("UNSPECIFIED v4/v6 with a parsed u16 port is always a bindable socket addr");
        tracing::info!(port, "pinned iroh QUIC bind port (HIVE_IROH_PORT)");
    }
    // Resolved BEFORE the builder so the mainline-DHT publisher below can sign
    // its pkarr record with the SAME key the endpoint binds with — that identity
    // is what makes the DHT record's key equal to this node's `EndpointId`.
    // `None` (no key file: tests/dev, ephemeral identity) keeps iroh's own
    // generate-on-bind behaviour, exactly as before.
    let secret = key_path.map(|path| load_or_create_secret(&path));
    if let Some(sk) = &secret {
        builder = builder.secret_key(sk.clone());
    }
    // Seed addresses are FILTERED to what a peer could actually dial.
    //
    // This fed every address a peer advertised straight into `MemoryLookup`,
    // unfiltered — including the RFC1918 10.x/172.16/192.168 addrs that
    // `peer_iroh.json` is full of (AGENTS.md documents that those are private and
    // not dialable from another region). Each such address becomes a connection
    // PATH CANDIDATE that can never complete, and iroh 1.0.x queues candidates in
    // an unbounded `VecDeque` (`pending_open_paths`, upstream #4390 — still
    // unfixed in 1.0.3, both community PRs closed unmerged).
    //
    // That queue is the fleet's OOM. Measured on fc-hongkong with jemalloc
    // profiling: 71,680 MiB — 99.8% of live heap — in ONE stack,
    // `RawVec::finish_grow -> VecDeque::grow -> remote_state::State::open_path_on_conn
    // -> RemoteStateActor::open_path_on_all_conns`.
    //
    // Publishing already applies exactly this filter (`dht::relay_and_public_ip_filter`);
    // the dial side simply never did. Relay URLs are always kept, so a peer behind
    // NAT stays reachable — the cost of dropping a private addr is a relayed
    // connection, which is what actually happened anyway once the direct path
    // failed. `HIVE_SEED_ALLOW_PRIVATE=1` restores the old behaviour for a
    // single-VPC deployment where private addrs genuinely are dialable.
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
        // A seed with no dialable transport left is not a usable hint; keeping it
        // would re-create the very candidate churn this filter exists to remove.
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
    // Public mainline DHT — strictly ADDITIVE. The seed `MemoryLookup` and any
    // Seer `PkarrResolver` above stay registered, and iroh polls every provider
    // CONCURRENTLY, emitting each item as it arrives, so a seed/Seer hit still
    // reaches the dial first on latency alone and no code path ever becomes
    // DHT-only. This is the only source that needs no fleet peer to be
    // reachable first, which is why it is worth having at all (see `dht`'s
    // module docs, including what becomes publicly resolvable).
    //
    // Built here rather than handed to iroh as an `AddressLookupBuilder`: iroh
    // propagates a builder error out of `bind()`, and a failed DHT socket must
    // degrade to a WARN, never to "P2P transport disabled".
    if let Some(lookup) = dht::lookup_from_env(secret.as_ref()).await {
        builder = builder.address_lookup(lookup);
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
        tracing::warn!(
            ?path,
            "could not persist iroh key; identity will be ephemeral"
        );
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
    )
    .await
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
    browser_admission: Option<BrowserAdmissionHandler>,
    browser_crr: Option<BrowserCrrHandler>,
) {
    let conn_limit = match max_inbound_conns() {
        0 => None,
        n => Some(Arc::new(tokio::sync::Semaphore::new(n))),
    };
    let browser_conn_limit = match max_browser_conns() {
        0 => None,
        n => Some(Arc::new(tokio::sync::Semaphore::new(n))),
    };
    let browser_resources = BrowserInboundResources::new();
    while let Some(incoming) = ep.accept().await {
        // Unknown/fragmented ClientHello classification consumes the low-trust
        // browser budget. Letting it consume the fleet budget would give a
        // hostile client a deliberate route around browser isolation.
        let proposed_browser = incoming
            .decrypt()
            .and_then(|d| d.alpns())
            .map(|alpns| {
                alpns
                    .filter_map(Result::ok)
                    .any(|protocol| protocol.as_ref() == BROWSER_ALPN)
            })
            .unwrap_or(true);
        let sem = if proposed_browser {
            &browser_conn_limit
        } else {
            &conn_limit
        };
        // Never await an exhausted semaphore in the single accept loop: doing
        // so head-of-line blocks every later connection, including fleet trunks.
        // Excess connections are rejected immediately; no waiter task or
        // handshake queue can grow without bound.
        let permit = match sem {
            Some(sem) => match sem.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    tracing::warn!(
                        class = if proposed_browser { "browser" } else { "fleet" },
                        "P2P connection budget exhausted; rejecting connection"
                    );
                    continue;
                }
                Err(tokio::sync::TryAcquireError::Closed) => return,
            },
            None => None,
        };
        let local = local_http.clone();
        let trust = trust.clone();
        let gossip = gossip.clone();
        let join = join.clone();
        let raw_resolver = raw_resolver.clone();
        let browser_admission = browser_admission.clone();
        let browser_crr = browser_crr.clone();
        let browser_resources = browser_resources.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(_) => return,
            };
            if conn.alpn() == BROWSER_ALPN {
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
            if conn.alpn() == HIVE_ALPN {
                serve_fleet_conn(
                    conn,
                    local,
                    max_concurrency,
                    trust,
                    gossip,
                    join,
                    raw_resolver,
                )
                .await;
            }
        });
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

/// Serves a single `hive/browser/0` connection: one connection → many bi
/// streams, each an op-tagged `[u32 len][op][payload]` request — NO mode-byte
/// selector, NO gossip/join/raw dispatch, NO trust-set check. This is
/// deliberately the SAME contract as `hive_browser::BrowserNode`'s own accept
/// loop (both sides read it out of `hive-browser-proto`), so the identical
/// browser-side call that round-trips browser-to-browser works unchanged
/// against a real fleet node. Bigger asks on this ALPN (real request routing,
/// admission scopes) are `bn-impl-invoke-routing`/`bn-impl-mesh-admission`'s
/// job, layered on top of this same accept path — never by adding a mode byte
/// or reaching into the HIVE_ALPN dispatch above.
///
/// The reply carries NO op byte. Echoing the request frame back verbatim — as
/// this did before the op byte existed — hands the caller the op byte as the
/// first character of the reply body, which silently corrupts every echo.
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
                // Per-op cap BEFORE the payload allocation (bn-browser-fleet-
                // crr-exchange: CrrSync frames may exceed BROWSER_MAX_FRAME up
                // to BROWSER_MAX_CRR_FRAME; Echo keeps its tighter cap).
                let len = check_len_for(op, lenb).map_err(|error| error.reset_code())?;
                if len == 0 {
                    return Err(browser_reset::MALFORMED_PAYLOAD);
                }
                match op {
                    Op::Echo if len - 1 > BROWSER_MAX_ECHO => {
                        return Err(browser_reset::FRAME_TOO_LARGE);
                    }
                    Op::Echo | Op::CrrSync => {}
                    // This ALPN's fleet side serves liveness (Echo) and the DB
                    // exchange (CrrSync); function invoke / asset pull are
                    // served by the BROWSER half, never to it.
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
                // Metered the moment the frame is fully read: 4 (u32 LE
                // prefix) + 1 (op byte) + payload. A refusal downstream of
                // this point still paid the inbound leg.
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
                    // The handler re-checks the grant, opens the replica, and
                    // does bounded disk IO; 30s is generous for local work and
                    // still refuses a wedged handler explicitly.
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
                    // The full framed reply (u32 LE prefix + body) is written;
                    // a FIN failure below does not un-send those bytes.
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
    // Signal end-of-stream EXPLICITLY rather than leaving it to `Drop`.
    //
    // `shutdown()` and not `finish()`: this handler is generic over
    // `W: AsyncWrite + Unpin`, not a concrete `noq::SendStream`, so `finish()`
    // isn't in scope — but they are the same operation here. noq implements
    // `poll_shutdown` for `SendStream` as `Poll::Ready(self.get_mut().finish())`
    // (noq-1.1.0/src/send_stream.rs:345), so tokio's `shutdown()` performs the
    // real QUIC FIN. Dropping the stream would also finish it, which is why this
    // worked before; iroh's QUIC guide is explicit that embedders should manage
    // stream closure rather than depend on drop semantics, and stating it here
    // means a future refactor that holds the stream longer can't silently delay
    // the peer's end-of-response.
    let _ = send.shutdown().await;
}

/// Test/diagnostic helper (#H4): accept P2P connections + bi streams but NEVER
/// write a response — the "accept-but-silent" peer. Holds both stream halves open
/// without answering, so a caller's first-byte timeout must fire. Not used in
/// production; exposed so the live timeout witnesses (`examples/pool_witness.rs`)
/// can stand up a real silent owner.
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
                    // Explicit empty response: the peer sees a clean failure, not a hang.
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
    let resp = handler(m[0], path, body, verified_signer).await;
    let len = (resp.len() as u32).to_be_bytes();
    let _ = send.write_all(&len).await;
    let _ = send.write_all(&resp).await;
    let _ = send.flush().await;
    // Explicit end-of-stream on every exit from this handler, including the
    // three rejection paths above — see `serve_join` for why `shutdown()` is
    // the right call on a generic `AsyncWrite` and why relying on `Drop` (which
    // does work today) is not good enough.
    let _ = send.shutdown().await;
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
