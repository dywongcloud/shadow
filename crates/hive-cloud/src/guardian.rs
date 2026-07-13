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

use std::sync::Arc;

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
pub(crate) type SqlDb = Arc<guardian_db::sql::Database<guardian_db::sql::GuardianRelationalStorage>>;

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
/// clean, retryable failure: the in-flight future is dropped (its owned
/// FsStore/redb/iroh Endpoint handles release synchronously on `Drop`, so a
/// retry does not inherit stuck locks), and the NEXT call to `handle()` tries
/// again from scratch instead of joining a wedged wait forever.
const GUARDIAN_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

/// Set once an init failure proves in-process retries can never succeed:
/// redb's "Database already open. Cannot acquire lock." means a PREVIOUS
/// leaked/timed-out attempt in THIS process still holds the file lock, so
/// every further attempt is guaranteed to fail — and to leak another full
/// iroh client doing so. Once latched, `handle()` fails fast with a message
/// naming the only real recovery (restart the node process).
static INIT_WEDGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_FAILED_INIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Lazily open (once) the GuardianDB KV store, retrying on a previous failure
/// — but throttled (see `INIT_RETRY_BACKOFF`) and never after a wedge latch.
async fn handle() -> anyhow::Result<&'static Handle> {
    use std::sync::atomic::Ordering;
    if let Some(h) = HANDLE.get() {
        return Ok(h);
    }
    if INIT_WEDGED.load(Ordering::Relaxed) {
        anyhow::bail!(
            "guardian is wedged in this process (redb lock held by a leaked prior init attempt); restart hive-cloud to recover"
        );
    }
    let last_failed = LAST_FAILED_INIT_MS.load(Ordering::Relaxed);
    if last_failed != 0 {
        let since = hive_core::now_ms().saturating_sub(last_failed);
        if since < INIT_RETRY_BACKOFF.as_millis() as u64 {
            anyhow::bail!("guardian init failed {since}ms ago; backing off before the next attempt");
        }
    }
    let result = HANDLE
        .get_or_try_init(|| async {
            match tokio::time::timeout(GUARDIAN_INIT_TIMEOUT, init_handle()).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!(
                    "guardian init timed out after {GUARDIAN_INIT_TIMEOUT:?} (iroh endpoint bind / keystore / docs bring-up never completed); will retry after backoff"
                )),
            }
        })
        .await;
    if let Err(e) = &result {
        LAST_FAILED_INIT_MS.store(hive_core::now_ms(), Ordering::Relaxed);
        if e.to_string().contains("Database already open") {
            INIT_WEDGED.store(true, Ordering::Relaxed);
            tracing::error!(
                "guardian init hit redb 'Database already open' — a leaked prior attempt holds the lock; \
                 latching wedged state (no further in-process retries; restart hive-cloud to recover)"
            );
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
    tracing::info!(count = boot_seeded, "guardian init: seeded known peers pre-open (for automatic DocTicket exchange)");

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
    Ok(Handle { db, kv, client: seed_client })
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
            Err(e) => tracing::warn!(error = %e, "GuardianDB init failed (snapshot kept on disk); will retry"),
        }
    });
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
pub fn replicate(snap: &PlatformSnapshot) {
    let docs = crate::persist::namespaced(snap);
    let payloads: Vec<(String, Vec<u8>)> = docs
        .into_iter()
        .filter_map(|(ns, doc)| serde_json::to_vec(&doc).ok().map(|v| (ns, v)))
        .collect();
    let full = serde_json::to_vec(snap).ok();
    let task = async move {
        let h = match handle().await {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(error = %e, "GuardianDB unavailable; snapshot kept on disk only");
                return;
            }
        };
        for (ns, json) in payloads {
            let key = format!("ns/{ns}/state");
            if let Err(e) = h.kv.put(&key, json).await {
                tracing::debug!(namespace = %ns, error = %e, "GuardianDB put failed");
            }
        }
        if let (Some(key), Some(bytes)) = (snapshot_key(), full) {
            if let Err(e) = h.kv.put(&key, bytes).await {
                tracing::debug!(error = %e, "GuardianDB full-snapshot put failed");
            }
        }
    };
    // Callers include the runtime-less `hive-persister` OS thread — a bare
    // `tokio::spawn` would panic there and kill the persister (see RUNTIME).
    match tokio::runtime::Handle::try_current().ok().or_else(|| RUNTIME.get().cloned()) {
        Some(rt) => {
            rt.spawn(task);
        }
        None => tracing::debug!("no tokio runtime; guardian replication skipped (snapshot on disk only)"),
    }
}

/// The replicated full snapshot for THIS node, if GuardianDB holds one.
pub async fn fetch_node_snapshot() -> Option<PlatformSnapshot> {
    let key = snapshot_key()?;
    let bytes = get(&key).await?;
    serde_json::from_slice(&bytes).ok()
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
        let Some(name) = key.strip_prefix("node/").and_then(|r| r.strip_suffix("/snapshot")) else {
            continue;
        };
        if name == me {
            continue;
        }
        let Some(bytes) = get(&key).await else { continue };
        let Ok(snap) = serde_json::from_slice::<PlatformSnapshot>(&bytes) else { continue };
        if best.as_ref().is_none_or(|(_, b)| snap.saved_ms > b.saved_ms) {
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
    if std::env::var("HIVE_GUARDIAN_RESTORE").map(|v| v == "0" || v == "false").unwrap_or(false) {
        return;
    }
    tokio::spawn(async move {
        let self_replica = fetch_node_snapshot().await;
        let Some(replica) = self_replica else {
            // No self-keyed replica — total-loss path. Give replication a
            // window to pull peer keys in, then adopt the newest peer copy's
            // SHARED state if it beats whatever the local file has.
            for attempt in 1u32..=6 {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let Some((peer, snap)) = fetch_newest_peer_snapshot().await else { continue };
                let local = crate::persist::load();
                if snap.saved_ms <= local.saved_ms {
                    tracing::debug!(peer = %peer, "guardian restore guard: peer snapshot not newer than local; keeping local");
                    return;
                }
                tracing::warn!(
                    peer = %peer,
                    attempt,
                    peer_ms = snap.saved_ms,
                    local_ms = local.saved_ms,
                    "TOTAL-LOSS RECOVERY — no self snapshot replicated; adopting SHARED state from newest peer snapshot (node-local runtime stripped)"
                );
                crate::persist::restore(&cloud, strip_node_local(snap));
                crate::persist::persist(&cloud);
                tracing::info!(peer = %peer, "guardian restore guard: shared state restored from peer snapshot");
                return;
            }
            tracing::debug!("guardian restore guard: no replicated snapshot (self or peer) found");
            return;
        };
        let local = crate::persist::load();
        if replica.saved_ms <= local.saved_ms {
            tracing::debug!(replica_ms = replica.saved_ms, local_ms = local.saved_ms, "guardian restore guard: local snapshot is current");
            return;
        }
        tracing::warn!(
            replica_ms = replica.saved_ms,
            local_ms = local.saved_ms,
            behind_secs = (replica.saved_ms.saturating_sub(local.saved_ms)) / 1000,
            "SNAPSHOT ROLLBACK DETECTED — local state older than the GuardianDB replica; restoring from replica"
        );
        crate::persist::restore(&cloud, replica);
        crate::persist::persist(&cloud); // rewrite the local file from the restored state
        tracing::info!("guardian restore guard: state restored from replicated snapshot");
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
pub async fn namespace_heads() -> std::collections::HashMap<String, Vec<guardian_db::traits::EntryHead>> {
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
pub async fn sync_with_peer(guardian_addr_json: &str) -> Result<guardian_db::traits::SyncOutcomeSummary, String> {
    let h = handle().await.map_err(|e| e.to_string())?;
    let addr: iroh::EndpointAddr =
        serde_json::from_str(guardian_addr_json).map_err(|e| format!("malformed guardian addr: {e}"))?;
    h.kv.sync_with_peer(addr).await.map_err(|e| e.to_string())
}
