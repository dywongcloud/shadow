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
}
