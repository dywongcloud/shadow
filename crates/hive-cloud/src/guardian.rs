//! GuardianDB backend, always on: every platform snapshot is written into a real iroh-native
//! GuardianDB key/value store (iroh-docs + iroh-blobs, BLAKE3), persisted under
//! `$HIVE_DATA/guardian` and replicated over the mesh by Willow range reconciliation. The local
//! file snapshot ([`crate::persist`]) stays the bootstrap source of truth; init and writes are
//! best-effort (a failure is logged and never breaks the request path or the on-disk snapshot).
//! State is partitioned per tenant namespace ([`crate::persist::namespaced`], key
//! `ns/<namespace>/state`), never one global blob.

use std::sync::{Arc, OnceLock};

use guardian_db::guardian::core::NewGuardianDBOptions;
use guardian_db::guardian::error::GuardianError;
use guardian_db::guardian::GuardianDB;
use guardian_db::p2p::network::client::IrohClient;
use guardian_db::p2p::network::config::{ClientConfig, MdnsDiscoveryAuth};
use guardian_db::traits::KeyValueStore;
use tokio::sync::OnceCell;

use crate::persist::PlatformSnapshot;

/// Live GuardianDB handle: the database (kept alive so its iroh endpoint keeps running), the opened
/// key/value store, and a clone of the iroh client that shares the same live endpoint, used by
/// `seed_known_peers` to seed peer addresses after construction.
struct Handle {
    db: GuardianDB,
    kv: Arc<dyn KeyValueStore<Error = GuardianError>>,
    client: IrohClient,
}

static HANDLE: OnceCell<Handle> = OnceCell::const_new();

/// Name of the sole iroh-docs KV namespace this node opens (`namespace_heads` keys its head map by
/// it).
const KV_NAMESPACE: &str = "hive-state";

/// Convenience alias for the native relational/SQL database this handle backs
/// (see [`crate::relational`]) — `guardian_db::sql::open_sql`'s return type.
pub(crate) type SqlDb =
    Arc<guardian_db::sql::Database<guardian_db::sql::GuardianRelationalStorage>>;

static SQL_HANDLE: OnceCell<SqlDb> = OnceCell::const_new();

/// Lazily opens (once) the in-process relational/SQL database on this SAME live `GuardianDB`
/// instance `handle()` uses (no network hop, never pgwire); every node opens the identical named
/// database "hive" and iroh-docs CRDT replication converges each node's copy.
pub(crate) async fn sql_db() -> anyhow::Result<SqlDb> {
    SQL_HANDLE
        .get_or_try_init(|| async {
            let h = handle().await?;
            guardian_db::sql::open_sql(&h.db, "hive")
                .await
                .map_err(|e| anyhow::anyhow!("guardian sql open: {e}"))
        })
        .await
        .cloned()
}

/// Upper bound on how long a caller WAITS for the whole bring-up (iroh endpoint bind, keystore,
/// docs/blobs spawn), never on the work itself: init can wedge with no error and no log, so the
/// wait fails retryably while the in-flight task is parked ([`INIT_INFLIGHT`]).
/// `HIVE_GUARDIAN_INIT_TIMEOUT_MS` overrides the 30 s default (a slow host needs longer; a very low
/// value exercises the expiry path).
fn guardian_init_timeout() -> std::time::Duration {
    static T: OnceLock<std::time::Duration> = OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("HIVE_GUARDIAN_INIT_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_secs(30))
    })
}

/// Minimum spacing between full init attempts after a failure: every guardian caller retries
/// `handle()` on its own cadence, and an ungated persistent failure re-ran a FULL iroh endpoint
/// bind plus blobs/docs bring-up every few seconds, leaking each partial client.
const INIT_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Set once an init failure proves in-process retries can never succeed: redb's "Database already
/// open" (a previous attempt of THIS process still holds the lock) or a panic inside init
/// (deterministic). Once latched, `handle()` fails fast and names the reason; the only recovery is
/// a node restart.
static INIT_WEDGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Why the wedge latched, so the fail-fast message doesn't misattribute a
/// panic to redb's lock (which sent an operator down the wrong path once).
static WEDGE_REASON: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static LAST_FAILED_INIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Init attempts this process has made, to tell a redb lock held by a prior attempt of OURS
/// (permanent: latch) from one still held by the OUTGOING process during a `systemctl restart`
/// overlap (transient: retry).
static INIT_ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The in-flight init task when a previous caller stopped WAITING for it. `timeout` cancels by
/// dropping, which used to strand the whole built stack (endpoint, stores, redb lock) and start a
/// second init on top; the init now runs in its own task and a timed-out caller parks its
/// `JoinHandle` here, so the next caller awaits the SAME attempt: at most one init stack exists and
/// a slow-but-successful init is adopted.
static INIT_INFLIGHT: std::sync::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<Handle>>>> =
    std::sync::Mutex::new(None);

fn inflight_slot(
) -> std::sync::MutexGuard<'static, Option<tokio::task::JoinHandle<anyhow::Result<Handle>>>> {
    INIT_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the in-flight init's `JoinHandle` while one caller awaits it and puts it back in
/// `INIT_INFLIGHT` on drop unless the attempt finished (`.0` set to `None`): a caller removed by
/// cancellation (a hung-up client, an aborted task) must not leave the slot empty while the init
/// keeps running.
struct InflightInit(Option<tokio::task::JoinHandle<anyhow::Result<Handle>>>);

impl Drop for InflightInit {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            *inflight_slot() = Some(task);
        }
    }
}

/// Lazily open (once) the GuardianDB KV store, retrying on a previous failure
/// — but throttled (see `INIT_RETRY_BACKOFF`) and never after a wedge latch.
async fn handle() -> anyhow::Result<&'static Handle> {
    use std::sync::atomic::Ordering;
    if let Some(h) = HANDLE.get() {
        return Ok(h);
    }
    if INIT_WEDGED.load(Ordering::Relaxed) {
        let why = WEDGE_REASON
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "redb lock held by a leaked prior init attempt".to_string());
        anyhow::bail!("guardian is wedged in this process ({why}); restart hive-cloud to recover");
    }
    let last_failed = LAST_FAILED_INIT_MS.load(Ordering::Relaxed);
    if last_failed != 0 {
        let since = hive_core::now_ms().saturating_sub(last_failed);
        if since < INIT_RETRY_BACKOFF.as_millis() as u64 {
            anyhow::bail!(
                "guardian init failed {since}ms ago; backing off before the next attempt"
            );
        }
    }
    let result = HANDLE
        .get_or_try_init(|| async {
            let mut task = InflightInit(Some(match inflight_slot().take() {
                Some(existing) => {
                    tracing::info!("guardian init: re-awaiting the init already in flight (not starting a second one)");
                    existing
                }
                None => {
                    INIT_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(init_handle())
                }
            }));
            let outcome = tokio::time::timeout(
                guardian_init_timeout(),
                task.0.as_mut().expect("in-flight init handle is held until completion"),
            )
            .await;
            match outcome {
                Ok(Ok(result)) => {
                    task.0 = None;
                    result
                }
                Err(_) => Err(anyhow::anyhow!(
                    "guardian init timed out after {:?} (iroh endpoint bind / keystore / docs bring-up never completed); the attempt is still running and will be re-awaited, not restarted",
                    guardian_init_timeout()
                )),
                Ok(Err(join_err)) => {
                    task.0 = None;
                    let what = if join_err.is_panic() {
                        let panic = join_err.into_panic();
                        panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic payload".to_string())
                    } else {
                        "init task cancelled".to_string()
                    };
                    INIT_WEDGED.store(true, Ordering::Relaxed);
                    if let Ok(mut slot) = WEDGE_REASON.lock() {
                        *slot = Some(format!("init panicked: {what}"));
                    }
                    tracing::error!(
                        panic = %what,
                        "guardian init PANICKED — latching wedged state so callers stop re-attempting. \
                         The guardian store is UNAVAILABLE on this node until the panic is fixed and \
                         hive-cloud is restarted."
                    );
                    Err(anyhow::anyhow!("guardian init panicked: {what}"))
                }
            }
        })
        .await;
    if let Err(e) = &result {
        LAST_FAILED_INIT_MS.store(hive_core::now_ms(), Ordering::Relaxed);
        if e.to_string().contains("Database already open") {
            let attempts = INIT_ATTEMPTS.load(Ordering::Relaxed);
            if attempts <= 1 {
                tracing::warn!(
                    attempts,
                    "guardian init hit redb 'Database already open' on this process's FIRST attempt — the lock is held by ANOTHER process (typically the outgoing hive-cloud during a restart overlap), not by a leak of ours; retrying after backoff instead of latching"
                );
            } else {
                INIT_WEDGED.store(true, Ordering::Relaxed);
                if let Ok(mut slot) = WEDGE_REASON.lock() {
                    *slot = Some(format!(
                        "redb still open after {attempts} init attempts; the first attempt's failure is the real cause (see the preceding 'guardian init' / 'GuardianDB init failed' lines)"
                    ));
                }
                tracing::error!(
                    attempts,
                    "guardian init hit redb 'Database already open' after repeated attempts — a leaked prior attempt in THIS process holds the lock; latching wedged state (no further in-process retries; restart hive-cloud to recover)"
                );
            }
        }
    }
    result
}

/// The actual GuardianDB bring-up, run under `handle()`'s timeout and split out so each major step
/// logs a distinct marker: a stall shows which sub-step (iroh client, GuardianDB open, KV open)
/// never returned. The large open futures are `Box::pin`ned (unboxed, their debug-build poll frames
/// overflowed the tokio worker stack at boot). Any failure past `IrohClient::new` shuts the partial
/// client down and closes the database first (`shutdown()` does not release its redb keystore),
/// since spawned tasks hold the Arcs and a leak costs a whole iroh stack per retry.
async fn init_handle() -> anyhow::Result<Handle> {
    let dir = crate::persist::data_dir().join("guardian");
    std::fs::create_dir_all(&dir).ok();

    let cfg = ClientConfig {
        data_store_path: Some(dir.join("iroh")),
        enable_discovery_n0: true,
        enable_discovery_mdns: true,
        mdns_discovery_auth: Some(MdnsDiscoveryAuth::from_key_material(
            crate::secrets::key_material(),
        )),
        port: 0,
        ..ClientConfig::default()
    };
    tracing::info!("guardian init: opening iroh client (endpoint bind + keystore)");
    let client = IrohClient::new(cfg)
        .await
        .map_err(|e| anyhow::anyhow!("guardian iroh client: {e}"))?;
    let node_id = client.node_id();
    tracing::info!(%node_id, "guardian init: iroh client ready");
    let seed_client = client.clone();

    let mut boot_seeded = 0usize;
    if let Some(addrs) = BOOT_SEED_PEERS.get() {
        for addr_json in addrs {
            if seed_peer(&client, addr_json).await {
                boot_seeded += 1;
            }
        }
    }
    tracing::info!(
        count = boot_seeded,
        "guardian init: seeded known peers pre-open (for automatic DocTicket exchange)"
    );

    let opts = NewGuardianDBOptions {
        directory: Some(dir.clone()),
        backend: Some(client.backend().clone()),
        ..Default::default()
    };
    let db = match Box::pin(GuardianDB::new(client, Some(opts))).await {
        Ok(db) => db,
        Err(e) => {
            let _ = seed_client.shutdown().await;
            return Err(anyhow::anyhow!("guardian open: {e}"));
        }
    };
    tracing::info!("guardian init: GuardianDB open, opening 'hive-state' KV store");
    let kv = match Box::pin(db.key_value(KV_NAMESPACE, None)).await {
        Ok(kv) => kv,
        Err(e) => {
            let _ = db.close().await;
            let _ = seed_client.shutdown().await;
            return Err(anyhow::anyhow!("guardian kv open: {e}"));
        }
    };

    tracing::info!(%node_id, dir = ?dir, "GuardianDB ready (iroh-docs KV 'hive-state', replicated)");
    Ok(Handle {
        db,
        kv,
        client: seed_client,
    })
}

/// Registers a peer's iroh address (`add_node_addr`: address resolution only) AND explicitly admits
/// it (`note_explicit_peer`, the admission `resolve_shared_ticket`'s DocTicket exchange actually
/// consults). CALLERS MUST PASS THE PEER'S GUARDIANDB-SPECIFIC ADDRESS
/// (`NodeInfo::guardian_iroh_addr`, from its `my_iroh_addr()`), never its hive-p2p mesh address:
/// GuardianDB runs its own iroh identity, and a mesh identity here caused a connect retry storm. A
/// malformed or unreachable entry is skipped, never aborting the others.
async fn seed_peer(client: &IrohClient, addr_json: &str) -> bool {
    match serde_json::from_str::<iroh::EndpointAddr>(addr_json) {
        Ok(addr) => {
            let peer_id = addr.id;
            if let Err(e) = client.add_node_addr(addr).await {
                tracing::debug!(error = %e, "guardian seed_peer: add_node_addr failed");
                return false;
            }
            client.backend().note_explicit_peer(peer_id).await;
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "guardian seed_peer: malformed addr_json");
            false
        }
    }
}

/// Seeds known peers (GuardianDB-specific addresses) against the open `Handle`'s client; idempotent
/// and called every gossip round, so a new or re-addressed peer is picked up without a restart. It
/// cannot fix an already-diverged namespace on THIS node (the KV store opens once) but keeps
/// `known_peers` fresh for any re-open.
pub async fn seed_known_peers(guardian_addr_jsons: &[String]) {
    let Ok(h) = handle().await else { return };
    for addr_json in guardian_addr_jsons {
        seed_peer(&h.client, addr_json).await;
    }
}

/// Peers' GuardianDB-specific addresses to seed on the FIRST (only) KV-store open, the one window
/// in which `resolve_shared_ticket`'s automatic DocTicket exchange runs (the lowest EndpointId
/// creates the namespace and the rest import its ticket; miss the window and every node creates its
/// OWN namespace). Set once from main.rs before `init_background()` (a second call is a no-op); an
/// empty or stale snapshot only means this node creates its own namespace.
static BOOT_SEED_PEERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Records peers' current GuardianDB-specific addresses (`guardian_iroh_addr`, never `iroh_addr`)
/// for `init_handle`'s first KV-store open; call before `init_background()`.
pub fn set_boot_seed_peers(guardian_addr_jsons: Vec<String>) {
    let _ = BOOT_SEED_PEERS.set(guardian_addr_jsons);
}

/// This node's OWN GuardianDB-specific dialable address (serialized `iroh::EndpointAddr`), gossiped
/// as `NodeInfo.guardian_iroh_addr`; `None` until the client has finished binding (never blocks;
/// re-polled each gossip round).
pub async fn my_iroh_addr() -> Option<String> {
    let h = handle().await.ok()?;
    let endpoint_arc = h.client.backend().get_endpoint().await.ok()?;
    let endpoint_lock = endpoint_arc.read().await;
    let endpoint = endpoint_lock.as_ref()?;
    serde_json::to_string(&endpoint.addr()).ok()
}

/// Warm the GuardianDB connection at startup so it is live before the first
/// snapshot. Best-effort and non-blocking; failures are logged.
pub fn init_background() {
    tokio::spawn(async move {
        match handle().await {
            Ok(h) => tracing::info!(keys = h.kv.keys().len(), "GuardianDB online"),
            Err(e) => {
                tracing::warn!(error = %e, "GuardianDB init failed (snapshot kept on disk); will retry")
            }
        }
    });
    spawn_blob_stats_collector();
}

/// This node's name (set once at boot) — keys the per-node full-snapshot replica
/// used by the restore-on-rollback guard.
static NODE_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_node_name(name: &str) {
    let _ = NODE_NAME.set(name.to_string());
}

fn snapshot_key() -> Option<String> {
    NODE_NAME.get().map(|n| format!("node/{n}/snapshot"))
}

/// Digest of a prepared value's canonical bytes; an unchanged digest is a `no_op` write (never
/// PUT).
type SnapshotDigest = [u8; 32];

#[derive(Clone)]
struct PreparedValue {
    change_digest: SnapshotDigest,
    bytes: Arc<Vec<u8>>,
}

#[derive(Clone)]
struct KeyedPreparedValue {
    key: String,
    /// Bounded write-observability key class (`"full"` or a `SNAPSHOT_PART_FIELDS` name), never a
    /// per-tenant string, so `WRITE_COUNTERS` stays a fixed small set.
    class: &'static str,
    value: PreparedValue,
}

#[derive(Clone)]
struct DesiredReplication {
    generation: u64,
    namespaces: std::collections::BTreeMap<String, PreparedValue>,
    parts: Vec<KeyedPreparedValue>,
    full: Option<KeyedPreparedValue>,
}

#[derive(Default)]
struct CommittedReplication {
    namespaces: std::collections::BTreeMap<String, SnapshotDigest>,
}

impl DesiredReplication {
    fn committed_state(&self) -> CommittedReplication {
        CommittedReplication {
            namespaces: self
                .namespaces
                .iter()
                .map(|(key, value)| (key.clone(), value.change_digest))
                .collect(),
        }
    }
}

const REPLICATION_RETRY_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const REPLICATION_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);
const VOLATILE_ROOT_FIELDS: [&str; 4] = ["saved_ms", "metrics_rollup", "database_data", "cron"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplicationGenerationStatus {
    Queued,
    Writing,
    Superseded,
    Committed,
    Failed,
    TimedOut,
}

struct ReplicationLifecycle {
    admission_open: bool,
    next_generation: u64,
    final_generation: Option<u64>,
    statuses: std::collections::BTreeMap<u64, ReplicationGenerationStatus>,
    failures: std::collections::BTreeMap<u64, String>,
}

impl Default for ReplicationLifecycle {
    fn default() -> Self {
        Self {
            admission_open: true,
            next_generation: 0,
            final_generation: None,
            statuses: std::collections::BTreeMap::new(),
            failures: std::collections::BTreeMap::new(),
        }
    }
}

static REPLICATION_LIFECYCLE: OnceLock<std::sync::Mutex<ReplicationLifecycle>> = OnceLock::new();
static REPLICATION_NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
static REPLICATION_QUEUE: OnceLock<tokio::sync::watch::Sender<Arc<DesiredReplication>>> =
    OnceLock::new();
static REPLICATION_SHUTDOWN: OnceLock<tokio_util::sync::CancellationToken> = OnceLock::new();
static REPLICATION_WRITER: OnceLock<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
    OnceLock::new();

fn replication_lifecycle() -> &'static std::sync::Mutex<ReplicationLifecycle> {
    REPLICATION_LIFECYCLE.get_or_init(|| std::sync::Mutex::new(ReplicationLifecycle::default()))
}

fn replication_notify() -> &'static tokio::sync::Notify {
    REPLICATION_NOTIFY.get_or_init(tokio::sync::Notify::new)
}

fn set_generation_status(
    generation: u64,
    status: ReplicationGenerationStatus,
    error: Option<String>,
) {
    let mut lifecycle = replication_lifecycle()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if lifecycle.statuses.contains_key(&generation) {
        lifecycle.statuses.insert(generation, status);
        if let Some(error) = error {
            lifecycle.failures.insert(generation, error);
        }
    }
    drop(lifecycle);
    replication_notify().notify_waiters();
    record_generation_status(status);
}

/// Cumulative monotonic counts of every replication-generation status transition this process has
/// seen (six fixed fields; the generation-level half of `/v1/admin/guardian/write-stats`);
/// `queued_total`/`superseded_total` expose churn a `Committed`-only view would hide.
#[derive(Default, Clone, serde::Serialize)]
struct GenerationCounters {
    queued_total: u64,
    writing_total: u64,
    superseded_total: u64,
    committed_total: u64,
    failed_total: u64,
    timed_out_total: u64,
}

static GENERATION_COUNTERS: OnceLock<std::sync::Mutex<GenerationCounters>> = OnceLock::new();

fn generation_counters() -> &'static std::sync::Mutex<GenerationCounters> {
    GENERATION_COUNTERS.get_or_init(|| std::sync::Mutex::new(GenerationCounters::default()))
}

fn record_generation_status(status: ReplicationGenerationStatus) {
    let mut counters = generation_counters()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match status {
        ReplicationGenerationStatus::Queued => counters.queued_total += 1,
        ReplicationGenerationStatus::Writing => counters.writing_total += 1,
        ReplicationGenerationStatus::Superseded => counters.superseded_total += 1,
        ReplicationGenerationStatus::Committed => counters.committed_total += 1,
        ReplicationGenerationStatus::Failed => counters.failed_total += 1,
        ReplicationGenerationStatus::TimedOut => counters.timed_out_total += 1,
    }
}

/// Per-write-class outcome counters (the key-class half of write-stats): `attempted` once the
/// change digest is compared, `no_op` the subset matching the live blob (never PUT), `committed`
/// actually PUT, `failed` a PUT error. A stable `no_op` with near-zero committed bytes across
/// no-change writes shows the blob-regrowth fix holding.
#[derive(Default, Clone, serde::Serialize)]
struct ClassWriteCounters {
    attempted: u64,
    attempted_bytes: u64,
    no_op: u64,
    no_op_bytes: u64,
    committed: u64,
    committed_bytes: u64,
    failed: u64,
}

impl ClassWriteCounters {
    fn record_attempt(&mut self, bytes: usize) {
        self.attempted += 1;
        self.attempted_bytes += bytes as u64;
    }
    fn record_no_op(&mut self, bytes: usize) {
        self.no_op += 1;
        self.no_op_bytes += bytes as u64;
    }
    fn record_committed(&mut self, bytes: usize) {
        self.committed += 1;
        self.committed_bytes += bytes as u64;
    }
    fn record_failed(&mut self) {
        self.failed += 1;
    }
}

/// Fixed key-class set: `"namespaces"` (aggregate over every tenant document, not one entry per
/// tenant), `"full"` (this node's full-snapshot replica) and one `parts` entry per
/// `SNAPSHOT_PART_FIELDS` name, so the map keeps a fixed size for the process life.
#[derive(Default, Clone, serde::Serialize)]
struct WriteCounters {
    namespaces: ClassWriteCounters,
    parts: std::collections::BTreeMap<String, ClassWriteCounters>,
    full: ClassWriteCounters,
    /// Complete-generation markers (`snap-v2/node/<c>/gen/<ms>-<sha>`), one
    /// bounded class like `full` — never per-generation keys.
    generation: ClassWriteCounters,
    namespace_deletes: u64,
    part_reap_deletes: u64,
    /// Exact v2 retention deletions (generation markers + superseded part
    /// metadata keys) confirmed by this process.
    v2_retention_deletes: u64,
}

static WRITE_COUNTERS: OnceLock<std::sync::Mutex<WriteCounters>> = OnceLock::new();

fn write_counters() -> &'static std::sync::Mutex<WriteCounters> {
    WRITE_COUNTERS.get_or_init(|| std::sync::Mutex::new(WriteCounters::default()))
}

fn write_counters_snapshot() -> WriteCounters {
    write_counters()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn sha256(bytes: &[u8]) -> SnapshotDigest {
    <sha2::Sha256 as sha2::Digest>::digest(bytes).into()
}

fn path_matches(path: &[String], pattern: &[&str]) -> bool {
    path.len() == pattern.len()
        && path
            .iter()
            .zip(pattern)
            .all(|(actual, expected)| *expected == "*" || actual == expected)
}

fn is_set_array(path: &[String]) -> bool {
    const PATHS: &[&[&str]] = &[
        &["cron"],
        &["webhooks"],
        &["databases"],
        &["builds"],
        &["incidents"],
        &["apikeys"],
        &["api_keys"],
        &["integrations"],
        &["svcgraphs"],
        &["orgs"],
        &["users"],
        &["billing"],
        &["billing_invoices"],
        &["billing_meters"],
        &["billing_checkouts"],
        &["domains"],
        &["docs"],
        &["gitops"],
        &["workflow_defs"],
        &["projects"],
        &["teams"],
        &["team_tombstones"],
        &["projects", "*", "env"],
        &["projects", "*", "domains"],
        &["projects", "*", "regions"],
        &["databases", "*", "replicas"],
        &["webhooks", "*", "events"],
        &["incidents", "*", "affected"],
        &["push", "subs"],
        &["push", "sms"],
        &["svcgraphs", "*", "nodes"],
        &["svcgraphs", "*", "edges"],
        &["svcgraphs", "*", "env_keys"],
        &["svcgraphs", "*", "compose_services"],
        &["domains", "*", "records"],
        &["domains", "*", "nameservers"],
        &["enterprise", "reports"],
        &["enterprise", "ip_blocks", "*"],
        &["sandboxes", "sandboxes"],
        &["sandboxes", "commands"],
        &["sandboxes", "snapshots"],
        &["sandboxes", "mounts"],
    ];
    PATHS.iter().any(|pattern| path_matches(path, pattern))
}

fn sort_by_fields(values: &mut [serde_json::Value], fields: &[&str]) {
    values.sort_by_cached_key(|value| {
        let selected: Vec<_> = fields
            .iter()
            .map(|field| {
                value
                    .get(*field)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();
        serde_json::to_vec(&selected).unwrap_or_default()
    });
}

fn canonicalize_json(value: &mut serde_json::Value, path: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            let mut entries: Vec<_> = std::mem::take(fields).into_iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (key, value) in &mut entries {
                path.push(key.clone());
                canonicalize_json(value, path);
                path.pop();
            }
            fields.extend(entries);
        }
        serde_json::Value::Array(values) => {
            path.push("*".to_string());
            for value in values.iter_mut() {
                canonicalize_json(value, path);
            }
            path.pop();

            if path_matches(path, &["deployments"]) {
                sort_by_fields(values, &["created_at_ms", "id"]);
            } else if path_matches(path, &["billing_ledger"]) {
                sort_by_fields(values, &["ts_ms", "id"]);
            } else if is_set_array(path) {
                values.sort_by_cached_key(|value| serde_json::to_vec(value).unwrap_or_default());
            }
        }
        _ => {}
    }
}

fn canonical_json_bytes(mut value: serde_json::Value) -> anyhow::Result<Vec<u8>> {
    canonicalize_json(&mut value, &mut Vec::new());
    Ok(serde_json::to_vec(&value)?)
}

fn remove_siem_delivery_counters(value: &mut serde_json::Value) {
    let Some(siem) = value
        .as_object_mut()
        .and_then(|root| root.get_mut("enterprise"))
        .and_then(|enterprise| enterprise.as_object_mut())
        .and_then(|enterprise| enterprise.get_mut("siem"))
    else {
        return;
    };
    let remove = |config: &mut serde_json::Value| {
        if let Some(fields) = config.as_object_mut() {
            fields.remove("delivered");
            fields.remove("failed");
        }
    };
    match siem {
        serde_json::Value::Object(configs) => {
            for config in configs.values_mut() {
                remove(config);
            }
        }
        serde_json::Value::Array(configs) => {
            for config in configs {
                remove(config);
            }
        }
        _ => {}
    }
}

const SNAPSHOT_PART_FIELDS: [&str; 5] = [
    "deployments",
    "database_data",
    "metrics_rollup",
    "builds",
    "sandboxes",
];
const SNAPSHOT_PARTS_FIELD: &str = "_guardian_parts";

struct CanonicalSnapshot {
    base: Vec<u8>,
    /// `(key_class, canonical_bytes)` per part key; the class is one of the fixed
    /// `SNAPSHOT_PART_FIELDS` names so the write chokepoint can attribute counters without
    /// re-parsing the key.
    parts: std::collections::BTreeMap<String, (&'static str, Vec<u8>)>,
}

/// Canonical node backup split into one base plus independently addressed large or frequently
/// changing parts. The base carries a digest manifest and is written LAST, so it is the commit
/// marker for an exact set of part bytes (a changed field costs one part, not a new multi-megabyte
/// blob). Only write-attempt metadata and non-authoritative SIEM delivery counters are omitted;
/// parts and base use the same canonical bytes for digest and payload.
fn shared_snapshot_canonical(
    snap: &PlatformSnapshot,
    snapshot_key: &str,
) -> anyhow::Result<CanonicalSnapshot> {
    let mut value = serde_json::to_value(snap)?;
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("PlatformSnapshot did not serialize as a JSON object"))?;
    for field in VOLATILE_ROOT_FIELDS {
        root.remove(field);
    }
    remove_siem_delivery_counters(&mut value);

    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("PlatformSnapshot did not serialize as a JSON object"))?;
    let mut parts = std::collections::BTreeMap::new();
    let mut manifest = serde_json::Map::new();
    let part_prefix = snapshot_v3_prefix(snapshot_key)?;
    for field in SNAPSHOT_PART_FIELDS {
        let part_value = root.remove(field).unwrap_or(serde_json::Value::Null);
        let mut wrapper = serde_json::Map::new();
        wrapper.insert(field.to_string(), part_value);
        let bytes = canonical_json_bytes(serde_json::Value::Object(wrapper))?;
        let digest = hex::encode(sha256(&bytes));
        let part_key = format!("{part_prefix}/{field}/{digest}");
        manifest.insert(
            field.to_string(),
            serde_json::json!({
                "key": part_key,
                "sha256": digest,
            }),
        );
        parts.insert(part_key, (field, bytes));
    }
    root.insert(
        SNAPSHOT_PARTS_FIELD.to_string(),
        serde_json::Value::Object(manifest),
    );

    Ok(CanonicalSnapshot {
        base: canonical_json_bytes(value)?,
        parts,
    })
}

fn prepare_replication(
    snap: &PlatformSnapshot,
    generation: u64,
) -> anyhow::Result<Arc<DesiredReplication>> {
    let mut namespaces = std::collections::BTreeMap::new();
    for (namespace, state) in crate::persist::guardian_v2_namespaced(snap)? {
        let key = guardian_v2_namespace_key(&namespace)?;
        let bytes = guardian_v2_namespace_bytes(state)?;
        namespaces.insert(
            key,
            PreparedValue {
                change_digest: sha256(&bytes),
                bytes: Arc::new(bytes),
            },
        );
    }

    let (parts, full) = match NODE_NAME.get() {
        Some(component) => {
            let (parts, head) = guardian_v2_prepare_snapshot(snap, component)?;
            (parts, Some(head))
        }
        None => (Vec::new(), None),
    };

    Ok(Arc::new(DesiredReplication {
        generation,
        namespaces,
        parts,
        full,
    }))
}

fn publish_replication(
    sender: &tokio::sync::watch::Sender<Arc<DesiredReplication>>,
    desired: Arc<DesiredReplication>,
) {
    sender.send_if_modified(|queued| {
        if queued.generation >= desired.generation {
            return false;
        }
        *queued = desired.clone();
        true
    });
}

fn is_namespace_state_key(key: &str) -> bool {
    key.strip_prefix("ns/")
        .and_then(|rest| rest.strip_suffix("/state"))
        .is_some_and(|namespace| !namespace.is_empty())
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

const GUARDIAN_V2_SNAPSHOT_MAGIC: &str = "hive-guardian-snapshot";
const GUARDIAN_V2_NAMESPACE_MAGIC: &str = "hive-guardian-namespace";
const GUARDIAN_V2_VERSION: u64 = 2;
const GUARDIAN_V2_HEAD_MAX_BYTES: usize = 4 * 1024;
const GUARDIAN_V2_PART_MAX_BYTES: usize = 512 * 1024 * 1024;
const GUARDIAN_V2_AGGREGATE_MAX_BYTES: usize = 1024 * 1024 * 1024;
const GUARDIAN_V2_PART_ENVELOPE_MAGIC: &[u8] = b"HIVE-GUARDIAN-V2-PART\0";
const GUARDIAN_V2_PART_ENVELOPE_VERSION: u8 = 1;
const GUARDIAN_V2_PART_ZSTD_LEVEL: i32 = 3;
const GUARDIAN_V2_COMPONENT_MAX_BYTES: usize = 255;
const GUARDIAN_V2_NODE_PREFIX: &str = "snap-v2/node/";
const GUARDIAN_V2_NAMESPACE_PREFIX: &str = "ns-v2/";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum GuardianV2PartKind {
    State,
    Deployments,
    DatabaseData,
    MetricsRollup,
    Builds,
    Sandboxes,
}

impl GuardianV2PartKind {
    const ALL: [Self; 6] = [
        Self::State,
        Self::Deployments,
        Self::DatabaseData,
        Self::MetricsRollup,
        Self::Builds,
        Self::Sandboxes,
    ];

    fn field(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Deployments => "deployments",
            Self::DatabaseData => "database_data",
            Self::MetricsRollup => "metrics_rollup",
            Self::Builds => "builds",
            Self::Sandboxes => "sandboxes",
        }
    }
}

#[derive(Clone, Debug)]
struct GuardianV2Reference {
    sha256: String,
    bytes: usize,
}

#[derive(Clone, Debug)]
struct GuardianV2Head {
    references: std::collections::BTreeMap<String, GuardianV2Reference>,
}

struct GuardianV2StrictValue(serde_json::Value);

impl<'de> serde::Deserialize<'de> for GuardianV2StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(GuardianV2StrictValueVisitor)
    }
}

struct GuardianV2StrictValueVisitor;

impl<'de> serde::de::Visitor<'de> for GuardianV2StrictValueVisitor {
    type Value = GuardianV2StrictValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object fields")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::Number(
            value.into(),
        )))
    }

    fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let value = i64::try_from(value)
            .map_err(|_| E::custom("JSON integer is outside the signed 64-bit range"))?;
        self.visit_i64(value)
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::Number(
            value.into(),
        )))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let value = u64::try_from(value)
            .map_err(|_| E::custom("JSON integer is outside the unsigned 64-bit range"))?;
        self.visit_u64(value)
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("JSON float is not finite"))?;
        Ok(GuardianV2StrictValue(serde_json::Value::Number(number)))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::String(
            value.to_string(),
        )))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(GuardianV2StrictValue(serde_json::Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <GuardianV2StrictValue as serde::Deserialize>::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(GuardianV2StrictValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(GuardianV2StrictValue(serde_json::Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut fields = serde_json::Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if fields.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {key:?}"
                )));
            }
            let GuardianV2StrictValue(value) = object.next_value()?;
            fields.insert(key, value);
        }
        Ok(GuardianV2StrictValue(serde_json::Value::Object(fields)))
    }
}

fn guardian_v2_canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            let mut entries: Vec<_> = std::mem::take(fields).into_iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (_, value) in &mut entries {
                guardian_v2_canonicalize_json(value);
            }
            fields.extend(entries);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                guardian_v2_canonicalize_json(value);
            }
        }
        _ => {}
    }
}

fn guardian_v2_canonical_json_bytes(mut value: serde_json::Value) -> anyhow::Result<Vec<u8>> {
    guardian_v2_canonicalize_json(&mut value);
    Ok(serde_json::to_vec(&value)?)
}

fn guardian_v2_parse_canonical_json(
    bytes: &[u8],
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<serde_json::Value> {
    if bytes.len() > max_bytes {
        anyhow::bail!(
            "Guardian v2 {label} is {} bytes; maximum is {max_bytes}",
            bytes.len()
        );
    }
    let GuardianV2StrictValue(value) = serde_json::from_slice(bytes)
        .map_err(|error| anyhow::anyhow!("Guardian v2 {label} JSON: {error}"))?;
    let canonical = guardian_v2_canonical_json_bytes(value.clone())?;
    if canonical != bytes {
        anyhow::bail!("Guardian v2 {label} is not byte-for-byte canonical JSON");
    }
    Ok(value)
}

fn guardian_v2_component_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn guardian_v2_encode_component(component: &str) -> anyhow::Result<String> {
    let bytes = component.as_bytes();
    if bytes.is_empty() {
        anyhow::bail!("Guardian v2 component decodes to an empty value");
    }
    if bytes.len() > GUARDIAN_V2_COMPONENT_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 component is {} decoded bytes; maximum is {}",
            bytes.len(),
            GUARDIAN_V2_COMPONENT_MAX_BYTES
        );
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if guardian_v2_component_safe(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    Ok(encoded)
}

fn guardian_v2_upper_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn guardian_v2_decode_component(encoded: &str) -> anyhow::Result<String> {
    if encoded.is_empty() {
        anyhow::bail!("Guardian v2 component is empty");
    }
    let source = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(source.len());
    let mut index = 0usize;
    while index < source.len() {
        let byte = source[index];
        if byte == b'%' {
            if index + 2 >= source.len() {
                anyhow::bail!("Guardian v2 component has a truncated percent escape");
            }
            let high = guardian_v2_upper_hex(source[index + 1]).ok_or_else(|| {
                anyhow::anyhow!("Guardian v2 component escape is not uppercase %HH")
            })?;
            let low = guardian_v2_upper_hex(source[index + 2]).ok_or_else(|| {
                anyhow::anyhow!("Guardian v2 component escape is not uppercase %HH")
            })?;
            let value = (high << 4) | low;
            if guardian_v2_component_safe(value) {
                anyhow::bail!("Guardian v2 component escapes a byte in the literal safe set");
            }
            decoded.push(value);
            index += 3;
            continue;
        }
        if !guardian_v2_component_safe(byte) {
            anyhow::bail!("Guardian v2 component contains an unsafe literal byte");
        }
        decoded.push(byte);
        index += 1;
    }

    if decoded.is_empty() {
        anyhow::bail!("Guardian v2 component decodes to an empty value");
    }
    if decoded.len() > GUARDIAN_V2_COMPONENT_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 component is {} decoded bytes; maximum is {}",
            decoded.len(),
            GUARDIAN_V2_COMPONENT_MAX_BYTES
        );
    }
    let decoded = String::from_utf8(decoded)
        .map_err(|_| anyhow::anyhow!("Guardian v2 component is not valid UTF-8"))?;
    if guardian_v2_encode_component(&decoded)? != encoded {
        anyhow::bail!("Guardian v2 component has a noncanonical alias encoding");
    }
    Ok(decoded)
}

fn guardian_v2_node_prefix(component: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{GUARDIAN_V2_NODE_PREFIX}{}",
        guardian_v2_encode_component(component)?
    ))
}

fn guardian_v2_head_key(component: &str) -> anyhow::Result<String> {
    Ok(format!("{}/head", guardian_v2_node_prefix(component)?))
}

fn guardian_v2_part_key(
    encoded_component: &str,
    kind: GuardianV2PartKind,
    digest: &str,
) -> anyhow::Result<String> {
    guardian_v2_decode_component(encoded_component)?;
    if !is_lower_hex_digest(digest) {
        anyhow::bail!("Guardian v2 part digest is not 64 lowercase hexadecimal characters");
    }
    Ok(format!(
        "{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/{}/{digest}",
        kind.field()
    ))
}

fn guardian_v2_namespace_key(namespace: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{GUARDIAN_V2_NAMESPACE_PREFIX}{}/state",
        guardian_v2_encode_component(namespace)?
    ))
}

fn guardian_v2_encoded_component_from_head_key(key: &str) -> anyhow::Result<String> {
    let encoded = key
        .strip_prefix(GUARDIAN_V2_NODE_PREFIX)
        .and_then(|rest| rest.strip_suffix("/head"))
        .filter(|component| !component.is_empty() && !component.contains('/'))
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 head key has an invalid shape"))?;
    guardian_v2_decode_component(encoded)?;
    Ok(encoded.to_string())
}

/// Complete-generation marker keys: `snap-v2/node/<enc>/gen/<%016x ms>-<sha>`. The canonical head
/// is overwritten in place, so a prior generation needs its OWN collision-safe root: the marker's
/// VALUE is the exact head bytes (the same BLAKE3 blob the head roots, so zero extra bytes),
/// keeping the prior head and its parts reachable until retention retires the marker. The key
/// embeds the generation's `saved_ms` (zero-padded hex, so ascending lexical order IS ascending
/// age) and the head's SHA-256 (same publish, same key; a different head in one millisecond, a
/// different key). A marker is written only after the full generation verified, so it never names
/// an unverified head.
const GUARDIAN_V2_GEN_SEGMENT: &str = "gen";

fn guardian_v2_generation_key(
    encoded_component: &str,
    saved_ms: u64,
    head_sha256: &str,
) -> anyhow::Result<String> {
    guardian_v2_decode_component(encoded_component)?;
    if !is_lower_hex_digest(head_sha256) {
        anyhow::bail!("Guardian v2 generation head SHA-256 is not 64 lowercase hex characters");
    }
    Ok(format!(
        "{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/{GUARDIAN_V2_GEN_SEGMENT}/{saved_ms:016x}-{head_sha256}"
    ))
}

/// `Some((saved_ms, head_sha256))` for a well-formed generation marker under this component;
/// malformed keys under the gen prefix return `None` and are never deleted (unknown shapes are
/// never retired).
fn guardian_v2_parse_generation_key(key: &str, encoded_component: &str) -> Option<(u64, String)> {
    let prefix = format!("{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/{GUARDIAN_V2_GEN_SEGMENT}/");
    let suffix = key.strip_prefix(&prefix)?;
    let (ms_hex, sha) = suffix.split_once('-')?;
    if ms_hex.len() != 16 || !ms_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let saved_ms = u64::from_str_radix(ms_hex, 16).ok()?;
    is_lower_hex_digest(sha).then(|| (saved_ms, sha.to_string()))
}

/// `Some((kind, digest))` when `key` is a v2 part key under this component.
fn guardian_v2_part_key_fields(
    key: &str,
    encoded_component: &str,
) -> Option<(GuardianV2PartKind, String)> {
    let prefix = format!("{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/");
    let rest = key.strip_prefix(&prefix)?;
    let (field, digest) = rest.split_once('/')?;
    if digest.contains('/') || !is_lower_hex_digest(digest) {
        return None;
    }
    GuardianV2PartKind::ALL
        .into_iter()
        .find(|kind| kind.field() == field)
        .map(|kind| (kind, digest.to_string()))
}

fn guardian_v2_remove_cron_telemetry(value: &mut serde_json::Value) -> anyhow::Result<()> {
    let jobs = value
        .as_object_mut()
        .and_then(|root| root.get_mut("cron"))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 snapshot cron is not an array"))?;
    for (index, job) in jobs.iter_mut().enumerate() {
        let fields = job.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("Guardian v2 snapshot cron[{index}] is not an object")
        })?;
        fields.remove("last_run_ms");
        fields.remove("next_run_ms");
        fields.remove("runs");
    }
    Ok(())
}

fn guardian_v2_snapshot_part_values(
    snapshot: &PlatformSnapshot,
) -> anyhow::Result<std::collections::BTreeMap<String, serde_json::Value>> {
    let mut state = serde_json::to_value(snapshot)?;
    let root = state
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("PlatformSnapshot did not serialize as a JSON object"))?;
    root.remove("saved_ms")
        .ok_or_else(|| anyhow::anyhow!("PlatformSnapshot omitted saved_ms before v2 transform"))?;

    let mut extracted = std::collections::BTreeMap::new();
    for kind in GuardianV2PartKind::ALL.into_iter().skip(1) {
        let field = kind.field();
        let value = root.remove(field).ok_or_else(|| {
            anyhow::anyhow!("PlatformSnapshot omitted required v2 root {field:?}")
        })?;
        extracted.insert(field.to_string(), value);
    }
    remove_siem_delivery_counters(&mut state);
    guardian_v2_remove_cron_telemetry(&mut state)?;

    let mut parts = std::collections::BTreeMap::new();
    parts.insert(GuardianV2PartKind::State.field().to_string(), state);
    parts.extend(extracted);
    Ok(parts)
}

fn guardian_v2_wrapper_bytes(
    kind: GuardianV2PartKind,
    value: serde_json::Value,
) -> anyhow::Result<Vec<u8>> {
    let mut wrapper = serde_json::Map::new();
    wrapper.insert(kind.field().to_string(), value);
    guardian_v2_canonical_json_bytes(serde_json::Value::Object(wrapper))
}

fn guardian_v2_encode_part(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    if raw.len() > GUARDIAN_V2_PART_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 raw part is {} bytes; maximum is {}",
            raw.len(),
            GUARDIAN_V2_PART_MAX_BYTES
        );
    }
    let compressed = zstd::bulk::compress(raw, GUARDIAN_V2_PART_ZSTD_LEVEL)
        .map_err(|error| anyhow::anyhow!("Guardian v2 part zstd encode: {error}"))?;
    let raw_len = u64::try_from(raw.len())
        .map_err(|_| anyhow::anyhow!("Guardian v2 raw part length does not fit u64"))?;
    let capacity = GUARDIAN_V2_PART_ENVELOPE_MAGIC
        .len()
        .checked_add(1 + std::mem::size_of::<u64>())
        .and_then(|header| header.checked_add(compressed.len()))
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 compressed part length overflow"))?;
    if capacity > GUARDIAN_V2_PART_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 compressed part is {capacity} bytes; maximum is {}",
            GUARDIAN_V2_PART_MAX_BYTES
        );
    }
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(GUARDIAN_V2_PART_ENVELOPE_MAGIC);
    encoded.push(GUARDIAN_V2_PART_ENVELOPE_VERSION);
    encoded.extend_from_slice(&raw_len.to_le_bytes());
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

fn guardian_v2_decode_part(bytes: &[u8]) -> anyhow::Result<std::borrow::Cow<'_, [u8]>> {
    if !bytes.starts_with(GUARDIAN_V2_PART_ENVELOPE_MAGIC) {
        return Ok(std::borrow::Cow::Borrowed(bytes));
    }
    let version_at = GUARDIAN_V2_PART_ENVELOPE_MAGIC.len();
    let length_at = version_at + 1;
    let payload_at = length_at + std::mem::size_of::<u64>();
    if bytes.len() < payload_at {
        anyhow::bail!("Guardian v2 compressed part header is truncated");
    }
    if bytes[version_at] != GUARDIAN_V2_PART_ENVELOPE_VERSION {
        anyhow::bail!(
            "Guardian v2 compressed part version {} is unsupported",
            bytes[version_at]
        );
    }
    let raw_len = u64::from_le_bytes(
        bytes[length_at..payload_at]
            .try_into()
            .map_err(|_| anyhow::anyhow!("Guardian v2 compressed part length is malformed"))?,
    );
    let raw_len = usize::try_from(raw_len).map_err(|_| {
        anyhow::anyhow!("Guardian v2 compressed part length does not fit this host")
    })?;
    if raw_len > GUARDIAN_V2_PART_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 compressed part declares {raw_len} raw bytes; maximum is {}",
            GUARDIAN_V2_PART_MAX_BYTES
        );
    }
    let raw = zstd::bulk::decompress(&bytes[payload_at..], raw_len)
        .map_err(|error| anyhow::anyhow!("Guardian v2 part zstd decode: {error}"))?;
    if raw.len() != raw_len {
        anyhow::bail!(
            "Guardian v2 compressed part decoded to {} bytes; header declares {raw_len}",
            raw.len()
        );
    }
    Ok(std::borrow::Cow::Owned(raw))
}

fn guardian_v2_parse_part<'a>(
    kind: GuardianV2PartKind,
    bytes: &'a [u8],
    reference: &GuardianV2Reference,
) -> anyhow::Result<(serde_json::Value, std::borrow::Cow<'a, [u8]>)> {
    if reference.bytes > GUARDIAN_V2_PART_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 {} reference is {} bytes; maximum is {}",
            kind.field(),
            reference.bytes,
            GUARDIAN_V2_PART_MAX_BYTES
        );
    }
    if bytes.len() != reference.bytes {
        anyhow::bail!(
            "Guardian v2 {} wrapper length is {}; head declares {}",
            kind.field(),
            bytes.len(),
            reference.bytes
        );
    }
    if hex::encode(sha256(bytes)) != reference.sha256 {
        anyhow::bail!("Guardian v2 {} wrapper SHA-256 mismatch", kind.field());
    }
    let raw = guardian_v2_decode_part(bytes)?;
    let value = guardian_v2_parse_canonical_json(
        raw.as_ref(),
        GUARDIAN_V2_PART_MAX_BYTES,
        &format!("{} wrapper", kind.field()),
    )?;
    let mut fields = value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 {} wrapper is not an object", kind.field()))?;
    if fields.len() != 1 {
        anyhow::bail!(
            "Guardian v2 {} wrapper does not have exactly one field",
            kind.field()
        );
    }
    let value = fields
        .remove(kind.field())
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 {} wrapper field is absent", kind.field()))?;
    Ok((value, raw))
}

fn guardian_v2_head_bytes(
    references: &std::collections::BTreeMap<String, GuardianV2Reference>,
) -> anyhow::Result<Vec<u8>> {
    if references.len() != GuardianV2PartKind::ALL.len() {
        anyhow::bail!("Guardian v2 head does not have exactly six references");
    }
    let mut fields = serde_json::Map::new();
    fields.insert(
        "magic".to_string(),
        serde_json::Value::String(GUARDIAN_V2_SNAPSHOT_MAGIC.to_string()),
    );
    fields.insert(
        "version".to_string(),
        serde_json::Value::Number(GUARDIAN_V2_VERSION.into()),
    );
    for kind in GuardianV2PartKind::ALL {
        let reference = references.get(kind.field()).ok_or_else(|| {
            anyhow::anyhow!("Guardian v2 head reference {} is absent", kind.field())
        })?;
        fields.insert(
            kind.field().to_string(),
            serde_json::json!({
                "sha256": reference.sha256,
                "bytes": reference.bytes as u64,
            }),
        );
    }
    let bytes = guardian_v2_canonical_json_bytes(serde_json::Value::Object(fields))?;
    if bytes.len() > GUARDIAN_V2_HEAD_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 head is {} bytes; maximum is {}",
            bytes.len(),
            GUARDIAN_V2_HEAD_MAX_BYTES
        );
    }
    guardian_v2_parse_head(&bytes)?;
    Ok(bytes)
}

fn guardian_v2_parse_head(bytes: &[u8]) -> anyhow::Result<GuardianV2Head> {
    let value =
        guardian_v2_parse_canonical_json(bytes, GUARDIAN_V2_HEAD_MAX_BYTES, "snapshot head")?;
    let mut fields = value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 head is not an object"))?;
    if fields.len() != GuardianV2PartKind::ALL.len() + 2 {
        anyhow::bail!("Guardian v2 head does not have exactly six references");
    }
    if fields
        .remove("magic")
        .and_then(|value| value.as_str().map(str::to_string))
        != Some(GUARDIAN_V2_SNAPSHOT_MAGIC.to_string())
    {
        anyhow::bail!("Guardian v2 head magic is invalid");
    }
    if fields.remove("version").and_then(|value| value.as_u64()) != Some(GUARDIAN_V2_VERSION) {
        anyhow::bail!("Guardian v2 head version is invalid");
    }

    let mut aggregate = 0usize;
    let mut references = std::collections::BTreeMap::new();
    for kind in GuardianV2PartKind::ALL {
        let mut reference = fields
            .remove(kind.field())
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Guardian v2 head reference {} is absent or invalid",
                    kind.field()
                )
            })?;
        if reference.len() != 2 {
            anyhow::bail!(
                "Guardian v2 head reference {} does not have exactly sha256 and bytes",
                kind.field()
            );
        }
        let sha256 = reference
            .remove("sha256")
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|digest| is_lower_hex_digest(digest))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Guardian v2 head reference {} has an invalid SHA-256",
                    kind.field()
                )
            })?;
        let bytes_u64 = reference
            .remove("bytes")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Guardian v2 head reference {} has an invalid byte length",
                    kind.field()
                )
            })?;
        if !reference.is_empty() {
            anyhow::bail!(
                "Guardian v2 head reference {} contains unknown fields",
                kind.field()
            );
        }
        let bytes = usize::try_from(bytes_u64).map_err(|_| {
            anyhow::anyhow!(
                "Guardian v2 head reference {} byte length does not fit this host",
                kind.field()
            )
        })?;
        if bytes > GUARDIAN_V2_PART_MAX_BYTES {
            anyhow::bail!(
                "Guardian v2 head reference {} is {} bytes; maximum is {}",
                kind.field(),
                bytes,
                GUARDIAN_V2_PART_MAX_BYTES
            );
        }
        aggregate = aggregate
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 aggregate byte length overflow"))?;
        if aggregate > GUARDIAN_V2_AGGREGATE_MAX_BYTES {
            anyhow::bail!(
                "Guardian v2 aggregate is {aggregate} bytes; maximum is {}",
                GUARDIAN_V2_AGGREGATE_MAX_BYTES
            );
        }
        references.insert(
            kind.field().to_string(),
            GuardianV2Reference { sha256, bytes },
        );
    }
    if !fields.is_empty() {
        anyhow::bail!("Guardian v2 head contains unknown fields");
    }
    Ok(GuardianV2Head { references })
}

fn guardian_v2_reconstruct(
    part_values: &std::collections::BTreeMap<String, serde_json::Value>,
    wrapper_bytes: &std::collections::BTreeMap<String, Vec<u8>>,
    saved_ms: u64,
) -> anyhow::Result<PlatformSnapshot> {
    if part_values.len() != GuardianV2PartKind::ALL.len()
        || wrapper_bytes.len() != GuardianV2PartKind::ALL.len()
    {
        anyhow::bail!("Guardian v2 reconstruction does not have exactly six parts");
    }
    let aggregate = wrapper_bytes.values().try_fold(0usize, |total, bytes| {
        total
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 raw aggregate byte length overflow"))
    })?;
    if aggregate > GUARDIAN_V2_AGGREGATE_MAX_BYTES {
        anyhow::bail!(
            "Guardian v2 raw aggregate is {aggregate} bytes; maximum is {}",
            GUARDIAN_V2_AGGREGATE_MAX_BYTES
        );
    }
    let mut root = part_values
        .get(GuardianV2PartKind::State.field())
        .and_then(serde_json::Value::as_object)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 state value is not an object"))?;
    if root.contains_key("saved_ms") {
        anyhow::bail!("Guardian v2 state illegally contains saved_ms");
    }
    for kind in GuardianV2PartKind::ALL.into_iter().skip(1) {
        if root.contains_key(kind.field()) {
            anyhow::bail!(
                "Guardian v2 state illegally contains extracted root {}",
                kind.field()
            );
        }
        let value = part_values.get(kind.field()).cloned().ok_or_else(|| {
            anyhow::anyhow!("Guardian v2 reconstruction is missing {}", kind.field())
        })?;
        root.insert(kind.field().to_string(), value);
    }
    root.insert(
        "saved_ms".to_string(),
        serde_json::Value::Number(saved_ms.into()),
    );
    let snapshot: PlatformSnapshot = serde_json::from_value(serde_json::Value::Object(root))
        .map_err(|error| anyhow::anyhow!("Guardian v2 reconstruction: {error}"))?;

    let regenerated = guardian_v2_snapshot_part_values(&snapshot)?;
    for kind in GuardianV2PartKind::ALL {
        let expected = wrapper_bytes.get(kind.field()).ok_or_else(|| {
            anyhow::anyhow!(
                "Guardian v2 reconstruction wrapper {} is absent",
                kind.field()
            )
        })?;
        let actual = guardian_v2_wrapper_bytes(
            kind,
            regenerated.get(kind.field()).cloned().ok_or_else(|| {
                anyhow::anyhow!("Guardian v2 regenerated part {} is absent", kind.field())
            })?,
        )?;
        if &actual != expected {
            anyhow::bail!(
                "Guardian v2 reconstruct-and-transform roundtrip changed {}",
                kind.field()
            );
        }
    }
    Ok(snapshot)
}

fn guardian_v2_validate_namespace_state(state: &serde_json::Value) -> anyhow::Result<()> {
    let fields = state
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace state is not an object"))?;
    let Some(projects) = fields.get("projects") else {
        return Ok(());
    };
    let projects = projects
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace projects is not an array"))?;
    let mut previous: Option<String> = None;
    for project in projects {
        let identity = project
            .as_object()
            .and_then(|fields| fields.get("project"))
            .and_then(serde_json::Value::as_str)
            .filter(|identity| !identity.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace project identity is invalid"))?;
        if previous.as_deref().is_some_and(|prior| prior >= identity) {
            anyhow::bail!(
                "Guardian v2 namespace project identities are not unique strict lexical order"
            );
        }
        previous = Some(identity.to_string());
    }
    Ok(())
}

fn guardian_v2_namespace_bytes(state: serde_json::Value) -> anyhow::Result<Vec<u8>> {
    guardian_v2_validate_namespace_state(&state)?;
    let mut fields = serde_json::Map::new();
    fields.insert(
        "magic".to_string(),
        serde_json::Value::String(GUARDIAN_V2_NAMESPACE_MAGIC.to_string()),
    );
    fields.insert(
        "version".to_string(),
        serde_json::Value::Number(GUARDIAN_V2_VERSION.into()),
    );
    fields.insert("state".to_string(), state);
    let bytes = guardian_v2_canonical_json_bytes(serde_json::Value::Object(fields))?;
    guardian_v2_parse_namespace(&bytes)?;
    Ok(bytes)
}

fn guardian_v2_parse_namespace(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    let value = guardian_v2_parse_canonical_json(bytes, usize::MAX, "namespace envelope")?;
    let mut fields = value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace envelope is not an object"))?;
    if fields.len() != 3 {
        anyhow::bail!("Guardian v2 namespace envelope does not have exactly three fields");
    }
    if fields
        .remove("magic")
        .and_then(|value| value.as_str().map(str::to_string))
        != Some(GUARDIAN_V2_NAMESPACE_MAGIC.to_string())
    {
        anyhow::bail!("Guardian v2 namespace magic is invalid");
    }
    if fields.remove("version").and_then(|value| value.as_u64()) != Some(GUARDIAN_V2_VERSION) {
        anyhow::bail!("Guardian v2 namespace version is invalid");
    }
    let state = fields
        .remove("state")
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace state is absent"))?;
    if !fields.is_empty() {
        anyhow::bail!("Guardian v2 namespace envelope contains unknown fields");
    }
    guardian_v2_validate_namespace_state(&state)?;
    Ok(state)
}

fn guardian_v2_prepare_snapshot(
    snapshot: &PlatformSnapshot,
    component: &str,
) -> anyhow::Result<(Vec<KeyedPreparedValue>, KeyedPreparedValue)> {
    let encoded_component = guardian_v2_encode_component(component)?;
    let values = guardian_v2_snapshot_part_values(snapshot)?;
    let mut wrapper_bytes = std::collections::BTreeMap::new();
    let mut references = std::collections::BTreeMap::new();
    let mut parts = Vec::with_capacity(GuardianV2PartKind::ALL.len());
    let mut aggregate = 0usize;

    for kind in GuardianV2PartKind::ALL {
        let raw = guardian_v2_wrapper_bytes(
            kind,
            values.get(kind.field()).cloned().ok_or_else(|| {
                anyhow::anyhow!("Guardian v2 prepared part {} is absent", kind.field())
            })?,
        )?;
        if raw.len() > GUARDIAN_V2_PART_MAX_BYTES {
            anyhow::bail!(
                "Guardian v2 {} wrapper is {} bytes; maximum is {}",
                kind.field(),
                raw.len(),
                GUARDIAN_V2_PART_MAX_BYTES
            );
        }
        aggregate = aggregate
            .checked_add(raw.len())
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 aggregate byte length overflow"))?;
        if aggregate > GUARDIAN_V2_AGGREGATE_MAX_BYTES {
            anyhow::bail!(
                "Guardian v2 aggregate is {aggregate} bytes; maximum is {}",
                GUARDIAN_V2_AGGREGATE_MAX_BYTES
            );
        }
        let bytes = guardian_v2_encode_part(&raw)?;
        let digest = hex::encode(sha256(&bytes));
        let key = guardian_v2_part_key(&encoded_component, kind, &digest)?;
        references.insert(
            kind.field().to_string(),
            GuardianV2Reference {
                sha256: digest,
                bytes: bytes.len(),
            },
        );
        wrapper_bytes.insert(kind.field().to_string(), raw);
        parts.push(KeyedPreparedValue {
            key,
            class: kind.field(),
            value: PreparedValue {
                change_digest: sha256(&bytes),
                bytes: Arc::new(bytes),
            },
        });
    }

    guardian_v2_reconstruct(&values, &wrapper_bytes, 0)?;
    let head_bytes = guardian_v2_head_bytes(&references)?;
    let head = KeyedPreparedValue {
        key: format!("{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/head"),
        class: "full",
        value: PreparedValue {
            change_digest: sha256(&head_bytes),
            bytes: Arc::new(head_bytes),
        },
    };
    Ok((parts, head))
}

/// The part-key prefix for a node's base key: a SIBLING (`.../parts-v3`), never a descendant of the
/// base key, because iroh-docs PUT and delete have prefix semantics (committing the base would
/// delete its parts).
fn snapshot_v3_prefix(base_key: &str) -> anyhow::Result<String> {
    let node_prefix = base_key
        .strip_suffix("/snapshot")
        .filter(|prefix| !prefix.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Guardian snapshot base key has invalid shape"))?;
    Ok(format!("{node_prefix}/parts-v3"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotPartFormat {
    Fixed,
    AddressedV1,
    AddressedV2,
    AddressedV3,
}

#[derive(Clone, Copy, Debug)]
struct SnapshotPartKey<'a> {
    field: &'static str,
    digest: Option<&'a str>,
    format: SnapshotPartFormat,
}

fn snapshot_part_key_shape<'a>(base_key: &str, key: &'a str) -> Option<SnapshotPartKey<'a>> {
    for field in SNAPSHOT_PART_FIELDS {
        if key == format!("{base_key}-part/{field}") {
            return Some(SnapshotPartKey {
                field,
                digest: None,
                format: SnapshotPartFormat::Fixed,
            });
        }
        let prior = format!("{base_key}-part/{field}/");
        if let Some(digest) = key
            .strip_prefix(&prior)
            .filter(|value| is_lower_hex_digest(value))
        {
            return Some(SnapshotPartKey {
                field,
                digest: Some(digest),
                format: SnapshotPartFormat::AddressedV1,
            });
        }
        let current = format!("{base_key}-part-v2/{field}/");
        if let Some(digest) = key
            .strip_prefix(&current)
            .filter(|value| is_lower_hex_digest(value))
        {
            return Some(SnapshotPartKey {
                field,
                digest: Some(digest),
                format: SnapshotPartFormat::AddressedV2,
            });
        }
        let non_prefix = format!("/{field}/");
        if let Some(digest) = key
            .strip_prefix(&snapshot_v3_prefix(base_key).ok()?)
            .and_then(|suffix| suffix.strip_prefix(&non_prefix))
            .filter(|value| is_lower_hex_digest(value))
        {
            return Some(SnapshotPartKey {
                field,
                digest: Some(digest),
                format: SnapshotPartFormat::AddressedV3,
            });
        }
    }
    None
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SnapshotKeyHead {
    timestamp: u64,
    hash: String,
}

fn latest_snapshot_head(
    heads: &[guardian_db::traits::EntryHead],
    key: &str,
) -> Option<SnapshotKeyHead> {
    heads
        .iter()
        .filter(|head| head.key == key)
        .max_by_key(|head| head.timestamp)
        .map(|head| SnapshotKeyHead {
            timestamp: head.timestamp,
            hash: head.hash.clone(),
        })
}

struct VerifiedCommittedSnapshot {
    base_digest: SnapshotDigest,
    base_head: SnapshotKeyHead,
    part_heads: std::collections::BTreeMap<String, SnapshotKeyHead>,
    parts: std::collections::BTreeSet<String>,
}

impl VerifiedCommittedSnapshot {
    fn heads_match(&self, heads: &[guardian_db::traits::EntryHead], base_key: &str) -> bool {
        latest_snapshot_head(heads, base_key).as_ref() == Some(&self.base_head)
            && self
                .part_heads
                .iter()
                .all(|(key, expected)| latest_snapshot_head(heads, key).as_ref() == Some(expected))
    }
}

async fn verified_committed_snapshot(
    h: &Handle,
    base_key: &str,
) -> anyhow::Result<VerifiedCommittedSnapshot> {
    let heads_before = h.kv.entry_heads().await?;
    let base_head = latest_snapshot_head(&heads_before, base_key)
        .ok_or_else(|| anyhow::anyhow!("committed Guardian snapshot base head is absent"))?;
    let bytes =
        h.kv.get(base_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("committed Guardian snapshot base bytes are absent"))?;
    if iroh_blobs::Hash::new(&bytes).to_hex() != base_head.hash {
        anyhow::bail!("committed Guardian snapshot base bytes do not match its head");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let manifest = value
        .get(SNAPSHOT_PARTS_FIELD)
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("committed Guardian snapshot has no part manifest"))?;
    if manifest.len() != SNAPSHOT_PART_FIELDS.len() {
        anyhow::bail!("committed Guardian snapshot part manifest is incomplete");
    }

    let mut keep = std::collections::BTreeSet::new();
    let mut part_heads = std::collections::BTreeMap::new();
    for field in SNAPSHOT_PART_FIELDS {
        let reference = manifest
            .get(field)
            .ok_or_else(|| anyhow::anyhow!("committed Guardian part {field} is absent"))?;
        let string_reference = reference.is_string();
        let (part_key, digest) = if let Some(digest) = reference.as_str() {
            (format!("{base_key}-part/{field}"), digest.to_string())
        } else {
            let reference = reference
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("committed Guardian part {field} is invalid"))?;
            if reference.len() != 2
                || !reference.contains_key("key")
                || !reference.contains_key("sha256")
            {
                anyhow::bail!("committed Guardian part {field} has unexpected fields");
            }
            let part_key = reference
                .get("key")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("committed Guardian part {field} has no key"))?;
            let digest = reference
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("committed Guardian part {field} has no digest"))?;
            (part_key.to_string(), digest.to_string())
        };
        let fixed = format!("{base_key}-part/{field}");
        let prior_addressed = format!("{base_key}-part/{field}/{digest}");
        let current_addressed = format!("{base_key}-part-v2/{field}/{digest}");
        let non_prefix_addressed = format!("/{field}/{digest}");
        let v3_matches = part_key
            .strip_prefix(&snapshot_v3_prefix(base_key)?)
            .is_some_and(|suffix| suffix == non_prefix_addressed);
        let key_matches_reference = if string_reference {
            part_key == fixed
        } else {
            part_key == prior_addressed || part_key == current_addressed || v3_matches
        };
        if !is_lower_hex_digest(&digest)
            || !key_matches_reference
            || snapshot_part_key_shape(base_key, &part_key).is_none_or(|part| part.field != field)
        {
            anyhow::bail!("committed Guardian part {field} reference is invalid");
        }
        let part_head = latest_snapshot_head(&heads_before, &part_key)
            .ok_or_else(|| anyhow::anyhow!("committed Guardian part {field} head is absent"))?;
        let part_bytes =
            h.kv.get(&part_key).await?.ok_or_else(|| {
                anyhow::anyhow!("committed Guardian part {field} bytes are absent")
            })?;
        if iroh_blobs::Hash::new(&part_bytes).to_hex() != part_head.hash {
            anyhow::bail!("committed Guardian part {field} bytes do not match its head");
        }
        if hex::encode(sha256(&part_bytes)) != digest {
            anyhow::bail!("committed Guardian part {field} digest does not match its bytes");
        }
        let part_value: serde_json::Value =
            serde_json::from_slice(&part_bytes).map_err(|error| {
                anyhow::anyhow!("committed Guardian part {field} is invalid JSON: {error}")
            })?;
        let part_object = part_value.as_object().ok_or_else(|| {
            anyhow::anyhow!("committed Guardian part {field} is not a JSON object")
        })?;
        if part_object.len() != 1 || !part_object.contains_key(field) {
            anyhow::bail!("committed Guardian part {field} is not an exact field wrapper");
        }
        part_heads.insert(part_key.clone(), part_head);
        keep.insert(part_key);
    }
    if keep.len() != SNAPSHOT_PART_FIELDS.len() {
        anyhow::bail!("committed Guardian part keep-set is not exactly five keys");
    }

    let verified = VerifiedCommittedSnapshot {
        base_digest: sha256(&bytes),
        base_head,
        part_heads,
        parts: keep,
    };
    let heads_after = h.kv.entry_heads().await?;
    if !verified.heads_match(&heads_after, base_key) {
        anyhow::bail!("committed Guardian snapshot head changed during verification");
    }
    Ok(verified)
}

async fn verified_committed_part_keys(
    h: &Handle,
    base_key: &str,
) -> anyhow::Result<(SnapshotDigest, std::collections::BTreeSet<String>)> {
    let verified = verified_committed_snapshot(h, base_key).await?;
    Ok((verified.base_digest, verified.parts))
}

#[derive(Default)]
struct PreparedSnapshotRoots {
    part_keys: std::collections::BTreeMap<String, usize>,
    base_digests: std::collections::BTreeMap<(String, String), usize>,
}

static PREPARED_SNAPSHOT_ROOTS: OnceLock<std::sync::Mutex<PreparedSnapshotRoots>> = OnceLock::new();
static SNAPSHOT_LIFECYCLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn prepared_snapshot_roots() -> &'static std::sync::Mutex<PreparedSnapshotRoots> {
    PREPARED_SNAPSHOT_ROOTS.get_or_init(|| std::sync::Mutex::new(PreparedSnapshotRoots::default()))
}

struct PreparedSnapshotRegistration {
    base_key: String,
    base_digest: String,
    part_keys: Vec<String>,
}

impl PreparedSnapshotRegistration {
    fn new(base_key: &str, base_digest: SnapshotDigest, part_keys: Vec<String>) -> Self {
        let base_digest = hex::encode(base_digest);
        let mut roots = prepared_snapshot_roots()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *roots
            .base_digests
            .entry((base_key.to_string(), base_digest.clone()))
            .or_default() += 1;
        for key in &part_keys {
            *roots.part_keys.entry(key.clone()).or_default() += 1;
        }
        drop(roots);
        Self {
            base_key: base_key.to_string(),
            base_digest,
            part_keys,
        }
    }
}

impl Drop for PreparedSnapshotRegistration {
    fn drop(&mut self) {
        let mut roots = prepared_snapshot_roots()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for key in &self.part_keys {
            match roots.part_keys.entry(key.clone()) {
                std::collections::btree_map::Entry::Occupied(mut entry) if *entry.get() > 1 => {
                    *entry.get_mut() -= 1;
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    entry.remove();
                }
                std::collections::btree_map::Entry::Vacant(_) => {}
            }
        }
        let identity = (self.base_key.clone(), self.base_digest.clone());
        match roots.base_digests.entry(identity) {
            std::collections::btree_map::Entry::Occupied(mut entry) if *entry.get() > 1 => {
                *entry.get_mut() -= 1;
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                entry.remove();
            }
            std::collections::btree_map::Entry::Vacant(_) => {}
        }
    }
}

fn prepared_snapshot_keys(
    base_key: &str,
) -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    let roots = prepared_snapshot_roots()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let parts = roots.part_keys.keys().cloned().collect();
    let bases = roots
        .base_digests
        .keys()
        .filter_map(|(key, digest)| (key == base_key).then_some(digest.clone()))
        .collect();
    (parts, bases)
}

/// Exact-key delete of a snapshot metadata entry: refuses when a descendant key exists (iroh-docs
/// deletion is prefix-based and would erase it) and verifies the key is gone afterwards;
/// `Ok(false)` when it was already absent.
async fn delete_snapshot_key(h: &Handle, key: &str) -> anyhow::Result<bool> {
    let before = h.kv.entry_heads().await?;
    if !before.iter().any(|head| head.key == key) {
        return Ok(false);
    }
    if let Some(collision) = before
        .iter()
        .find(|head| head.key != key && head.key.as_bytes().starts_with(key.as_bytes()))
    {
        anyhow::bail!(
            "Guardian exact metadata delete {key} is blocked by descendant key {}",
            collision.key
        );
    }
    h.kv.delete(key).await?;
    let remains =
        h.kv.entry_heads()
            .await?
            .into_iter()
            .any(|head| head.key == key);
    if remains {
        anyhow::bail!("Guardian metadata delete left exact key {key} present");
    }
    Ok(true)
}

#[derive(Clone)]
struct ProvenStalePart {
    key: String,
    field: &'static str,
    digest: String,
    head: SnapshotKeyHead,
}

#[derive(Clone)]
struct ProvenStaleMarker {
    key: String,
    generation_digest: String,
    head: SnapshotKeyHead,
}

struct SnapshotCleanupInventory {
    part_count: usize,
    marker_count: usize,
    stale_parts: Vec<ProvenStalePart>,
    stale_markers: Vec<ProvenStaleMarker>,
}

fn in_reserved_tree(key: &str, root: &str) -> bool {
    key == root
        || key
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn summarize_cleanup_keys(keys: &[String]) -> String {
    let mut shown = keys.iter().take(8).cloned().collect::<Vec<_>>();
    if keys.len() > shown.len() {
        shown.push(format!("... and {} more", keys.len() - shown.len()));
    }
    shown.join(", ")
}

fn snapshot_cleanup_refusal(reason: String) -> anyhow::Error {
    tracing::error!(reason = %reason, "Guardian snapshot cleanup REFUSED (global shape unproven)");
    anyhow::anyhow!(reason)
}

fn latest_reserved_snapshot_heads(
    heads: &[guardian_db::traits::EntryHead],
    roots: &[&str],
) -> anyhow::Result<std::collections::BTreeMap<String, SnapshotKeyHead>> {
    let mut latest = std::collections::BTreeMap::<String, SnapshotKeyHead>::new();
    for head in heads {
        if !roots.iter().any(|root| in_reserved_tree(&head.key, root)) {
            continue;
        }
        let identity = SnapshotKeyHead {
            timestamp: head.timestamp,
            hash: head.hash.clone(),
        };
        match latest.entry(head.key.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(identity);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if identity.timestamp > entry.get().timestamp =>
            {
                entry.insert(identity);
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if identity.timestamp == entry.get().timestamp
                    && identity.hash != entry.get().hash =>
            {
                anyhow::bail!(
                    "reserved Guardian key {} has ambiguous heads at timestamp {}",
                    entry.key(),
                    identity.timestamp
                );
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    Ok(latest)
}

async fn verify_addressed_snapshot_part(h: &Handle, part: &ProvenStalePart) -> anyhow::Result<()> {
    let bytes =
        h.kv.get(&part.key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("addressed part bytes are absent"))?;
    if iroh_blobs::Hash::new(&bytes).to_hex() != part.head.hash {
        anyhow::bail!("addressed part bytes do not match their document head");
    }
    if hex::encode(sha256(&bytes)) != part.digest {
        anyhow::bail!("addressed part digest does not match its exact key");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("addressed part is not a JSON object"))?;
    if object.len() != 1 || !object.contains_key(part.field) {
        anyhow::bail!("addressed part is not an exact {} wrapper", part.field);
    }
    let heads_after = h.kv.entry_heads().await?;
    if latest_snapshot_head(&heads_after, &part.key).as_ref() != Some(&part.head) {
        anyhow::bail!("addressed part head changed during proof");
    }
    Ok(())
}

async fn snapshot_cleanup_inventory(
    h: &Handle,
    base_key: &str,
) -> anyhow::Result<SnapshotCleanupInventory> {
    let verified = verified_committed_snapshot(h, base_key).await?;
    if verified.parts.len() != SNAPSHOT_PART_FIELDS.len() {
        return Err(snapshot_cleanup_refusal(
            "Guardian part cleanup requires an exact nonempty five-key current manifest".into(),
        ));
    }

    let legacy_root = format!("{base_key}-part");
    let current_root = format!("{base_key}-part-v2");
    let non_prefix_root = snapshot_v3_prefix(base_key)?;
    let legacy_marker_root = format!("{base_key}-part-reap");
    let node_prefix = base_key
        .strip_suffix("/snapshot")
        .filter(|prefix| !prefix.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Guardian snapshot base key has invalid shape"))?;
    let marker_root = format!("{node_prefix}/part-reap-v3");
    let legacy_marker_prefix = format!("{legacy_marker_root}/");
    let marker_prefix = format!("{marker_root}/");
    let reserved_roots = [
        legacy_root.as_str(),
        current_root.as_str(),
        non_prefix_root.as_str(),
        legacy_marker_root.as_str(),
        marker_root.as_str(),
    ];

    let inventory_heads = h.kv.entry_heads().await?;
    let latest = latest_reserved_snapshot_heads(&inventory_heads, &reserved_roots).map_err(
        |error| {
            snapshot_cleanup_refusal(format!(
                "Guardian part cleanup could not classify the exact reserved head population: {error}"
            ))
        },
    )?;

    let (prepared_parts, prepared_bases) = prepared_snapshot_keys(base_key);
    let current_base_digest = hex::encode(verified.base_digest);
    let mut parts = std::collections::BTreeMap::new();
    let mut markers = Vec::new();
    let mut ambiguous = Vec::new();
    for (key, head) in &latest {
        if in_reserved_tree(key, &legacy_marker_root) || in_reserved_tree(key, &marker_root) {
            let suffix = key
                .strip_prefix(&marker_prefix)
                .or_else(|| key.strip_prefix(&legacy_marker_prefix));
            if let Some(digest) = suffix.filter(|value| is_lower_hex_digest(value)) {
                markers.push((key.clone(), digest.to_string(), head.clone()));
            } else {
                ambiguous.push(key.clone());
            }
            continue;
        }
        if in_reserved_tree(key, &legacy_root)
            || in_reserved_tree(key, &current_root)
            || in_reserved_tree(key, &non_prefix_root)
        {
            if let Some(shape) = snapshot_part_key_shape(base_key, key) {
                parts.insert(key.clone(), (shape, head.clone()));
            } else {
                ambiguous.push(key.clone());
            }
        }
    }

    if !ambiguous.is_empty() {
        return Err(snapshot_cleanup_refusal(format!(
            "Guardian part cleanup found {} unclassified key(s) inside reserved snapshot trees: {}",
            ambiguous.len(),
            summarize_cleanup_keys(&ambiguous)
        )));
    }
    if !verified.parts.iter().all(|key| parts.contains_key(key)) {
        return Err(snapshot_cleanup_refusal(
            "Guardian part cleanup current five-key keep-set is not fully materialized".into(),
        ));
    }

    let mut legacy_stale = Vec::new();
    let mut stale_parts = Vec::new();
    for (key, (shape, head)) in &parts {
        if verified.parts.contains(key) {
            continue;
        }
        if shape.format == SnapshotPartFormat::Fixed {
            legacy_stale.push(key.clone());
            continue;
        }
        if prepared_parts.contains(key) || head.timestamp > verified.base_head.timestamp {
            continue;
        }
        stale_parts.push(ProvenStalePart {
            key: key.clone(),
            field: shape.field,
            digest: shape.digest.unwrap_or_default().to_string(),
            head: head.clone(),
        });
    }
    if !legacy_stale.is_empty() {
        return Err(snapshot_cleanup_refusal(format!(
            "Guardian part cleanup found {} stale legacy fixed key(s) whose immutable generation cannot be proven: {}",
            legacy_stale.len(),
            summarize_cleanup_keys(&legacy_stale)
        )));
    }

    for part in &stale_parts {
        if let Err(error) = verify_addressed_snapshot_part(h, part).await {
            return Err(snapshot_cleanup_refusal(format!(
                "Guardian part cleanup could not prove addressed generation {}: {error}",
                part.key
            )));
        }
    }
    #[cfg(debug_assertions)]
    cleanup_diagnostic_before_global_recheck().await;
    let verified_after = verified_committed_snapshot(h, base_key).await?;
    if verified_after.base_head != verified.base_head || verified_after.parts != verified.parts {
        return Err(snapshot_cleanup_refusal(
            "Guardian current snapshot changed while the global cleanup population was being proven"
                .into(),
        ));
    }
    let final_heads = h.kv.entry_heads().await?;
    let latest_after = latest_reserved_snapshot_heads(&final_heads, &reserved_roots).map_err(
        |error| {
            snapshot_cleanup_refusal(format!(
                "Guardian part cleanup could not reclassify the exact reserved head population: {error}"
            ))
        },
    )?;
    if latest_after != latest {
        return Err(snapshot_cleanup_refusal(
            "Guardian reserved snapshot population changed while its complete global shape was being proven"
                .into(),
        ));
    }

    let mut stale_markers = markers
        .iter()
        .filter(|(_, digest, head)| {
            digest != &current_base_digest
                && !prepared_bases.contains(digest)
                && head.timestamp <= verified.base_head.timestamp
        })
        .map(|(key, generation_digest, head)| ProvenStaleMarker {
            key: key.clone(),
            generation_digest: generation_digest.clone(),
            head: head.clone(),
        })
        .collect::<Vec<_>>();
    stale_parts.sort_by(|left, right| {
        left.head
            .timestamp
            .cmp(&right.head.timestamp)
            .then_with(|| left.key.cmp(&right.key))
    });
    stale_markers.sort_by(|left, right| {
        left.head
            .timestamp
            .cmp(&right.head.timestamp)
            .then_with(|| left.key.cmp(&right.key))
    });

    Ok(SnapshotCleanupInventory {
        part_count: parts.len(),
        marker_count: markers.len(),
        stale_parts,
        stale_markers,
    })
}

fn snapshot_cleanup_limits() -> (usize, f64) {
    let max_keys = std::env::var("HIVE_GUARDIAN_PART_REAP_MAX_KEYS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(256);
    let max_fraction = std::env::var("HIVE_GUARDIAN_PART_REAP_MAX_FRACTION")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| *value > 0.0 && *value <= 1.0)
        .unwrap_or(0.5);
    (max_keys, max_fraction)
}

fn snapshot_cleanup_batch_limit(population: usize, max_keys: usize, max_fraction: f64) -> usize {
    max_keys.min((population as f64 * max_fraction).floor() as usize)
}

#[cfg(debug_assertions)]
static CLEANUP_DIAGNOSTIC_FAIL_AFTER: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(-1);
#[cfg(debug_assertions)]
static CLEANUP_DIAGNOSTIC_PAUSE_AFTER: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(-1);
#[cfg(debug_assertions)]
static CLEANUP_DIAGNOSTIC_PAUSE_BEFORE_GLOBAL_RECHECK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn cleanup_diagnostic_before_delete() -> anyhow::Result<()> {
    #[cfg(debug_assertions)]
    {
        use std::sync::atomic::Ordering;
        let remaining = CLEANUP_DIAGNOSTIC_FAIL_AFTER.load(Ordering::SeqCst);
        if remaining == 0 {
            CLEANUP_DIAGNOSTIC_FAIL_AFTER.store(-1, Ordering::SeqCst);
            anyhow::bail!("injected Guardian cleanup delete failure");
        }
        if remaining > 0 {
            CLEANUP_DIAGNOSTIC_FAIL_AFTER.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn cleanup_diagnostic_paused() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

#[cfg(debug_assertions)]
fn cleanup_diagnostic_resume() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

#[cfg(debug_assertions)]
async fn cleanup_diagnostic_before_global_recheck() {
    if CLEANUP_DIAGNOSTIC_PAUSE_BEFORE_GLOBAL_RECHECK
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        cleanup_diagnostic_paused().notify_one();
        cleanup_diagnostic_resume().notified().await;
    }
}

#[cfg(debug_assertions)]
async fn cleanup_diagnostic_after_delete() {
    use std::sync::atomic::Ordering;
    let remaining = CLEANUP_DIAGNOSTIC_PAUSE_AFTER.load(Ordering::SeqCst);
    if remaining <= 0 {
        return;
    }
    if remaining == 1 {
        CLEANUP_DIAGNOSTIC_PAUSE_AFTER.store(-1, Ordering::SeqCst);
        cleanup_diagnostic_paused().notify_one();
        cleanup_diagnostic_resume().notified().await;
    } else {
        CLEANUP_DIAGNOSTIC_PAUSE_AFTER.fetch_sub(1, Ordering::SeqCst);
    }
}

fn exact_delete_collision<'a>(
    heads: &'a [guardian_db::traits::EntryHead],
    key: &str,
) -> Option<&'a str> {
    heads
        .iter()
        .find(|head| head.key != key && head.key.as_bytes().starts_with(key.as_bytes()))
        .map(|head| head.key.as_str())
}

async fn delete_proven_stale_part(
    h: &Handle,
    base_key: &str,
    part: &ProvenStalePart,
) -> anyhow::Result<bool> {
    verify_addressed_snapshot_part(h, part).await?;
    let current = verified_committed_snapshot(h, base_key).await?;
    let (prepared_parts, _) = prepared_snapshot_keys(base_key);
    if current.parts.contains(&part.key)
        || prepared_parts.contains(&part.key)
        || part.head.timestamp > current.base_head.timestamp
    {
        return Ok(false);
    }
    let heads = h.kv.entry_heads().await?;
    if !current.heads_match(&heads, base_key) {
        anyhow::bail!("Guardian current snapshot head changed immediately before part batch");
    }
    let Some(target_head) = latest_snapshot_head(&heads, &part.key) else {
        return Ok(false);
    };
    if target_head != part.head {
        anyhow::bail!("Guardian stale part head changed immediately before exact delete");
    }
    if let Some(collision) = exact_delete_collision(&heads, &part.key) {
        anyhow::bail!(
            "Guardian exact stale part delete {} is blocked by descendant key {collision}",
            part.key
        );
    }
    cleanup_diagnostic_before_delete()?;
    h.kv.delete(&part.key).await?;
    if latest_snapshot_head(&h.kv.entry_heads().await?, &part.key).is_some() {
        anyhow::bail!("Guardian exact stale part delete left {} present", part.key);
    }
    Ok(true)
}

async fn delete_proven_stale_marker(
    h: &Handle,
    base_key: &str,
    marker: &ProvenStaleMarker,
) -> anyhow::Result<bool> {
    let current = verified_committed_snapshot(h, base_key).await?;
    let (_, prepared_bases) = prepared_snapshot_keys(base_key);
    if hex::encode(current.base_digest) == marker.generation_digest
        || prepared_bases.contains(&marker.generation_digest)
        || marker.head.timestamp > current.base_head.timestamp
    {
        return Ok(false);
    }
    let heads = h.kv.entry_heads().await?;
    if !current.heads_match(&heads, base_key) {
        anyhow::bail!("Guardian current snapshot head changed immediately before marker batch");
    }
    let Some(target_head) = latest_snapshot_head(&heads, &marker.key) else {
        return Ok(false);
    };
    if target_head != marker.head {
        anyhow::bail!("Guardian stale protection marker changed immediately before exact delete");
    }
    if let Some(collision) = exact_delete_collision(&heads, &marker.key) {
        anyhow::bail!(
            "Guardian exact stale marker delete {} is blocked by descendant key {collision}",
            marker.key
        );
    }
    cleanup_diagnostic_before_delete()?;
    h.kv.delete(&marker.key).await?;
    if latest_snapshot_head(&h.kv.entry_heads().await?, &marker.key).is_some() {
        anyhow::bail!(
            "Guardian exact stale protection marker delete left {} present",
            marker.key
        );
    }
    Ok(true)
}

async fn cleanup_committed_snapshot_parts_locked(
    h: &Handle,
    base_key: &str,
) -> anyhow::Result<usize> {
    let inventory = snapshot_cleanup_inventory(h, base_key).await?;
    let (max_keys, max_fraction) = snapshot_cleanup_limits();
    let mut deleted = 0usize;

    if !inventory.stale_parts.is_empty() {
        let limit = snapshot_cleanup_batch_limit(inventory.part_count, max_keys, max_fraction);
        if limit == 0 {
            return Err(snapshot_cleanup_refusal(format!(
                "Guardian part cleanup cannot delete one exact addressed generation within max_fraction={max_fraction:.6} over {} part keys",
                inventory.part_count
            )));
        }
        for part in inventory.stale_parts.iter().take(limit) {
            if delete_proven_stale_part(h, base_key, part).await? {
                deleted += 1;
                #[cfg(debug_assertions)]
                cleanup_diagnostic_after_delete().await;
            }
        }
        return Ok(deleted);
    }

    if !inventory.stale_markers.is_empty() {
        let population = inventory.part_count.saturating_add(inventory.marker_count);
        let limit = snapshot_cleanup_batch_limit(population, max_keys, max_fraction);
        if limit == 0 {
            return Err(snapshot_cleanup_refusal(format!(
                "Guardian marker cleanup cannot delete one exact stale marker within max_fraction={max_fraction:.6} over {population} lifecycle keys"
            )));
        }
        for marker in inventory.stale_markers.iter().take(limit) {
            if delete_proven_stale_marker(h, base_key, marker).await? {
                deleted += 1;
                #[cfg(debug_assertions)]
                cleanup_diagnostic_after_delete().await;
            }
        }
    }
    Ok(deleted)
}

async fn cleanup_committed_snapshot_parts(h: &Handle, base_key: &str) -> anyhow::Result<usize> {
    let _lifecycle = SNAPSHOT_LIFECYCLE_LOCK.lock().await;
    cleanup_committed_snapshot_parts_locked(h, base_key).await
}

#[derive(Default)]
struct ReplicationWriteStats {
    namespace_puts: usize,
    full_puts: usize,
    deletes: usize,
    put_bytes: usize,
}

/// Bounded v2 retention for THIS node's own component only (single writer): the head is overwritten
/// in place, so superseded immutable parts and prior heads (rooted by their generation markers)
/// would otherwise accumulate forever. Each pass retires (1) generation markers beyond the keep
/// window (count AND age must both pass), oldest-first in lexical order, and (2) part metadata keys
/// referenced by neither the current head, any surviving marker nor a Drop-guarded in-flight
/// preparation, older than the grace window, resuming from an explicit cursor.
/// The marker naming the CURRENT head is the anchor: without it the pass refuses, and the writer
/// creates it on the next verified publication. Deleting the metadata entry un-roots the part's
/// blob and the vendored collector reclaims the bytes later. Any doubt (absent or malformed head,
/// unreadable marker, empty reference set) is a typed refusal for the whole pass, never a guess;
/// the cursor advances only on non-failed passes.
struct RetentionConfig {
    /// Prior COMPLETE generations kept beyond the current one.
    keep_generations: usize,
    /// Markers/parts younger than this are never retired, whatever the count.
    grace_ms: u64,
    /// Per-pass delete budget (markers + parts) — the blast-radius bound.
    max_deletes: usize,
    /// Per-pass ceiling as a fraction of the listed marker population.
    max_fraction: f64,
    /// Page size for bounded lexical listing.
    page: usize,
    /// Hard cap on the marker population a pass will even consider.
    max_generations: usize,
}

fn retention_config() -> RetentionConfig {
    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(default)
    }
    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(default)
    }
    RetentionConfig {
        keep_generations: env_usize("HIVE_GUARDIAN_V2_KEEP_GENERATIONS", 2),
        grace_ms: env_u64("HIVE_GUARDIAN_V2_GEN_GRACE_SECS", 24 * 60 * 60).saturating_mul(1000),
        max_deletes: env_usize("HIVE_GUARDIAN_V2_RETENTION_MAX_DELETES", 64).max(1),
        max_fraction: std::env::var("HIVE_GUARDIAN_V2_RETENTION_MAX_FRACTION")
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| *value > 0.0 && *value <= 1.0)
            .unwrap_or(0.5),
        page: env_usize("HIVE_GUARDIAN_V2_RETENTION_PAGE", 512).max(8),
        max_generations: env_usize("HIVE_GUARDIAN_V2_RETENTION_MAX_GENERATIONS", 8192).max(8),
    }
}

/// Snapshot of the most recent retention pass — bounded observability only.
#[derive(Clone, Default, serde::Serialize)]
struct RetentionReport {
    /// `completed` | `noop` | `refused` | `failed`.
    status: String,
    reason: Option<String>,
    started_ms: u64,
    finished_ms: u64,
    generations_total: u64,
    generations_kept: u64,
    generations_retired: u64,
    parts_listed: u64,
    parts_referenced: u64,
    parts_retired: u64,
    /// Declared entry bytes of retired part metadata (bytes UN-ROOTED this pass); blob bytes are
    /// reclaimed later by the vendored collector and counted there, never double-claimed.
    parts_unrooted_bytes: u64,
    parts_skipped_young: u64,
    parts_skipped_inflight: u64,
    /// Lexical resume cursor after this pass (None = prefix exhausted; the
    /// next pass starts from the beginning).
    cursor: Option<String>,
}

#[derive(Default)]
struct RetentionState {
    cursor: Option<String>,
    last: Option<RetentionReport>,
    passes: u64,
    refusals: u64,
    retired_generations_total: u64,
    retired_parts_total: u64,
    unrooted_bytes_total: u64,
}

static RETENTION_STATE: OnceLock<std::sync::Mutex<RetentionState>> = OnceLock::new();

fn retention_state() -> &'static std::sync::Mutex<RetentionState> {
    RETENTION_STATE.get_or_init(|| std::sync::Mutex::new(RetentionState::default()))
}

fn retention_refusal(report: &mut RetentionReport, reason: String) {
    tracing::warn!(reason = %reason, "Guardian v2 retention REFUSED (no deletion)");
    report.status = "refused".to_string();
    report.reason = Some(reason);
}

/// One bounded retention pass over THIS node's own v2 subtree. Never propagates an error: every
/// outcome is a typed report, recorded for the observability endpoint and returned for the caller's
/// log line.
async fn guardian_v2_retention_pass(h: &Handle) -> RetentionReport {
    guardian_v2_retention_pass_with(h, retention_config()).await
}

/// Explicit-config variant: the env-independent entrypoint the diagnostic
/// drives with deterministic bounds (never `std::env::set_var` at runtime).
async fn guardian_v2_retention_pass_with(h: &Handle, cfg: RetentionConfig) -> RetentionReport {
    let mut report = RetentionReport {
        started_ms: hive_core::now_ms(),
        status: "completed".to_string(),
        ..Default::default()
    };
    let _lifecycle = SNAPSHOT_LIFECYCLE_LOCK.lock().await;

    'pass: {
        let Some(component) = NODE_NAME.get().filter(|name| !name.trim().is_empty()) else {
            retention_refusal(&mut report, "node identity is uninitialized".to_string());
            break 'pass;
        };
        let encoded = match guardian_v2_encode_component(component) {
            Ok(encoded) => encoded,
            Err(error) => {
                retention_refusal(&mut report, format!("component encoding failed: {error}"));
                break 'pass;
            }
        };
        let node_prefix = format!("{GUARDIAN_V2_NODE_PREFIX}{encoded}/");
        let head_key = format!("{node_prefix}head");
        let gen_prefix = format!("{node_prefix}{GUARDIAN_V2_GEN_SEGMENT}/");

        let head_bytes = match h.kv.get(&head_key).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                retention_refusal(
                    &mut report,
                    "current v2 head is absent; nothing may be retired without a verified \
                     current generation"
                        .to_string(),
                );
                break 'pass;
            }
            Err(error) => {
                retention_refusal(&mut report, format!("current v2 head read failed: {error}"));
                break 'pass;
            }
        };
        let head = match guardian_v2_parse_head(&head_bytes) {
            Ok(head) => head,
            Err(error) => {
                retention_refusal(
                    &mut report,
                    format!("current v2 head is malformed: {error}"),
                );
                break 'pass;
            }
        };
        let head_sha256 = hex::encode(sha256(&head_bytes));

        let mut markers: Vec<guardian_db::traits::EntryHeadPage> = Vec::new();
        let mut marker_cursor: Option<String> = None;
        loop {
            let page = match h
                .kv
                .entry_heads_page(&gen_prefix, marker_cursor.as_deref(), cfg.page)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    retention_refusal(
                        &mut report,
                        format!("generation marker listing failed: {error}"),
                    );
                    break 'pass;
                }
            };
            let exhausted = page.len() < cfg.page;
            for row in page {
                marker_cursor = Some(row.key.clone());
                if guardian_v2_parse_generation_key(&row.key, &encoded).is_some() {
                    markers.push(row);
                }
            }
            if markers.len() > cfg.max_generations {
                retention_refusal(
                    &mut report,
                    format!(
                        "generation marker population exceeds \
                         HIVE_GUARDIAN_V2_RETENTION_MAX_GENERATIONS={}",
                        cfg.max_generations
                    ),
                );
                break 'pass;
            }
            if exhausted {
                break;
            }
        }
        report.generations_total = markers.len() as u64;

        let now_ms = hive_core::now_ms();
        if !markers.iter().any(|row| {
            guardian_v2_parse_generation_key(&row.key, &encoded)
                .is_some_and(|(_, sha)| sha == head_sha256)
        }) {
            retention_refusal(
                &mut report,
                "no complete-generation marker names the current head yet".to_string(),
            );
            break 'pass;
        }

        let mut prior: Vec<&guardian_db::traits::EntryHeadPage> = markers
            .iter()
            .filter(|row| {
                guardian_v2_parse_generation_key(&row.key, &encoded)
                    .is_some_and(|(_, sha)| sha != head_sha256)
            })
            .collect();
        prior.sort_by(|left, right| left.key.cmp(&right.key));
        let retire_eligible = prior.len().saturating_sub(cfg.keep_generations);
        let mut retire_markers: Vec<&guardian_db::traits::EntryHeadPage> = prior
            .iter()
            .take(retire_eligible)
            .filter(|row| {
                guardian_v2_parse_generation_key(&row.key, &encoded)
                    .is_some_and(|(ms, _)| now_ms.saturating_sub(ms) >= cfg.grace_ms)
            })
            .copied()
            .collect();
        if !retire_markers.is_empty() && retire_markers.len() >= markers.len() {
            retention_refusal(
                &mut report,
                "retire set would cover every generation marker (implausible keep set)".to_string(),
            );
            break 'pass;
        }
        let fraction_cap = ((markers.len() as f64) * cfg.max_fraction).floor() as usize;
        let allowed_markers = retire_markers
            .len()
            .min(cfg.max_deletes)
            .min(fraction_cap.max(1));
        retire_markers.truncate(allowed_markers);

        let mut budget = cfg.max_deletes;
        let mut retired_marker_keys = std::collections::BTreeSet::new();
        for row in &retire_markers {
            if budget == 0 {
                break;
            }
            match delete_snapshot_key(h, &row.key).await {
                Ok(true) => {
                    budget -= 1;
                    report.generations_retired += 1;
                    retired_marker_keys.insert(row.key.clone());
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .v2_retention_deletes += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    report.status = "failed".to_string();
                    report.reason = Some(format!(
                        "generation marker delete failed at {}: {error}",
                        row.key
                    ));
                    break 'pass;
                }
            }
        }
        report.generations_kept = report
            .generations_total
            .saturating_sub(report.generations_retired);

        let mut referenced = std::collections::BTreeSet::new();
        for kind in GuardianV2PartKind::ALL {
            let Some(reference) = head.references.get(kind.field()) else {
                retention_refusal(
                    &mut report,
                    format!("current head lost reference {}", kind.field()),
                );
                break 'pass;
            };
            match guardian_v2_part_key(&encoded, kind, &reference.sha256) {
                Ok(key) => {
                    referenced.insert(key);
                }
                Err(error) => {
                    retention_refusal(&mut report, format!("current head part key: {error}"));
                    break 'pass;
                }
            }
        }
        for row in &markers {
            if retired_marker_keys.contains(&row.key) {
                continue;
            }
            let bytes = match h.kv.get(&row.key).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    retention_refusal(
                        &mut report,
                        format!(
                            "generation marker {} is unreadable (metadata present, bytes absent)",
                            row.key
                        ),
                    );
                    break 'pass;
                }
                Err(error) => {
                    retention_refusal(
                        &mut report,
                        format!("generation marker {} read failed: {error}", row.key),
                    );
                    break 'pass;
                }
            };
            let marker_head = match guardian_v2_parse_head(&bytes) {
                Ok(head) => head,
                Err(error) => {
                    retention_refusal(
                        &mut report,
                        format!("generation marker {} is malformed: {error}", row.key),
                    );
                    break 'pass;
                }
            };
            for kind in GuardianV2PartKind::ALL {
                if let Some(reference) = marker_head.references.get(kind.field()) {
                    if let Ok(key) = guardian_v2_part_key(&encoded, kind, &reference.sha256) {
                        referenced.insert(key);
                    }
                }
            }
        }
        if referenced.is_empty() {
            retention_refusal(&mut report, "referenced part set is empty".to_string());
            break 'pass;
        }
        report.parts_referenced = referenced.len() as u64;

        let (inflight_parts, _) = prepared_snapshot_keys(&head_key);

        let mut cursor = retention_state()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cursor
            .clone();
        let grace_us = cfg.grace_ms.saturating_mul(1000);
        let now_us = now_ms.saturating_mul(1000);
        loop {
            let page = match h
                .kv
                .entry_heads_page(&node_prefix, cursor.as_deref(), cfg.page)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    report.status = "failed".to_string();
                    report.reason = Some(format!("part listing failed: {error}"));
                    break 'pass;
                }
            };
            let exhausted = page.len() < cfg.page;
            for row in page {
                cursor = Some(row.key.clone());
                let Some(_) = guardian_v2_part_key_fields(&row.key, &encoded) else {
                    continue;
                };
                report.parts_listed += 1;
                if referenced.contains(&row.key) {
                    continue;
                }
                if inflight_parts.contains(&row.key) {
                    report.parts_skipped_inflight += 1;
                    continue;
                }
                if now_us.saturating_sub(row.timestamp) < grace_us {
                    report.parts_skipped_young += 1;
                    continue;
                }
                if budget == 0 {
                    break;
                }
                match delete_snapshot_key(h, &row.key).await {
                    Ok(true) => {
                        budget -= 1;
                        report.parts_retired += 1;
                        report.parts_unrooted_bytes += row.content_len;
                        write_counters()
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .v2_retention_deletes += 1;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        report.status = "failed".to_string();
                        report.reason = Some(format!("part delete failed at {}: {error}", row.key));
                        break 'pass;
                    }
                }
            }
            if budget == 0 {
                break;
            }
            if exhausted {
                cursor = None;
                break;
            }
        }
        report.cursor = cursor;
        if report.generations_retired == 0 && report.parts_retired == 0 {
            report.status = "noop".to_string();
        }
    }

    report.finished_ms = hive_core::now_ms();
    {
        let mut state = retention_state().lock().unwrap_or_else(|p| p.into_inner());
        state.passes += 1;
        if report.status == "refused" {
            state.refusals += 1;
        }
        state.retired_generations_total += report.generations_retired;
        state.retired_parts_total += report.parts_retired;
        state.unrooted_bytes_total += report.parts_unrooted_bytes;
        if report.status != "failed" {
            state.cursor = report.cursor.clone();
        }
        state.last = Some(report.clone());
    }
    report
}

fn guardian_v2_latest_entry<'a>(
    heads: &'a [guardian_db::traits::EntryHead],
    key: &str,
) -> Option<&'a guardian_db::traits::EntryHead> {
    heads
        .iter()
        .filter(|head| head.key == key)
        .max_by_key(|head| head.timestamp)
}

/// Same decision as [`guardian_v2_current_exact`] against an ALREADY-FETCHED head listing.
/// `entry_heads()` walks EVERY entry in the replicated doc, so it must never be called once per key
/// (a 72-namespace batch cost ~225 full walks per generation, and a writer that never returns holds
/// the only replication slot): `write_replication_batch` lists once before and once after the batch
/// and verifies every key against those listings.
async fn guardian_v2_current_exact_in(
    heads: &[guardian_db::traits::EntryHead],
    h: &Handle,
    key: &str,
    value: &PreparedValue,
) -> anyhow::Result<bool> {
    let Some(head) = guardian_v2_latest_entry(heads, key) else {
        return Ok(false);
    };
    if head.content_local == Some(false)
        || head.hash != iroh_blobs::Hash::new(value.bytes.as_ref()).to_hex()
    {
        return Ok(false);
    }
    Ok(matches!(h.kv.get(key).await, Ok(Some(bytes)) if bytes.as_slice() == value.bytes.as_slice()))
}

async fn guardian_v2_current_exact(
    h: &Handle,
    key: &str,
    value: &PreparedValue,
) -> anyhow::Result<bool> {
    let heads = h.kv.entry_heads().await?;
    guardian_v2_current_exact_in(&heads, h, key, value).await
}

async fn guardian_v2_publish_immutable(
    h: &Handle,
    item: &KeyedPreparedValue,
    protection: &guardian_db::p2p::network::core::BlobProtection,
) -> anyhow::Result<bool> {
    let heads = h.kv.entry_heads().await?;
    let current = guardian_v2_latest_entry(&heads, &item.key);
    let readable = h.kv.get(&item.key).await.ok().flatten();
    if let Some(bytes) = &readable {
        if bytes != item.value.bytes.as_ref() {
            anyhow::bail!(
                "Guardian v2 immutable collision at {}: the SHA-addressed key has different readable bytes",
                item.key
            );
        }
    }

    let expected_blake3 = iroh_blobs::Hash::new(item.value.bytes.as_ref()).to_hex();
    if let Some(head) = current {
        if head.hash != expected_blake3 {
            anyhow::bail!(
                "Guardian v2 immutable collision at {}: current BLAKE3 metadata names different bytes",
                item.key
            );
        }
        if head.content_local != Some(false) && readable.is_some() {
            return Ok(false);
        }
    }

    h.kv.put_gc_protected(&item.key, item.value.bytes.as_ref().clone(), protection)
        .await
        .map_err(|error| anyhow::anyhow!("Guardian v2 immutable put {}: {error}", item.key))?;
    Ok(true)
}

async fn guardian_v2_verify_namespace(
    h: &Handle,
    heads: &[guardian_db::traits::EntryHead],
    heads_before: &[guardian_db::traits::EntryHead],
    key: &str,
    expected: &PreparedValue,
    was_reused: bool,
) -> anyhow::Result<()> {
    let source = guardian_v2_latest_entry(heads, key)
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace metadata is absent for {key}"))?;
    if source.content_local == Some(false) {
        anyhow::bail!("Guardian v2 namespace content is not local for {key}");
    }
    let bytes =
        h.kv.get(key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 namespace bytes are absent for {key}"))?;
    if bytes.as_slice() != expected.bytes.as_slice()
        || bytes.len() != expected.bytes.len()
        || sha256(&bytes) != expected.change_digest
        || iroh_blobs::Hash::new(&bytes).to_hex() != source.hash
    {
        anyhow::bail!("Guardian v2 namespace identity mismatch for {key}");
    }
    guardian_v2_parse_namespace(&bytes)?;
    if was_reused {
        let head = SnapshotKeyHead {
            timestamp: source.timestamp,
            hash: source.hash.clone(),
        };
        if latest_snapshot_head(heads_before, key) != Some(head) {
            anyhow::bail!("Guardian v2 namespace metadata changed during verification for {key}");
        }
    }
    Ok(())
}

fn guardian_v2_validate_prepared_generation(
    parts: &[KeyedPreparedValue],
    head: &KeyedPreparedValue,
) -> anyhow::Result<(String, GuardianV2Head)> {
    if parts.len() != GuardianV2PartKind::ALL.len() {
        anyhow::bail!("Guardian v2 prepared generation does not have exactly six parts");
    }
    if head.class != "full" {
        anyhow::bail!("Guardian v2 prepared head has the wrong write class");
    }
    let encoded_component = guardian_v2_encoded_component_from_head_key(&head.key)?;
    if sha256(head.value.bytes.as_ref()) != head.value.change_digest {
        anyhow::bail!("Guardian v2 prepared head SHA-256 does not match its bytes");
    }
    let parsed_head = guardian_v2_parse_head(head.value.bytes.as_ref())?;
    let mut values = std::collections::BTreeMap::new();
    let mut wrappers = std::collections::BTreeMap::new();
    for (kind, part) in GuardianV2PartKind::ALL.into_iter().zip(parts) {
        if part.class != kind.field() {
            anyhow::bail!(
                "Guardian v2 prepared part order/class mismatch: wanted {}, got {}",
                kind.field(),
                part.class
            );
        }
        let reference = parsed_head
            .references
            .get(kind.field())
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 prepared head lacks {}", kind.field()))?;
        let expected_key = guardian_v2_part_key(&encoded_component, kind, &reference.sha256)?;
        if part.key != expected_key {
            anyhow::bail!(
                "Guardian v2 prepared {} key does not match its SHA address",
                kind.field()
            );
        }
        if sha256(part.value.bytes.as_ref()) != part.value.change_digest
            || hex::encode(part.value.change_digest) != reference.sha256
        {
            anyhow::bail!(
                "Guardian v2 prepared {} SHA-256 does not match its bytes",
                kind.field()
            );
        }
        let (value, raw) = guardian_v2_parse_part(kind, part.value.bytes.as_ref(), reference)?;
        values.insert(kind.field().to_string(), value);
        wrappers.insert(kind.field().to_string(), raw.into_owned());
    }
    guardian_v2_reconstruct(&values, &wrappers, 0)?;
    Ok((encoded_component, parsed_head))
}

#[cfg(debug_assertions)]
fn guardian_v2_diagnostic_generation(
    desired: &DesiredReplication,
    component: &str,
) -> anyhow::Result<(Vec<KeyedPreparedValue>, KeyedPreparedValue)> {
    let source_head = desired
        .full
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic source head is absent"))?;
    let encoded = guardian_v2_encode_component(component)?;
    let mut parts = Vec::with_capacity(GuardianV2PartKind::ALL.len());
    for (kind, source) in GuardianV2PartKind::ALL.into_iter().zip(&desired.parts) {
        let digest = hex::encode(source.value.change_digest);
        parts.push(KeyedPreparedValue {
            key: guardian_v2_part_key(&encoded, kind, &digest)?,
            class: kind.field(),
            value: source.value.clone(),
        });
    }
    let head = KeyedPreparedValue {
        key: format!("{GUARDIAN_V2_NODE_PREFIX}{encoded}/head"),
        class: "full",
        value: source_head.value.clone(),
    };
    guardian_v2_validate_prepared_generation(&parts, &head)?;
    Ok((parts, head))
}

#[cfg(debug_assertions)]
fn guardian_v2_diagnostic_snapshot(
    parts: &[KeyedPreparedValue],
    head: &KeyedPreparedValue,
) -> anyhow::Result<PlatformSnapshot> {
    let parsed_head = guardian_v2_parse_head(head.value.bytes.as_ref())?;
    let mut values = std::collections::BTreeMap::new();
    let mut wrappers = std::collections::BTreeMap::new();
    for (kind, part) in GuardianV2PartKind::ALL.into_iter().zip(parts) {
        let reference = parsed_head
            .references
            .get(kind.field())
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic head lacks {}", kind.field()))?;
        let (value, raw) = guardian_v2_parse_part(kind, part.value.bytes.as_ref(), reference)?;
        values.insert(kind.field().to_string(), value);
        wrappers.insert(kind.field().to_string(), raw.into_owned());
    }
    guardian_v2_reconstruct(&values, &wrappers, 1)
}

#[cfg(debug_assertions)]
async fn guardian_v2_diagnostic_publish_generation(
    h: &Handle,
    parts: &[KeyedPreparedValue],
    head: &KeyedPreparedValue,
) -> anyhow::Result<usize> {
    let (encoded, _) = guardian_v2_validate_prepared_generation(parts, head)?;
    let mut hashes = parts
        .iter()
        .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
        .collect::<Vec<_>>();
    hashes.push(iroh_blobs::Hash::new(head.value.bytes.as_ref()));
    let mut protection = h.client.protect_hashes(hashes).await?;
    protection.finish_tag_installation();
    let registration = PreparedSnapshotRegistration::new(
        &head.key,
        head.value.change_digest,
        parts.iter().map(|part| part.key.clone()).collect(),
    );
    let mut writes = 0usize;
    for part in parts {
        writes += usize::from(guardian_v2_publish_immutable(h, part, &protection).await?);
    }
    if !guardian_v2_current_exact(h, &head.key, &head.value).await? {
        h.kv.put_gc_protected(&head.key, head.value.bytes.as_ref().clone(), &protection)
            .await?;
        writes += 1;
    }
    guardian_v2_verify_generation(h, &encoded).await?;
    drop(protection);
    drop(registration);
    Ok(writes)
}

#[cfg(debug_assertions)]
async fn guardian_v2_diagnostic_wait_for_gc(
    h: &Handle,
    successful_runs_before: u64,
) -> anyhow::Result<()> {
    for _ in 0..100 {
        if h.client.backend().gc_health().await.successful_runs > successful_runs_before {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "Guardian v2 diagnostic did not observe concurrent GC; use disposable data with GUARDIAN_GC_SECS=1"
    )
}

#[cfg(debug_assertions)]
async fn guardian_v2_diagnostic_remove_local_blob(
    h: &Handle,
    target: iroh_blobs::Hash,
) -> anyhow::Result<()> {
    use futures::StreamExt;

    let store = h.client.backend().get_store_for_blobs().await?;
    let store = store.read().await;
    let all_hashes = store.blobs().list().hashes().await?;
    if !all_hashes.contains(&target) {
        anyhow::bail!("Guardian v2 repair diagnostic target is not local before removal");
    }
    let protected_hashes = all_hashes
        .into_iter()
        .filter(|hash| *hash != target)
        .collect::<Vec<_>>();
    if protected_hashes.is_empty() {
        anyhow::bail!("Guardian v2 repair diagnostic has no non-target blobs to protect");
    }

    let protection = h
        .client
        .protect_hashes(protected_hashes.iter().copied())
        .await?;
    let mut tags = store.tags().list().await?;
    let mut target_tags = Vec::new();
    while let Some(tag) = tags.next().await {
        let tag = tag?;
        if tag.hash == target {
            target_tags.push(tag.name);
        }
    }
    drop(tags);
    for tag in target_tags {
        store.tags().delete(tag.as_ref()).await?;
    }

    let mut live = protected_hashes
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    iroh_blobs::store::gc_run_once(store.as_ref(), &mut live).await?;
    if store.blobs().has(target).await? {
        anyhow::bail!("Guardian v2 repair diagnostic target survived exact raw GC");
    }
    drop(protection);
    Ok(())
}

#[cfg(debug_assertions)]
async fn guardian_v2_diagnostic(
    h: &Handle,
    desired: &DesiredReplication,
) -> anyhow::Result<String> {
    let initial_heads = h.kv.entry_heads().await?;
    if initial_heads
        .iter()
        .any(|head| head.key.starts_with(GUARDIAN_V2_NODE_PREFIX))
    {
        anyhow::bail!("Guardian v2 diagnostic requires a fresh disposable Guardian store");
    }
    let nonce = hive_core::now_ms();

    for component in ["plain", "a.b_c-1", "slash/name", "é", "%"] {
        let encoded = guardian_v2_encode_component(component)?;
        if guardian_v2_decode_component(&encoded)? != component {
            anyhow::bail!("Guardian v2 component roundtrip changed {component:?}");
        }
    }
    for malformed in ["", "%", "%2", "%2f", "%41", "/", "é", "%FF"] {
        if guardian_v2_decode_component(malformed).is_ok() {
            anyhow::bail!("Guardian v2 component decoder accepted {malformed:?}");
        }
    }
    if guardian_v2_encode_component(&"x".repeat(256)).is_ok() {
        anyhow::bail!("Guardian v2 component encoder accepted more than 255 decoded bytes");
    }

    let map_a = guardian_v2_canonical_json_bytes(serde_json::json!({"z": 1, "a": 2}))?;
    let map_b = guardian_v2_canonical_json_bytes(serde_json::json!({"a": 2, "z": 1}))?;
    if map_a != map_b
        || guardian_v2_parse_canonical_json(&map_a, 1024, "diagnostic map").is_err()
        || guardian_v2_parse_canonical_json(br#"{"z":1,"a":2}"#, 1024, "diagnostic map").is_ok()
        || guardian_v2_parse_canonical_json(br#"{"a":1,"a":2}"#, 1024, "diagnostic duplicate")
            .is_ok()
    {
        anyhow::bail!("Guardian v2 canonical map/duplicate diagnostic failed");
    }
    let ordered_a = guardian_v2_canonical_json_bytes(serde_json::json!([1, 2]))?;
    let ordered_b = guardian_v2_canonical_json_bytes(serde_json::json!([2, 1]))?;
    if ordered_a == ordered_b {
        anyhow::bail!("Guardian v2 canonicalizer reordered a semantic array");
    }

    let mut cron = serde_json::json!({
        "cron": [{
            "id": "configured",
            "schedule": "0 * * * *",
            "last_run_ms": 1,
            "next_run_ms": 2,
            "runs": 3
        }]
    });
    guardian_v2_remove_cron_telemetry(&mut cron)?;
    let cron_job = &cron["cron"][0];
    if cron_job.get("id").and_then(serde_json::Value::as_str) != Some("configured")
        || cron_job.get("schedule").is_none()
        || cron_job.get("last_run_ms").is_some()
        || cron_job.get("next_run_ms").is_some()
        || cron_job.get("runs").is_some()
    {
        anyhow::bail!("Guardian v2 cron transform changed configuration or retained telemetry");
    }

    for namespace in desired.namespaces.values() {
        guardian_v2_parse_namespace(namespace.bytes.as_ref())?;
    }
    if guardian_v2_namespace_bytes(serde_json::json!({
        "projects": [{"project": "z"}, {"project": "a"}]
    }))
    .is_ok()
    {
        anyhow::bail!("Guardian v2 namespace accepted out-of-order projects");
    }

    let source_head = desired
        .full
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic source head is absent"))?;
    let parsed_source_head = guardian_v2_parse_head(source_head.value.bytes.as_ref())?;
    let mut head_unknown: serde_json::Value =
        serde_json::from_slice(source_head.value.bytes.as_ref())?;
    head_unknown
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic head is not an object"))?
        .insert("unknown".to_string(), serde_json::Value::Null);
    if guardian_v2_parse_head(&guardian_v2_canonical_json_bytes(head_unknown)?).is_ok() {
        anyhow::bail!("Guardian v2 head accepted an unknown field");
    }
    let mut head_missing: serde_json::Value =
        serde_json::from_slice(source_head.value.bytes.as_ref())?;
    head_missing
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic head is not an object"))?
        .remove("state");
    if guardian_v2_parse_head(&guardian_v2_canonical_json_bytes(head_missing)?).is_ok()
        || guardian_v2_parse_head(&vec![b' '; GUARDIAN_V2_HEAD_MAX_BYTES + 1]).is_ok()
    {
        anyhow::bail!("Guardian v2 head accepted a missing field or oversized body");
    }

    let first_part = desired
        .parts
        .first()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic source part is absent"))?;
    let first_kind = GuardianV2PartKind::State;
    if !first_part
        .value
        .bytes
        .starts_with(GUARDIAN_V2_PART_ENVELOPE_MAGIC)
    {
        anyhow::bail!("Guardian v2 prepared part omitted the compression envelope");
    }
    let first_raw = guardian_v2_decode_part(first_part.value.bytes.as_ref())?;
    let legacy_reference = GuardianV2Reference {
        sha256: hex::encode(sha256(first_raw.as_ref())),
        bytes: first_raw.len(),
    };
    if guardian_v2_parse_part(first_kind, first_raw.as_ref(), &legacy_reference).is_err() {
        anyhow::bail!("Guardian v2 part decoder rejected a legacy raw wrapper");
    }
    let mut unknown_version = first_part.value.bytes.as_ref().clone();
    unknown_version[GUARDIAN_V2_PART_ENVELOPE_MAGIC.len()] =
        GUARDIAN_V2_PART_ENVELOPE_VERSION.saturating_add(1);
    let unknown_reference = GuardianV2Reference {
        sha256: hex::encode(sha256(&unknown_version)),
        bytes: unknown_version.len(),
    };
    if guardian_v2_parse_part(first_kind, &unknown_version, &unknown_reference).is_ok() {
        anyhow::bail!("Guardian v2 part decoder accepted an unknown envelope version");
    }
    let mut malformed_wrapper: serde_json::Value = serde_json::from_slice(first_raw.as_ref())?;
    malformed_wrapper
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic wrapper is not an object"))?
        .insert("unknown".to_string(), serde_json::Value::Null);
    let malformed_wrapper = guardian_v2_canonical_json_bytes(malformed_wrapper)?;
    let malformed_reference = GuardianV2Reference {
        sha256: hex::encode(sha256(&malformed_wrapper)),
        bytes: malformed_wrapper.len(),
    };
    if guardian_v2_parse_part(first_kind, &malformed_wrapper, &malformed_reference).is_ok() {
        anyhow::bail!("Guardian v2 part accepted a non-exact wrapper");
    }

    let mut changed_references = parsed_source_head.references.clone();
    let changed_metrics_raw = guardian_v2_wrapper_bytes(
        GuardianV2PartKind::MetricsRollup,
        serde_json::json!({"guardian_v2_diagnostic": nonce}),
    )?;
    let changed_metrics = guardian_v2_encode_part(&changed_metrics_raw)?;
    let changed_metrics_sha = hex::encode(sha256(&changed_metrics));
    let original_metrics_sha = changed_references
        .get(GuardianV2PartKind::MetricsRollup.field())
        .map(|reference| reference.sha256.clone())
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 diagnostic metrics reference is absent"))?;
    if changed_metrics_sha == original_metrics_sha {
        anyhow::bail!("Guardian v2 diagnostic metrics mutation did not change its address");
    }
    changed_references.insert(
        GuardianV2PartKind::MetricsRollup.field().to_string(),
        GuardianV2Reference {
            sha256: changed_metrics_sha,
            bytes: changed_metrics.len(),
        },
    );
    let changed_head = guardian_v2_head_bytes(&changed_references)?;
    if sha256(&changed_head) == source_head.value.change_digest {
        anyhow::bail!("Guardian v2 metrics-only mutation did not change the head");
    }
    for kind in GuardianV2PartKind::ALL {
        if kind != GuardianV2PartKind::MetricsRollup
            && changed_references
                .get(kind.field())
                .map(|reference| &reference.sha256)
                != parsed_source_head
                    .references
                    .get(kind.field())
                    .map(|reference| &reference.sha256)
        {
            anyhow::bail!("Guardian v2 metrics-only mutation changed {}", kind.field());
        }
    }

    let (fixture_parts, fixture_head) =
        guardian_v2_diagnostic_generation(desired, &format!("guardian-v2-diag-fixture-{nonce}"))?;
    let fixture_snapshot = guardian_v2_diagnostic_snapshot(&fixture_parts, &fixture_head)?;
    let legacy_bytes = serde_json::to_vec(&fixture_snapshot)?;

    let legacy_peer = format!("guardian-v2-diag-legacy-peer-{nonce}");
    h.kv.put(
        &format!("node/{legacy_peer}/snapshot"),
        legacy_bytes.clone(),
    )
    .await?;
    let legacy_selected = fetch_newest_peer_snapshot().await;
    if legacy_selected.as_ref().map(|selected| selected.0.as_str()) != Some(legacy_peer.as_str()) {
        anyhow::bail!("Guardian v2 peer fallback did not select legacy when v2 was absent");
    }

    let legacy_self = format!("guardian-v2-diag-legacy-self-{nonce}");
    let legacy_self_key = format!("node/{legacy_self}/snapshot");
    h.kv.put(&legacy_self_key, legacy_bytes.clone()).await?;
    if !matches!(
        fetch_snapshot_at_result(&legacy_self_key).await,
        SnapshotFetch::Ready(_)
    ) {
        anyhow::bail!("Guardian v2 self fallback did not select legacy after two absent probes");
    }

    let mut first_incomplete_peer: Option<String> = None;
    for cut in 0..=GuardianV2PartKind::ALL.len() {
        let component = format!("guardian-v2-diag-cut-{cut}-{nonce}");
        let (parts, head) = guardian_v2_diagnostic_generation(desired, &component)?;
        if cut > 0 {
            let mut hashes = parts
                .iter()
                .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
                .collect::<Vec<_>>();
            hashes.push(iroh_blobs::Hash::new(head.value.bytes.as_ref()));
            let mut protection = h.client.protect_hashes(hashes).await?;
            protection.finish_tag_installation();
            for part in parts.iter().take(cut) {
                h.kv.put_gc_protected(&part.key, part.value.bytes.as_ref().clone(), &protection)
                    .await?;
            }
            drop(protection);
            first_incomplete_peer.get_or_insert(component.clone());
        }
        let encoded = guardian_v2_encode_component(&component)?;
        let probe = guardian_v2_probe(h, &encoded).await;
        if (cut == 0 && !matches!(probe, GuardianV2Probe::Absent))
            || (cut > 0 && !matches!(probe, GuardianV2Probe::Incomplete))
        {
            anyhow::bail!("Guardian v2 pre-head cut {cut} had the wrong tri-state");
        }
    }

    if fetch_newest_peer_snapshot().await.is_some() {
        anyhow::bail!("Guardian v2 incomplete peer population did not suppress legacy peers");
    }

    let incomplete_self = format!("guardian-v2-diag-incomplete-self-{nonce}");
    let incomplete_self_key = format!("node/{incomplete_self}/snapshot");
    h.kv.put(&incomplete_self_key, legacy_bytes.clone()).await?;
    let (incomplete_parts, _) = guardian_v2_diagnostic_generation(desired, &incomplete_self)?;
    h.kv.put(
        &incomplete_parts[0].key,
        incomplete_parts[0].value.bytes.as_ref().clone(),
    )
    .await?;
    if !matches!(
        fetch_snapshot_at_result(&incomplete_self_key).await,
        SnapshotFetch::Incomplete
    ) {
        anyhow::bail!("Guardian v2 incomplete self did not suppress legacy");
    }

    for missing in 0..GuardianV2PartKind::ALL.len() {
        let component = format!("guardian-v2-diag-missing-{missing}-{nonce}");
        let (parts, head) = guardian_v2_diagnostic_generation(desired, &component)?;
        let mut hashes = parts
            .iter()
            .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
            .collect::<Vec<_>>();
        hashes.push(iroh_blobs::Hash::new(head.value.bytes.as_ref()));
        let mut protection = h.client.protect_hashes(hashes).await?;
        protection.finish_tag_installation();
        for (index, part) in parts.iter().enumerate() {
            if index != missing {
                h.kv.put_gc_protected(&part.key, part.value.bytes.as_ref().clone(), &protection)
                    .await?;
            }
        }
        h.kv.put_gc_protected(&head.key, head.value.bytes.as_ref().clone(), &protection)
            .await?;
        let encoded = guardian_v2_encode_component(&component)?;
        if !matches!(
            guardian_v2_probe(h, &encoded).await,
            GuardianV2Probe::Incomplete
        ) {
            anyhow::bail!("Guardian v2 accepted a head missing part index {missing}");
        }
        drop(protection);
    }

    let collision_component = format!("guardian-v2-diag-collision-{nonce}");
    let (collision_parts, collision_head) =
        guardian_v2_diagnostic_generation(desired, &collision_component)?;
    h.kv.put(&collision_parts[0].key, b"immutable-collision".to_vec())
        .await?;
    let mut collision_hashes = collision_parts
        .iter()
        .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
        .collect::<Vec<_>>();
    collision_hashes.push(iroh_blobs::Hash::new(collision_head.value.bytes.as_ref()));
    let mut collision_protection = h.client.protect_hashes(collision_hashes).await?;
    collision_protection.finish_tag_installation();
    if guardian_v2_publish_immutable(h, &collision_parts[0], &collision_protection)
        .await
        .is_ok()
    {
        anyhow::bail!("Guardian v2 immutable collision was not rejected");
    }
    drop(collision_protection);

    let ready_peer = format!("guardian-v2-diag-ready-peer-{nonce}");
    let (ready_parts, ready_head) = guardian_v2_diagnostic_generation(desired, &ready_peer)?;
    let first_writes =
        guardian_v2_diagnostic_publish_generation(h, &ready_parts, &ready_head).await?;
    let no_op_writes =
        guardian_v2_diagnostic_publish_generation(h, &ready_parts, &ready_head).await?;
    if first_writes != GuardianV2PartKind::ALL.len() + 1 || no_op_writes != 0 {
        anyhow::bail!(
            "Guardian v2 semantic no-op diagnostic wrote {no_op_writes} values after {first_writes} first-pass writes"
        );
    }
    let repair_part = ready_parts
        .first()
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 repair diagnostic part is absent"))?;
    let repair_hash = iroh_blobs::Hash::new(repair_part.value.bytes.as_ref());
    let repair_hash_hex = repair_hash.to_hex();
    let repair_heads = h.kv.entry_heads().await?;
    let repair_before = guardian_v2_latest_entry(&repair_heads, &repair_part.key)
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 repair diagnostic metadata is absent"))?;
    let repair_timestamp = repair_before.timestamp;
    if repair_before.hash != repair_hash_hex || repair_before.content_local != Some(true) {
        anyhow::bail!("Guardian v2 repair diagnostic did not start from exact local content");
    }

    guardian_v2_diagnostic_remove_local_blob(h, repair_hash).await?;
    let missing_heads = h.kv.entry_heads().await?;
    let missing = guardian_v2_latest_entry(&missing_heads, &repair_part.key)
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 repair diagnostic lost its metadata"))?;
    if missing.hash != repair_hash_hex
        || missing.timestamp != repair_timestamp
        || missing.content_local != Some(false)
    {
        anyhow::bail!("Guardian v2 repair diagnostic did not retain exact unavailable metadata");
    }

    let mut repair_protection = h.client.protect_hash(repair_hash).await?;
    repair_protection.finish_tag_installation();
    if !guardian_v2_publish_immutable(h, repair_part, &repair_protection).await? {
        anyhow::bail!("Guardian v2 unavailable immutable content was not repaired");
    }
    drop(repair_protection);
    let repaired_heads = h.kv.entry_heads().await?;
    let repaired = guardian_v2_latest_entry(&repaired_heads, &repair_part.key)
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 repaired metadata is absent"))?;
    if repaired.hash != repair_hash_hex
        || repaired.content_local != Some(true)
        || h.kv.get(&repair_part.key).await?.as_deref() != Some(repair_part.value.bytes.as_slice())
    {
        anyhow::bail!("Guardian v2 immutable repair did not restore exact local bytes");
    }

    let selected_v2 = fetch_newest_peer_snapshot().await;
    if selected_v2.as_ref().map(|selected| selected.0.as_str()) != Some(ready_peer.as_str()) {
        anyhow::bail!("Guardian v2 peer selection did not prefer the valid v2 candidate");
    }

    let ready_self = format!("guardian-v2-diag-ready-self-{nonce}");
    let ready_self_key = format!("node/{ready_self}/snapshot");
    h.kv.put(&ready_self_key, legacy_bytes).await?;
    let (ready_self_parts, ready_self_head) =
        guardian_v2_diagnostic_generation(desired, &ready_self)?;
    guardian_v2_diagnostic_publish_generation(h, &ready_self_parts, &ready_self_head).await?;
    let verified_self =
        guardian_v2_verify_generation(h, &guardian_v2_encode_component(&ready_self)?).await?;
    match fetch_snapshot_at_result(&ready_self_key).await {
        SnapshotFetch::Ready(snapshot)
            if snapshot.saved_ms == verified_self.head.timestamp / 1000 => {}
        _ => anyhow::bail!("Guardian v2 ready self did not win over legacy"),
    }

    let gc_component = format!("guardian-v2-diag-gc-{nonce}");
    let (gc_parts, gc_head) = guardian_v2_diagnostic_generation(desired, &gc_component)?;
    let (_, _) = guardian_v2_validate_prepared_generation(&gc_parts, &gc_head)?;
    let mut gc_hashes = gc_parts
        .iter()
        .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
        .collect::<Vec<_>>();
    gc_hashes.push(iroh_blobs::Hash::new(gc_head.value.bytes.as_ref()));
    let mut gc_protection = h.client.protect_hashes(gc_hashes).await?;
    gc_protection.finish_tag_installation();
    let gc_registration = PreparedSnapshotRegistration::new(
        &gc_head.key,
        gc_head.value.change_digest,
        gc_parts.iter().map(|part| part.key.clone()).collect(),
    );
    let successful_runs_before = h.client.backend().gc_health().await.successful_runs;
    for part in &gc_parts {
        guardian_v2_publish_immutable(h, part, &gc_protection).await?;
    }
    guardian_v2_diagnostic_wait_for_gc(h, successful_runs_before).await?;
    for part in &gc_parts {
        let hash = iroh_blobs::Hash::new(part.value.bytes.as_ref()).to_hex();
        if !h.client.has_blob_local(&hash).await {
            anyhow::bail!("Guardian v2 protected part disappeared during concurrent GC");
        }
    }
    h.kv.put_gc_protected(
        &gc_head.key,
        gc_head.value.bytes.as_ref().clone(),
        &gc_protection,
    )
    .await?;
    guardian_v2_verify_generation(h, &guardian_v2_encode_component(&gc_component)?).await?;
    if !h
        .client
        .has_blob_local(&iroh_blobs::Hash::new(gc_head.value.bytes.as_ref()).to_hex())
        .await
    {
        anyhow::bail!("Guardian v2 protected head disappeared during concurrent GC");
    }
    drop(gc_protection);
    drop(gc_registration);

    Ok(format!(
        "GUARDIAN_V2_DIAGNOSTIC_OK nonce={nonce} codec=ok canonical=ok transforms=ok cuts=7 missing=6 collision=ok repair=ok fallback=ok noop=ok gc=ok incomplete_peer={}",
        first_incomplete_peer.unwrap_or_default()
    ))
}

#[cfg(debug_assertions)]
async fn maybe_guardian_v2_diagnostic(
    h: &Handle,
    desired: &DesiredReplication,
) -> anyhow::Result<()> {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let enabled = std::env::var("HIVE_GUARDIAN_V2_DIAGNOSTIC")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled || STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    let report = guardian_v2_diagnostic(h, desired).await?;
    tracing::info!(report, "Guardian v2 disposable diagnostic passed");
    Ok(())
}

/// Debug-only fault injection on the REAL write path, armed one-shot by the lifecycle diagnostic
/// right before its terminal replicate: holds the final v2 commit for
/// `GUARDIAN_FINAL_FLUSH_DIAGNOSTIC_HOLD_MS` so `await_final_generation` can be driven into its
/// timeout (with a short `HIVE_GUARDIAN_SHUTDOWN_TIMEOUT_MS`) while the previous head stays
/// untouched. Unarmed it changes nothing; release builds compile it out.
#[cfg(debug_assertions)]
static FINAL_FLUSH_HOLD_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

async fn write_replication_batch(
    desired: &DesiredReplication,
    _committed: &CommittedReplication,
    owned_namespaces: &mut std::collections::BTreeMap<String, SnapshotDigest>,
) -> anyhow::Result<ReplicationWriteStats> {
    let h = handle().await?;
    #[cfg(debug_assertions)]
    maybe_guardian_v2_diagnostic(h, desired).await?;

    let mut stats = ReplicationWriteStats::default();
    let heads_before = h.kv.entry_heads().await?;
    let mut reused = std::collections::BTreeSet::new();
    for (key, value) in &desired.namespaces {
        let live_matches = guardian_v2_current_exact_in(&heads_before, h, key, value).await?;
        {
            let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
            counters.namespaces.record_attempt(value.bytes.len());
            if live_matches {
                counters.namespaces.record_no_op(value.bytes.len());
                reused.insert(key.clone());
            }
        }
        if !live_matches {
            let put_result = h.kv.put(key, value.bytes.as_ref().clone()).await;
            if put_result.is_ok() {
                write_counters()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .namespaces
                    .record_committed(value.bytes.len());
            } else {
                write_counters()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .namespaces
                    .record_failed();
            }
            put_result
                .map_err(|error| anyhow::anyhow!("Guardian v2 namespace put {key}: {error}"))?;
            stats.namespace_puts += 1;
            stats.put_bytes += value.bytes.len();
        }
    }

    let heads_after = h.kv.entry_heads().await?;
    for (key, value) in &desired.namespaces {
        guardian_v2_verify_namespace(
            h,
            &heads_after,
            &heads_before,
            key,
            value,
            reused.contains(key),
        )
        .await?;
        owned_namespaces.insert(key.clone(), value.change_digest);
    }

    if let Some(head) = &desired.full {
        let (encoded_component, prepared_head) =
            guardian_v2_validate_prepared_generation(&desired.parts, head)?;
        let _lifecycle = SNAPSHOT_LIFECYCLE_LOCK.lock().await;

        #[cfg(debug_assertions)]
        if let Some(hold_ms) = std::env::var("GUARDIAN_FINAL_FLUSH_DIAGNOSTIC_HOLD_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .filter(|_| FINAL_FLUSH_HOLD_ARMED.swap(false, std::sync::atomic::Ordering::SeqCst))
        {
            tracing::warn!(
                hold_ms,
                "GUARDIAN_FINAL_FLUSH_DIAGNOSTIC_HOLD_MS armed — holding the v2 commit"
            );
            tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
        }

        let mut prepared_hashes = desired
            .parts
            .iter()
            .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
            .collect::<Vec<_>>();
        prepared_hashes.push(iroh_blobs::Hash::new(head.value.bytes.as_ref()));
        let mut protection = h
            .client
            .protect_hashes(prepared_hashes)
            .await
            .map_err(|error| anyhow::anyhow!("Guardian v2 generation protect: {error}"))?;
        protection.finish_tag_installation();
        let registration = PreparedSnapshotRegistration::new(
            &head.key,
            head.value.change_digest,
            desired.parts.iter().map(|part| part.key.clone()).collect(),
        );

        for part in &desired.parts {
            {
                let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
                counters
                    .parts
                    .entry(part.class.to_string())
                    .or_default()
                    .record_attempt(part.value.bytes.len());
            }
            match guardian_v2_publish_immutable(h, part, &protection).await {
                Ok(true) => {
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .parts
                        .entry(part.class.to_string())
                        .or_default()
                        .record_committed(part.value.bytes.len());
                    stats.full_puts += 1;
                    stats.put_bytes += part.value.bytes.len();
                }
                Ok(false) => {
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .parts
                        .entry(part.class.to_string())
                        .or_default()
                        .record_no_op(part.value.bytes.len());
                }
                Err(error) => {
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .parts
                        .entry(part.class.to_string())
                        .or_default()
                        .record_failed();
                    return Err(error);
                }
            }
        }

        let head_matches = guardian_v2_current_exact(h, &head.key, &head.value).await?;
        {
            let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
            counters.full.record_attempt(head.value.bytes.len());
            if head_matches {
                counters.full.record_no_op(head.value.bytes.len());
            }
        }
        if !head_matches {
            let put_result =
                h.kv.put_gc_protected(&head.key, head.value.bytes.as_ref().clone(), &protection)
                    .await;
            if put_result.is_ok() {
                write_counters()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .full
                    .record_committed(head.value.bytes.len());
            } else {
                write_counters()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .full
                    .record_failed();
            }
            put_result
                .map_err(|error| anyhow::anyhow!("Guardian v2 head put {}: {error}", head.key))?;
            stats.full_puts += 1;
            stats.put_bytes += head.value.bytes.len();
        }

        let verified = guardian_v2_verify_generation(h, &encoded_component).await?;
        if verified.head_sha256 != head.value.change_digest
            || verified.head.hash != iroh_blobs::Hash::new(head.value.bytes.as_ref()).to_hex()
            || verified.snapshot.saved_ms != verified.head.timestamp / 1000
        {
            anyhow::bail!("Guardian v2 committed head does not match the prepared generation");
        }
        for (kind, part) in GuardianV2PartKind::ALL.into_iter().zip(&desired.parts) {
            let reference = prepared_head.references.get(kind.field()).ok_or_else(|| {
                anyhow::anyhow!(
                    "Guardian v2 prepared reference {} disappeared",
                    kind.field()
                )
            })?;
            let identity = verified.part_heads.get(kind.field()).ok_or_else(|| {
                anyhow::anyhow!("Guardian v2 verified part {} disappeared", kind.field())
            })?;
            if identity.hash != iroh_blobs::Hash::new(part.value.bytes.as_ref()).to_hex()
                || reference.sha256 != hex::encode(part.value.change_digest)
                || reference.bytes != part.value.bytes.len()
            {
                anyhow::bail!(
                    "Guardian v2 committed {} does not match the prepared generation",
                    kind.field()
                );
            }
        }

        {
            let generation_key = guardian_v2_generation_key(
                &encoded_component,
                verified.snapshot.saved_ms,
                &hex::encode(head.value.change_digest),
            )?;
            let generation_item = KeyedPreparedValue {
                key: generation_key,
                class: "generation",
                value: head.value.clone(),
            };
            {
                let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
                counters
                    .generation
                    .record_attempt(generation_item.value.bytes.len());
            }
            match guardian_v2_publish_immutable(h, &generation_item, &protection).await {
                Ok(true) => {
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .generation
                        .record_committed(generation_item.value.bytes.len());
                    stats.full_puts += 1;
                    stats.put_bytes += generation_item.value.bytes.len();
                }
                Ok(false) => {
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .generation
                        .record_no_op(generation_item.value.bytes.len());
                }
                Err(error) => {
                    write_counters()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .generation
                        .record_failed();
                    return Err(error);
                }
            }
        }

        drop(protection);
        drop(registration);
    } else if !desired.parts.is_empty() {
        anyhow::bail!("Guardian v2 parts exist without a head commit marker");
    }

    Ok(stats)
}

fn guardian_v2_retention_cadence() -> std::time::Duration {
    std::env::var("HIVE_GUARDIAN_PART_REAP_CHECK_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(600))
}

async fn run_guardian_v2_retention_pass() {
    match handle().await {
        Ok(h) => {
            let report = guardian_v2_retention_pass(h).await;
            tracing::info!(
                status = %report.status,
                reason = report.reason.as_deref().unwrap_or(""),
                generations_total = report.generations_total,
                generations_retired = report.generations_retired,
                parts_listed = report.parts_listed,
                parts_retired = report.parts_retired,
                parts_unrooted_bytes = report.parts_unrooted_bytes,
                parts_skipped_young = report.parts_skipped_young,
                cursor = report.cursor.as_deref().unwrap_or(""),
                "Guardian v2 retention pass"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "Guardian v2 retention pass could not open GuardianDB");
        }
    }
}

async fn replication_writer(
    mut receiver: tokio::sync::watch::Receiver<Arc<DesiredReplication>>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut committed = CommittedReplication::default();
    let mut owned_namespaces = std::collections::BTreeMap::new();
    let mut retry_delay = REPLICATION_RETRY_MIN;
    let retention_cadence = guardian_v2_retention_cadence();
    let mut next_retention = tokio::time::Instant::now() + retention_cadence;
    loop {
        if tokio::time::Instant::now() >= next_retention {
            run_guardian_v2_retention_pass().await;
            next_retention = tokio::time::Instant::now() + retention_cadence;
        }
        let desired = receiver.borrow_and_update().clone();
        set_generation_status(
            desired.generation,
            ReplicationGenerationStatus::Writing,
            None,
        );
        match write_replication_batch(&desired, &committed, &mut owned_namespaces).await {
            Ok(stats) => {
                committed = desired.committed_state();
                owned_namespaces.clone_from(&committed.namespaces);
                retry_delay = REPLICATION_RETRY_MIN;
                set_generation_status(
                    desired.generation,
                    ReplicationGenerationStatus::Committed,
                    None,
                );
                tracing::info!(
                    generation = desired.generation,
                    namespace_puts = stats.namespace_puts,
                    full_puts = stats.full_puts,
                    deletes = stats.deletes,
                    put_bytes = stats.put_bytes,
                    semantic_noop = stats.put_bytes == 0 && stats.deletes == 0,
                    "GuardianDB snapshot batch committed"
                );
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = tokio::time::sleep_until(next_retention) => {}
                }
            }
            Err(error) => {
                set_generation_status(
                    desired.generation,
                    ReplicationGenerationStatus::Failed,
                    Some(error.to_string()),
                );
                tracing::warn!(
                    generation = desired.generation,
                    error = %error,
                    retry_ms = retry_delay.as_millis() as u64,
                    "GuardianDB snapshot batch failed; retaining exact desired generation for retry"
                );
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(retry_delay) => {}
                }
                retry_delay = std::cmp::min(retry_delay.saturating_mul(2), REPLICATION_RETRY_MAX);
            }
        }
    }
}

/// Starts the writer on the BULKHEAD runtime, never the serving one: `write_replication_batch`
/// re-decompresses, re-parses and re-canonicalizes the whole snapshot TWICE per admitted generation
/// as plain synchronous code (seconds of CPU per generation). `bulkhead::spawn` holds its own
/// runtime handle, so it also works from the `hive-persister` OS thread, which has no runtime
/// context.
fn ensure_replication_writer(
    initial: Arc<DesiredReplication>,
) -> &'static tokio::sync::watch::Sender<Arc<DesiredReplication>> {
    REPLICATION_QUEUE.get_or_init(|| {
        let (sender, receiver) = tokio::sync::watch::channel(initial);
        let shutdown = REPLICATION_SHUTDOWN
            .get_or_init(tokio_util::sync::CancellationToken::new)
            .clone();
        let task = crate::bulkhead::spawn(replication_writer(receiver, shutdown));
        let slot = REPLICATION_WRITER.get_or_init(|| std::sync::Mutex::new(None));
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
        sender
    })
}

/// Admits one exact replication generation: one document per tenant namespace PLUS the full
/// snapshot under this node's own key (`node/<name>/snapshot`), the durable copy the boot-time
/// rollback guard restores from. Once shutdown closes admission this returns `None`; no caller can
/// enqueue work behind the final drain target.
pub fn replicate(snap: &PlatformSnapshot) -> Option<u64> {
    let mut lifecycle = replication_lifecycle()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !lifecycle.admission_open {
        tracing::error!(
            final_generation = ?lifecycle.final_generation,
            "Guardian replication admission is closed; refusing a post-barrier snapshot"
        );
        return None;
    }
    let Some(generation) = lifecycle.next_generation.checked_add(1) else {
        tracing::error!("GuardianDB snapshot generation exhausted");
        return None;
    };
    let desired = match prepare_replication(snap, generation) {
        Ok(desired) => desired,
        Err(error) => {
            tracing::warn!(error = %error, "GuardianDB snapshot preparation failed before admission");
            return None;
        }
    };
    lifecycle.next_generation = generation;
    for status in lifecycle.statuses.values_mut() {
        if *status == ReplicationGenerationStatus::Queued {
            *status = ReplicationGenerationStatus::Superseded;
        }
    }
    lifecycle
        .statuses
        .insert(generation, ReplicationGenerationStatus::Queued);
    while lifecycle.statuses.len() > 256 {
        let Some(oldest) = lifecycle.statuses.keys().next().copied() else {
            break;
        };
        if matches!(
            lifecycle.statuses.get(&oldest),
            Some(ReplicationGenerationStatus::Queued | ReplicationGenerationStatus::Writing)
        ) {
            break;
        }
        lifecycle.statuses.remove(&oldest);
        lifecycle.failures.remove(&oldest);
    }
    let sender = ensure_replication_writer(desired.clone());
    publish_replication(sender, desired);
    drop(lifecycle);
    replication_notify().notify_waiters();
    Some(generation)
}

/// Close snapshot admission at the exact generation returned by the terminal
/// file save. Idempotent calls must name the same target.
pub fn close_replication_admission(final_generation: Option<u64>) -> anyhow::Result<Option<u64>> {
    let mut lifecycle = replication_lifecycle()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !lifecycle.admission_open {
        if lifecycle.final_generation != final_generation {
            anyhow::bail!(
                "Guardian replication admission already closed at {:?}, not {:?}",
                lifecycle.final_generation,
                final_generation
            );
        }
        return Ok(lifecycle.final_generation);
    }
    if final_generation.is_some_and(|generation| generation != lifecycle.next_generation) {
        anyhow::bail!(
            "Guardian final generation {:?} is not the latest admitted generation {}",
            final_generation,
            lifecycle.next_generation
        );
    }
    lifecycle.admission_open = false;
    lifecycle.final_generation = final_generation;
    tracing::info!(
        final_generation = ?final_generation,
        "Guardian replication admission closed"
    );
    Ok(final_generation)
}

/// Failure-path close for when the terminal file write cannot complete: freezes Guardian at the
/// latest already-admitted generation so the backend can drain and shut down without accepting work
/// during process exit.
pub fn close_replication_admission_at_latest() -> Option<u64> {
    let mut lifecycle = replication_lifecycle()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if lifecycle.admission_open {
        lifecycle.admission_open = false;
        lifecycle.final_generation =
            (lifecycle.next_generation > 0).then_some(lifecycle.next_generation);
        tracing::error!(
            final_generation = ?lifecycle.final_generation,
            "Guardian replication admission force-closed after terminal persistence failure"
        );
    }
    lifecycle.final_generation
}

async fn await_final_generation(
    generation: u64,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let notified = replication_notify().notified();
        let (status, failure) = {
            let lifecycle = replication_lifecycle()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                lifecycle.statuses.get(&generation).copied(),
                lifecycle.failures.get(&generation).cloned(),
            )
        };
        match status {
            Some(ReplicationGenerationStatus::Committed) => return Ok(()),
            Some(ReplicationGenerationStatus::Superseded) => {
                anyhow::bail!("Guardian final generation {generation} was superseded")
            }
            Some(ReplicationGenerationStatus::TimedOut) => {
                anyhow::bail!("Guardian final generation {generation} previously timed out")
            }
            None => anyhow::bail!("Guardian final generation {generation} is untracked"),
            Some(
                ReplicationGenerationStatus::Queued
                | ReplicationGenerationStatus::Writing
                | ReplicationGenerationStatus::Failed,
            ) => {}
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            set_generation_status(
                generation,
                ReplicationGenerationStatus::TimedOut,
                failure.clone(),
            );
            anyhow::bail!(
                "Guardian final generation {generation} timed out after {}ms (last failure: {})",
                timeout.as_millis(),
                failure.as_deref().unwrap_or("none")
            );
        }
    }
}

/// Budget for EACH of `shutdown`'s three sequential waits (final generation, writer join, backend
/// shutdown): 10 s, because the writer commits one generation per 40-75 s here, so a wait that
/// needs a full cadence cannot succeed anyway and only keeps a node with a closed listener alive as
/// a mesh black hole under the 75 s hard deadline.
fn guardian_shutdown_timeout() -> std::time::Duration {
    std::env::var("HIVE_GUARDIAN_SHUTDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(10))
}

/// Drains the terminal replication generation, stops and joins its sole writer, then durably shuts
/// down Docs, Router, blob store and endpoint. `None` means the caller could not name a terminal
/// generation (the terminal file save failed): admission is then force-closed at the LATEST
/// admitted generation and that exact generation is drained, never abandoned mid-write.
pub async fn shutdown(final_generation: Option<u64>) -> anyhow::Result<()> {
    let final_generation = match final_generation {
        Some(generation) => close_replication_admission(Some(generation))?,
        None => close_replication_admission_at_latest(),
    };
    let timeout = guardian_shutdown_timeout();
    if let Some(generation) = final_generation {
        await_final_generation(generation, timeout).await?;
        tracing::info!(
            generation,
            "Guardian final replication generation durably committed"
        );
    }

    if let Some(token) = REPLICATION_SHUTDOWN.get() {
        token.cancel();
    }
    if let Some(slot) = REPLICATION_WRITER.get() {
        let task = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut task) = task {
            match tokio::time::timeout(timeout, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    anyhow::bail!("Guardian replication writer failed during shutdown: {error}")
                }
                Err(_) => {
                    task.abort();
                    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
                    anyhow::bail!(
                        "Guardian replication writer did not terminate within {}ms; abort requested and handle retained",
                        timeout.as_millis()
                    );
                }
            }
        }
    }

    if let Some(handle) = HANDLE.get() {
        tokio::time::timeout(timeout, handle.client.shutdown())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Guardian backend shutdown timed out after {}ms",
                    timeout.as_millis()
                )
            })??;
    }
    tracing::info!("Guardian durable shutdown complete");
    Ok(())
}

enum SnapshotFetch {
    Absent,
    Incomplete,
    Ready(PlatformSnapshot),
}

struct GuardianV2Verified {
    snapshot: PlatformSnapshot,
    head: SnapshotKeyHead,
    head_sha256: SnapshotDigest,
    part_heads: std::collections::BTreeMap<String, SnapshotKeyHead>,
}

enum GuardianV2Probe {
    Absent,
    Incomplete,
    Ready(GuardianV2Verified),
}

async fn guardian_v2_verify_generation(
    h: &Handle,
    encoded_component: &str,
) -> anyhow::Result<GuardianV2Verified> {
    guardian_v2_decode_component(encoded_component)?;
    let head_key = format!("{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/head");
    let before = h.kv.entry_heads().await?;
    let head_source = guardian_v2_latest_entry(&before, &head_key)
        .ok_or_else(|| anyhow::anyhow!("Guardian v2 head metadata is absent"))?;
    if head_source.content_local == Some(false) {
        anyhow::bail!("Guardian v2 head content is not local");
    }
    let head = SnapshotKeyHead {
        timestamp: head_source.timestamp,
        hash: head_source.hash.clone(),
    };
    let head_bytes =
        h.kv.get(&head_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 head bytes are absent"))?;
    if head_bytes.len() > GUARDIAN_V2_HEAD_MAX_BYTES
        || iroh_blobs::Hash::new(&head_bytes).to_hex() != head.hash
    {
        anyhow::bail!("Guardian v2 head length/BLAKE3 metadata mismatch");
    }
    let parsed_head = guardian_v2_parse_head(&head_bytes)?;
    let head_sha256 = sha256(&head_bytes);

    let mut part_values = std::collections::BTreeMap::new();
    let mut wrapper_bytes = std::collections::BTreeMap::new();
    let mut part_heads = std::collections::BTreeMap::new();
    for kind in GuardianV2PartKind::ALL {
        let reference = parsed_head.references.get(kind.field()).ok_or_else(|| {
            anyhow::anyhow!("Guardian v2 head reference {} is absent", kind.field())
        })?;
        let key = guardian_v2_part_key(encoded_component, kind, &reference.sha256)?;
        let source = guardian_v2_latest_entry(&before, &key)
            .ok_or_else(|| anyhow::anyhow!("Guardian v2 {} metadata is absent", kind.field()))?;
        if source.content_local == Some(false) {
            anyhow::bail!("Guardian v2 {} content is not local", kind.field());
        }
        let identity = SnapshotKeyHead {
            timestamp: source.timestamp,
            hash: source.hash.clone(),
        };
        let bytes =
            h.kv.get(&key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Guardian v2 {} bytes are absent", kind.field()))?;
        if iroh_blobs::Hash::new(&bytes).to_hex() != identity.hash {
            anyhow::bail!("Guardian v2 {} BLAKE3 metadata mismatch", kind.field());
        }
        let (value, raw) = guardian_v2_parse_part(kind, &bytes, reference)?;
        part_values.insert(kind.field().to_string(), value);
        wrapper_bytes.insert(kind.field().to_string(), raw.into_owned());
        part_heads.insert(kind.field().to_string(), identity);
    }

    let saved_ms = head.timestamp / 1000;
    let snapshot = guardian_v2_reconstruct(&part_values, &wrapper_bytes, saved_ms)?;
    let after = h.kv.entry_heads().await?;
    if latest_snapshot_head(&after, &head_key) != Some(head.clone()) {
        anyhow::bail!("Guardian v2 head metadata changed during verification");
    }
    for kind in GuardianV2PartKind::ALL {
        let reference = parsed_head.references.get(kind.field()).ok_or_else(|| {
            anyhow::anyhow!("Guardian v2 head reference {} disappeared", kind.field())
        })?;
        let key = guardian_v2_part_key(encoded_component, kind, &reference.sha256)?;
        if latest_snapshot_head(&after, &key) != part_heads.get(kind.field()).cloned() {
            anyhow::bail!(
                "Guardian v2 {} metadata changed during verification",
                kind.field()
            );
        }
    }

    Ok(GuardianV2Verified {
        snapshot,
        head,
        head_sha256,
        part_heads,
    })
}

async fn guardian_v2_probe(h: &Handle, encoded_component: &str) -> GuardianV2Probe {
    if guardian_v2_decode_component(encoded_component).is_err() {
        return GuardianV2Probe::Incomplete;
    }
    let prefix = format!("{GUARDIAN_V2_NODE_PREFIX}{encoded_component}/");
    let Ok(heads) = h.kv.entry_heads().await else {
        return GuardianV2Probe::Incomplete;
    };
    if !heads.iter().any(|head| head.key.starts_with(&prefix)) {
        return GuardianV2Probe::Absent;
    }
    let head_key = format!("{prefix}head");
    if guardian_v2_latest_entry(&heads, &head_key).is_none() {
        return GuardianV2Probe::Incomplete;
    }
    match guardian_v2_verify_generation(h, encoded_component).await {
        Ok(verified) => GuardianV2Probe::Ready(verified),
        Err(error) => {
            tracing::warn!(
                component = encoded_component,
                %error,
                "Guardian v2 snapshot is incomplete"
            );
            GuardianV2Probe::Incomplete
        }
    }
}

async fn fetch_legacy_snapshot_at_result(key: &str) -> SnapshotFetch {
    let Ok(h) = handle().await else {
        return SnapshotFetch::Incomplete;
    };
    let Ok(heads) = h.kv.entry_heads().await else {
        return SnapshotFetch::Incomplete;
    };
    let Some(head) = heads
        .into_iter()
        .filter(|head| head.key == key)
        .max_by_key(|head| head.timestamp)
    else {
        return SnapshotFetch::Absent;
    };
    macro_rules! incomplete {
        ($($arg:tt)*) => {{
            tracing::debug!(key, $($arg)*);
            return SnapshotFetch::Incomplete;
        }};
    }
    let Ok(Some(bytes)) = h.kv.get(key).await else {
        incomplete!("legacy fetch: base bytes are absent");
    };
    if iroh_blobs::Hash::new(&bytes).to_hex() != head.hash {
        incomplete!("legacy fetch: base bytes do not match the newest head");
    }
    let written_ms = head.timestamp / 1000;
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return SnapshotFetch::Incomplete;
    };
    let Some(root) = value.as_object_mut() else {
        return SnapshotFetch::Incomplete;
    };
    let manifest = root
        .remove(SNAPSHOT_PARTS_FIELD)
        .and_then(|manifest| manifest.as_object().cloned());

    if let Some(manifest) = manifest {
        let mut merged = std::collections::BTreeSet::new();
        for (field, reference) in manifest {
            if !SNAPSHOT_PART_FIELDS.contains(&field.as_str()) {
                incomplete!(field, "legacy fetch: manifest names an unknown field");
            }
            let (part_key, expected) = if let Some(expected) = reference.as_str() {
                (format!("{key}-part/{field}"), expected)
            } else {
                let Some(reference) = reference.as_object() else {
                    return SnapshotFetch::Incomplete;
                };
                let Some(part_key) = reference.get("key").and_then(|v| v.as_str()) else {
                    return SnapshotFetch::Incomplete;
                };
                let Some(expected) = reference.get("sha256").and_then(|v| v.as_str()) else {
                    return SnapshotFetch::Incomplete;
                };
                let current_prefix = format!("{key}-part-v2/{field}/");
                let prior_addressed_prefix = format!("{key}-part/{field}/");
                let non_prefix = snapshot_v3_prefix(key)
                    .ok()
                    .map(|prefix| format!("{prefix}/{field}/"));
                let suffix = part_key
                    .strip_prefix(&current_prefix)
                    .or_else(|| part_key.strip_prefix(&prior_addressed_prefix))
                    .or_else(|| {
                        non_prefix
                            .as_deref()
                            .and_then(|prefix| part_key.strip_prefix(prefix))
                    });
                if suffix != Some(expected)
                    || expected.len() != 64
                    || !expected
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    return SnapshotFetch::Incomplete;
                }
                (part_key.to_string(), expected)
            };
            let Ok(Some(part_bytes)) = h.kv.get(&part_key).await else {
                incomplete!(part_key, "legacy fetch: part bytes are absent");
            };
            if hex::encode(sha256(&part_bytes)) != expected {
                incomplete!(part_key, "legacy fetch: part digest mismatch");
            }
            let Ok(mut part) = serde_json::from_slice::<serde_json::Value>(&part_bytes) else {
                incomplete!(part_key, "legacy fetch: part is not valid JSON");
            };
            let Some(part_value) = part.as_object_mut().and_then(|part| part.remove(&field)) else {
                incomplete!(part_key, "legacy fetch: part is not an exact field wrapper");
            };
            merged.insert(field.clone());
            if part_value.is_null() {
                continue;
            }
            let Some(root) = value.as_object_mut() else {
                incomplete!("legacy fetch: base value is not an object mid-merge");
            };
            root.insert(field, part_value);
        }
        if SNAPSHOT_PART_FIELDS
            .iter()
            .any(|field| !merged.contains(*field))
        {
            incomplete!("legacy fetch: a manifest field is still missing after the merge");
        }
    }

    let snapshot = match serde_json::from_value::<PlatformSnapshot>(value) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            incomplete!(%error, "legacy fetch: merged snapshot does not deserialize");
        }
    };
    let mut snapshot = snapshot;
    snapshot.saved_ms = written_ms;
    SnapshotFetch::Ready(snapshot)
}

fn guardian_v2_component_from_legacy_key(key: &str) -> Option<&str> {
    key.strip_prefix("node/")
        .and_then(|rest| rest.strip_suffix("/snapshot"))
        .filter(|component| !component.is_empty())
}

async fn fetch_snapshot_at_result(key: &str) -> SnapshotFetch {
    let Some(component) = guardian_v2_component_from_legacy_key(key) else {
        return fetch_legacy_snapshot_at_result(key).await;
    };
    let Ok(encoded) = guardian_v2_encode_component(component) else {
        return SnapshotFetch::Incomplete;
    };
    let Ok(h) = handle().await else {
        return SnapshotFetch::Incomplete;
    };
    match guardian_v2_probe(h, &encoded).await {
        GuardianV2Probe::Ready(verified) => SnapshotFetch::Ready(verified.snapshot),
        GuardianV2Probe::Incomplete => SnapshotFetch::Incomplete,
        GuardianV2Probe::Absent => {
            tokio::task::yield_now().await;
            match guardian_v2_probe(h, &encoded).await {
                GuardianV2Probe::Ready(verified) => SnapshotFetch::Ready(verified.snapshot),
                GuardianV2Probe::Incomplete => SnapshotFetch::Incomplete,
                GuardianV2Probe::Absent => fetch_legacy_snapshot_at_result(key).await,
            }
        }
    }
}

async fn fetch_snapshot_at(key: &str) -> Option<PlatformSnapshot> {
    match fetch_snapshot_at_result(key).await {
        SnapshotFetch::Ready(snapshot) => Some(snapshot),
        SnapshotFetch::Absent | SnapshotFetch::Incomplete => None,
    }
}

/// The replicated full snapshot for THIS node, if GuardianDB holds one.
pub async fn fetch_node_snapshot() -> Option<PlatformSnapshot> {
    fetch_snapshot_at(&snapshot_key()?).await
}

struct GuardianV2PeerPopulation {
    components: std::collections::BTreeMap<String, String>,
    malformed: bool,
}

fn guardian_v2_peer_population(
    heads: &[guardian_db::traits::EntryHead],
    me: &str,
) -> GuardianV2PeerPopulation {
    let mut components = std::collections::BTreeMap::new();
    let mut malformed = false;
    for head in heads {
        let Some(rest) = head.key.strip_prefix(GUARDIAN_V2_NODE_PREFIX) else {
            continue;
        };
        let Some((encoded, suffix)) = rest.split_once('/') else {
            malformed = true;
            continue;
        };
        let decoded = match guardian_v2_decode_component(encoded) {
            Ok(decoded) => decoded,
            Err(_) => {
                malformed = true;
                continue;
            }
        };
        if decoded == me {
            continue;
        }
        if suffix.is_empty() {
            malformed = true;
            continue;
        }
        components.insert(encoded.to_string(), decoded);
    }
    GuardianV2PeerPopulation {
        components,
        malformed,
    }
}

/// The newest replicated snapshot from any OTHER node: any valid v2 generation outranks every
/// legacy candidate, an incomplete v2 population suppresses ALL legacy peer fallback, and the
/// chosen candidate is fetched and verified again right before it is returned.
pub async fn fetch_newest_peer_snapshot() -> Option<(String, PlatformSnapshot)> {
    let me = NODE_NAME.get()?.as_str();
    let h = handle().await.ok()?;
    let heads = h.kv.entry_heads().await.ok()?;
    let population = guardian_v2_peer_population(&heads, me);
    let mut incomplete_v2 = population.malformed;
    let mut ready_v2 = Vec::new();
    for (encoded, name) in &population.components {
        match guardian_v2_probe(h, encoded).await {
            GuardianV2Probe::Ready(verified) => {
                ready_v2.push((encoded.clone(), name.clone(), verified.snapshot.saved_ms));
            }
            GuardianV2Probe::Incomplete => incomplete_v2 = true,
            GuardianV2Probe::Absent => {}
        }
    }
    ready_v2.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.0.cmp(&right.0))
    });
    for (encoded, name, _) in ready_v2 {
        match guardian_v2_probe(h, &encoded).await {
            GuardianV2Probe::Ready(verified) => return Some((name, verified.snapshot)),
            GuardianV2Probe::Incomplete => incomplete_v2 = true,
            GuardianV2Probe::Absent => {}
        }
    }
    if incomplete_v2 {
        return None;
    }

    tokio::task::yield_now().await;
    let reprobe_heads = h.kv.entry_heads().await.ok()?;
    let reprobe = guardian_v2_peer_population(&reprobe_heads, me);
    if reprobe.malformed || !reprobe.components.is_empty() {
        return None;
    }

    let mut legacy_keys = std::collections::BTreeMap::new();
    for head in &reprobe_heads {
        let Some(name) = head
            .key
            .strip_prefix("node/")
            .and_then(|rest| rest.strip_suffix("/snapshot"))
            .filter(|name| !name.is_empty() && *name != me)
        else {
            continue;
        };
        legacy_keys.insert(name.to_string(), head.key.clone());
    }
    let mut ready_legacy = Vec::new();
    for (name, key) in legacy_keys {
        if let SnapshotFetch::Ready(snapshot) = fetch_legacy_snapshot_at_result(&key).await {
            ready_legacy.push((name, key, snapshot.saved_ms));
        }
    }
    ready_legacy.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
    for (name, key, _) in ready_legacy {
        if let SnapshotFetch::Ready(snapshot) = fetch_legacy_snapshot_at_result(&key).await {
            return Some((name, snapshot));
        }
    }
    None
}

fn strip_node_local(mut snap: PlatformSnapshot) -> PlatformSnapshot {
    snap.deployments = Vec::new();
    snap.sandboxes = Default::default();
    snap.metrics_rollup = Default::default();
    snap.database_data = Default::default();
    snap.builds = Vec::new();
    for job in &mut snap.cron {
        job.last_run_ms = None;
        job.next_run_ms = None;
        job.runs = 0;
    }
    snap
}

/// Boot-time restore-on-rollback guard: once GuardianDB is online, compares its replicated
/// snapshot's `saved_ms` with the CURRENT on-disk snapshot and, if the replica is NEWER
/// (crash-restored old disk, wiped data dir, bad copy), restores it into live state and rewrites
/// the local file; the comparison re-reads the disk at adoption time, so any post-boot mutation
/// vetoes adoption. With NO replicated snapshot of its own (total loss) it falls back to the newest
/// PEER snapshot, adopting only SHARED tenant/control-plane state (`strip_node_local`), and only
/// after every self retry is exhausted. Retries ~60 s (replicated keys reappear only after a peer
/// sync); cron stays node-local, so the local cron state survives adoption. Opt-out:
/// `HIVE_GUARDIAN_RESTORE=0`.
pub fn spawn_restore_guard(cloud: Arc<crate::state::CloudState>) {
    if std::env::var("HIVE_GUARDIAN_RESTORE")
        .map(|v| v == "0" || v == "false")
        .unwrap_or(false)
    {
        return;
    }
    tokio::spawn(async move {
        let Some(self_key) = snapshot_key() else {
            return;
        };
        let mut peer_fallback: Option<(String, PlatformSnapshot)> = None;
        let mut terminal_self_absent = false;
        for attempt in 1u32..=7 {
            if attempt > 1 {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
            match fetch_snapshot_at_result(&self_key).await {
                SnapshotFetch::Ready(replica) => {
                    let local = crate::persist::load();
                    if replica.saved_ms <= local.saved_ms {
                        tracing::debug!(
                            replica_ms = replica.saved_ms,
                            local_ms = local.saved_ms,
                            "guardian restore guard: local snapshot is current"
                        );
                        return;
                    }
                    tracing::warn!(
                        replica_ms = replica.saved_ms,
                        local_ms = local.saved_ms,
                        behind_secs = (replica.saved_ms.saturating_sub(local.saved_ms)) / 1000,
                        "SNAPSHOT ROLLBACK DETECTED — local state older than the GuardianDB replica; restoring from replica"
                    );
                    let local_cron = cloud.cron.list();
                    crate::persist::restore(&cloud, replica);
                    cloud.cron.replace_all(local_cron);
                    crate::persist::persist(&cloud);
                    tracing::info!(
                        "guardian restore guard: state restored from replicated snapshot"
                    );
                    return;
                }
                SnapshotFetch::Incomplete => {
                    terminal_self_absent = false;
                    tracing::debug!(
                        attempt,
                        "guardian restore guard: self snapshot metadata or committed bytes are incomplete; waiting for replication"
                    );
                    continue;
                }
                SnapshotFetch::Absent => {
                    terminal_self_absent = attempt == 7;
                }
            }

            if attempt == 1 {
                continue;
            }
            if let Some((peer, snapshot)) = fetch_newest_peer_snapshot().await {
                let replace = peer_fallback
                    .as_ref()
                    .is_none_or(|(_, held)| snapshot.saved_ms > held.saved_ms);
                if replace {
                    peer_fallback = Some((peer, snapshot));
                }
            }
        }

        if terminal_self_absent {
            if let Some((peer, snapshot)) = peer_fallback {
                let local = crate::persist::load();
                if snapshot.saved_ms > local.saved_ms {
                    tracing::warn!(
                        peer = %peer,
                        peer_ms = snapshot.saved_ms,
                        local_ms = local.saved_ms,
                        "TOTAL-LOSS RECOVERY — self snapshot remained absent through all retries; adopting SHARED state from newest peer snapshot (node-local runtime stripped)"
                    );
                    let local_cron = cloud.cron.list();
                    crate::persist::restore(&cloud, strip_node_local(snapshot));
                    if !local_cron.is_empty() {
                        cloud.cron.replace_all(local_cron);
                    }
                    crate::persist::persist(&cloud);
                    tracing::info!(peer = %peer, "guardian restore guard: shared state restored from peer snapshot");
                    return;
                }
                tracing::debug!(
                    peer = %peer,
                    peer_ms = snapshot.saved_ms,
                    local_ms = local.saved_ms,
                    "guardian restore guard: terminal peer fallback is not newer than local; keeping local"
                );
            }
        }
        tracing::debug!(
            "guardian restore guard: no complete self snapshot or eligible peer snapshot found"
        );
    });
}

/// Read a key back from GuardianDB (the durable, replicated copy).
pub async fn get(key: &str) -> Option<Vec<u8>> {
    let h = handle().await.ok()?;
    h.kv.get(key).await.ok().flatten()
}

/// Write an arbitrary key into GuardianDB (replicated). Best-effort — used for
/// cluster-shared artifacts like AEAD-encrypted TLS bundles (`tls/…`).
pub async fn put(key: &str, bytes: Vec<u8>) {
    match handle().await {
        Ok(h) => {
            if let Err(e) = h.kv.put(key, bytes).await {
                tracing::warn!(%key, error = %e, "GuardianDB put failed");
            }
        }
        Err(e) => tracing::warn!(%key, error = %e, "GuardianDB unavailable for put"),
    }
}

/// Removes a key from GuardianDB (a replicated tombstone); best-effort like `put`, used to retire
/// transient mesh-shared entries (a delivered or dead-lettered world_queue job).
pub async fn delete(key: &str) {
    match handle().await {
        Ok(h) => {
            if let Err(e) = h.kv.delete(key).await {
                tracing::warn!(%key, error = %e, "GuardianDB delete failed");
            }
        }
        Err(e) => tracing::warn!(%key, error = %e, "GuardianDB unavailable for delete"),
    }
}

/// Every key currently in GuardianDB (durable copy), via `KeyValueStore::keys()` and not
/// `all().into_keys()`: `all()` returns only lazily cached values and would silently under-report
/// keys not read since the last index rebuild.
pub async fn keys() -> Vec<String> {
    match handle().await {
        Ok(h) => h.kv.keys(),
        Err(_) => Vec::new(),
    }
}

/// Operator-declared roster of node NAMES that belong to this fleet (`HIVE_NODE_ROSTER`,
/// comma-separated); unset returns `None`, which makes [`reap_departed_node_snapshots`] refuse
/// everything. A NAME list in its own variable because `HIVE_TRUSTED_NODE_IDS` holds 64-hex peer
/// IDs, which cannot say whether `node/<name>/snapshot` belongs to a node we still run.
fn node_roster() -> Option<std::collections::HashSet<String>> {
    let raw = std::env::var("HIVE_NODE_ROSTER").ok()?;
    let set: std::collections::HashSet<String> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    (!set.is_empty()).then_some(set)
}

/// `(node name, is_part)` when `key` belongs to a node's snapshot generation (the base, or a part
/// under any legacy or current layout), else `None`.
fn node_snapshot_key_owner(key: &str) -> Option<(&str, bool)> {
    let segments: Vec<_> = key.split('/').collect();
    match segments.as_slice() {
        ["node", name, "snapshot"] if !name.is_empty() => Some((name, false)),
        ["node", name, "snapshot-part", field]
            if !name.is_empty() && SNAPSHOT_PART_FIELDS.contains(field) =>
        {
            Some((name, true))
        }
        ["node", name, "snapshot-part", field, digest]
            if !name.is_empty()
                && SNAPSHOT_PART_FIELDS.contains(field)
                && is_lower_hex_digest(digest) =>
        {
            Some((name, true))
        }
        ["node", name, "snapshot-part-v2", field, digest]
        | ["node", name, "parts-v3", field, digest]
            if !name.is_empty()
                && SNAPSHOT_PART_FIELDS.contains(field)
                && is_lower_hex_digest(digest) =>
        {
            Some((name, true))
        }
        ["node", name, "snapshot-part-reap", digest] | ["node", name, "part-reap-v3", digest]
            if !name.is_empty() && is_lower_hex_digest(digest) =>
        {
            Some((name, true))
        }
        _ => None,
    }
}

/// Reaps `node/<name>/snapshot` keys of nodes that left the fleet: a doc entry replicates forever
/// while its blob becomes permanently unfetchable once its only holder is gone, so `kv_store`'s
/// index sync retried the fetch on every pass and the noise masked genuine transient failures. Age
/// comes from the entry head's timestamp (`namespace_heads`, read without fetching bytes), since
/// the snapshot's own `saved_ms` is unreadable by construction. The delete is irreversible and fans
/// out to every peer, so it refuses rather than guesses (the `gc_rootfs_images` blast-radius
/// rules): no `HIVE_NODE_ROSTER` (fail closed: an unset roster makes EVERY node look departed),
/// more than `HIVE_REAP_MAX_FRACTION` (default 0.34) of node keys reapable (the roster is wrong,
/// not the fleet), and a node younger than `HIVE_REAP_MIN_AGE_DAYS` (default 30) is withheld; this
/// node's own key is never a candidate. Leader-only by caller contract (a replicated-store
/// mutation).
pub async fn reap_departed_node_snapshots() -> (usize, usize) {
    let Some(me) = NODE_NAME
        .get()
        .filter(|name| !name.trim().is_empty())
        .cloned()
    else {
        tracing::error!(
            "guardian reap: local node identity is uninitialized or empty — refusing (fail closed)"
        );
        return (0, 0);
    };
    let Some(roster) = node_roster() else {
        tracing::debug!("guardian reap: HIVE_NODE_ROSTER unset — refusing (fail closed)");
        return (0, 0);
    };
    let min_age_days: u64 = std::env::var("HIVE_REAP_MIN_AGE_DAYS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(30);
    let max_fraction: f64 = std::env::var("HIVE_REAP_MAX_FRACTION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0 && *f <= 1.0)
        .unwrap_or(0.34);
    let Ok(h) = handle().await else {
        return (0, 0);
    };
    let Ok(entries) = h.kv.entry_heads().await else {
        return (0, 0);
    };

    #[derive(Default)]
    struct NodeKeys {
        bases: std::collections::BTreeSet<String>,
        parts: std::collections::BTreeSet<String>,
        latest_timestamp: u64,
    }

    let mut nodes = std::collections::BTreeMap::<String, NodeKeys>::new();
    for entry in &entries {
        let Some((name, is_part)) = node_snapshot_key_owner(&entry.key) else {
            continue;
        };
        let keys = nodes.entry(name.to_string()).or_default();
        keys.latest_timestamp = keys.latest_timestamp.max(entry.timestamp);
        if is_part {
            keys.parts.insert(entry.key.clone());
        } else {
            keys.bases.insert(entry.key.clone());
        }
    }

    let now_us = hive_core::now_ms().saturating_mul(1000);
    let min_age_us = min_age_days.saturating_mul(24 * 60 * 60 * 1000 * 1000);
    let mut candidates = Vec::new();
    let mut withheld_young = 0usize;
    for (name, keys) in &nodes {
        if name == &me || roster.contains(name) {
            continue;
        }
        if now_us.saturating_sub(keys.latest_timestamp) < min_age_us {
            withheld_young += 1;
            continue;
        }
        candidates.push(name.clone());
    }

    if !nodes.is_empty() {
        let frac = candidates.len() as f64 / nodes.len() as f64;
        if frac > max_fraction {
            tracing::error!(
                candidates = candidates.len(),
                node_keys = nodes.len(),
                fraction = frac,
                max_fraction,
                "guardian reap: REFUSING — too large a share of node snapshot prefixes look departed, which means HIVE_NODE_ROSTER is wrong, not that the fleet left. Raise HIVE_REAP_MAX_FRACTION to override."
            );
            return (0, candidates.len());
        }
    }

    let mut reaped = 0usize;
    let mut deleted_keys = 0usize;
    for name in candidates {
        let Some(keys) = nodes.get(&name) else {
            continue;
        };
        let mut parts_ok = true;
        let mut part_keys = keys.parts.iter().collect::<Vec<_>>();
        part_keys.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        for part_key in part_keys {
            match delete_snapshot_key(h, part_key).await {
                Ok(true) => deleted_keys += 1,
                Ok(false) => {}
                Err(error) => {
                    parts_ok = false;
                    tracing::warn!(%part_key, %error, "guardian reap: exact snapshot part delete failed");
                }
            }
        }

        let mut base_ok = parts_ok;
        if parts_ok {
            for base_key in &keys.bases {
                match delete_snapshot_key(h, base_key).await {
                    Ok(true) => deleted_keys += 1,
                    Ok(false) => {}
                    Err(error) => {
                        base_ok = false;
                        tracing::warn!(%base_key, %error, "guardian reap: exact snapshot base delete failed");
                    }
                }
            }
        }
        if base_ok {
            reaped += 1;
            tracing::warn!(
                node = %name,
                parts = keys.parts.len(),
                bases = keys.bases.len(),
                "guardian reap: deleted departed node's snapshot prefix"
            );
        }
    }

    if reaped > 0 || withheld_young > 0 || deleted_keys > 0 {
        tracing::info!(
            reaped,
            deleted_keys,
            withheld_young,
            node_keys = nodes.len(),
            min_age_days,
            "guardian reap pass complete"
        );
    }
    (reaped, withheld_young)
}

#[cfg(debug_assertions)]
async fn diagnostic_put_build_part(
    h: &Handle,
    base_key: &str,
    serial: usize,
) -> anyhow::Result<(String, String)> {
    let bytes = format!("{{\"builds\":[]{} }}", " ".repeat(serial + 1)).into_bytes();
    let digest = hex::encode(sha256(&bytes));
    let key = format!("{}/builds/{digest}", snapshot_v3_prefix(base_key)?);
    h.kv.put(&key, bytes).await?;
    Ok((key, digest))
}

#[cfg(debug_assertions)]
async fn diagnostic_advance_base_head(
    h: &Handle,
    base_key: &str,
    serial: u64,
) -> anyhow::Result<Vec<u8>> {
    let bytes =
        h.kv.get(base_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("diagnostic base bytes absent"))?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("diagnostic base is not an object"))?
        .insert("saved_ms".to_string(), serde_json::json!(serial));
    let bytes = canonical_json_bytes(value)?;
    h.kv.put(base_key, bytes.clone()).await?;
    verified_committed_snapshot(h, base_key).await?;
    Ok(bytes)
}

#[cfg(debug_assertions)]
async fn diagnostic_commit_part_reference(
    h: &Handle,
    base_key: &str,
    field: &str,
    part_key: &str,
    digest: &str,
) -> anyhow::Result<Vec<u8>> {
    let bytes =
        h.kv.get(base_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("diagnostic base bytes absent"))?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let reference = value
        .get_mut(SNAPSHOT_PARTS_FIELD)
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|manifest| manifest.get_mut(field))
        .ok_or_else(|| anyhow::anyhow!("diagnostic {field} manifest reference absent"))?;
    *reference = serde_json::json!({"key": part_key, "sha256": digest});
    let bytes = canonical_json_bytes(value)?;
    h.kv.put(base_key, bytes.clone()).await?;
    let verified = verified_committed_snapshot(h, base_key).await?;
    if !verified.parts.contains(part_key) {
        anyhow::bail!("diagnostic committed manifest did not select {part_key}");
    }
    Ok(bytes)
}

#[cfg(debug_assertions)]
async fn diagnostic_present_keys(
    h: &Handle,
    keys: &[String],
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let heads = h.kv.entry_heads().await?;
    Ok(keys
        .iter()
        .filter(|key| heads.iter().any(|head| &head.key == *key))
        .cloned()
        .collect())
}

#[cfg(debug_assertions)]
#[cfg(debug_assertions)]
async fn p2_wait_gc(
    h: &Handle,
    what: &str,
    pred: impl Fn(&guardian_db::p2p::network::core::GcHealth) -> bool,
) -> anyhow::Result<guardian_db::p2p::network::core::GcHealth> {
    for _ in 0..600 {
        let health = h.client.backend().gc_health().await;
        if pred(&health) {
            return Ok(health);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("Guardian p2 diagnostic timed out waiting for GC: {what} (GUARDIAN_GC_SECS=1?)")
}

#[cfg(debug_assertions)]
fn p2_snapshot(tag: &str) -> PlatformSnapshot {
    let mut snapshot = PlatformSnapshot::default();
    snapshot.cron.push(hive_edge::CronJob {
        id: format!("p2-{tag}"),
        name: format!("p2 diagnostic {tag}"),
        schedule: "0 0 * * * *".to_string(),
        deployment: "p2-diagnostic".to_string(),
        path: "/api/p2".to_string(),
        enabled: false,
        last_run_ms: None,
        next_run_ms: None,
        runs: 0,
        source: "manual".to_string(),
        tenant: "p2-diagnostic".to_string(),
    });
    snapshot
}

/// Publishes one full v2 generation (parts + head, verified) and records its complete-generation
/// marker at an explicit `marker_ms`; returns the head item and the state part's (key, entry bytes)
/// for retirement accounting.
#[cfg(debug_assertions)]
async fn p2_publish_generation(
    h: &Handle,
    component: &str,
    tag: &str,
    marker_ms: u64,
) -> anyhow::Result<(KeyedPreparedValue, String, usize)> {
    let snapshot = p2_snapshot(tag);
    let (parts, head) = guardian_v2_prepare_snapshot(&snapshot, component)?;
    guardian_v2_diagnostic_publish_generation(h, &parts, &head).await?;
    let encoded = guardian_v2_encode_component(component)?;
    let marker_key =
        guardian_v2_generation_key(&encoded, marker_ms, &hex::encode(head.value.change_digest))?;
    let marker = KeyedPreparedValue {
        key: marker_key,
        class: "generation",
        value: head.value.clone(),
    };
    let mut protection = h
        .client
        .protect_hashes([iroh_blobs::Hash::new(head.value.bytes.as_ref())])
        .await?;
    protection.finish_tag_installation();
    guardian_v2_publish_immutable(h, &marker, &protection).await?;
    drop(protection);
    let state_part = parts
        .first()
        .ok_or_else(|| anyhow::anyhow!("p2 generation lost its state part"))?;
    Ok((head, state_part.key.clone(), state_part.value.bytes.len()))
}

#[cfg(debug_assertions)]
async fn guardian_v2_writer_cadence_diagnostic() -> anyhow::Result<(String, Option<u64>)> {
    let enabled = std::env::var("HIVE_GUARDIAN_WRITER_CADENCE_DIAGNOSTIC")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled {
        return Ok(("skipped".to_string(), None));
    }

    let cadence = guardian_v2_retention_cadence();
    anyhow::ensure!(
        cadence <= std::time::Duration::from_secs(2),
        "writer cadence diagnostic requires HIVE_GUARDIAN_PART_REAP_CHECK_SECS<=2"
    );
    let passes_before = retention_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .passes;
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let accepted_task = accepted.clone();
    let last_generation_task = last_generation.clone();
    let producer_window = cadence.saturating_mul(4);
    let producer = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + producer_window;
        let delay = std::cmp::max(cadence / 20, std::time::Duration::from_millis(10));
        let mut serial = 0u64;
        while tokio::time::Instant::now() < deadline {
            serial += 1;
            let snapshot = p2_snapshot(&format!("writer-cadence-{serial}"));
            if let Some(generation) = replicate(&snapshot) {
                accepted_task.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                last_generation_task.store(generation, std::sync::atomic::Ordering::Release);
            }
            tokio::time::sleep(delay).await;
        }
    });
    producer.await?;

    let passes_after_flood = retention_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .passes;
    anyhow::ensure!(
        passes_after_flood >= passes_before.saturating_add(2),
        "retention starved under continuous snapshot traffic: passes {passes_before}->{passes_after_flood}"
    );
    let admitted = accepted.load(std::sync::atomic::Ordering::Relaxed);
    anyhow::ensure!(
        admitted >= 20,
        "writer cadence diagnostic admitted only {admitted} changes"
    );
    let final_generation = last_generation.load(std::sync::atomic::Ordering::Acquire);
    anyhow::ensure!(
        final_generation > 0,
        "writer cadence diagnostic admitted no final generation"
    );
    await_final_generation(final_generation, cadence.saturating_mul(10)).await?;
    let passes_after_commit = retention_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .passes;

    Ok((
        format!(
            "passes={passes_before}->{passes_after_flood}->{passes_after_commit} admitted={admitted} final_generation={final_generation} committed_after_pass=true single_writer=true"
        ),
        Some(final_generation),
    ))
}

#[cfg(debug_assertions)]
pub async fn writer_cadence_diagnostic() -> anyhow::Result<String> {
    set_node_name("guardian-writer-cadence-diagnostic");
    let _ = handle().await?;

    let raw = guardian_v2_wrapper_bytes(
        GuardianV2PartKind::State,
        serde_json::json!({"diagnostic": "compressible-compressible-compressible"}),
    )?;
    let encoded = guardian_v2_encode_part(&raw)?;
    anyhow::ensure!(
        encoded.starts_with(GUARDIAN_V2_PART_ENVELOPE_MAGIC),
        "writer cadence diagnostic did not emit a compressed part envelope"
    );
    anyhow::ensure!(
        guardian_v2_decode_part(&encoded)?.as_ref() == raw.as_slice(),
        "writer cadence diagnostic compressed part did not roundtrip"
    );
    let legacy_reference = GuardianV2Reference {
        sha256: hex::encode(sha256(&raw)),
        bytes: raw.len(),
    };
    guardian_v2_parse_part(GuardianV2PartKind::State, &raw, &legacy_reference)?;
    let mut unknown_version = encoded.clone();
    unknown_version[GUARDIAN_V2_PART_ENVELOPE_MAGIC.len()] =
        GUARDIAN_V2_PART_ENVELOPE_VERSION.saturating_add(1);
    let unknown_reference = GuardianV2Reference {
        sha256: hex::encode(sha256(&unknown_version)),
        bytes: unknown_version.len(),
    };
    anyhow::ensure!(
        guardian_v2_parse_part(
            GuardianV2PartKind::State,
            &unknown_version,
            &unknown_reference,
        )
        .is_err(),
        "writer cadence diagnostic accepted an unknown compression envelope version"
    );

    let (writer, final_generation) = guardian_v2_writer_cadence_diagnostic().await?;
    let final_generation = final_generation.ok_or_else(|| {
        anyhow::anyhow!(
            "writer cadence diagnostic requires HIVE_GUARDIAN_WRITER_CADENCE_DIAGNOSTIC=1"
        )
    })?;
    let retention = {
        let state = retention_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let last = state.last.as_ref().ok_or_else(|| {
            anyhow::anyhow!("writer cadence diagnostic observed no retention report")
        })?;
        format!(
            "passes={} last_status={} retired_generations={} retired_parts={}",
            state.passes, last.status, state.retired_generations_total, state.retired_parts_total
        )
    };
    let shutdown_started = tokio::time::Instant::now();
    shutdown(Some(final_generation)).await?;
    let shutdown_ms = shutdown_started.elapsed().as_millis();

    Ok(format!(
        "GUARDIAN_WRITER_CADENCE_PASS compression=roundtrip legacy=read unknown_version=refused {writer} {retention} shutdown_ms={shutdown_ms}"
    ))
}

/// Guardian v2 Phase-2 diagnostic matrix against the REAL vendored GuardianDB in this process's
/// disposable HIVE_DATA store: retention keep/grace/budget/cursor, typed GC refusals,
/// confirmed-work byte accounting, partial-generation fetch behaviour, legacy fallback only on true
/// absence, concurrent publish under GC churn and (env-gated) the final-flush timeout with
/// previous-head survival.
#[cfg(debug_assertions)]
async fn guardian_v2_phase2_diagnostic(h: &Handle) -> anyhow::Result<String> {
    let component = NODE_NAME
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("p2 diagnostic node name absent"))?;
    let encoded = guardian_v2_encode_component(&component)?;
    let base_key = snapshot_key().ok_or_else(|| anyhow::anyhow!("p2 base key absent"))?;
    let cfg = |keep: usize, grace_ms: u64, max_deletes: usize, max_fraction: f64| RetentionConfig {
        keep_generations: keep,
        grace_ms,
        max_deletes,
        max_fraction,
        page: 512,
        max_generations: 8192,
    };

    let report = guardian_v2_retention_pass_with(h, cfg(2, 0, 8, 0.5)).await;
    if report.status != "refused"
        || !report
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("head is absent")
    {
        anyhow::bail!(
            "p2 S0: empty-inventory retention did not refuse on absent head: {} {:?}",
            report.status,
            report.reason
        );
    }

    let now = hive_core::now_ms();
    let (_head_a, state_a, state_a_len) =
        p2_publish_generation(h, &component, "a", now.saturating_sub(5000)).await?;
    let (_head_b, state_b, state_b_len) =
        p2_publish_generation(h, &component, "b", now.saturating_sub(4000)).await?;
    let (head_c, state_c, state_c_len) =
        p2_publish_generation(h, &component, "c", now.saturating_sub(3000)).await?;
    if !matches!(
        guardian_v2_probe(h, &encoded).await,
        GuardianV2Probe::Ready(_)
    ) {
        anyhow::bail!("p2 S1: generation C is not Ready after publish");
    }

    let collision = KeyedPreparedValue {
        key: state_c.clone(),
        class: "state",
        value: PreparedValue {
            change_digest: sha256(b"different"),
            bytes: Arc::new(b"different".to_vec()),
        },
    };
    let mut collision_guard = h
        .client
        .protect_hashes([iroh_blobs::Hash::new(b"different")])
        .await?;
    collision_guard.finish_tag_installation();
    let collision_result = guardian_v2_publish_immutable(h, &collision, &collision_guard).await;
    drop(collision_guard);
    if !collision_result
        .err()
        .map(|error| error.to_string())
        .is_some_and(|text| text.contains("collision"))
    {
        anyhow::bail!("p2 S1: immutable collision was not refused");
    }

    let snap_d = p2_snapshot("d");
    let (_, head_d) = guardian_v2_prepare_snapshot(&snap_d, &component)?;
    let head_d_sha = hex::encode(head_d.value.change_digest);
    let generation =
        replicate(&snap_d).ok_or_else(|| anyhow::anyhow!("p2 S2: replicate refused"))?;
    let mut writer_marker = None;
    for _ in 0..600 {
        if let GuardianV2Probe::Ready(verified) = guardian_v2_probe(h, &encoded).await {
            if hex::encode(verified.head_sha256) == head_d_sha {
                let expected =
                    guardian_v2_generation_key(&encoded, verified.snapshot.saved_ms, &head_d_sha)?;
                if h.kv.get(&expected).await?.is_some() {
                    writer_marker = Some(expected);
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let writer_marker = writer_marker.ok_or_else(|| {
        anyhow::anyhow!("p2 S2: writer did not commit generation {generation} with its marker")
    })?;

    if !delete_snapshot_key(h, &writer_marker).await? {
        anyhow::bail!("p2 S3: could not remove the current marker for the anchor test");
    }
    let report = guardian_v2_retention_pass_with(h, cfg(0, 0, 8, 0.9)).await;
    if report.status != "refused"
        || !report
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("names the current head")
    {
        anyhow::bail!("p2 S3: retention did not refuse without the current-head marker");
    }
    let restore_marker = KeyedPreparedValue {
        key: writer_marker.clone(),
        class: "generation",
        value: head_d.value.clone(),
    };
    let mut protection = h
        .client
        .protect_hashes([iroh_blobs::Hash::new(head_d.value.bytes.as_ref())])
        .await?;
    protection.finish_tag_installation();
    guardian_v2_publish_immutable(h, &restore_marker, &protection).await?;
    drop(protection);

    let report = guardian_v2_retention_pass_with(h, cfg(1, 365 * 24 * 3600 * 1000, 8, 0.9)).await;
    if report.status != "noop" || report.generations_retired != 0 || report.parts_retired != 0 {
        anyhow::bail!(
            "p2 S4: grace window did not hold every young generation: {} retired {}/{}",
            report.status,
            report.generations_retired,
            report.parts_retired
        );
    }
    let report = guardian_v2_retention_pass_with(h, cfg(1, 0, 1, 0.9)).await;
    if report.generations_retired != 1 || report.parts_retired != 0 {
        anyhow::bail!("p2 S4: budget-1 pass did not retire exactly marker A");
    }
    let report = guardian_v2_retention_pass_with(h, cfg(1, 0, 1, 0.9)).await;
    if report.generations_retired != 1 {
        anyhow::bail!("p2 S4: second budget-1 pass did not retire marker B");
    }
    let report = guardian_v2_retention_pass_with(h, cfg(0, 0, 8, 0.5)).await;
    if report.generations_retired != 1 {
        anyhow::bail!(
            "p2 S4: fraction cap 0.5 over two markers did not retire exactly C: {}",
            report.generations_retired
        );
    }

    let mut unrooted = 0u64;
    let mut retired_parts = 0u64;
    let mut saw_cursor = false;
    for pass in 0..8 {
        let report = guardian_v2_retention_pass_with(h, cfg(0, 0, 1, 0.9)).await;
        if report.status == "failed" {
            anyhow::bail!("p2 S4: part retirement failed: {:?}", report.reason);
        }
        unrooted += report.parts_unrooted_bytes;
        retired_parts += report.parts_retired;
        saw_cursor |= report.cursor.is_some();
        if pass == 1 {
            retention_state()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cursor = None;
        }
        if retired_parts >= 3 {
            break;
        }
    }
    if retired_parts != 3 {
        anyhow::bail!(
            "p2 S4: expected exactly 3 superseded state parts retired, got {retired_parts}"
        );
    }
    let expected_unrooted = (state_a_len + state_b_len + state_c_len) as u64;
    if unrooted != expected_unrooted {
        anyhow::bail!(
            "p2 S4: unrooted byte accounting {unrooted} != exact expected {expected_unrooted}"
        );
    }
    if !saw_cursor {
        anyhow::bail!("p2 S4: bounded part retirement never reported a resume cursor");
    }
    for key in [&state_a, &state_b, &state_c] {
        if h.kv.get(key).await?.is_some() {
            anyhow::bail!("p2 S4: retired part {key} still has readable metadata");
        }
    }
    let report = guardian_v2_retention_pass_with(h, cfg(0, 0, 8, 0.9)).await;
    if report.status != "noop" {
        anyhow::bail!(
            "p2 S4: retention did not converge to noop: {}",
            report.status
        );
    }
    if !matches!(
        guardian_v2_probe(h, &encoded).await,
        GuardianV2Probe::Ready(_)
    ) {
        anyhow::bail!("p2 S4: current generation D lost readiness after retention");
    }

    let baseline = p2_wait_gc(h, "quiescent no-op pass", |health| {
        health
            .last_pass
            .as_ref()
            .is_some_and(|pass| pass.status.is_completed() && pass.reclaimed_objects == 0)
    })
    .await?;
    let probe_small = vec![0x51u8; 10_000];
    let probe_large = vec![0x52u8; 20_000];
    let probe_small_hash = iroh_blobs::Hash::new(&probe_small).to_hex();
    let probe_large_hash = iroh_blobs::Hash::new(&probe_large).to_hex();
    h.kv.put("p2-probe-small", probe_small.clone()).await?;
    h.kv.put("p2-probe-large", probe_large.clone()).await?;
    h.kv.delete("p2-probe-small").await?;
    h.kv.delete("p2-probe-large").await?;
    let reclaimed_before = baseline.reclaimed_bytes_total;
    p2_wait_gc(h, "probe reclamation", |health| {
        health.reclaimed_bytes_total >= reclaimed_before + 30_000
    })
    .await?;
    if h.client.has_blob_local(&probe_small_hash).await
        || h.client.has_blob_local(&probe_large_hash).await
    {
        anyhow::bail!("p2 S5: probe blobs survived a pass that claimed their bytes");
    }
    let after = h.client.backend().gc_health().await;
    let delta = after.reclaimed_bytes_total - reclaimed_before;
    if delta != 30_000 {
        anyhow::bail!("p2 S5: confirmed reclaimed byte accounting {delta} != exact expected 30000");
    }

    let probe_guard = vec![0x53u8; 4_096];
    let probe_guard_hash = iroh_blobs::Hash::new(&probe_guard).to_hex();
    h.kv.put("p2-probe-guard", probe_guard.clone()).await?;
    h.kv.delete("p2-probe-guard").await?;
    let fake_head = "snap-v2/node/p2-fake/head";
    let refusal_matrix: [(&str, Vec<u8>, &str); 3] = [
        (
            "malformed",
            b"not-json".to_vec(),
            "incomplete_malformed_descriptor",
        ),
        (
            "unknown-version",
            serde_json::to_vec(&serde_json::json!({
                "magic": GUARDIAN_V2_SNAPSHOT_MAGIC, "version": 3
            }))?,
            "incomplete_unknown_version",
        ),
        (
            "missing-part",
            serde_json::to_vec(&serde_json::json!({
                "magic": GUARDIAN_V2_SNAPSHOT_MAGIC, "version": 2,
                "state": {"sha256": "b".repeat(64), "bytes": 3},
                "deployments": {"sha256": "b".repeat(64), "bytes": 3},
                "database_data": {"sha256": "b".repeat(64), "bytes": 3},
                "metrics_rollup": {"sha256": "b".repeat(64), "bytes": 3},
                "builds": {"sha256": "b".repeat(64), "bytes": 3},
                "sandboxes": {"sha256": "b".repeat(64), "bytes": 3},
            }))?,
            "incomplete_missing_part",
        ),
    ];
    for (label, bytes, expected_status) in refusal_matrix {
        h.kv.put(fake_head, bytes).await?;
        let incomplete_before = h.client.backend().gc_health().await.incomplete_runs;
        let health = p2_wait_gc(h, label, |health| {
            health.incomplete_runs > incomplete_before
        })
        .await?;
        let status = health
            .last_pass
            .as_ref()
            .map(|pass| pass.status.as_str())
            .unwrap_or("none");
        if status != expected_status {
            anyhow::bail!(
                "p2 S5b: {label} head produced status {status}, wanted {expected_status}"
            );
        }
        if !h.client.has_blob_local(&probe_guard_hash).await {
            anyhow::bail!("p2 S5b: a REFUSED {label} pass reclaimed the guard probe");
        }
    }
    delete_snapshot_key(h, fake_head).await?;
    p2_wait_gc(h, "post-refusal completion", |health| {
        health
            .last_pass
            .as_ref()
            .is_some_and(|pass| pass.status.is_completed())
    })
    .await?;
    p2_wait_gc(h, "guard probe reclaim", |health| {
        health
            .last_pass
            .as_ref()
            .is_some_and(|pass| pass.status.is_completed())
    })
    .await?;
    for _ in 0..600 {
        if !h.client.has_blob_local(&probe_guard_hash).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if h.client.has_blob_local(&probe_guard_hash).await {
        anyhow::bail!("p2 S5b: guard probe was never reclaimed after the fault cleared");
    }

    let mut missing_part_gc_witnessed = false;
    if !matches!(
        guardian_v2_probe(h, &encoded).await,
        GuardianV2Probe::Ready(_)
    ) {
        anyhow::bail!("p2 S6: current generation is not Ready before the matrix");
    }
    let head_bytes_now =
        h.kv.get(&format!("{GUARDIAN_V2_NODE_PREFIX}{encoded}/head"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("p2 S6: head bytes absent"))?;
    let parsed_now = guardian_v2_parse_head(&head_bytes_now)?;
    for kind in GuardianV2PartKind::ALL {
        let reference = parsed_now
            .references
            .get(kind.field())
            .ok_or_else(|| anyhow::anyhow!("p2 S6: reference {} absent", kind.field()))?;
        let part_key = guardian_v2_part_key(&encoded, kind, &reference.sha256)?;
        let part_bytes =
            h.kv.get(&part_key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("p2 S6: part bytes absent for {part_key}"))?;
        if !delete_snapshot_key(h, &part_key).await? {
            anyhow::bail!("p2 S6: could not delete {part_key}");
        }
        if !matches!(
            fetch_snapshot_at_result(&base_key).await,
            SnapshotFetch::Incomplete
        ) {
            anyhow::bail!(
                "p2 S6: fetch with {} missing was not Incomplete (legacy fallback leak?)",
                kind.field()
            );
        }
        if !missing_part_gc_witnessed {
            let incomplete_before = h.client.backend().gc_health().await.incomplete_runs;
            let health = p2_wait_gc(h, "missing current part", |health| {
                health.incomplete_runs > incomplete_before
            })
            .await?;
            if health
                .last_pass
                .as_ref()
                .map(|pass| pass.status.as_str())
                .unwrap_or("none")
                != "incomplete_missing_part"
            {
                anyhow::bail!("p2 S6: GC did not refuse typed on a missing current part");
            }
            missing_part_gc_witnessed = true;
        }
        let mut protection = h
            .client
            .protect_hashes([iroh_blobs::Hash::new(&part_bytes)])
            .await?;
        protection.finish_tag_installation();
        h.kv.put_gc_protected(&part_key, part_bytes, &protection)
            .await?;
        drop(protection);
        if !matches!(
            guardian_v2_probe(h, &encoded).await,
            GuardianV2Probe::Ready(_)
        ) {
            anyhow::bail!("p2 S6: restore of {} did not return Ready", kind.field());
        }
    }

    let legacy_only = "node/guardian-diagnostic-legacyonly/snapshot";
    h.kv.put(
        legacy_only,
        serde_json::to_vec(&PlatformSnapshot::default())?,
    )
    .await?;
    if !matches!(
        fetch_snapshot_at_result(legacy_only).await,
        SnapshotFetch::Ready(_)
    ) {
        anyhow::bail!("p2 S6: legacy fallback did not serve on TRUE v2 absence");
    }

    for round in 0..3u32 {
        let runs_before = h.client.backend().gc_health().await.successful_runs;
        let (_, _, _) = p2_publish_generation(
            h,
            &component,
            &format!("churn-{round}"),
            hive_core::now_ms(),
        )
        .await?;
        guardian_v2_verify_generation(h, &encoded).await?;
        p2_wait_gc(h, "churn pass", |health| {
            health.successful_runs > runs_before
        })
        .await?;
        guardian_v2_verify_generation(h, &encoded).await?;
    }
    let churn_health = h.client.backend().gc_health().await;
    if churn_health
        .last_pass
        .as_ref()
        .is_some_and(|pass| pass.status.as_str() == "incomplete_changed_during_pass")
    {
        anyhow::bail!("p2 S7: the exclusive gate leaked a mid-pass inventory change");
    }

    {
        let registration = PreparedSnapshotRegistration::new(
            &format!("{GUARDIAN_V2_NODE_PREFIX}{encoded}/head"),
            sha256(b"p2-cancelled"),
            vec![format!(
                "{GUARDIAN_V2_NODE_PREFIX}{encoded}/state/{}",
                "c".repeat(64)
            )],
        );
        drop(registration);
        let (parts, _) =
            prepared_snapshot_keys(&format!("{GUARDIAN_V2_NODE_PREFIX}{encoded}/head"));
        if parts.iter().any(|key| key.ends_with(&"c".repeat(64))) {
            anyhow::bail!("p2 S8: cancelled preparation left a prepared root behind");
        }
    }

    let (writer_cadence, _) = guardian_v2_writer_cadence_diagnostic().await?;

    let mut final_flush = "skipped".to_string();
    if std::env::var("GUARDIAN_FINAL_FLUSH_DIAGNOSTIC_HOLD_MS").is_ok() {
        let pre = match guardian_v2_probe(h, &encoded).await {
            GuardianV2Probe::Ready(verified) => hex::encode(verified.head_sha256),
            _ => anyhow::bail!("p2 S9: no Ready head before the final-flush scenario"),
        };
        FINAL_FLUSH_HOLD_ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
        let generation = replicate(&p2_snapshot("final-flush"))
            .ok_or_else(|| anyhow::anyhow!("p2 S9: replicate refused"))?;
        let error = match shutdown(Some(generation)).await {
            Ok(()) => anyhow::bail!("p2 S9: held final flush unexpectedly drained in time"),
            Err(error) => error.to_string(),
        };
        if !error.contains("timed out") {
            anyhow::bail!("p2 S9: final flush failed for a non-timeout reason: {error}");
        }
        let post = match guardian_v2_probe(h, &encoded).await {
            GuardianV2Probe::Ready(verified) => hex::encode(verified.head_sha256),
            _ => anyhow::bail!("p2 S9: previous head unreadable after the timed-out final flush"),
        };
        if post != pre {
            anyhow::bail!("p2 S9: the held final flush replaced the previous head");
        }
        final_flush = format!("timeout_witnessed head_preserved={}", post == pre);
    }

    Ok(format!(
        "p2: empty_refusal=true collision=true writer_marker=true anchor_refusal=true \
         grace_hold=true budget_retire=true fraction_cap=true parts_retired={retired_parts} \
         unrooted_bytes={unrooted} cursor_resume={saw_cursor} restart_resume=true noop=true \
         reclaim_exact_bytes=30000 refusal_matrix=3 six_parts_missing=true \
         legacy_true_absence_only=true churn_rounds=3 cancel_prepare=true \
         writer_cadence={writer_cadence} final_flush={final_flush}"
    ))
}

#[cfg(debug_assertions)]
pub async fn lifecycle_diagnostic() -> anyhow::Result<String> {
    set_node_name("guardian-diagnostic-self");
    let h = handle().await?;
    let base_key = snapshot_key().ok_or_else(|| anyhow::anyhow!("diagnostic node name absent"))?;
    let snapshot = PlatformSnapshot::default();
    let first = shared_snapshot_canonical(&snapshot, &base_key)?;
    let mut first_hashes = first
        .parts
        .values()
        .map(|(_, bytes)| iroh_blobs::Hash::new(bytes))
        .collect::<Vec<_>>();
    first_hashes.push(iroh_blobs::Hash::new(&first.base));
    let mut first_guard = h.client.protect_hashes(first_hashes).await?;
    first_guard.finish_tag_installation();
    for (key, (_, bytes)) in &first.parts {
        h.kv.put_gc_protected(key, bytes.clone(), &first_guard)
            .await?;
    }
    h.kv.put_gc_protected(&base_key, first.base.clone(), &first_guard)
        .await?;
    let (first_base_digest, first_keep) = verified_committed_part_keys(h, &base_key).await?;
    let first_expected: std::collections::BTreeSet<_> = first.parts.keys().cloned().collect();
    if first_base_digest != sha256(&first.base) || first_keep != first_expected {
        anyhow::bail!("first non-prefix split commit does not match its prepared generation");
    }
    drop(first_guard);
    if first_keep.len() != SNAPSHOT_PART_FIELDS.len() {
        anyhow::bail!(
            "first non-prefix split commit is incomplete: keep set has {} keys",
            first_keep.len()
        );
    }
    match fetch_snapshot_at_result(&base_key).await {
        SnapshotFetch::Ready(_) => {}
        SnapshotFetch::Absent => {
            anyhow::bail!("first non-prefix split commit is incomplete: fetch says Absent")
        }
        SnapshotFetch::Incomplete => {
            anyhow::bail!("first non-prefix split commit is incomplete: fetch says Incomplete")
        }
    }

    let mut second = shared_snapshot_canonical(&snapshot, &base_key)?;
    let old_deployments = second
        .parts
        .keys()
        .find(|key| key.contains("/deployments/"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("deployments part absent"))?;
    let (changed_class, mut changed) = second
        .parts
        .remove(&old_deployments)
        .ok_or_else(|| anyhow::anyhow!("deployments bytes absent"))?;
    changed.push(b' ');
    let changed_digest = hex::encode(sha256(&changed));
    let changed_key = format!(
        "{}/deployments/{changed_digest}",
        snapshot_v3_prefix(&base_key)?
    );
    second
        .parts
        .insert(changed_key.clone(), (changed_class, changed));
    let mut second_base: serde_json::Value = serde_json::from_slice(&second.base)?;
    let reference = second_base
        .get_mut(SNAPSHOT_PARTS_FIELD)
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|manifest| manifest.get_mut("deployments"))
        .ok_or_else(|| anyhow::anyhow!("deployments manifest reference absent"))?;
    *reference = serde_json::json!({"key": changed_key, "sha256": changed_digest});
    second.base = canonical_json_bytes(second_base)?;

    let mut second_hashes = second
        .parts
        .values()
        .map(|(_, bytes)| iroh_blobs::Hash::new(bytes))
        .collect::<Vec<_>>();
    second_hashes.push(iroh_blobs::Hash::new(&second.base));
    let mut second_guard = h.client.protect_hashes(second_hashes).await?;
    second_guard.finish_tag_installation();
    for (key, (_, bytes)) in &second.parts {
        h.kv.put_gc_protected(key, bytes.clone(), &second_guard)
            .await?;
    }
    let before_gc = h.client.backend().gc_health().await.successful_runs;
    let mut completed_precommit_gc = false;
    for _ in 0..50 {
        if h.client.backend().gc_health().await.successful_runs > before_gc {
            completed_precommit_gc = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !completed_precommit_gc {
        anyhow::bail!(
            "concurrent GC did not complete inside the protected precommit window; run the disposable diagnostic with GUARDIAN_GC_SECS=1"
        );
    }
    for (key, (_, bytes)) in &second.parts {
        let hash = iroh_blobs::Hash::new(bytes);
        if !h.client.has_blob_local(&hash.to_hex()).await {
            anyhow::bail!("prepared part disappeared while GC waited: {key}");
        }
    }
    h.kv.put_gc_protected(&base_key, second.base.clone(), &second_guard)
        .await?;
    let (second_base_digest, second_keep) = verified_committed_part_keys(h, &base_key).await?;
    let second_expected: std::collections::BTreeSet<_> = second.parts.keys().cloned().collect();
    if second_base_digest != sha256(&second.base) || second_keep != second_expected {
        anyhow::bail!("second split commit does not match its prepared generation");
    }
    drop(second_guard);
    for _ in 0..50 {
        if h.client.backend().gc_health().await.successful_runs > before_gc {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !matches!(
        fetch_snapshot_at_result(&base_key).await,
        SnapshotFetch::Ready(_)
    ) {
        anyhow::bail!("second split commit is incomplete after concurrent GC");
    }

    if second_keep.len() != SNAPSHOT_PART_FIELDS.len() {
        anyhow::bail!("second committed keep set is incomplete");
    }
    let mut cleanup_passes = 1usize;
    let normal_deleted = cleanup_committed_snapshot_parts(h, &base_key).await?;
    if normal_deleted != 1 {
        anyhow::bail!("normal one-generation cleanup deleted {normal_deleted}; expected 1");
    }
    let remaining_after_commit =
        h.kv.entry_heads()
            .await?
            .iter()
            .filter(|head| snapshot_part_key_shape(&base_key, &head.key).is_some())
            .count();
    if remaining_after_commit != second_keep.len() {
        anyhow::bail!(
            "verified post-commit cleanup retained {remaining_after_commit} parts; expected {}",
            second_keep.len()
        );
    }

    let ordinary = "queue/x/snapshot-part-v2/y";
    h.kv.put(ordinary, b"ordinary-root".to_vec()).await?;

    let (refusal_candidate, _) = diagnostic_put_build_part(h, &base_key, 10).await?;
    let malformed = format!("{}/builds/not-a-digest", snapshot_v3_prefix(&base_key)?);
    h.kv.put(&malformed, b"malformed".to_vec()).await?;
    diagnostic_advance_base_head(h, &base_key, 10).await?;
    let malformed_error = cleanup_committed_snapshot_parts(h, &base_key)
        .await
        .expect_err("malformed reserved key unexpectedly authorized cleanup");
    if !malformed_error.to_string().contains("unclassified key")
        || !diagnostic_present_keys(h, std::slice::from_ref(&refusal_candidate))
            .await?
            .contains(&refusal_candidate)
    {
        return Err(malformed_error);
    }
    delete_snapshot_key(h, &malformed).await?;

    let legacy_fixed = format!("{base_key}-part/builds");
    h.kv.put(&legacy_fixed, b"{\"builds\":[]}".to_vec()).await?;
    let legacy_error = cleanup_committed_snapshot_parts(h, &base_key)
        .await
        .expect_err("stale fixed legacy key unexpectedly authorized cleanup");
    if !legacy_error.to_string().contains("stale legacy fixed key")
        || !diagnostic_present_keys(h, std::slice::from_ref(&refusal_candidate))
            .await?
            .contains(&refusal_candidate)
    {
        return Err(legacy_error);
    }
    delete_snapshot_key(h, &legacy_fixed).await?;
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 1 {
        anyhow::bail!("addressed candidate did not clean up after ambiguity was removed");
    }
    cleanup_passes += 1;

    let mut historical = Vec::new();
    for serial in 100usize..106 {
        let (key, digest) = diagnostic_put_build_part(h, &base_key, serial).await?;
        historical.push((key, digest));
    }
    diagnostic_advance_base_head(h, &base_key, 1_000).await?;
    let (max_keys, max_fraction) = snapshot_cleanup_limits();
    let expected_first_batch = snapshot_cleanup_batch_limit(11, max_keys, max_fraction).min(6);
    if expected_first_batch != 5 {
        anyhow::bail!(
            "lifecycle diagnostic requires default part cleanup limits (expected first batch 5, configured {expected_first_batch})"
        );
    }
    let historical_keys = historical
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    CLEANUP_DIAGNOSTIC_PAUSE_BEFORE_GLOBAL_RECHECK.store(true, std::sync::atomic::Ordering::SeqCst);
    let paused = cleanup_diagnostic_paused().notified();
    let population_base = base_key.clone();
    let population_task =
        tokio::spawn(async move { cleanup_committed_snapshot_parts(h, &population_base).await });
    if tokio::time::timeout(std::time::Duration::from_secs(10), paused)
        .await
        .is_err()
    {
        population_task.abort();
        let _ = population_task.await;
        anyhow::bail!("global-population diagnostic never reached its final head recheck");
    }
    let changed_population_key = format!(
        "{}/builds/not-a-digest-during-proof",
        snapshot_v3_prefix(&base_key)?
    );
    let population_change =
        h.kv.put(&changed_population_key, b"changed-population".to_vec())
            .await;
    cleanup_diagnostic_resume().notify_one();
    population_change?;
    let population_error = population_task
        .await?
        .expect_err("changed reserved population unexpectedly authorized cleanup");
    if !population_error
        .to_string()
        .contains("reserved snapshot population changed")
        || diagnostic_present_keys(h, &historical_keys).await?.len() != historical_keys.len()
    {
        return Err(population_error);
    }
    delete_snapshot_key(h, &changed_population_key).await?;

    let first_historical_batch = cleanup_committed_snapshot_parts(h, &base_key).await?;
    cleanup_passes += 1;
    if first_historical_batch != expected_first_batch {
        anyhow::bail!(
            "5+6 addressed cleanup deleted {first_historical_batch}; expected {expected_first_batch}"
        );
    }
    let remaining_historical = diagnostic_present_keys(h, &historical_keys).await?;
    if remaining_historical.len() != 1 {
        anyhow::bail!(
            "bounded 5+6 cleanup retained {} historical generations; expected 1",
            remaining_historical.len()
        );
    }

    let promoted_key = remaining_historical
        .iter()
        .next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("historical promotion target absent"))?;
    let promoted_digest = historical
        .iter()
        .find_map(|(key, digest)| (key == &promoted_key).then_some(digest.clone()))
        .ok_or_else(|| anyhow::anyhow!("historical promotion digest absent"))?;
    diagnostic_commit_part_reference(h, &base_key, "builds", &promoted_key, &promoted_digest)
        .await?;
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 1 {
        anyhow::bail!("post-head-change cleanup did not reap exactly the old current generation");
    }
    cleanup_passes += 1;
    if !diagnostic_present_keys(h, std::slice::from_ref(&promoted_key))
        .await?
        .contains(&promoted_key)
    {
        anyhow::bail!("newly committed generation was deleted after the head changed");
    }

    let (prepared_key, _) = diagnostic_put_build_part(h, &base_key, 200).await?;
    let prepared_digest = sha256(b"diagnostic-prepared-base");
    let prepared_marker = format!(
        "{}/part-reap-v3/{}",
        base_key.trim_end_matches("/snapshot"),
        hex::encode(prepared_digest)
    );
    h.kv.put(&prepared_marker, b"prepared-marker".to_vec())
        .await?;
    diagnostic_advance_base_head(h, &base_key, 2_000).await?;
    let prepared_registration =
        PreparedSnapshotRegistration::new(&base_key, prepared_digest, vec![prepared_key.clone()]);
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 0 {
        anyhow::bail!("active prepared roots were included in cleanup");
    }
    let protected =
        diagnostic_present_keys(h, &[prepared_key.clone(), prepared_marker.clone()]).await?;
    if protected.len() != 2 {
        anyhow::bail!("active prepared part or marker was deleted");
    }
    drop(prepared_registration);
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 1
        || cleanup_committed_snapshot_parts(h, &base_key).await? != 1
    {
        anyhow::bail!("released prepared part/marker populations did not converge separately");
    }
    cleanup_passes += 2;

    let malformed_marker = format!(
        "{}/part-reap-v3/not-a-digest",
        base_key.trim_end_matches("/snapshot")
    );
    h.kv.put(&malformed_marker, b"malformed-marker".to_vec())
        .await?;
    let marker_error = cleanup_committed_snapshot_parts(h, &base_key)
        .await
        .expect_err("malformed marker unexpectedly authorized cleanup");
    if !marker_error.to_string().contains("unclassified key") {
        return Err(marker_error);
    }
    delete_snapshot_key(h, &malformed_marker).await?;

    let marker_root = format!("{}/part-reap-v3", base_key.trim_end_matches("/snapshot"));
    let mut stale_markers = Vec::new();
    for serial in 0usize..257 {
        let digest = hex::encode(sha256(format!("stale-marker-{serial}").as_bytes()));
        let key = format!("{marker_root}/{digest}");
        h.kv.put(&key, b"stale-marker".to_vec()).await?;
        stale_markers.push(key);
    }
    diagnostic_advance_base_head(h, &base_key, 3_000).await?;
    let current_digest = hex::encode(verified_committed_snapshot(h, &base_key).await?.base_digest);
    let current_marker = format!("{marker_root}/{current_digest}");
    h.kv.put(&current_marker, b"current-marker".to_vec())
        .await?;
    let mut marker_passes = 0usize;
    let mut marker_deleted = 0usize;
    loop {
        let before = diagnostic_present_keys(h, &stale_markers).await?;
        if before.is_empty() {
            break;
        }
        let population = SNAPSHOT_PART_FIELDS.len() + before.len() + 1;
        let allowed = snapshot_cleanup_batch_limit(population, max_keys, max_fraction);
        let deleted = cleanup_committed_snapshot_parts(h, &base_key).await?;
        if deleted == 0 || deleted > allowed || deleted > max_keys {
            anyhow::bail!(
                "stale marker batch made invalid progress: deleted={deleted}, allowed={allowed}, population={population}"
            );
        }
        marker_deleted += deleted;
        marker_passes += 1;
        cleanup_passes += 1;
        if marker_passes > 32 {
            anyhow::bail!("257-marker cleanup did not converge within 32 passes");
        }
    }
    if marker_deleted != stale_markers.len()
        || !diagnostic_present_keys(h, std::slice::from_ref(&current_marker))
            .await?
            .contains(&current_marker)
    {
        anyhow::bail!("marker backlog cleanup lost progress or deleted the current marker");
    }
    diagnostic_advance_base_head(h, &base_key, 3_001).await?;
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 1 {
        anyhow::bail!("former current marker did not become independently reapable");
    }
    cleanup_passes += 1;

    let mut failure_keys = Vec::new();
    for serial in 300usize..303 {
        failure_keys.push(diagnostic_put_build_part(h, &base_key, serial).await?.0);
    }
    diagnostic_advance_base_head(h, &base_key, 4_000).await?;
    CLEANUP_DIAGNOSTIC_FAIL_AFTER.store(1, std::sync::atomic::Ordering::SeqCst);
    let injected = cleanup_committed_snapshot_parts(h, &base_key)
        .await
        .expect_err("injected cleanup delete failure did not fire");
    if !injected
        .to_string()
        .contains("injected Guardian cleanup delete failure")
        || diagnostic_present_keys(h, &failure_keys).await?.len() != 2
    {
        return Err(injected);
    }
    cleanup_passes += 1;
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 2 {
        anyhow::bail!("cleanup did not resume from the injected delete failure");
    }
    cleanup_passes += 1;

    let mut cancellation_keys = Vec::new();
    for serial in 400usize..403 {
        cancellation_keys.push(diagnostic_put_build_part(h, &base_key, serial).await?.0);
    }
    diagnostic_advance_base_head(h, &base_key, 5_000).await?;
    CLEANUP_DIAGNOSTIC_PAUSE_AFTER.store(1, std::sync::atomic::Ordering::SeqCst);
    let paused = cleanup_diagnostic_paused().notified();
    let cancellation_base = base_key.clone();
    let cancellation_task =
        tokio::spawn(async move { cleanup_committed_snapshot_parts(h, &cancellation_base).await });
    if tokio::time::timeout(std::time::Duration::from_secs(10), paused)
        .await
        .is_err()
    {
        cancellation_task.abort();
        let _ = cancellation_task.await;
        anyhow::bail!("cleanup cancellation diagnostic never reached its interruption point");
    }
    cancellation_task.abort();
    let cancellation_result = cancellation_task.await;
    if cancellation_result.is_ok()
        || diagnostic_present_keys(h, &cancellation_keys).await?.len() != 2
    {
        anyhow::bail!("cancelled cleanup did not preserve exactly one confirmed delete");
    }
    cleanup_passes += 1;
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 2 {
        anyhow::bail!("cleanup did not resume after cancellation");
    }
    cleanup_passes += 1;

    let mut moving = Vec::new();
    for serial in 500usize..503 {
        moving.push(diagnostic_put_build_part(h, &base_key, serial).await?);
    }
    diagnostic_advance_base_head(h, &base_key, 6_000).await?;
    let heads = h.kv.entry_heads().await?;
    moving.sort_by(|left, right| {
        latest_snapshot_head(&heads, &left.0)
            .map(|head| head.timestamp)
            .cmp(&latest_snapshot_head(&heads, &right.0).map(|head| head.timestamp))
            .then_with(|| left.0.cmp(&right.0))
    });
    let (moving_current_key, moving_current_digest) = moving
        .get(1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("head-change promotion target absent"))?;
    CLEANUP_DIAGNOSTIC_PAUSE_AFTER.store(1, std::sync::atomic::Ordering::SeqCst);
    let paused = cleanup_diagnostic_paused().notified();
    let moving_base = base_key.clone();
    let moving_task =
        tokio::spawn(async move { cleanup_committed_snapshot_parts(h, &moving_base).await });
    if tokio::time::timeout(std::time::Duration::from_secs(10), paused)
        .await
        .is_err()
    {
        moving_task.abort();
        let _ = moving_task.await;
        anyhow::bail!("head-change diagnostic never reached its inter-group point");
    }
    let commit_result = diagnostic_commit_part_reference(
        h,
        &base_key,
        "builds",
        &moving_current_key,
        &moving_current_digest,
    )
    .await;
    cleanup_diagnostic_resume().notify_one();
    commit_result?;
    let moving_deleted = moving_task.await??;
    cleanup_passes += 1;
    if moving_deleted != 2
        || !diagnostic_present_keys(h, std::slice::from_ref(&moving_current_key))
            .await?
            .contains(&moving_current_key)
    {
        anyhow::bail!("head change between groups deleted the newly-current generation");
    }
    if cleanup_committed_snapshot_parts(h, &base_key).await? != 1 {
        anyhow::bail!("post-change pass did not reap the newly-superseded prior current key");
    }
    cleanup_passes += 1;

    let remaining =
        h.kv.entry_heads()
            .await?
            .iter()
            .filter(|head| snapshot_part_key_shape(&base_key, &head.key).is_some())
            .count();
    if remaining != SNAPSHOT_PART_FIELDS.len() {
        anyhow::bail!("snapshot part cleanup did not converge: {remaining} remain");
    }
    if !matches!(
        fetch_snapshot_at_result(&base_key).await,
        SnapshotFetch::Ready(_)
    ) {
        anyhow::bail!("head-change cleanup left the current snapshot unreadable");
    }
    if h.kv.get(ordinary).await?.as_deref() != Some(b"ordinary-root") {
        anyhow::bail!("prefix-like ordinary key was reaped");
    }

    let departed_base = "node/guardian-diagnostic-departed/snapshot";
    h.kv.put(departed_base, b"{}".to_vec()).await?;
    let departed_digest = "a".repeat(64);
    let departed_parts = [
        "node/guardian-diagnostic-departed/snapshot-part/deployments".to_string(),
        format!("node/guardian-diagnostic-departed/snapshot-part/deployments/{departed_digest}"),
        format!("node/guardian-diagnostic-departed/snapshot-part-v2/deployments/{departed_digest}"),
        format!("node/guardian-diagnostic-departed/parts-v3/deployments/{departed_digest}"),
    ];
    for key in &departed_parts {
        h.kv.put(key, key.as_bytes().to_vec()).await?;
    }
    let malformed_departed =
        format!("node/guardian-diagnostic-departed/parts-v3/not-a-field/{departed_digest}");
    h.kv.put(&malformed_departed, b"malformed-ordinary-root".to_vec())
        .await?;
    let absent_exact =
        format!("node/guardian-diagnostic-departed/parts-v3/builds/{departed_digest}");
    let absent_exact_child = format!("{absent_exact}/ordinary");
    h.kv.put(&absent_exact_child, b"exact-delete-noop-root".to_vec())
        .await?;
    if delete_snapshot_key(h, &absent_exact).await? {
        anyhow::bail!("absent exact metadata delete unexpectedly reported a deletion");
    }
    if h.kv.get(&absent_exact_child).await?.as_deref() != Some(b"exact-delete-noop-root") {
        anyhow::bail!("absent exact metadata delete erased its prefix-like ordinary child");
    }
    if !delete_snapshot_key(h, &departed_parts[1]).await? {
        anyhow::bail!("interruption pre-delete did not remove the first exact child");
    }
    let (reaped, _) = reap_departed_node_snapshots().await;
    if reaped != 1 {
        anyhow::bail!("departed exact-key reap did not converge after interruption: {reaped}");
    }
    let heads = h.kv.entry_heads().await?;
    if heads
        .iter()
        .any(|head| head.key == departed_base || departed_parts.iter().any(|key| key == &head.key))
    {
        anyhow::bail!("departed base or exact part key survived reap");
    }
    if h.kv.get(&malformed_departed).await?.as_deref() != Some(b"malformed-ordinary-root") {
        anyhow::bail!("malformed departed prefix-like key was not retained as an ordinary root");
    }

    let p2 = guardian_v2_phase2_diagnostic(h).await?;

    Ok(format!(
        "HIVE_GUARDIAN_LIFECYCLE_PASS first_keep={} second_keep={} cleanup_passes={} normal_small={} first_5_plus_6_batch={} marker_backlog={} marker_passes={} concurrent_gc=true exact_reap=true ordinary_roots=true ambiguous_refusal=true population_change_refusal=true prepared_roots=true delete_failure_resume=true cancellation_resume=true head_change_between_groups=true {p2}",
        first_keep.len(),
        second_keep.len(),
        cleanup_passes,
        normal_deleted,
        first_historical_batch,
        marker_deleted,
        marker_passes,
    ))
}

/// Per-namespace HEAD map of this node's local GuardianDB replica: a (key, content-hash, timestamp)
/// triple per live entry, WITHOUT reading any value bytes (`entry_heads`). iroh-docs exposes no
/// per-namespace root hash, so a head is the set of per-key heads (today one namespace,
/// `KV_NAMESPACE`). Served at `GET /v1/guardian/heads` (`admin::guardian_heads` +
/// `gossip::dispatch`) so a peer can diff without pulling content.
pub async fn namespace_heads(
) -> std::collections::HashMap<String, Vec<guardian_db::traits::EntryHead>> {
    let mut out = std::collections::HashMap::new();
    if let Ok(h) = handle().await {
        match h.kv.entry_heads().await {
            Ok(heads) => {
                out.insert(KV_NAMESPACE.to_string(), heads);
            }
            Err(e) => tracing::debug!(error = %e, "guardian namespace_heads: entry_heads failed"),
        }
    }
    out
}

/// Triggers a REAL targeted iroh-docs range reconciliation of THIS node's `KV_NAMESPACE` document
/// against one peer, never a full-database refresh. `guardian_addr_json` MUST be the peer's
/// GuardianDB-specific address (see `seed_peer`). Returns the entries pulled/pushed, or the failure
/// reason.
pub async fn sync_with_peer(
    guardian_addr_json: &str,
) -> Result<guardian_db::traits::SyncOutcomeSummary, String> {
    let h = handle().await.map_err(|e| e.to_string())?;
    let addr: iroh::EndpointAddr = serde_json::from_str(guardian_addr_json)
        .map_err(|e| format!("malformed guardian addr: {e}"))?;
    h.kv.sync_with_peer(addr).await.map_err(|e| e.to_string())
}

/// Bounded on-disk blob-store snapshot refreshed on a background cadence
/// (`HIVE_GUARDIAN_STATS_SECS`, default 60 s), never on the request path. The store is the single
/// redb-backed `iroh_store/blobs.db` that inlines every small blob (every snapshot payload so far),
/// so `blobs_db_bytes` is where real growth shows; only blobs over redb's inline threshold spill to
/// files under `iroh_store/data/` (with `temp/`), and `docs_db_bytes` is the separate
/// `iroh_docs/docs.redb` entry history.
#[derive(Default, Clone, serde::Serialize)]
struct BlobStoreSnapshot {
    collected_at_ms: u64,
    /// `iroh_store/blobs.db` — the redb file holding every inlined blob.
    blobs_db_bytes: u64,
    /// `iroh_docs/docs.redb` — entry/metadata history for the KV document.
    docs_db_bytes: u64,
    /// Individual files under `iroh_store/data/` (blobs too large to inline); zero for this
    /// workload today.
    complete_bytes: u64,
    complete_count: u64,
    /// `iroh_store/temp/` — in-flight (including partial) large-blob writes
    /// not yet moved into `data/`.
    temp_bytes: u64,
    temp_count: u64,
    /// This pass hit `HIVE_GUARDIAN_STATS_MAX_FILES` while walking `data/`/`temp/`, so those two
    /// totals are a lower bound (the two `stat()` totals are never truncated).
    truncated: bool,
}

static BLOB_STORE_SNAPSHOT: OnceLock<std::sync::Mutex<BlobStoreSnapshot>> = OnceLock::new();

fn blob_store_snapshot_cell() -> &'static std::sync::Mutex<BlobStoreSnapshot> {
    BLOB_STORE_SNAPSHOT.get_or_init(|| std::sync::Mutex::new(BlobStoreSnapshot::default()))
}

fn guardian_stats_max_files() -> usize {
    std::env::var("HIVE_GUARDIAN_STATS_MAX_FILES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(200_000)
}

fn guardian_stats_interval_secs() -> u64 {
    std::env::var("HIVE_GUARDIAN_STATS_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(60)
}

/// Blocking I/O: run on `spawn_blocking`, never on the async runtime or request path. The two
/// `stat()` calls are O(1); the directory walks are bounded by `guardian_stats_max_files`.
fn collect_blob_store_snapshot() -> BlobStoreSnapshot {
    let max_files = guardian_stats_max_files();
    let mut out = BlobStoreSnapshot {
        collected_at_ms: hive_core::now_ms(),
        ..Default::default()
    };
    let guardian_dir = crate::persist::data_dir().join("guardian");
    let store_root = guardian_dir.join("iroh").join("iroh_store");
    if let Ok(meta) = std::fs::metadata(store_root.join("blobs.db")) {
        out.blobs_db_bytes = meta.len();
    }
    if let Ok(meta) = std::fs::metadata(
        guardian_dir
            .join("iroh")
            .join("iroh_docs")
            .join("docs.redb"),
    ) {
        out.docs_db_bytes = meta.len();
    }
    let mut scanned = 0usize;
    if let Ok(entries) = std::fs::read_dir(store_root.join("data")) {
        for entry in entries.flatten() {
            if scanned >= max_files {
                out.truncated = true;
                break;
            }
            scanned += 1;
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            out.complete_bytes += meta.len();
            out.complete_count += 1;
        }
    }
    if !out.truncated {
        if let Ok(entries) = std::fs::read_dir(store_root.join("temp")) {
            for entry in entries.flatten() {
                if scanned >= max_files {
                    out.truncated = true;
                    break;
                }
                scanned += 1;
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                out.temp_bytes += meta.len();
                out.temp_count += 1;
            }
        }
    }
    out
}

/// Spawn the periodic, bounded background collector. Called once from
/// `init_background`; no separate boot wiring needed elsewhere.
fn spawn_blob_stats_collector() {
    tokio::spawn(async move {
        loop {
            let snap = tokio::task::spawn_blocking(collect_blob_store_snapshot)
                .await
                .unwrap_or_default();
            *blob_store_snapshot_cell()
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = snap;
            tokio::time::sleep(std::time::Duration::from_secs(
                guardian_stats_interval_secs(),
            ))
            .await;
        }
    });
}

#[derive(Default, serde::Serialize)]
struct GcStatusView {
    /// `false` means GuardianDB itself is not reachable (init failed or wedged): every other field
    /// is then a meaningless zero, not "GC is healthy".
    reachable: bool,
    enabled: bool,
    running: bool,
    shutting_down: bool,
    /// The active pass crossed its advertised deadline and is still
    /// retained — a stalled GC looks like this, not like a flat "healthy".
    overdue: bool,
    /// Abort was requested but the JoinHandle has not resolved in time.
    stuck: bool,
    successful_runs: u64,
    failed_runs: u64,
    /// Typed refusals (no deletion) — distinct from hard failures.
    incomplete_runs: u64,
    consecutive_failures: u32,
    legacy_tags_removed: u64,
    /// Cumulative CONFIRMED reclaimed work across completed passes only.
    reclaimed_objects_total: u64,
    reclaimed_bytes_total: u64,
    last_attempt_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    active_deadline_ms: Option<u64>,
    last_success_ms: Option<u64>,
    overdue_since_ms: Option<u64>,
    last_error: Option<String>,
    /// Structured result of the most recent pass that reached a verdict.
    last_pass: Option<GcPassView>,
}

/// Serializable projection of the vendored `GcPassReport`.
#[derive(serde::Serialize)]
struct GcPassView {
    /// Typed status: `completed`, `incomplete_*` (typed refusal, no
    /// deletion), or `failed`.
    status: &'static str,
    reason: Option<String>,
    started_ms: u64,
    finished_ms: u64,
    duration_ms: u64,
    considered_objects: u64,
    considered_bytes: u64,
    reclaimed_objects: u64,
    reclaimed_bytes: u64,
    protected_objects: u64,
    reachable_entries: u64,
    descriptors: u64,
    skipped_objects: u64,
    legacy_tags_removed: u64,
}

impl From<guardian_db::p2p::network::core::GcPassReport> for GcPassView {
    fn from(report: guardian_db::p2p::network::core::GcPassReport) -> Self {
        Self {
            status: report.status.as_str(),
            reason: report.reason,
            started_ms: report.started_ms,
            finished_ms: report.finished_ms,
            duration_ms: report.duration_ms,
            considered_objects: report.considered_objects,
            considered_bytes: report.considered_bytes,
            reclaimed_objects: report.reclaimed_objects,
            reclaimed_bytes: report.reclaimed_bytes,
            protected_objects: report.protected_objects,
            reachable_entries: report.reachable_entries,
            descriptors: report.descriptors,
            skipped_objects: report.skipped_objects,
            legacy_tags_removed: report.legacy_tags_removed,
        }
    }
}

/// Per-part status of this node's own current v2 generation from BOUNDED lookups only (one
/// exact-prefix page per key, one head read), never a full store scan on the request path.
#[derive(serde::Serialize)]
struct GuardianV2PartStatusView {
    field: &'static str,
    sha256: String,
    declared_bytes: u64,
    /// The part's metadata entry exists with a matching declared length.
    ready: bool,
    entry_bytes: Option<u64>,
}

#[derive(serde::Serialize)]
struct GuardianV2StatusView {
    component: Option<String>,
    /// `ready` | `absent` | `incomplete` | `unreachable`: the restore path's tri-state with
    /// reachability made explicit; `absent` only when nothing exists under this node's v2 prefix
    /// head key.
    head_status: &'static str,
    /// Generation identity: the committed head's SHA-256 + BLAKE3 + saved_ms.
    head_sha256: Option<String>,
    head_blake3: Option<String>,
    saved_ms: Option<u64>,
    /// Complete-generation markers currently retained (bounded count).
    generation_markers: u64,
    generation_markers_truncated: bool,
    parts: Vec<GuardianV2PartStatusView>,
}

impl Default for GuardianV2StatusView {
    fn default() -> Self {
        Self {
            component: None,
            head_status: "absent",
            head_sha256: None,
            head_blake3: None,
            saved_ms: None,
            generation_markers: 0,
            generation_markers_truncated: false,
            parts: Vec::new(),
        }
    }
}

async fn guardian_v2_status_view(h: &Handle) -> GuardianV2StatusView {
    let mut view = GuardianV2StatusView {
        head_status: "absent",
        ..Default::default()
    };
    let Some(component) = NODE_NAME.get().filter(|name| !name.trim().is_empty()) else {
        view.head_status = "unreachable";
        return view;
    };
    view.component = Some(component.clone());
    let Ok(encoded) = guardian_v2_encode_component(component) else {
        view.head_status = "incomplete";
        return view;
    };
    let node_prefix = format!("{GUARDIAN_V2_NODE_PREFIX}{encoded}/");
    let head_key = format!("{node_prefix}head");

    let head_entry = match h.kv.entry_heads_page(&head_key, None, 2).await {
        Ok(page) => page.into_iter().find(|row| row.key == head_key),
        Err(_) => {
            view.head_status = "unreachable";
            return view;
        }
    };
    let Some(head_entry) = head_entry else {
        return view;
    };
    view.head_blake3 = Some(head_entry.hash.clone());
    view.saved_ms = Some(head_entry.timestamp / 1000);

    let head = match h.kv.get(&head_key).await {
        Ok(Some(bytes)) => {
            view.head_sha256 = Some(hex::encode(sha256(&bytes)));
            match guardian_v2_parse_head(&bytes) {
                Ok(head) => Some(head),
                Err(_) => None,
            }
        }
        _ => None,
    };
    let Some(head) = head else {
        view.head_status = "incomplete";
        return view;
    };

    let mut all_ready = true;
    for kind in GuardianV2PartKind::ALL {
        let Some(reference) = head.references.get(kind.field()) else {
            all_ready = false;
            continue;
        };
        let Ok(part_key) = guardian_v2_part_key(&encoded, kind, &reference.sha256) else {
            all_ready = false;
            continue;
        };
        let entry = match h.kv.entry_heads_page(&part_key, None, 2).await {
            Ok(page) => page.into_iter().find(|row| row.key == part_key),
            Err(_) => None,
        };
        let entry_bytes = entry.as_ref().map(|row| row.content_len);
        let ready = entry_bytes == Some(reference.bytes as u64);
        all_ready &= ready;
        view.parts.push(GuardianV2PartStatusView {
            field: kind.field(),
            sha256: reference.sha256.clone(),
            declared_bytes: reference.bytes as u64,
            ready,
            entry_bytes,
        });
    }
    view.head_status = if all_ready { "ready" } else { "incomplete" };

    let gen_prefix = format!("{node_prefix}{GUARDIAN_V2_GEN_SEGMENT}/");
    let mut cursor: Option<String> = None;
    for _ in 0..8 {
        match h
            .kv
            .entry_heads_page(&gen_prefix, cursor.as_deref(), 512)
            .await
        {
            Ok(page) => {
                let exhausted = page.len() < 512;
                view.generation_markers += page.len() as u64;
                cursor = page.into_iter().last().map(|row| row.key);
                if exhausted {
                    return view;
                }
            }
            Err(_) => return view,
        }
    }
    view.generation_markers_truncated = true;
    view
}

/// Bounded snapshot of the v2 retention loop's state.
#[derive(serde::Serialize)]
struct RetentionView {
    passes: u64,
    refusals: u64,
    retired_generations_total: u64,
    retired_parts_total: u64,
    unrooted_bytes_total: u64,
    cursor: Option<String>,
    last: Option<RetentionReport>,
}

#[derive(serde::Serialize)]
struct ProtectedRootsView {
    /// Live count of temporarily protected snapshot PART keys (this process's in-flight
    /// publications, `PreparedSnapshotRoots`), read as an in-memory map length.
    part_keys: usize,
    /// Live count of temporarily protected snapshot BASE (full-snapshot)
    /// digests.
    base_digests: usize,
}

#[derive(serde::Serialize)]
struct GuardianWriteStatsResponse {
    generations: GenerationCounters,
    writes: WriteCounters,
    blob_store: BlobStoreSnapshot,
    protected_roots: ProtectedRootsView,
    gc: GcStatusView,
    /// This node's own current v2 generation: head identity + per-part
    /// hash/bytes/readiness (bounded lookups, no store scan).
    v2: GuardianV2StatusView,
    /// The v2 retention loop's bounded pass/refusal/cursor state.
    retention: RetentionView,
}

async fn guardian_write_stats_handler() -> axum::Json<GuardianWriteStatsResponse> {
    let protected_roots = {
        let roots = prepared_snapshot_roots()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ProtectedRootsView {
            part_keys: roots.part_keys.len(),
            base_digests: roots.base_digests.len(),
        }
    };

    let (gc, v2) = match handle().await {
        Ok(h) => {
            let health = h.client.backend().gc_health().await;
            let gc = GcStatusView {
                reachable: true,
                enabled: health.enabled,
                running: health.running,
                shutting_down: health.shutting_down,
                overdue: health.overdue,
                stuck: health.stuck,
                successful_runs: health.successful_runs,
                failed_runs: health.failed_runs,
                incomplete_runs: health.incomplete_runs,
                consecutive_failures: health.consecutive_failures,
                legacy_tags_removed: health.legacy_tags_removed,
                reclaimed_objects_total: health.reclaimed_objects_total,
                reclaimed_bytes_total: health.reclaimed_bytes_total,
                last_attempt_ms: health.last_attempt_ms,
                last_heartbeat_ms: health.last_heartbeat_ms,
                active_deadline_ms: health.active_deadline_ms,
                last_success_ms: health.last_success_ms,
                overdue_since_ms: health.overdue_since_ms,
                last_error: health.last_error,
                last_pass: health.last_pass.map(GcPassView::from),
            };
            (gc, guardian_v2_status_view(h).await)
        }
        Err(_) => (
            GcStatusView::default(),
            GuardianV2StatusView {
                head_status: "unreachable",
                ..Default::default()
            },
        ),
    };

    let retention = {
        let state = retention_state().lock().unwrap_or_else(|p| p.into_inner());
        RetentionView {
            passes: state.passes,
            refusals: state.refusals,
            retired_generations_total: state.retired_generations_total,
            retired_parts_total: state.retired_parts_total,
            unrooted_bytes_total: state.unrooted_bytes_total,
            cursor: state.cursor.clone(),
            last: state.last.clone(),
        }
    };

    axum::Json(GuardianWriteStatsResponse {
        generations: generation_counters()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone(),
        writes: write_counters_snapshot(),
        blob_store: blob_store_snapshot_cell()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone(),
        protected_roots,
        gc,
        v2,
        retention,
    })
}

/// Self-contained router merged onto the auth-enforced admin router in `main.rs` (same
/// `.merge(crate::<module>::routes())` shape as `hrana`, `drive_api` and `browser_admission`).
/// Node-local, like `/v1/dns/stats`: read per node, never aggregated.
pub fn routes() -> axum::Router<Arc<crate::state::CloudState>> {
    axum::Router::new().route(
        "/v1/admin/guardian/write-stats",
        axum::routing::get(guardian_write_stats_handler),
    )
}
