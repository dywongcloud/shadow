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
/// endpoint stays running) plus the opened key/value store.
struct Handle {
    _db: GuardianDB,
    kv: Arc<dyn KeyValueStore<Error = GuardianError>>,
}

static HANDLE: OnceCell<Handle> = OnceCell::const_new();

/// Lazily open (once) the GuardianDB KV store, retrying on a previous failure.
async fn handle() -> anyhow::Result<&'static Handle> {
    HANDLE
        .get_or_try_init(|| async {
            let dir = crate::persist::data_dir().join("guardian");
            std::fs::create_dir_all(&dir).ok();

            // Its own iroh endpoint (random UDP port, n0 discovery) — independent
            // of the request-routing mesh in hive-p2p. Persisted store on disk.
            let cfg = ClientConfig {
                data_store_path: Some(dir.join("iroh")),
                enable_discovery_n0: true,
                port: 0,
                ..ClientConfig::default()
            };
            let client = IrohClient::new(cfg)
                .await
                .map_err(|e| anyhow::anyhow!("guardian iroh client: {e}"))?;
            let node_id = client.node_id();

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
            let kv = db
                .key_value("hive-state", None)
                .await
                .map_err(|e| anyhow::anyhow!("guardian kv open: {e}"))?;

            tracing::info!(%node_id, dir = ?dir, "GuardianDB ready (iroh-docs KV 'hive-state', replicated)");
            Ok::<Handle, anyhow::Error>(Handle { _db: db, kv })
        })
        .await
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
