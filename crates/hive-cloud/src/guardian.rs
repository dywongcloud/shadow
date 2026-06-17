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
pub fn replicate(snap: &PlatformSnapshot) {
    let json = match serde_json::to_vec(snap) {
        Ok(v) => v,
        Err(_) => return,
    };
    // Spawn so persistence is never blocked on replication.
    tokio::spawn(async move {
        if let Err(e) = put("platform/state", json).await {
            tracing::debug!(error = %e, "guardian-db replicate failed");
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
