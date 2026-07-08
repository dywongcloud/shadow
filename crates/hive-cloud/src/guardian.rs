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
    _db: GuardianDB,
    kv: Arc<dyn KeyValueStore<Error = GuardianError>>,
    client: IrohClient,
}

static HANDLE: OnceCell<Handle> = OnceCell::const_new();

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

/// Lazily open (once) the GuardianDB KV store, retrying on a previous failure.
async fn handle() -> anyhow::Result<&'static Handle> {
    HANDLE
        .get_or_try_init(|| async {
            match tokio::time::timeout(GUARDIAN_INIT_TIMEOUT, init_handle()).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!(
                    "guardian init timed out after {GUARDIAN_INIT_TIMEOUT:?} (iroh endpoint bind / keystore / docs bring-up never completed); will retry on next call"
                )),
            }
        })
        .await
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
    // exactly once (cached in the OnceCell `Handle`) and, on that single call,
    // guardian-db tries automatic DocTicket exchange with whatever peers are
    // already in its `known_peers` set (lowest-EndpointId node creates the
    // namespace, everyone else imports its ticket — see
    // IrohBackend::resolve_shared_ticket). Miss this window and every node
    // independently creates its OWN namespace instead — the actual root cause
    // behind the fleet's frozen, per-node-divergent key counts (verified live:
    // zero cross-node content after minutes of healthy mesh uptime once init
    // stopped hanging). `add_node_addr` alone (below, and in the periodic
    // `seed_known_peers`) only teaches the endpoint HOW to reach a peer; it
    // does not enter `known_peers`, which only `note_known_peer` populates —
    // that distinction is exactly what this boot-time seed step closes.
    let boot_seed_count = seed_peers(&client, BOOT_SEED_PEERS.get().map(Vec::as_slice).unwrap_or(&[])).await;
    tracing::info!(count = boot_seed_count, "guardian init: seeded known peers pre-open (for automatic DocTicket exchange)");

    // The database must share the client's iroh backend (its endpoint,
    // blobs + docs stores) — pass it explicitly in the options.
    let opts = NewGuardianDBOptions {
        directory: Some(dir.clone()),
        backend: Some(client.backend().clone()),
        ..Default::default()
    };
    let db = GuardianDB::new(client, Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("guardian open: {e}"))?;
    tracing::info!("guardian init: GuardianDB open, opening 'hive-state' KV store");
    let kv = db
        .key_value("hive-state", None)
        .await
        .map_err(|e| anyhow::anyhow!("guardian kv open: {e}"))?;

    tracing::info!(%node_id, dir = ?dir, "GuardianDB ready (iroh-docs KV 'hive-state', replicated)");
    Ok(Handle { _db: db, kv, client: seed_client })
}

/// Register every given peer's iroh address AND mark it as a known peer,
/// against a specific `IrohClient` (used both pre-open, against a client that
/// doesn't have a `Handle` yet, and post-open via `seed_known_peers` below).
/// `add_node_addr` (`IrohClient`, a real tested upstream API) adds a static
/// `MemoryLookup` entry to the endpoint's address-lookup services — the
/// mechanism hive-p2p's bootstrap-seed path already uses for the main mesh.
/// `note_known_peer` (`IrohBackend`, also public) is the SEPARATE set that
/// `resolve_shared_ticket`'s automatic DocTicket exchange actually consults —
/// `add_node_addr` alone never touches it. `addr_json` entries are the SAME
/// serialized `iroh::EndpointAddr` format hive-p2p stores in `NodeInfo.iroh_addr`
/// / `peer_iroh` (both crates share one `iroh = "1.0.0"` resolution per
/// Cargo.lock, so the type is literally identical, no translation needed). A
/// malformed/unreachable entry is skipped, never aborts the others. Returns
/// how many entries were successfully seeded (for logging).
async fn seed_peers(client: &IrohClient, addr_jsons: &[String]) -> usize {
    let mut seeded = 0usize;
    for addr_json in addr_jsons {
        match serde_json::from_str::<iroh::EndpointAddr>(addr_json) {
            Ok(addr) => {
                let peer_id = addr.id;
                if let Err(e) = client.add_node_addr(addr).await {
                    tracing::debug!(error = %e, "guardian seed_peers: add_node_addr failed");
                    continue;
                }
                client.backend().note_known_peer(peer_id).await;
                seeded += 1;
            }
            Err(e) => tracing::debug!(error = %e, "guardian seed_peers: malformed addr_json"),
        }
    }
    seeded
}

/// Snapshot of mesh peer iroh addresses to seed GuardianDB's known-peer set
/// with the FIRST time its iroh client is constructed (see `init_handle`).
/// Set once, from main.rs, right before `init_background()` — best-effort:
/// an empty/stale snapshot just means this node falls back to creating its
/// own namespace (the pre-fix behavior), never a hard failure.
static BOOT_SEED_PEERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Record the mesh's currently-known peer iroh addresses for `init_handle` to
/// seed on the FIRST (only) GuardianDB KV-store open. Call before
/// `init_background()`. A second call is a no-op (`OnceLock`) — boot-time
/// only; ongoing peer changes are covered by the periodic `seed_known_peers`.
pub fn set_boot_seed_peers(addr_jsons: Vec<String>) {
    let _ = BOOT_SEED_PEERS.set(addr_jsons);
}

/// Periodic (gossip-loop-cadence) re-seed of known peers against the already-
/// open `Handle`'s client. Keeps `known_peers` fresh for peers that join or
/// change address after boot; the KV store itself only opens once, so this
/// cannot retroactively fix an already-diverged namespace, but keeps the
/// automatic-exchange machinery ready for any future re-open (retry after a
/// prior init failure, etc).
pub async fn seed_known_peers(addr_jsons: &[String]) {
    let Ok(h) = handle().await else { return };
    seed_peers(&h.client, addr_jsons).await;
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

/// Boot-time restore-on-rollback guard: once GuardianDB is online, compare its
/// replicated snapshot's `saved_ms` against the CURRENT on-disk snapshot. If the
/// replica is NEWER, the local file regressed (crash-restored old disk, wiped
/// data dir, bad copy) — adopt the replica: restore it into the live state and
/// rewrite the local file. The comparison re-reads the disk at adoption time, so
/// any post-boot user mutation (which bumps the local `saved_ms` past the
/// replica's) automatically vetoes adoption — no clobbering live changes.
/// Opt-out: `HIVE_GUARDIAN_RESTORE=0`.
pub fn spawn_restore_guard(cloud: Arc<crate::state::CloudState>) {
    if std::env::var("HIVE_GUARDIAN_RESTORE").map(|v| v == "0" || v == "false").unwrap_or(false) {
        return;
    }
    tokio::spawn(async move {
        let Some(replica) = fetch_node_snapshot().await else {
            tracing::debug!("guardian restore guard: no replicated snapshot for this node yet");
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
