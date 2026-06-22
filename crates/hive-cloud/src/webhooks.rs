//! Outbound webhooks — alert your own code/services on platform events
//! (deployment lifecycle, database provisioning, etc). Each delivery is signed
//! with HMAC-SHA256 over the JSON body so receivers can verify authenticity, the
//! same scheme Vercel/GitHub use (`X-Hive-Signature: sha256=<hex>`).

use hive_core::now_ms;
use hmac::{Hmac, Mac};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::VecDeque;
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

/// Event types a webhook can subscribe to (Vercel-style catalog).
pub const ALL_EVENTS: &[&str] = &[
    // Deployment
    "deployment.created",
    "deployment.building",
    "deployment.ready",
    "deployment.succeeded",
    "deployment.error",
    "deployment.canceled",
    "deployment.promoted",
    "deployment.rollback",
    "deployment.checks.failed",
    "deployment.checks.succeeded",
    // Project
    "project.created",
    "project.removed",
    "project.renamed",
    "project.env-variable.created",
    "project.env-variable.updated",
    "project.env-variable.deleted",
    // Domain
    "domain.added",
    "domain.created",
    "domain.dns.records.changed",
    "project.domain.verified",
    // Data
    "database.created",
    "database.ready",
    // Observability
    "alerts.triggered",
    // Firewall
    "firewall.attack",
    // Incidents
    "incident.opened",
    "incident.resolved",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    /// Owning team/tenant. `#[serde(default)]` keeps pre-existing persisted
    /// webhooks loadable (they normalize to "personal"), so adding tenant
    /// ownership is backward-compatible with on-disk state.
    #[serde(default)]
    pub team: String,
    /// Project this webhook belongs to ("*" = all projects in the team).
    pub project: String,
    pub url: String,
    /// Subscribed event names; empty = all events.
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub secret: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_ms: u64,
}
fn default_true() -> bool {
    true
}

impl Webhook {
    fn subscribed(&self, event: &str) -> bool {
        self.enabled && (self.events.is_empty() || self.events.iter().any(|e| e == event))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Delivery {
    pub id: String,
    pub webhook_id: String,
    pub event: String,
    pub url: String,
    pub status: u16,
    pub ok: bool,
    pub ts_ms: u64,
    pub error: String,
}

pub struct WebhookStore {
    hooks: RwLock<Vec<Webhook>>,
    deliveries: RwLock<VecDeque<Delivery>>,
    http: reqwest::Client,
}

impl WebhookStore {
    pub fn new() -> WebhookStore {
        WebhookStore {
            hooks: RwLock::new(Vec::new()),
            deliveries: RwLock::new(VecDeque::with_capacity(256)),
            http: reqwest::Client::new(),
        }
    }

    pub fn list(&self, project: Option<&str>) -> Vec<Webhook> {
        let h = self.hooks.read();
        match project {
            Some(p) => h.iter().filter(|w| w.project == p || w.project == "*").cloned().collect(),
            None => h.clone(),
        }
    }

    pub fn snapshot(&self) -> Vec<Webhook> {
        self.hooks.read().clone()
    }

    pub fn load(&self, data: Vec<Webhook>) {
        *self.hooks.write() = data;
    }

    pub fn add(&self, mut w: Webhook) -> Webhook {
        if w.id.is_empty() {
            w.id = format!("wh_{}", uuid::Uuid::new_v4().simple());
        }
        if w.secret.is_empty() {
            w.secret = format!("whsec_{}", uuid::Uuid::new_v4().simple());
        }
        w.created_ms = now_ms();
        self.hooks.write().push(w.clone());
        w
    }

    pub fn remove(&self, id: &str) {
        self.hooks.write().retain(|w| w.id != id);
    }

    pub fn deliveries(&self, limit: usize) -> Vec<Delivery> {
        let d = self.deliveries.read();
        d.iter().rev().take(limit).cloned().collect()
    }

    fn record_delivery(&self, d: Delivery) {
        let mut q = self.deliveries.write();
        if q.len() >= 200 {
            q.pop_front();
        }
        q.push_back(d);
    }
}

impl Default for WebhookStore {
    fn default() -> Self {
        Self::new()
    }
}

fn sign(secret: &str, body: &str) -> String {
    match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mut mac) => {
            mac.update(body.as_bytes());
            format!("sha256={}", hex_encode(&mac.finalize().into_bytes()))
        }
        Err(_) => String::new(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Fire an event to all subscribed webhooks for `project`, signed + recorded.
/// Spawns background tasks; returns immediately.
pub fn dispatch(store: &Arc<WebhookStore>, project: &str, event: &str, payload: Value) {
    let targets: Vec<Webhook> = store
        .hooks
        .read()
        .iter()
        .filter(|w| (w.project == project || w.project == "*") && w.subscribed(event))
        .cloned()
        .collect();
    if targets.is_empty() {
        return;
    }
    let body = json!({
        "type": event,
        "createdAt": now_ms(),
        "project": project,
        "payload": payload,
    });
    let body_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    for w in targets {
        let store = store.clone();
        let body_str = body_str.clone();
        let event = event.to_string();
        tokio::spawn(async move {
            let sig = sign(&w.secret, &body_str);
            let res = store
                .http
                .post(&w.url)
                .header("content-type", "application/json")
                .header("x-hive-event", &event)
                .header("x-hive-signature", &sig)
                .header("user-agent", "Hive-Webhooks/1.0")
                .body(body_str)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            let delivery = match res {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    Delivery {
                        id: format!("evt_{}", uuid::Uuid::new_v4().simple()),
                        webhook_id: w.id.clone(),
                        event,
                        url: w.url.clone(),
                        status,
                        ok: (200..300).contains(&status),
                        ts_ms: now_ms(),
                        error: String::new(),
                    }
                }
                Err(e) => Delivery {
                    id: format!("evt_{}", uuid::Uuid::new_v4().simple()),
                    webhook_id: w.id.clone(),
                    event,
                    url: w.url.clone(),
                    status: 0,
                    ok: false,
                    ts_ms: now_ms(),
                    error: e.to_string(),
                },
            };
            store.record_delivery(delivery);
        });
    }
}
