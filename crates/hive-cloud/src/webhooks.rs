//! Outbound webhooks: alert your own code or services on platform events (deployment lifecycle,
//! database provisioning, ...). Each delivery is signed with HMAC-SHA256 over the JSON body so
//! receivers can verify authenticity, the scheme Vercel and GitHub use (`X-Hive-Signature:
//! sha256=<hex>`).

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
    "project.created",
    "project.removed",
    "project.renamed",
    "project.env-variable.created",
    "project.env-variable.updated",
    "project.env-variable.deleted",
    "domain.added",
    "domain.created",
    "domain.dns.records.changed",
    "project.domain.verified",
    "database.created",
    "database.ready",
    "alerts.triggered",
    "firewall.attack",
    "incident.opened",
    "incident.resolved",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    /// Owning team/tenant. `#[serde(default)]` keeps pre-existing persisted webhooks loadable (they
    /// normalize to "personal"), so adding tenant ownership is backward-compatible with on-disk
    /// state.
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
    /// For API responses TO THE OWNING TENANT ONLY: decrypts `secret` back to its real value (they
    /// need it to verify signatures on their receiving endpoint); the stored form is AEAD-sealed.
    /// `decrypt` is a passthrough for an already-plaintext legacy record.
    pub fn decrypted(mut self) -> Self {
        self.secret = crate::secrets::decrypt(&self.secret);
        self
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
            Some(p) => h
                .iter()
                .filter(|w| w.project == p || w.project == "*")
                .cloned()
                .collect(),
            None => h.clone(),
        }
    }

    /// Moves this project's subscriptions to its new name. A webhook row whose `project` no longer
    /// matches any project is silently undeliverable (every future event for the renamed project
    /// would match nothing, and the settings page would still show a webhook that can never fire).
    /// `*` rows (team-wide) are untouched.
    pub fn rename_project(&self, from: &str, to: &str) -> usize {
        if from == to {
            return 0;
        }
        let mut hooks = self.hooks.write();
        let mut n = 0;
        for w in hooks.iter_mut().filter(|w| w.project == from) {
            w.project = to.to_string();
            n += 1;
        }
        n
    }

    pub fn snapshot(&self) -> Vec<Webhook> {
        self.hooks.read().clone()
    }

    pub fn load(&self, data: Vec<Webhook>) {
        *self.hooks.write() = data;
    }

    /// Registers a webhook, minting its id and a `whsec_` signing secret when absent, and
    /// AEAD-seals the secret before it is stored or persisted (`secrets::encrypt`, idempotent:
    /// re-adding an already sealed secret is safe). A webhook secret leaked from a snapshot, backup
    /// or GuardianDB replica lets an attacker forge signed deliveries, which is why it sits under
    /// the primitive that protects sensitive project env vars.
    pub fn add(&self, mut w: Webhook) -> Webhook {
        if w.id.is_empty() {
            w.id = format!("wh_{}", uuid::Uuid::new_v4().simple());
        }
        if w.secret.is_empty() {
            w.secret = format!("whsec_{}", uuid::Uuid::new_v4().simple());
        }
        w.secret = crate::secrets::encrypt(&w.secret);
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

/// `sha256=<hex>` HMAC of `body`, the `x-hive-signature` every signed delivery carries (webhooks and
/// queue consumer batches).
pub(crate) fn sign(secret: &str, body: &str) -> String {
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
/// Spawns one background delivery round and returns immediately.
pub fn dispatch(store: &Arc<WebhookStore>, project: &str, event: &str, payload: Value) {
    let store = store.clone();
    let project = project.to_string();
    let event = event.to_string();
    let event_id = format!("evt_{}", uuid::Uuid::new_v4().simple());
    let created_ms = now_ms();
    tokio::spawn(async move {
        let _ = dispatch_durable(&store, &event_id, created_ms, &project, &event, payload).await;
    });
}

/// Delivers one durable lifecycle-outbox entry. The stable event id and creation time survive
/// retries, and success means every currently subscribed endpoint acknowledged with 2xx (or there
/// were no subscribers). Each delivery is signed with the webhook's secret, decrypted per delivery
/// (a passthrough for a legacy plaintext secret).
pub async fn dispatch_durable(
    store: &Arc<WebhookStore>,
    event_id: &str,
    created_ms: u64,
    project: &str,
    event: &str,
    payload: Value,
) -> bool {
    let targets: Vec<Webhook> = store
        .hooks
        .read()
        .iter()
        .filter(|webhook| {
            (webhook.project == project || webhook.project == "*") && webhook.subscribed(event)
        })
        .cloned()
        .collect();
    if targets.is_empty() {
        return true;
    }
    let body = json!({
        "id": event_id,
        "type": event,
        "createdAt": created_ms,
        "project": project,
        "payload": payload,
    });
    let body_str = match serde_json::to_string(&body) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, %event_id, "durable webhook body serialization failed");
            return false;
        }
    };
    let deliveries = targets.into_iter().map(|webhook| {
        let store = store.clone();
        let body_str = body_str.clone();
        let event = event.to_string();
        let event_id = event_id.to_string();
        async move {
            let signature = sign(&crate::secrets::decrypt(&webhook.secret), &body_str);
            let response = store
                .http
                .post(&webhook.url)
                .header("content-type", "application/json")
                .header("x-hive-event", &event)
                .header("x-hive-delivery", &event_id)
                .header("x-hive-signature", &signature)
                .header("user-agent", "Hive-Webhooks/1.0")
                .body(body_str)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            let delivery = match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    Delivery {
                        id: event_id,
                        webhook_id: webhook.id.clone(),
                        event,
                        url: webhook.url.clone(),
                        status,
                        ok: (200..300).contains(&status),
                        ts_ms: now_ms(),
                        error: String::new(),
                    }
                }
                Err(error) => Delivery {
                    id: event_id,
                    webhook_id: webhook.id.clone(),
                    event,
                    url: webhook.url.clone(),
                    status: 0,
                    ok: false,
                    ts_ms: now_ms(),
                    error: error.to_string(),
                },
            };
            let ok = delivery.ok;
            store.record_delivery(delivery);
            ok
        }
    });
    futures::future::join_all(deliveries)
        .await
        .into_iter()
        .all(|delivered| delivered)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REGRESSION TEST for a real, confirmed gap: webhook HMAC signing secrets
    /// used to be persisted in PLAINTEXT in the same snapshot files this
    /// platform's own secrets.rs primitive protects everything else in — a
    /// leaked snapshot/backup/GuardianDB replica let an attacker forge signed
    /// webhook deliveries to a customer's endpoint. Confirms: (1) the STORED
    /// record is sealed (never the raw secret), (2) the owner-facing
    /// `.decrypted()` view still returns the real value, (3) a pre-existing
    /// plaintext legacy secret (from before this fix) still round-trips
    /// correctly through the same decrypt call (backward compatible).
    #[test]
    fn webhook_secret_is_encrypted_at_rest_but_recoverable_for_owner_and_signing() {
        let store = WebhookStore::new();
        let created = store.add(Webhook {
            id: String::new(),
            team: "acme".into(),
            project: "*".into(),
            url: "https://example.com/hook".into(),
            events: vec![],
            secret: "whsec_realvalue123".into(),
            enabled: true,
            created_ms: 0,
        });

        // The record actually held in the store (== what gets persisted) must
        // NOT be the plaintext secret.
        let stored = store
            .snapshot()
            .into_iter()
            .find(|w| w.id == created.id)
            .unwrap();
        assert_ne!(
            stored.secret, "whsec_realvalue123",
            "stored secret must be sealed, not plaintext"
        );
        assert!(
            stored.secret.starts_with("enc:v1:"),
            "stored secret must use the AEAD envelope"
        );

        // The owner-facing decrypted view recovers the real value.
        assert_eq!(stored.clone().decrypted().secret, "whsec_realvalue123");

        // A legacy, already-plaintext record (persisted before this fix
        // shipped) must still decrypt correctly (passthrough, not garbled).
        let legacy = Webhook {
            secret: "whsec_legacy_plaintext".into(),
            ..stored.clone()
        };
        assert_eq!(legacy.decrypted().secret, "whsec_legacy_plaintext");
    }
}
