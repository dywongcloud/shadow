//! Incidents — the operational record the platform owner manages from the ops
//! dashboard (status-page style: severity, lifecycle, timeline of updates).

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Minor,
    Major,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncidentStatus {
    Investigating,
    Identified,
    Monitoring,
    Resolved,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IncidentUpdate {
    pub ts_ms: u64,
    pub status: IncidentStatus,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub status: IncidentStatus,
    #[serde(default)]
    pub affected: Vec<String>, // regions / components
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub updates: Vec<IncidentUpdate>,
}

#[derive(Deserialize)]
pub struct OpenReq {
    pub title: String,
    pub severity: Severity,
    #[serde(default)]
    pub affected: Vec<String>,
    #[serde(default)]
    pub message: String,
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub status: IncidentStatus,
    pub message: String,
}

pub struct IncidentStore {
    items: RwLock<Vec<Incident>>,
}

impl IncidentStore {
    pub fn new() -> IncidentStore {
        IncidentStore { items: RwLock::new(Vec::new()) }
    }

    pub fn list(&self) -> Vec<Incident> {
        let mut v = self.items.read().clone();
        v.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
        v
    }

    pub fn open_count(&self) -> usize {
        self.items.read().iter().filter(|i| i.status != IncidentStatus::Resolved).count()
    }

    pub fn snapshot(&self) -> Vec<Incident> {
        self.items.read().clone()
    }

    pub fn load(&self, data: Vec<Incident>) {
        *self.items.write() = data;
    }

    pub fn open(&self, req: OpenReq) -> Incident {
        let now = now_ms();
        let inc = Incident {
            id: format!("inc_{}", uuid::Uuid::new_v4().simple()),
            title: req.title,
            severity: req.severity,
            status: IncidentStatus::Investigating,
            affected: req.affected,
            created_ms: now,
            updated_ms: now,
            updates: vec![IncidentUpdate {
                ts_ms: now,
                status: IncidentStatus::Investigating,
                message: if req.message.is_empty() { "Incident opened.".into() } else { req.message },
            }],
        };
        self.items.write().push(inc.clone());
        inc
    }

    pub fn update(&self, id: &str, req: UpdateReq) -> Option<Incident> {
        let now = now_ms();
        let mut items = self.items.write();
        let inc = items.iter_mut().find(|i| i.id == id)?;
        inc.status = req.status;
        inc.updated_ms = now;
        inc.updates.push(IncidentUpdate { ts_ms: now, status: req.status, message: req.message });
        Some(inc.clone())
    }
}

impl Default for IncidentStore {
    fn default() -> Self {
        Self::new()
    }
}
