//! Notifications — derived from real platform signals (failed deploys, error /
//! usage anomalies) with per-notification read & archived state, scoped per team.
//!
//! Notifications are computed live from current state every read; the only thing
//! we persist is which ids the user has read or archived, so the inbox reflects
//! reality without a separate event pipeline.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Clone, Serialize)]
pub struct Notification {
    pub id: String,
    /// "error" | "warning" | "info"
    pub severity: String,
    /// "deploy" | "anomaly" | "usage" | "domain"
    pub category: String,
    pub project: String,
    /// "Production" | "Preview" | "" — shown in the message line.
    pub environment: String,
    pub message: String,
    pub ts_ms: u64,
    pub read: bool,
    pub archived: bool,
}

#[derive(Default)]
pub struct NotificationStore {
    archived: RwLock<HashSet<String>>,
    read: RwLock<HashSet<String>>,
}

impl NotificationStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_archived(&self, id: &str) -> bool {
        self.archived.read().contains(id)
    }
    pub fn is_read(&self, id: &str) -> bool {
        self.read.read().contains(id)
    }
    pub fn archive(&self, id: &str) {
        self.archived.write().insert(id.to_string());
    }
    pub fn archive_all(&self, ids: &[String]) {
        let mut a = self.archived.write();
        for id in ids {
            a.insert(id.clone());
        }
    }
    pub fn mark_read(&self, ids: &[String]) {
        let mut r = self.read.write();
        for id in ids {
            r.insert(id.clone());
        }
    }

    /// Read/archived state for the fleet follower sync. Notifications
    /// themselves are recomputed live per node; only this read/archived
    /// bookkeeping is stateful and leader-authored (mark-read/archive
    /// mutations forward to the leader), so it must replicate or a follower
    /// shows already-read items as unread. Sorted for deterministic wire bytes
    /// (the sync's change-gate is a raw byte compare).
    pub fn snapshot(&self) -> NotificationState {
        let mut archived: Vec<String> = self.archived.read().iter().cloned().collect();
        let mut read: Vec<String> = self.read.read().iter().cloned().collect();
        archived.sort();
        read.sort();
        NotificationState { archived, read }
    }

    pub fn load(&self, s: NotificationState) {
        *self.archived.write() = s.archived.into_iter().collect();
        *self.read.write() = s.read.into_iter().collect();
    }
}

/// Serializable snapshot of `NotificationStore`'s read/archived bookkeeping.
#[derive(Clone, Serialize, serde::Deserialize)]
pub struct NotificationState {
    pub archived: Vec<String>,
    pub read: Vec<String>,
}
