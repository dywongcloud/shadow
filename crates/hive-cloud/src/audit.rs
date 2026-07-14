//! Append-only **audit log** — the durable, historical record of every state
//! mutation, for compliance + ACID durability.
//!
//! ACID model for the platform store:
//! * **Atomicity** — each mutation is followed by an atomic snapshot write
//!   (temp file + `rename`, see [`crate::persist::save`]).
//! * **Consistency** — tenant scoping + cardinality are enforced at the API
//!   layer; the audit log records the resulting transition.
//! * **Isolation** — every store guards its data with a `RwLock`, so concurrent
//!   readers/writers never observe a partial update.
//! * **Durability** — the snapshot is `fsync`-ed and every audit entry is
//!   appended (and flushed) to `audit.jsonl`, an immutable history that is never
//!   rewritten in place.

use hive_core::now_ms;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonic sequence number (transaction ordering).
    pub seq: u64,
    pub ts_ms: u64,
    pub tenant: String,
    pub actor: String,
    /// e.g. "create" | "update" | "delete" | "charge".
    pub action: String,
    /// e.g. "deployment" | "database" | "env" | "team" | "billing".
    pub entity: String,
    pub entity_id: String,
    #[serde(default)]
    pub detail: String,
}

pub struct AuditLog {
    entries: Mutex<VecDeque<AuditEntry>>,
    seq: AtomicU64,
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        // Bootstrap the in-memory ring + sequence from the durable JSONL history.
        let mut entries = VecDeque::with_capacity(1024);
        let mut max_seq = 0u64;
        if let Ok(s) = std::fs::read_to_string(&path) {
            for line in s.lines().rev().take(1000) {
                if let Ok(e) = serde_json::from_str::<AuditEntry>(line) {
                    max_seq = max_seq.max(e.seq);
                    entries.push_front(e);
                }
            }
        }
        AuditLog {
            entries: Mutex::new(entries),
            seq: AtomicU64::new(max_seq + 1),
            path,
        }
    }

    /// Record a mutation: append to the immutable history (durable) + ring.
    pub fn record(&self, tenant: &str, actor: &str, action: &str, entity: &str, entity_id: &str, detail: &str) {
        let entry = AuditEntry {
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            ts_ms: now_ms(),
            tenant: if tenant.is_empty() { "personal".into() } else { tenant.into() },
            actor: actor.to_string(),
            action: action.to_string(),
            entity: entity.to_string(),
            entity_id: entity_id.to_string(),
            detail: detail.to_string(),
        };
        // Append + flush to disk (durability).
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            if let Ok(line) = serde_json::to_string(&entry) {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
                let _ = f.sync_all();
            }
        }
        // Stream to any configured enterprise SIEM sink (best-effort, async;
        // no-op unless the tenant has SIEM enabled). See [`crate::enterprise`].
        crate::enterprise::siem_emit(&entry);
        let mut q = self.entries.lock();
        if q.len() >= 2000 {
            q.pop_front();
        }
        q.push_back(entry);
    }

    /// Recent entries (newest first), optionally filtered to one tenant.
    pub fn recent(&self, limit: usize, tenant: Option<&str>) -> Vec<AuditEntry> {
        let q = self.entries.lock();
        q.iter()
            .rev()
            .filter(|e| tenant.map(|t| e.tenant == t).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Snapshot (oldest→newest) for the fleet follower sync. Audit records are
    /// written on the control-plane leader during every mutation (mutations
    /// forward there via `admin_ingress`), so the leader's buffer is the
    /// authoritative fleet-wide audit; a follower otherwise shows only its own
    /// local events. Bounded by the buffer's own capacity (recent history is
    /// all the operator audit view renders).
    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.entries.lock().iter().cloned().collect()
    }

    pub fn load(&self, data: Vec<AuditEntry>) {
        let mut q = self.entries.lock();
        q.clear();
        q.extend(data);
    }
}
