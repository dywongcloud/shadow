//! Node wiring for SHADW signed logs (`hive-cores`, feature `cores`): opens the
//! core store under `$HIVE_DATA/cores` and serves `STREAM_CORE` from it.
//!
//! Serving only. Which cores a node follows and from whom it pulls (the
//! replication policy) is the next step of the chain plan's L3; until it lands,
//! cores are created and followed by operator tooling, never by a request path.
//! The store is NODE-LOCAL state: no admin endpoint reads it yet, so the
//! round-robin read rule in AGENTS.md does not apply until one does.

use std::sync::Arc;

use hive_cores::CoreStore;

/// The `STREAM_CORE` handler for this node, or `None` (logged) when the store
/// cannot be opened — a broken store never stops the node from booting.
pub async fn handler() -> Option<hive_p2p::CoreHandler> {
    let root = crate::persist::data_dir().join("cores");
    match CoreStore::open(&root).await {
        Ok(store) => Some(Arc::clone(&store).handler()),
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "hive-cores: store did not open; 0x06 streams will be reset");
            None
        }
    }
}
