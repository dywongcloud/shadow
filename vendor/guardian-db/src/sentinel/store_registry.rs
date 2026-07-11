//! Persistent registry of stores created through the admin surface (G1).
//!
//! `GuardianDB` keeps its open stores in an **in-memory** map that starts empty on
//! every boot — nothing on disk says "these stores existed, reopen them". So a
//! store created via the TUI would vanish on restart. This registry is the missing
//! **catalog of what to reopen**: a small redb table mapping a store's local name
//! to its [`StoreSpec`] (kind + creation options). It is *not* the store's data
//! (that lives in redb/iroh); it only records how to reopen it.
//!
//! On boot the owner process ([`crate::sentinel::AdminContext::reopen_stores`]) reads
//! this registry and reopens each store with its options, repopulating
//! `db.list_stores()` so the panel shows them again.

use crate::guardian::error::{GuardianError, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const REGISTRY_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("store_registry");

/// The reopen spec for one store (G1). Serialized as JSON under the store's name.
/// `#[serde(default)]` on every option keeps older/newer registry files readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreSpec {
    /// `"eventlog"` | `"keyvalue"` | `"document"`.
    pub kind: String,
    /// Replicate over the network (vs. local-only storage).
    #[serde(default)]
    pub replicate: bool,
    /// Keep the store strictly local (no replication/announcement).
    #[serde(default)]
    pub local_only: bool,
    /// Open as a read-only replica (refuses local writes; iroh-docs stores).
    #[serde(default)]
    pub read_only: bool,
    /// Address of an access controller to attach, if any.
    #[serde(default)]
    pub acl_address: Option<String>,
    /// A `DocTicket` to import a peer's shared namespace (G3), if any.
    #[serde(default)]
    pub doc_ticket: Option<String>,
}

/// A redb-backed catalog of admin-created stores, kept in `<data-dir>/store_registry`.
#[derive(Debug)]
pub struct StoreRegistry {
    db: Database,
}

// Safe for the same reason as `RedbKeystore`: redb's `Database` is thread-safe.
unsafe impl Send for StoreRegistry {}
unsafe impl Sync for StoreRegistry {}

impl StoreRegistry {
    /// Open (creating if needed) the registry at `path`.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| GuardianError::Other(format!("store registry dir: {e}")))?;
        }
        let db = Database::create(&path)
            .map_err(|e| GuardianError::Other(format!("store registry open: {e}")))?;
        // Ensure the table exists so first-run reads don't error.
        {
            let w = db
                .begin_write()
                .map_err(|e| GuardianError::Other(format!("store registry txn: {e}")))?;
            {
                let _ = w
                    .open_table(REGISTRY_TABLE)
                    .map_err(|e| GuardianError::Other(format!("store registry table: {e}")))?;
            }
            w.commit()
                .map_err(|e| GuardianError::Other(format!("store registry commit: {e}")))?;
        }
        Ok(Self { db })
    }

    /// Record (or overwrite) the spec for a store name.
    pub fn put(&self, name: &str, spec: &StoreSpec) -> Result<()> {
        let bytes = serde_json::to_vec(spec)
            .map_err(|e| GuardianError::Other(format!("store spec encode: {e}")))?;
        let w = self
            .db
            .begin_write()
            .map_err(|e| GuardianError::Other(format!("store registry txn: {e}")))?;
        {
            let mut t = w
                .open_table(REGISTRY_TABLE)
                .map_err(|e| GuardianError::Other(format!("store registry table: {e}")))?;
            t.insert(name, &bytes[..])
                .map_err(|e| GuardianError::Other(format!("store registry insert: {e}")))?;
        }
        w.commit()
            .map_err(|e| GuardianError::Other(format!("store registry commit: {e}")))?;
        Ok(())
    }

    /// Remove a store's spec (used when the store is dropped, G2).
    pub fn remove(&self, name: &str) -> Result<()> {
        let w = self
            .db
            .begin_write()
            .map_err(|e| GuardianError::Other(format!("store registry txn: {e}")))?;
        {
            let mut t = w
                .open_table(REGISTRY_TABLE)
                .map_err(|e| GuardianError::Other(format!("store registry table: {e}")))?;
            t.remove(name)
                .map_err(|e| GuardianError::Other(format!("store registry remove: {e}")))?;
        }
        w.commit()
            .map_err(|e| GuardianError::Other(format!("store registry commit: {e}")))?;
        Ok(())
    }

    /// True if a spec exists for `name`.
    pub fn contains(&self, name: &str) -> Result<bool> {
        Ok(self.get(name)?.is_some())
    }

    /// Fetch one store's spec, if recorded.
    pub fn get(&self, name: &str) -> Result<Option<StoreSpec>> {
        let r = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("store registry read: {e}")))?;
        let t = match r.open_table(REGISTRY_TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        match t.get(name) {
            Ok(Some(v)) => Ok(serde_json::from_slice(v.value()).ok()),
            Ok(None) => Ok(None),
            Err(e) => Err(GuardianError::Other(format!("store registry get: {e}"))),
        }
    }

    /// All recorded stores, as `(name, spec)` pairs.
    pub fn list(&self) -> Result<Vec<(String, StoreSpec)>> {
        let r = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("store registry read: {e}")))?;
        let t = match r.open_table(REGISTRY_TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for item in t
            .iter()
            .map_err(|e| GuardianError::Other(format!("store registry iter: {e}")))?
        {
            let (k, v) =
                item.map_err(|e| GuardianError::Other(format!("store registry entry: {e}")))?;
            if let Ok(spec) = serde_json::from_slice::<StoreSpec>(v.value()) {
                out.push((k.value().to_string(), spec));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrips_and_persists() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("store_registry");
        let spec = StoreSpec {
            kind: "keyvalue".into(),
            replicate: true,
            local_only: false,
            read_only: false,
            acl_address: None,
            doc_ticket: None,
        };
        {
            let reg = StoreRegistry::open(path.clone()).unwrap();
            reg.put("settings", &spec).unwrap();
            assert!(reg.contains("settings").unwrap());
            assert_eq!(reg.list().unwrap().len(), 1);
        }
        // Reopen: the spec must still be there (survives "relaunch").
        let reg2 = StoreRegistry::open(path).unwrap();
        assert_eq!(reg2.get("settings").unwrap().as_ref(), Some(&spec));
        reg2.remove("settings").unwrap();
        assert!(reg2.list().unwrap().is_empty());
    }
}
