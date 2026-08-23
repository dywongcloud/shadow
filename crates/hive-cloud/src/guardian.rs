//! GuardianDB backend — **always on**, in local/dev and production alike.
//!
//! [guardian-db] is an iroh-native, content-addressed store (iroh-docs +
//! iroh-blobs, BLAKE3, QUIC). Every platform snapshot is written into a real
//! GuardianDB key/value store: persisted on disk under `$HIVE_DATA/guardian` and
//! replicated across the iroh mesh via Willow range-based reconciliation. This is
//! NOT a mock — it is the durable, peer-replicated copy of platform state.
//!
//! The local file snapshot ([`crate::persist`]) remains the bootstrap source of
//! truth for fast cold start; GuardianDB adds durability + replication on top.
//! Init and writes are best-effort: any failure is logged and never breaks the
//! request path or the on-disk snapshot, so a node always boots and serves.
//!
//! State is partitioned by **tenant namespace** ([`crate::persist::namespaced`])
//! and each namespace is stored under its own key (`ns/<namespace>/state`), so
//! every org/team's projects, deployments and databases stay scoped and isolated
//! in the replicated store — never commingled in one global blob.
//!
//! [guardian-db]: https://github.com/wmaslonek/guardian-db

use std::sync::{Arc, OnceLock};

use guardian_db::guardian::core::NewGuardianDBOptions;
use guardian_db::guardian::error::GuardianError;
use guardian_db::guardian::GuardianDB;
use guardian_db::p2p::network::client::IrohClient;
use guardian_db::p2p::network::config::ClientConfig;
use guardian_db::traits::KeyValueStore;
use tokio::sync::OnceCell;

use crate::persist::PlatformSnapshot;

/// Live GuardianDB handle: the database (kept alive so its backend / iroh
/// endpoint stays running), the opened key/value store, and a CLONE of the
/// iroh client used to seed known-peer addresses after construction (see
/// `seed_known_peers`). `IrohClient` is `#[derive(Clone)]` over an inner
/// `Arc<IrohBackend>`, so this clone shares the exact same live endpoint
/// `GuardianDB` itself uses internally — seeding through it reaches the real
/// connection the docs/blobs Willow sync dials from.
struct Handle {
    db: GuardianDB,
    kv: Arc<dyn KeyValueStore<Error = GuardianError>>,
    client: IrohClient,
}

static HANDLE: OnceCell<Handle> = OnceCell::const_new();

/// Name of the sole iroh-docs KV namespace this node currently opens (see
/// `init_handle`). Extracted to a const so the head-CID-exchange RPC
/// (`namespace_heads`) doesn't repeat the literal — kept open for when a node
/// opens more than one store.
const KV_NAMESPACE: &str = "hive-state";

/// Convenience alias for the native relational/SQL database this handle backs
/// (see [`crate::relational`]) — `guardian_db::sql::open_sql`'s return type.
pub(crate) type SqlDb =
    Arc<guardian_db::sql::Database<guardian_db::sql::GuardianRelationalStorage>>;

static SQL_HANDLE: OnceCell<SqlDb> = OnceCell::const_new();

/// Lazily open (once) the native in-process relational/SQL database backed by
/// this SAME live, relay-patched `GuardianDB` instance `handle()` uses — zero
/// network hop, NOT the pgwire protocol (that layer is never enabled; see
/// `crates/hive-cloud/Cargo.toml`'s guardian-db feature comment). Every node
/// opens the identical named database ("hive"); guardian-db's own iroh-docs
/// CRDT replication converges each node's local copy of every table within
/// seconds of a write anywhere in the fleet.
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

/// Upper bound on the whole GuardianDB bring-up (iroh endpoint bind, keystore,
/// docs/blobs spawn). Live evidence (2026-07-06 onward) showed init can wedge
/// indefinitely with zero error and zero log output — `tokio::sync::OnceCell`
/// then blocks every future caller forever, since a never-resolving init future
/// never lets `get_or_try_init` return. A bounded timeout converts that into a
/// clean, retryable failure so the NEXT call to `handle()` can make progress
/// instead of joining a wedged wait forever.
///
/// This bounds the WAIT, never the work. An earlier version of this comment
/// claimed the dropped init future released "its owned FsStore/redb/iroh
/// Endpoint handles ... synchronously on `Drop`, so a retry does not inherit
/// stuck locks" — that was WRONG, and live evidence contradicted it: the
/// spawned actor tasks inside guardian-db hold the Arcs, so a cancelled init
/// kept the redb lock and the next attempt hit "Database already open". See
/// [`INIT_INFLIGHT`] for what actually happens on expiry now.
///
/// `HIVE_GUARDIAN_INIT_TIMEOUT_MS` overrides the 30s default. Tunable because a
/// slow host legitimately needs longer, and because the expiry path is otherwise
/// unreachable to exercise on a healthy node — a very low value drives the
/// park-and-re-await branch on demand.
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

/// Minimum spacing between full init attempts after a failure. Every guardian
/// caller (gossip puts, the anti-entropy loop, the relational mirror loop,
/// admin SQL reads) retries `handle()` on its own cadence; without this gate
/// a persistent failure had them collectively re-running `init_handle` —
/// a FULL iroh endpoint bind + blobs/docs store bring-up — every few seconds.
/// Live-witnessed on fc-virginia/fc-virginia-3: each failed attempt leaked
/// the partially-built client (spawned actor tasks keep the Arc alive), and
/// the retry storm grew hive-cloud to a 96.9GB anon-RSS OOM kill in ~8h,
/// freezing two hosts into console-reboot territory.
const INIT_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Set once an init failure proves in-process retries can never succeed. Two
/// causes latch it:
///  - redb's "Database already open. Cannot acquire lock." — a PREVIOUS
///    leaked/timed-out attempt in THIS process still holds the file lock, so
///    every further attempt is guaranteed to fail, and to leak another full
///    iroh client doing so.
///  - a PANIC inside init (see `handle`) — deterministic, so it will panic
///    identically on every retry.
///
/// Once latched, `handle()` fails fast, naming the reason and the only real
/// recovery (restart the node process).
static INIT_WEDGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Why the wedge latched, so the fail-fast message doesn't misattribute a
/// panic to redb's lock (which sent an operator down the wrong path once).
static WEDGE_REASON: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static LAST_FAILED_INIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// How many init attempts THIS process has made. Used to tell apart the two very
/// different causes of redb's "Database already open": a lock held by a prior
/// attempt of OURS (permanent — latch), versus a lock still held by the OUTGOING
/// process during a `systemctl restart` overlap (transient — retry). See the
/// latch site for the incident this distinction fixes.
static INIT_ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The in-flight init task, when a previous caller stopped WAITING for it.
///
/// `init_handle` already shuts the partial client down on every failure past
/// `IrohClient::new`, but that code cannot run when the init future is
/// CANCELLED — and `tokio::time::timeout` cancels by dropping. So a timed-out
/// init used to strand everything it had built: the iroh endpoint and
/// blobs/docs stores stayed alive (their spawned actor tasks hold the Arc), the
/// redb file lock was never released, and `INIT_RETRY_BACKOFF` then started a
/// SECOND full init 30s later on top of the first. Every subsequent attempt
/// added another stranded stack, so the node both wedged on redb's "Database
/// already open" and grew without bound — measured on fc-sanjose-2 at ~7 GB/h
/// up to a 47.3 GB RSS with guardian permanently unavailable.
///
/// The fix is to stop cancelling the WORK when we stop cancelling the WAIT: the
/// init runs in its own task, and a caller that times out parks the
/// `JoinHandle` here instead of dropping the future. The next caller awaits the
/// SAME attempt rather than racing a second one, so at most one init stack can
/// ever exist, and a slow-but-successful init is adopted instead of thrown away.
static INIT_INFLIGHT: std::sync::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<Handle>>>> =
    std::sync::Mutex::new(None);

fn inflight_slot(
) -> std::sync::MutexGuard<'static, Option<tokio::task::JoinHandle<anyhow::Result<Handle>>>> {
    // A poisoned lock here must not take guardian down — the slot holds a
    // JoinHandle, and recovering it is strictly better than failing init.
    INIT_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner())
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
            // A PANIC inside init must not unwind past this closure. It now runs
            // in its own task, so the panic arrives as a `JoinError` rather than
            // needing `catch_unwind` around the future.
            // Live-witnessed fleet-wide: guardian-db's `IrohClient::new` panics
            // "Hash table capacity overflow" (hashbrown) on EVERY attempt. A
            // panic unwinds straight past the failure bookkeeping below, so
            // `LAST_FAILED_INIT_MS` was never stamped, the backoff gate never
            // engaged, and every guardian caller re-ran a full init immediately
            // — ~18 panics/minute on all 7 nodes, indefinitely. Converting the
            // panic into an `Err` lets the existing throttle apply, and because
            // a deterministic panic cannot succeed on retry we also latch the
            // wedge so subsequent calls fail fast instead of re-panicking.
            // Adopt an init that a previous caller stopped waiting for, rather
            // than spawning a second one on top of it (see `INIT_INFLIGHT`).
            // Only a genuinely NEW attempt counts toward `INIT_ATTEMPTS`, which
            // keeps the "Database already open" discrimination below honest:
            // attempts > 1 now means we really did start a second full init.
            let mut task = match inflight_slot().take() {
                Some(existing) => {
                    tracing::info!("guardian init: re-awaiting the init already in flight (not starting a second one)");
                    existing
                }
                None => {
                    INIT_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(init_handle())
                }
            };
            // The timeout bounds only how long we WAIT. On expiry the task is
            // parked, still running, so nothing it built is stranded.
            match tokio::time::timeout(guardian_init_timeout(), &mut task).await {
                Ok(Ok(result)) => result,
                Err(_) => {
                    *inflight_slot() = Some(task);
                    Err(anyhow::anyhow!(
                        "guardian init timed out after {:?} (iroh endpoint bind / keystore / docs bring-up never completed); the attempt is still running and will be re-awaited, not restarted",
                        guardian_init_timeout()
                    ))
                }
                Ok(Err(join_err)) => {
                    // The task panicked (or was aborted, which we never do).
                    // `JoinError` carries the payload, so the panic no longer has
                    // to be caught with `catch_unwind` around the future itself.
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
            // Only OUR OWN leaked attempt makes this permanent. On the FIRST
            // attempt of a fresh process the lock belongs to somebody else —
            // in practice the outgoing hive-cloud during a `systemctl restart`
            // overlap, which releases it moments later.
            //
            // Latching on that transient case disabled guardian for the entire
            // life of the new process: witnessed on fc-bangkok latching 31s
            // into a FRESH start and then rejecting every mesh/roster put with
            // "restart hive-cloud to recover" — advice that could not work,
            // because each restart re-raced the same overlap. Retry instead;
            // the existing INIT_RETRY_BACKOFF throttles it and a genuinely
            // stuck lock still latches on the next attempt.
            let attempts = INIT_ATTEMPTS.load(Ordering::Relaxed);
            if attempts <= 1 {
                // LAST_FAILED_INIT_MS is already stamped just above, so the
                // normal INIT_RETRY_BACKOFF gate applies and the next caller
                // retries once the outgoing process has released the lock.
                tracing::warn!(
                    attempts,
                    "guardian init hit redb 'Database already open' on this process's FIRST attempt — the lock is held by ANOTHER process (typically the outgoing hive-cloud during a restart overlap), not by a leak of ours; retrying after backoff instead of latching"
                );
            } else {
                INIT_WEDGED.store(true, Ordering::Relaxed);
                tracing::error!(
                    attempts,
                    "guardian init hit redb 'Database already open' after repeated attempts — a leaked prior attempt in THIS process holds the lock; latching wedged state (no further in-process retries; restart hive-cloud to recover)"
                );
            }
        }
    }
    result
}

/// The actual GuardianDB bring-up, run under `handle()`'s timeout. Broken into
/// its own function (rather than inlined in the `OnceCell` closure) so each
/// major step logs a distinct marker — on a future stall, the log shows
/// exactly which sub-step (iroh client vs. GuardianDB open vs. KV open) never
/// returned, instead of the totally silent gap this replaces.
async fn init_handle() -> anyhow::Result<Handle> {
    let dir = crate::persist::data_dir().join("guardian");
    std::fs::create_dir_all(&dir).ok();

    // Its own iroh endpoint (random UDP port, n0 discovery for the
    // INITIAL bind) — independent of the request-routing mesh in
    // hive-p2p, but NOT independent of the platform's own known peer
    // set: `seed_known_peers` (called periodically from the gossip
    // loop, see main.rs) registers every mesh peer's iroh address
    // directly via `add_node_addr`, so cross-node docs/blobs sync
    // works from OUR OWN gossip-derived membership instead of
    // depending on n0's public discovery service ever finding this
    // node's peers (the same class of unreliable-from-cloud-hosts
    // discovery the main mesh already moved off of; see hive-p2p's
    // self-hosted relay/Seer migration history).
    let cfg = ClientConfig {
        data_store_path: Some(dir.join("iroh")),
        enable_discovery_n0: true,
        port: 0,
        ..ClientConfig::default()
    };
    tracing::info!("guardian init: opening iroh client (endpoint bind + keystore)");
    let client = IrohClient::new(cfg)
        .await
        .map_err(|e| anyhow::anyhow!("guardian iroh client: {e}"))?;
    let node_id = client.node_id();
    tracing::info!(%node_id, "guardian init: iroh client ready");
    // Clone BEFORE GuardianDB::new consumes `client` by value — the
    // clone shares the same underlying Arc<IrohBackend>/endpoint.
    let seed_client = client.clone();

    // Seed known peers BEFORE the KV store opens. `key_value()` below runs
    // exactly once (cached in the OnceCell `Handle`) and, on that single
    // call, guardian-db tries automatic DocTicket exchange with whatever
    // peers are already in `known_peers` (lowest-EndpointId node creates the
    // namespace, everyone else imports its ticket — see
    // IrohBackend::resolve_shared_ticket). Miss this window and every node
    // independently creates its OWN namespace instead. `BOOT_SEED_PEERS`
    // holds GuardianDB-specific addresses only (never hive-p2p mesh
    // addresses — see seed_peer's doc comment for why that distinction is
    // load-bearing, not cosmetic).
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

    // The database must share the client's iroh backend (its endpoint,
    // blobs + docs stores) — pass it explicitly in the options.
    let opts = NewGuardianDBOptions {
        directory: Some(dir.clone()),
        backend: Some(client.backend().clone()),
        ..Default::default()
    };
    // On ANY failure past this point the partially-built client MUST be shut
    // down before returning Err: its iroh endpoint, blobs/docs stores and
    // spawned actor tasks otherwise outlive the dropped handle (tasks hold
    // the Arc), leaking the whole stack per retry — the exact mechanism
    // behind the 96.9GB OOM freeze on the Virginia hosts.
    let db = match GuardianDB::new(client, Some(opts)).await {
        Ok(db) => db,
        Err(e) => {
            let _ = seed_client.shutdown().await;
            return Err(anyhow::anyhow!("guardian open: {e}"));
        }
    };
    tracing::info!("guardian init: GuardianDB open, opening 'hive-state' KV store");
    let kv = match db.key_value(KV_NAMESPACE, None).await {
        Ok(kv) => kv,
        Err(e) => {
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

/// Register a peer's iroh address AND mark it known, against a specific
/// `IrohClient`. `add_node_addr` registers a static `MemoryLookup` entry
/// (address resolution only). `note_known_peer` is the SEPARATE set that
/// `IrohBackend::resolve_shared_ticket`'s automatic DocTicket exchange
/// actually consults — `add_node_addr` alone never touches it. CALLERS MUST
/// PASS THIS NODE'S GUARDIANDB-SPECIFIC ADDRESS, never its hive-p2p mesh
/// address: GuardianDB runs its own, separate iroh identity per node (a
/// different EndpointId — confirmed empirically, logged as a different
/// node_id than the mesh's peer_id). Feeding a peer's mesh identity here
/// previously caused a live, reverted retry-storm (endpoint.connect() to a
/// NodeId nothing is listening as under this ALPN); this exists to prevent a
/// repeat, not just document one. `addr_json` is a serialized
/// `iroh::EndpointAddr` (same iroh version as hive-p2p, per Cargo.lock, but
/// the VALUE must originate from `my_iroh_addr()` — see `NodeInfo.
/// guardian_iroh_addr`'s doc comment for how it's gossiped). A malformed/
/// unreachable entry is skipped, never aborts the others.
async fn seed_peer(client: &IrohClient, addr_json: &str) -> bool {
    match serde_json::from_str::<iroh::EndpointAddr>(addr_json) {
        Ok(addr) => {
            let peer_id = addr.id;
            if let Err(e) = client.add_node_addr(addr).await {
                tracing::debug!(error = %e, "guardian seed_peer: add_node_addr failed");
                return false;
            }
            client.backend().note_known_peer(peer_id).await;
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "guardian seed_peer: malformed addr_json");
            false
        }
    }
}

/// Seed known peers (GuardianDB-specific addresses — see `seed_peer`) against
/// the already-open `Handle`'s client. Best-effort and idempotent — called on
/// every gossip round so a newly-joined or address-changed peer is picked up
/// without a restart. The KV store itself only opens once (cached in the
/// `OnceCell`), so this cannot retroactively fix an already-diverged
/// namespace on THIS node, but keeps `known_peers` fresh for any future
/// re-open (retry after a prior init failure, etc).
pub async fn seed_known_peers(guardian_addr_jsons: &[String]) {
    let Ok(h) = handle().await else { return };
    for addr_json in guardian_addr_jsons {
        seed_peer(&h.client, addr_json).await;
    }
}

/// Snapshot of peers' GuardianDB-specific addresses to seed the known-peer set
/// with on the FIRST (only) GuardianDB KV-store open — the one window
/// `resolve_shared_ticket`'s automatic DocTicket exchange is ever consulted
/// in (see `init_handle`). Set once, from main.rs, right before
/// `init_background()`; a second call is a no-op (`OnceLock`). Best-effort:
/// an empty/stale snapshot just means this node falls back to creating its
/// own namespace, never a hard failure. Ongoing peer changes after boot are
/// covered by the periodic `seed_known_peers` above.
static BOOT_SEED_PEERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Record peers' current GuardianDB-specific addresses for `init_handle` to
/// seed on the FIRST GuardianDB KV-store open. Call before
/// `init_background()`. CALLERS MUST PASS `guardian_iroh_addr`, never
/// `iroh_addr` — see `seed_peer`'s doc comment.
pub fn set_boot_seed_peers(guardian_addr_jsons: Vec<String>) {
    let _ = BOOT_SEED_PEERS.set(guardian_addr_jsons);
}

/// This node's OWN GuardianDB-specific dialable address (serialized
/// `iroh::EndpointAddr`), for the caller to gossip so PEERS can seed it
/// correctly (see `NodeInfo.guardian_iroh_addr`). `None` until GuardianDB's
/// client has finished binding — best-effort, never blocks; the caller
/// re-polls this every gossip round until it resolves.
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
            Ok(h) => tracing::info!(keys = h.kv.all().len(), "GuardianDB online"),
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

/// Handle of the main tokio runtime, captured at boot. `replicate` is called
/// from the dedicated `hive-persister` OS thread, which has NO runtime context —
/// a bare `tokio::spawn` there panics ("must be called from the context of a
/// Tokio runtime"), killing the persister thread and silently dropping every
/// periodic GuardianDB replication. Spawn onto this handle instead.
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

pub fn set_node_name(name: &str) {
    let _ = NODE_NAME.set(name.to_string());
    // Called from async main → the runtime is current here; remember it for
    // spawns from non-runtime threads (the persister).
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        let _ = RUNTIME.set(h);
    }
}

fn snapshot_key() -> Option<String> {
    NODE_NAME.get().map(|n| format!("node/{n}/snapshot"))
}

/// Replicate a snapshot into GuardianDB, one document per tenant namespace, PLUS
/// the full snapshot under this node's own key (`node/<name>/snapshot`) — the
/// durable copy the boot-time rollback guard restores from when the local file
/// regressed. Spawned so persistence is never blocked on replication.
type SnapshotDigest = [u8; 32];

#[derive(Clone)]
struct PreparedValue {
    change_digest: SnapshotDigest,
    bytes: Arc<Vec<u8>>,
}

#[derive(Clone)]
struct KeyedPreparedValue {
    key: String,
    /// Bounded write-observability key class (`"full"` or one of
    /// `SNAPSHOT_PART_FIELDS`) — never a per-tenant/per-key string, so the
    /// counters this drives (`WRITE_COUNTERS`) stay a fixed small set instead
    /// of growing per namespace/deployment.
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
const VOLATILE_ROOT_FIELDS: [&str; 1] = ["saved_ms"];

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

/// Cumulative, monotonic counts of every replication-generation status
/// transition this process has ever observed. Bounded (six fixed fields,
/// never grows) — the generation-level half of `/v1/admin/guardian/write-stats`.
/// `queued_total`/`superseded_total` are what let an operator see churn a
/// single `Committed`-only view would hide: a generation that gets
/// superseded before it is ever written never shows up as a write at all.
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

/// Per-write-class outcome counters — the key-class half of
/// `/v1/admin/guardian/write-stats`. `attempted` fires once the change digest
/// is known and compared; `no_op` is the subset that matched the live blob
/// and was never PUT (the guardian-blob-regrowth-after-cf2b2ba fix's proof
/// point: a stable no_op count with near-zero committed_bytes across repeat
/// no-change writes is what shows the fix is holding). `committed` is the
/// subset that was actually PUT to GuardianDB; `failed` is a PUT that
/// returned an error (attempted/no_op still count — the digest comparison
/// still happened).
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

/// Fixed key-class set: `"namespaces"` (aggregate over every tenant
/// namespace document — NOT one entry per tenant, which would be unbounded),
/// `"full"` (this node's own full-snapshot replica), and one entry per
/// `SNAPSHOT_PART_FIELDS` name. `parts` is a `BTreeMap` keyed by that fixed
/// field-name set, not by tenant/key, so it stays at exactly
/// `SNAPSHOT_PART_FIELDS.len()` entries for the life of the process.
#[derive(Default, Clone, serde::Serialize)]
struct WriteCounters {
    namespaces: ClassWriteCounters,
    parts: std::collections::BTreeMap<String, ClassWriteCounters>,
    full: ClassWriteCounters,
    namespace_deletes: u64,
    part_reap_deletes: u64,
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
    /// `(key_class, canonical_bytes)` per part key — the class is one of the
    /// fixed `SNAPSHOT_PART_FIELDS` names, carried through so the write
    /// chokepoint can attribute observability counters without re-parsing
    /// the key.
    parts: std::collections::BTreeMap<String, (&'static str, Vec<u8>)>,
}

/// Canonical node backup split into one base plus independently-addressed large
/// or frequently-changing fields. The base carries a digest manifest and is
/// written last, making it the commit marker for an exact set of part bytes.
/// This preserves authoritative deployments, build logs, database data,
/// metrics and sandbox records without copying every one of them into a new
/// multi-megabyte blob when only one field changes. Only write-attempt metadata
/// and non-authoritative SIEM delivery counters are omitted. Every part and the
/// base use the same canonical bytes for their digest and Guardian payload.
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
        // iroh-docs PUT has prefix-replacement semantics. Parts must not be
        // descendants of the base key (`.../snapshot`), or committing the base
        // deletes the parts it references in the same operation.
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
    for (namespace, value) in crate::persist::namespaced(snap) {
        let bytes = canonical_json_bytes(value)?;
        namespaces.insert(
            format!("ns/{namespace}/state"),
            PreparedValue {
                change_digest: sha256(&bytes),
                bytes: Arc::new(bytes),
            },
        );
    }
    let (parts, full) = match snapshot_key() {
        Some(key) => {
            let canonical = shared_snapshot_canonical(snap, &key)?;
            let parts = canonical
                .parts
                .into_iter()
                .map(|(part_key, (class, bytes))| KeyedPreparedValue {
                    key: part_key,
                    class,
                    value: PreparedValue {
                        change_digest: sha256(&bytes),
                        bytes: Arc::new(bytes),
                    },
                })
                .collect();
            let bytes = canonical.base;
            let full = Some(KeyedPreparedValue {
                key,
                class: "full",
                value: PreparedValue {
                    change_digest: sha256(&bytes),
                    bytes: Arc::new(bytes),
                },
            });
            (parts, full)
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

async fn delete_snapshot_key(h: &Handle, key: &str) -> anyhow::Result<bool> {
    let before = h.kv.entry_heads().await?;
    if !before.iter().any(|head| head.key == key) {
        return Ok(false);
    }
    if let Some(collision) = before
        .iter()
        .find(|head| head.key != key && head.key.as_bytes().starts_with(key.as_bytes()))
    {
        // iroh-docs deletion is prefix-based. Refuse rather than let an exact
        // metadata retirement erase an unrelated descendant key.
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
        // A part published after the current base is a preparation, not a stale
        // generation. The in-process registry additionally covers an old digest
        // deliberately being prepared for recommit.
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

    // Prove the entire addressed candidate population before selecting a bounded
    // batch. This is what keeps batching from turning a suspicious global set
    // into a series of locally-small, unjustified deletes.
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

    // Addressed generations are data descendants and always retire before their
    // protection-marker bases. The whole candidate population was proven above;
    // only then is the deterministic oldest/lexical prefix selected.
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

    // Markers are a separate population. A marker is eligible only when its
    // digest is neither the current base nor any Drop-guarded preparation and it
    // predates the current base. Five verified current parts anchor the fraction,
    // so even a marker-only backlog converges instead of sticking at one item.
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

async fn write_replication_batch(
    desired: &DesiredReplication,
    committed: &CommittedReplication,
    owned_namespaces: &mut std::collections::BTreeMap<String, SnapshotDigest>,
) -> anyhow::Result<ReplicationWriteStats> {
    let h = handle().await?;
    let live = h.kv.all();
    let mut stats = ReplicationWriteStats::default();

    for (key, value) in &desired.namespaces {
        let live_matches = live
            .get(key)
            .is_some_and(|bytes| sha256(bytes) == value.change_digest);
        {
            let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
            counters.namespaces.record_attempt(value.bytes.len());
            if live_matches {
                counters.namespaces.record_no_op(value.bytes.len());
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
            put_result.map_err(|e| anyhow::anyhow!("guardian namespace put {key}: {e}"))?;
            owned_namespaces.insert(key.clone(), value.change_digest);
            stats.namespace_puts += 1;
            stats.put_bytes += value.bytes.len();
        }
    }

    let mut deletes: Vec<String> = live
        .iter()
        .filter_map(|(key, bytes)| {
            if !is_namespace_state_key(key) || desired.namespaces.contains_key(key) {
                return None;
            }
            let live_digest = sha256(bytes);
            let owned = owned_namespaces.get(key) == Some(&live_digest)
                || committed.namespaces.get(key) == Some(&live_digest);
            owned.then(|| key.clone())
        })
        .collect();
    deletes.sort_unstable();
    for key in deletes {
        h.kv.delete(&key)
            .await
            .map_err(|e| anyhow::anyhow!("guardian namespace delete {key}: {e}"))?;
        owned_namespaces.remove(&key);
        stats.deletes += 1;
        write_counters()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .namespace_deletes += 1;
    }

    if let Some(full) = &desired.full {
        // All local publication and cleanup for this node's fixed base key share
        // one writer gate. The per-delete head proofs below still defend against
        // replicated head movement, while this lock closes the local recommit race.
        let _lifecycle = SNAPSHOT_LIFECYCLE_LOCK.lock().await;

        // Install Drop-released temporary roots atomically against GC before the
        // first part publication. Protect the base blob too: its document head is
        // not a durable root until the final publication succeeds.
        let mut prepared_hashes: Vec<_> = desired
            .parts
            .iter()
            .map(|part| iroh_blobs::Hash::new(part.value.bytes.as_ref()))
            .collect();
        prepared_hashes.push(iroh_blobs::Hash::new(full.value.bytes.as_ref()));
        let mut part_guard = h
            .client
            .protect_hashes(prepared_hashes)
            .await
            .map_err(|error| anyhow::anyhow!("guardian snapshot parts protect: {error}"))?;

        // Tag installation is complete. Release only its GC setup gate before
        // publishing: iroh-docs takes the same fair RwLock in its actor, so holding
        // a read guard while a collector writer is queued would deadlock the next
        // publication. BlobProtection continues to own every temporary tag until
        // it is dropped after the committed manifest has been verified.
        part_guard.finish_tag_installation();
        let prepared_registration = PreparedSnapshotRegistration::new(
            &full.key,
            full.value.change_digest,
            desired.parts.iter().map(|part| part.key.clone()).collect(),
        );

        // Parts are installed before the base manifest. The base is the commit
        // marker, so readers never adopt a manifest whose referenced bytes were not
        // successfully written by this batch.
        for part in &desired.parts {
            let live_matches = live
                .get(&part.key)
                .is_some_and(|bytes| sha256(bytes) == part.value.change_digest);
            {
                let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
                let class = counters.parts.entry(part.class.to_string()).or_default();
                class.record_attempt(part.value.bytes.len());
                if live_matches {
                    class.record_no_op(part.value.bytes.len());
                }
            }
            if !live_matches {
                let put_result = h
                    .kv
                    .put_gc_protected(&part.key, part.value.bytes.as_ref().clone(), &part_guard)
                    .await;
                {
                    let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
                    let class = counters.parts.entry(part.class.to_string()).or_default();
                    if put_result.is_ok() {
                        class.record_committed(part.value.bytes.len());
                    } else {
                        class.record_failed();
                    }
                }
                put_result
                    .map_err(|e| anyhow::anyhow!("guardian snapshot part put {}: {e}", part.key))?;
                stats.full_puts += 1;
                stats.put_bytes += part.value.bytes.len();
            }
        }

        let live_matches = live
            .get(&full.key)
            .is_some_and(|bytes| sha256(bytes) == full.value.change_digest);
        {
            let mut counters = write_counters().lock().unwrap_or_else(|p| p.into_inner());
            counters.full.record_attempt(full.value.bytes.len());
            if live_matches {
                counters.full.record_no_op(full.value.bytes.len());
            }
        }
        if !live_matches {
            let put_result =
                h.kv.put_gc_protected(&full.key, full.value.bytes.as_ref().clone(), &part_guard)
                    .await;
            if put_result.is_ok() {
                write_counters()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .full
                    .record_committed(full.value.bytes.len());
            } else {
                write_counters()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .full
                    .record_failed();
            }
            put_result
                .map_err(|e| anyhow::anyhow!("guardian full snapshot put {}: {e}", full.key))?;
            stats.full_puts += 1;
            stats.put_bytes += full.value.bytes.len();
        }

        // Keep temporary roots until the exact base bytes we prepared are the
        // stable readable current head and every manifest reference is bound to
        // its own stable current head and immutable bytes. A successful PUT alone
        // is not proof: a conflicting/malformed head must never authorize
        // retirement. Before the base PUT, any error or cancellation drops only
        // this prepared generation's temporary roots; the prior committed
        // generation remains untouched.
        let (committed_base_digest, committed_parts) =
            verified_committed_part_keys(h, &full.key).await?;
        if committed_base_digest != full.value.change_digest {
            anyhow::bail!(
                "committed Guardian snapshot base does not match the prepared generation"
            );
        }
        let desired_parts: std::collections::BTreeSet<_> =
            desired.parts.iter().map(|part| part.key.clone()).collect();
        if committed_parts != desired_parts {
            anyhow::bail!(
                "committed Guardian snapshot manifest does not match the prepared generation"
            );
        }

        // No await may intervene in this transfer: current document metadata now
        // owns reachability for the exact prepared generation, so dropping the
        // temporary tags cannot expose it to GC.
        drop(part_guard);
        drop(prepared_registration);
        let reaped = cleanup_committed_snapshot_parts_locked(h, &full.key).await?;
        stats.deletes += reaped;
        if reaped > 0 {
            write_counters()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .part_reap_deletes += reaped as u64;
        }
    } else if !desired.parts.is_empty() {
        anyhow::bail!("Guardian snapshot parts exist without a base commit marker");
    }

    Ok(stats)
}

async fn replication_writer(
    mut receiver: tokio::sync::watch::Receiver<Arc<DesiredReplication>>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut committed = CommittedReplication::default();
    let mut owned_namespaces = std::collections::BTreeMap::new();
    let mut retry_delay = REPLICATION_RETRY_MIN;
    loop {
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
                let cleanup_secs = std::env::var("HIVE_GUARDIAN_PART_REAP_CHECK_SECS")
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or(600);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(cleanup_secs)) => {}
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

fn ensure_replication_writer(
    runtime: &tokio::runtime::Handle,
    initial: Arc<DesiredReplication>,
) -> &'static tokio::sync::watch::Sender<Arc<DesiredReplication>> {
    REPLICATION_QUEUE.get_or_init(|| {
        let (sender, receiver) = tokio::sync::watch::channel(initial);
        let shutdown = REPLICATION_SHUTDOWN
            .get_or_init(tokio_util::sync::CancellationToken::new)
            .clone();
        let task = runtime.spawn(replication_writer(receiver, shutdown));
        let slot = REPLICATION_WRITER.get_or_init(|| std::sync::Mutex::new(None));
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
        sender
    })
}

/// Admit one exact replication generation. Once shutdown closes admission this
/// returns `None`; no caller can enqueue work behind the final drain target.
pub fn replicate(snap: &PlatformSnapshot) -> Option<u64> {
    let runtime = tokio::runtime::Handle::try_current()
        .ok()
        .or_else(|| RUNTIME.get().cloned());
    let Some(runtime) = runtime else {
        tracing::error!("no tokio runtime; Guardian replication generation was not admitted");
        return None;
    };

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
    let sender = ensure_replication_writer(&runtime, desired.clone());
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

/// Failure-path close used when the terminal file write itself cannot complete.
/// It still freezes Guardian at the latest already-admitted generation so the
/// backend can drain and shut down without accepting work during process exit.
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

fn guardian_shutdown_timeout() -> std::time::Duration {
    std::env::var("HIVE_GUARDIAN_SHUTDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(60))
}

/// Drain the terminal replication generation, stop and join its sole writer,
/// then durably shut down Docs, Router, blob store, and endpoint in the backend.
pub async fn shutdown(final_generation: Option<u64>) -> anyhow::Result<()> {
    let final_generation = close_replication_admission(final_generation)?;
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

async fn fetch_snapshot_at_result(key: &str) -> SnapshotFetch {
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
    let Ok(Some(bytes)) = h.kv.get(key).await else {
        return SnapshotFetch::Incomplete;
    };
    // The value index deliberately retains its prior readable bytes when a
    // newer doc head's blob is unavailable. Bind bytes and freshness to the
    // same head; never stamp fallback bytes with an unrelated newer timestamp.
    if iroh_blobs::Hash::new(&bytes).to_hex() != head.hash {
        return SnapshotFetch::Incomplete;
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

    // Old monolithic snapshots have no manifest. The first split format stores
    // a digest string and resolves the fixed `<base>-part/<field>` key. Current
    // manifests bind both the immutable content-addressed key and its digest.
    if let Some(manifest) = manifest {
        for (field, reference) in manifest {
            if !SNAPSHOT_PART_FIELDS.contains(&field.as_str()) {
                return SnapshotFetch::Incomplete;
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
                return SnapshotFetch::Incomplete;
            };
            if hex::encode(sha256(&part_bytes)) != expected {
                return SnapshotFetch::Incomplete;
            }
            let Ok(mut part) = serde_json::from_slice::<serde_json::Value>(&part_bytes) else {
                return SnapshotFetch::Incomplete;
            };
            let Some(part_value) = part.as_object_mut().and_then(|part| part.remove(&field)) else {
                return SnapshotFetch::Incomplete;
            };
            let Some(root) = value.as_object_mut() else {
                return SnapshotFetch::Incomplete;
            };
            root.insert(field, part_value);
        }
        if SNAPSHOT_PART_FIELDS
            .iter()
            .any(|field| value.get(*field).is_none())
        {
            return SnapshotFetch::Incomplete;
        }
    }

    let Ok(mut snapshot) = serde_json::from_value::<PlatformSnapshot>(value) else {
        return SnapshotFetch::Incomplete;
    };
    // `saved_ms` is write-attempt volatility and is deliberately absent from
    // canonical payloads. The iroh-docs head timestamp is the durable commit
    // time for these exact bytes, so restore freshness remains meaningful
    // without manufacturing a new content blob on every unchanged attempt.
    snapshot.saved_ms = written_ms;
    SnapshotFetch::Ready(snapshot)
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

/// The newest replicated snapshot from any OTHER node (`node/<peer>/snapshot`),
/// discovered by enumerating the replicated store's own keys (no registry
/// dependency — at the boot moment this runs, gossip may not have resynced
/// yet, but replicated keys survive in the local guardian store). Returns the
/// owning peer's name alongside the snapshot.
pub async fn fetch_newest_peer_snapshot() -> Option<(String, PlatformSnapshot)> {
    let me = NODE_NAME.get()?.as_str();
    let mut best: Option<(String, PlatformSnapshot)> = None;
    for key in keys().await {
        let Some(name) = key
            .strip_prefix("node/")
            .and_then(|r| r.strip_suffix("/snapshot"))
        else {
            continue;
        };
        if name == me {
            continue;
        }
        let Some(snap) = fetch_snapshot_at(&key).await else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(_, b)| snap.saved_ms > b.saved_ms)
        {
            best = Some((name.to_string(), snap));
        }
    }
    best
}

/// Strip the NODE-LOCAL runtime portions from a snapshot before adopting it
/// from a PEER: `deployments` are this-node cell/serving records (adopting a
/// peer's would fabricate phantom deployments pointing at cells that only
/// exist on the peer), `sandboxes` likewise, `metrics_rollup` is per-node
/// observed traffic (adopting a peer's would double-count its traffic here),
/// and `database_data` is the in-process store payload for locally-hosted DBs.
/// Everything else — projects, teams, billing, domains, webhooks, DB records,
/// workflow defs, enterprise config, users/orgs — is tenant/control-plane
/// state shared fleet-wide via gossip, exactly what a wiped node needs back.
fn strip_node_local(mut snap: PlatformSnapshot) -> PlatformSnapshot {
    snap.deployments = Vec::new();
    snap.sandboxes = Default::default();
    snap.metrics_rollup = Default::default();
    snap.database_data = Default::default();
    snap.builds = Vec::new();
    snap
}

/// Boot-time restore-on-rollback guard: once GuardianDB is online, compare its
/// replicated snapshot's `saved_ms` against the CURRENT on-disk snapshot. If the
/// replica is NEWER, the local file regressed (crash-restored old disk, wiped
/// data dir, bad copy) — adopt the replica: restore it into the live state and
/// rewrite the local file. The comparison re-reads the disk at adoption time, so
/// any post-boot user mutation (which bumps the local `saved_ms` past the
/// replica's) automatically vetoes adoption — no clobbering live changes.
///
/// PEER FALLBACK (audit proposal step 9): when this node has NO replicated
/// snapshot of its own (guardian dir wiped alongside the local file — total
/// loss), fall back to the newest PEER snapshot in the replicated store,
/// adopting only its SHARED tenant/control-plane state (node-local runtime
/// stripped — see `strip_node_local`). Before this, the guard could never
/// recover a node's data from a peer's copy, defeating its own stated purpose.
/// Retries for ~60s: after a wipe, the replicated keys only reappear once the
/// guardian store has synced with a peer.
/// Opt-out: `HIVE_GUARDIAN_RESTORE=0`.
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
                    crate::persist::restore(&cloud, replica);
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

            // Re-check self first on every cadence. A peer is only remembered
            // while the self base is genuinely absent, and is never adopted
            // until all self retries are exhausted. This gives an independently
            // arriving self base and its delayed parts the full retry budget.
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
                    crate::persist::restore(&cloud, strip_node_local(snapshot));
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

/// Remove a key from GuardianDB (replicated tombstone). Best-effort, same
/// failure posture as `put` -- used to retire transient mesh-shared entries
/// (e.g. a delivered/dead-lettered world_queue job) once no longer needed.
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

/// Snapshot of all keys currently stored in GuardianDB (durable copy).
pub async fn keys() -> Vec<String> {
    match handle().await {
        Ok(h) => h.kv.all().into_keys().collect(),
        Err(_) => Vec::new(),
    }
}

/// Operator-declared roster of node NAMES that legitimately belong to this
/// fleet (`HIVE_NODE_ROSTER`, comma-separated). Empty/unset returns `None`,
/// which makes [`reap_departed_node_snapshots`] refuse to do anything at all.
///
/// This is deliberately a NAME list and deliberately its own variable:
/// `HIVE_TRUSTED_NODE_IDS` carries 64-hex peer IDs, not names, so it cannot
/// answer "is `node/fc-lax2/snapshot` a key for a node we still run".
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

/// Reap `node/<name>/snapshot` keys belonging to nodes that have left the fleet.
///
/// # The problem this solves
///
/// A doc entry (key + content hash) replicates independently of whether any
/// peer can still serve that hash's BLOB. When a node leaves — renamed, retired,
/// reprovisioned — its snapshot key lives on forever while its blob becomes
/// permanently unfetchable, because the only node that ever held those bytes is
/// gone. `kv_store`'s index sync then retries the fetch on EVERY pass, forever.
/// Measured live 2026-07-31: ~35 warnings/minute on every one of 14 nodes, for
/// `node/fc-lax2/snapshot`, `node/fc-cvm-sj-1/snapshot` and
/// `node/fc-cvm-sj-2/snapshot` — none of which are in the fleet any more. That
/// volume also MASKS the genuinely transient failures on live nodes' keys,
/// which is the worse half of the cost.
///
/// # Why age comes from the entry head, not the snapshot
///
/// The obvious age source — the snapshot's own `saved_ms` — is unreadable by
/// construction here: the blob cannot be fetched, which is the entire reason
/// the key is a candidate. [`namespace_heads`] returns iroh-docs'
/// `Entry::timestamp()` WITHOUT reading any value bytes, so it works precisely
/// where a content read cannot.
///
/// # Refusal conditions (all mirror `gc_rootfs_images`'s blast-radius rules)
///
/// Deleting from the replicated store is irreversible and fans out to every
/// peer, so this refuses rather than guesses:
/// * no `HIVE_NODE_ROSTER` configured → refuse entirely (fail closed — an
///   unset roster makes EVERY node look departed, the exact shape of the
///   empty-keep-set bug that would have deleted every live deployment's disk);
/// * more than `HIVE_REAP_MAX_FRACTION` of node keys look reapable → refuse,
///   because that means the roster is wrong, not that the fleet vanished;
/// * this node's own key is never a candidate, whatever the roster says.
///
/// Leader-only by caller contract: this is a replicated-store MUTATION, and the
/// platform's single-writer discipline for those routes them through the
/// control-plane leader.
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

    // iroh-docs stamps entries in microseconds; `now_ms` is milliseconds. Age
    // the whole node prefix by its newest entry, so an in-progress part prepare
    // cannot be mistaken for an old orphan before its base commit arrives.
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

        // The base is the commit marker and is always last. If any exact part
        // delete failed, retain it so the next pass can rediscover and finish.
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

    // Report BOTH halves: a reap that withheld everything must be
    // distinguishable from one with nothing to do (the same "ineffective vs
    // exhausted" lesson `ensure_disk_headroom` learned the hard way).
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
    // Semantically-valid `builds: []` wrappers with distinct trailing whitespace
    // give the real content-addressed path unique immutable generations.
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
    if first_keep.len() != SNAPSHOT_PART_FIELDS.len()
        || !matches!(
            fetch_snapshot_at_result(&base_key).await,
            SnapshotFetch::Ready(_)
        )
    {
        anyhow::bail!("first non-prefix split commit is incomplete");
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

    // A malformed reserved child and a stale fixed legacy key each make the
    // global shape unprovable. Neither refusal may consume a valid addressed
    // candidate sitting beside it.
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

    // Five current parts plus six fully-proven addressed generations exceed the
    // default 0.5 global candidate fraction. The verified population is batched
    // to five deletes, not refused and not silently truncated before proof.
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

    // Change the reserved key population after every initial candidate has been
    // proved but before batching. The exact before/after head maps must refuse
    // the whole pass without consuming any member of the previously-valid set.
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

    // Move the head between bounded passes onto the one generation the first
    // pass deliberately left. The next pass must preserve it and reap the prior
    // builds generation instead.
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

    // A Drop-registered preparation protects both an already-addressed part and
    // its generation marker even after a newer base timestamp would otherwise
    // classify them as stale. Releasing the guard makes each independently
    // resumable population converge on the following passes.
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

    // Invalid marker grammar is globally ambiguous and must refuse without
    // nibbling the marker population.
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

    // 257 exact stale markers used to exceed max_keys forever. They now retire
    // in fraction- and key-bounded passes, while the marker for the current base
    // stays protected until a later head makes it stale.
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

    // Inject a failure after one confirmed exact delete. The first tombstone is
    // durable progress; the retry resumes from the two remaining keys.
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

    // Cancellation after one confirmed delete has the same restart point. The
    // lifecycle lock is Drop-released with the aborted future.
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

    // Change the base while a pass is paused between exact delete groups. The
    // next group re-verifies all six current heads and skips the freshly-current
    // target; only a following inventory pass discovers the old current key.
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

    Ok(format!(
        "HIVE_GUARDIAN_LIFECYCLE_PASS first_keep={} second_keep={} cleanup_passes={} normal_small={} first_5_plus_6_batch={} marker_backlog={} marker_passes={} concurrent_gc=true exact_reap=true ordinary_roots=true ambiguous_refusal=true population_change_refusal=true prepared_roots=true delete_failure_resume=true cancellation_resume=true head_change_between_groups=true",
        first_keep.len(),
        second_keep.len(),
        cleanup_passes,
        normal_deleted,
        first_historical_batch,
        marker_deleted,
        marker_passes,
    ))
}

/// design-head-cid-exchange-rpc's data source: per-namespace HEAD map for
/// this node's local GuardianDB replica — a (key, content-hash, timestamp)
/// triple for every live entry, WITHOUT ever reading a value's bytes (see
/// `guardian_db::traits::KeyValueStore::entry_heads`, backed by iroh-docs'
/// `Entry::content_hash()`/`timestamp()`). There is no single per-namespace
/// root hash exposed by iroh-docs' public API, so a namespace's "head" is
/// represented as the set of its per-key heads. Today there is exactly one
/// namespace (`KV_NAMESPACE`); the map shape stays open for future stores.
/// Served over the mesh at `GET /v1/guardian/heads` (see `admin::guardian_heads`
/// + `gossip::dispatch`) so a peer can diff against its own without pulling
/// any content.
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

/// implement-reconciliation-trigger: trigger a REAL targeted iroh-docs
/// range-reconciliation sync of THIS node's `KV_NAMESPACE` document against
/// one specific peer — never a full-database refresh. `guardian_addr_json`
/// MUST be that peer's GuardianDB-specific address (`NodeInfo.
/// guardian_iroh_addr`), never its hive-p2p mesh address — see `seed_peer`'s
/// doc comment for why that distinction is load-bearing. Returns the real
/// entries pulled/pushed on success, or the failure reason on error (for
/// implement-convergence-logging's warn-level path).
pub async fn sync_with_peer(
    guardian_addr_json: &str,
) -> Result<guardian_db::traits::SyncOutcomeSummary, String> {
    let h = handle().await.map_err(|e| e.to_string())?;
    let addr: iroh::EndpointAddr = serde_json::from_str(guardian_addr_json)
        .map_err(|e| format!("malformed guardian addr: {e}"))?;
    h.kv.sync_with_peer(addr).await.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// guardian-growth-and-gc-observability: bounded, node-local write/GC
// observability. Every field below is either an in-memory counter updated at
// the exact write/GC chokepoint (`write_replication_batch`,
// `set_generation_status`) or a periodically-collected, cached snapshot
// (`spawn_blob_stats_collector`) — the request handler itself never touches
// the store tree. Mirrors the `/v1/dns/stats` / `/v1/databases/sqlite-pools`
// node-local-status precedent: no cross-node aggregation, read per node.
// ---------------------------------------------------------------------------

/// Bounded on-disk blob-store snapshot, refreshed on a background cadence
/// (`HIVE_GUARDIAN_STATS_SECS`, default 60s) — never computed on the request
/// path. **Verified live against a running node, not inferred from reading
/// `iroh-blobs` source alone**: this vendored build opens the store as
/// `FsStore::load_with_opts(iroh_store/blobs.db, ..)` (see
/// `guardian_db::p2p::network::core::mod.rs`'s `init_blobs`/`repo_stat`) — a
/// single redb-backed file that inlines every small blob (every Guardian
/// snapshot payload observed so far: a few KB to a few hundred KB). Only a
/// blob over redb's inline threshold spills to an individual file under
/// `iroh_store/data/` (with its `temp/` staging directory) — for this
/// workload those stay empty, so `blobs_db_bytes` is where the real growth
/// shows up, not `complete_bytes`/`temp_bytes`. `docs_db_bytes` is
/// `iroh_docs/docs.redb`, the separate document-metadata/entry-history
/// store this same replica writes to on every commit.
#[derive(Default, Clone, serde::Serialize)]
struct BlobStoreSnapshot {
    collected_at_ms: u64,
    /// `iroh_store/blobs.db` — the redb file holding every inlined blob.
    blobs_db_bytes: u64,
    /// `iroh_docs/docs.redb` — entry/metadata history for the KV document.
    docs_db_bytes: u64,
    /// Individual files under `iroh_store/data/` — blobs too large to
    /// inline into `blobs.db`. Zero for this workload today; wired for when
    /// a part payload eventually crosses the inline threshold.
    complete_bytes: u64,
    complete_count: u64,
    /// `iroh_store/temp/` — in-flight (including partial) large-blob writes
    /// not yet moved into `data/`.
    temp_bytes: u64,
    temp_count: u64,
    /// This pass hit `HIVE_GUARDIAN_STATS_MAX_FILES` while walking
    /// `data/`/`temp/` and stopped early — those two totals are then a
    /// lower bound, not exact, for this one collection (`blobs_db_bytes`/
    /// `docs_db_bytes` are single `stat()` calls and are never truncated).
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

/// Blocking I/O — run on a `spawn_blocking` thread, never on the async
/// runtime or the request path. The two `stat()` calls (`blobs.db`,
/// `docs.redb`) are O(1) and never truncate; the `data/`/`temp/` directory
/// walks are bounded by `guardian_stats_max_files` for the day a payload
/// crosses redb's inline threshold and starts spilling to individual files.
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
    if let Ok(meta) = std::fs::metadata(guardian_dir.join("iroh").join("iroh_docs").join("docs.redb"))
    {
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
    /// `false` means GuardianDB itself is not reachable right now (init
    /// failed/wedged) — every other field below is then a meaningless
    /// zero/false, not "GC is healthy".
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
    consecutive_failures: u32,
    legacy_tags_removed: u64,
    last_attempt_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    active_deadline_ms: Option<u64>,
    last_success_ms: Option<u64>,
    overdue_since_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(serde::Serialize)]
struct ProtectedRootsView {
    /// Live count of temporarily protected snapshot PART keys (this
    /// process's own in-flight publications) — `PreparedSnapshotRoots`,
    /// read as an in-memory map length, never a store scan.
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

    let gc = match handle().await {
        Ok(h) => {
            let health = h.client.backend().gc_health().await;
            GcStatusView {
                reachable: true,
                enabled: health.enabled,
                running: health.running,
                shutting_down: health.shutting_down,
                overdue: health.overdue,
                stuck: health.stuck,
                successful_runs: health.successful_runs,
                failed_runs: health.failed_runs,
                consecutive_failures: health.consecutive_failures,
                legacy_tags_removed: health.legacy_tags_removed,
                last_attempt_ms: health.last_attempt_ms,
                last_heartbeat_ms: health.last_heartbeat_ms,
                active_deadline_ms: health.active_deadline_ms,
                last_success_ms: health.last_success_ms,
                overdue_since_ms: health.overdue_since_ms,
                last_error: health.last_error,
            }
        }
        Err(_) => GcStatusView::default(),
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
    })
}

/// Self-contained router merged directly onto the auth-enforced admin router
/// in `main.rs` (`admin.rs` is a different write surface's exclusive scope
/// for this change; this stays self-contained so no line there needs to
/// change) — same `.merge(crate::<module>::routes())` shape as `hrana`,
/// `drive_api`, `browser_admission`, etc. Node-local, like `/v1/dns/stats`:
/// read it per node, never aggregated.
pub fn routes() -> axum::Router<Arc<crate::state::CloudState>> {
    axum::Router::new().route(
        "/v1/admin/guardian/write-stats",
        axum::routing::get(guardian_write_stats_handler),
    )
}
