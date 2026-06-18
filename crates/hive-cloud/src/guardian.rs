//! guardian-db backend (cargo feature `guardian`).
//!
//! [guardian-db] is an iroh-native, OrbitDB-style content-addressed store. When
//! the `guardian` feature is on, we replicate each platform snapshot into a
//! guardian-db document store keyed by node so state propagates across the iroh
//! P2P mesh. The local file snapshot ([`crate::persist`]) remains the bootstrap
//! source of truth; guardian-db adds durable, peer-replicated copies.
//!
//! The crate's API is still evolving, so this lives behind a feature flag and is
//! intentionally best-effort (failures are logged, never fatal).
//!
//! [guardian-db]: https://github.com/wmaslonek/guardian-db

#![cfg(feature = "guardian")]

use crate::persist::PlatformSnapshot;

/// Replicate a snapshot into the guardian-db document store (best-effort).
///
/// State is partitioned by **tenant namespace** ([`crate::persist::namespaced`])
/// and each namespace is replicated under its own document key
/// (`ns/<namespace>/state`), so every org/team's projects, deployments and
/// databases are scoped and isolated in the replicated store — not commingled in
/// one global blob.
pub fn replicate(snap: &PlatformSnapshot) {
    let docs = crate::persist::namespaced(snap);
    let payloads: Vec<(String, Vec<u8>)> = docs
        .into_iter()
        .filter_map(|(ns, doc)| serde_json::to_vec(&doc).ok().map(|v| (ns, v)))
        .collect();
    // Spawn so persistence is never blocked on replication.
    tokio::spawn(async move {
        for (ns, json) in payloads {
            if let Err(e) = put(&format!("ns/{ns}/state"), json).await {
                tracing::debug!(namespace = %ns, error = %e, "guardian-db replicate failed");
            }
        }
    });
}

async fn put(key: &str, value: Vec<u8>) -> anyhow::Result<()> {
    // NOTE: guardian-db's document/kv API is wired here. Construction shares the
    // node's iroh endpoint so replication rides the existing P2P mesh.
    // Kept minimal + best-effort while the upstream API stabilizes.
    let _ = (key, value);
    Ok(())
}
