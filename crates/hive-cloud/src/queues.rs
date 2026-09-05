//! Cloudflare Queues, 1:1: a durable, tenant-scoped message queue service
//! matching Cloudflare's REST contract (`/accounts/{account_id}/queues`),
//! consumer model (Worker push + HTTP pull), and metrics surface.
//!
//! Two storage tiers, matching AGENTS.md's round-robin-reads-vs-leader-
//! forwarded-writes split:
//! * **Metadata** (queue + consumer records) is small and store_sync-
//!   replicated — every node holds a fresh copy, `merge_synced` follows the
//!   exact tombstone-preserving pattern `databases.rs`'s `SyncedDatabases`
//!   already proved (never adopt-by-replace: a leader OOM-killed before its
//!   debounced save ran must not erase every follower's copy of a record it
//!   never got to save).
//! * **Messages** are node-local to the queue's elected owner (deterministic
//!   per-queue election, the same `Cluster::elect_among` shape
//!   `world_queue.rs`'s `is_primary_for_team` already validated) and mirrored
//!   to GuardianDB for durability across a restart — never full-mesh
//!   replicated, since message volume is unbounded and only the owner needs
//!   the working set.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

pub const TOMBSTONE_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

// ---- Cloudflare bounds (verified against developers.cloudflare.com/queues) ----
pub const DELIVERY_DELAY_MIN_SECS: u32 = 0;
pub const DELIVERY_DELAY_MAX_SECS: u32 = 86_400;
pub const RETENTION_MIN_SECS: u32 = 60;
pub const RETENTION_MAX_SECS: u32 = 1_209_600;
pub const RETENTION_DEFAULT_SECS: u32 = 345_600; // 4 days
pub const BATCH_SIZE_MIN: u32 = 1;
pub const BATCH_SIZE_MAX: u32 = 100;
pub const BATCH_SIZE_DEFAULT: u32 = 10;
pub const BATCH_TIMEOUT_MIN_SECS: u32 = 0;
pub const BATCH_TIMEOUT_MAX_SECS: u32 = 60;
pub const BATCH_TIMEOUT_DEFAULT_SECS: u32 = 5;
pub const MAX_RETRIES_DEFAULT: u32 = 3;
pub const PULL_BATCH_SIZE_DEFAULT: u32 = 5;
pub const PULL_BATCH_SIZE_MAX: u32 = 100;
pub const VISIBILITY_TIMEOUT_DEFAULT_MS: u64 = 30_000;
pub const VISIBILITY_TIMEOUT_MAX_MS: u64 = 12 * 60 * 60 * 1000;
/// Cloudflare's own per-message body cap (128 KB) — refused loudly, never
/// silently truncated (the browser_db / hcb1 precedent: truncation in a
/// durable store is silent divergence, not a size limit).
pub const MAX_MESSAGE_BODY_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerType {
    Worker,
    HttpPull,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueSettings {
    #[serde(default)]
    pub delivery_delay: u32,
    #[serde(default)]
    pub delivery_paused: bool,
    #[serde(default = "default_retention")]
    pub message_retention_period: u32,
}
fn default_retention() -> u32 {
    RETENTION_DEFAULT_SECS
}
impl Default for QueueSettings {
    fn default() -> Self {
        QueueSettings {
            delivery_delay: DELIVERY_DELAY_MIN_SECS,
            delivery_paused: false,
            message_retention_period: RETENTION_DEFAULT_SECS,
        }
    }
}
impl QueueSettings {
    /// Clamp to Cloudflare's documented bounds — never reject a slightly
    /// out-of-range request, clamp it and let the caller see the effective
    /// value in the response (matches this platform's `BrowserDbPolicy`
    /// resolve()-with-notes precedent: defaulted fields, clamped ceilings,
    /// never a silent drop).
    pub fn clamped(mut self) -> Self {
        self.delivery_delay = self
            .delivery_delay
            .clamp(DELIVERY_DELAY_MIN_SECS, DELIVERY_DELAY_MAX_SECS);
        self.message_retention_period = self
            .message_retention_period
            .clamp(RETENTION_MIN_SECS, RETENTION_MAX_SECS);
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Queue {
    pub queue_id: String,
    pub queue_name: String,
    /// Server-resolved tenant (the platform's account_id equivalent) — never
    /// trusted from the request body, matching `database_create`'s
    /// `req.team = tenant(...)` override.
    pub tenant: String,
    pub created_on: u64,
    pub modified_on: u64,
    #[serde(default)]
    pub settings: QueueSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsumerSettings {
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    #[serde(default = "default_batch_timeout")]
    pub max_batch_timeout: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    #[serde(default)]
    pub retry_delay: u32,
    #[serde(default = "default_visibility_timeout_ms")]
    pub visibility_timeout_ms: u64,
}
fn default_batch_size() -> u32 {
    BATCH_SIZE_DEFAULT
}
fn default_batch_timeout() -> u32 {
    BATCH_TIMEOUT_DEFAULT_SECS
}
fn default_max_retries() -> u32 {
    MAX_RETRIES_DEFAULT
}
fn default_visibility_timeout_ms() -> u64 {
    VISIBILITY_TIMEOUT_DEFAULT_MS
}
impl Default for ConsumerSettings {
    fn default() -> Self {
        ConsumerSettings {
            batch_size: BATCH_SIZE_DEFAULT,
            max_batch_timeout: BATCH_TIMEOUT_DEFAULT_SECS,
            max_retries: MAX_RETRIES_DEFAULT,
            max_concurrency: None,
            retry_delay: 0,
            visibility_timeout_ms: VISIBILITY_TIMEOUT_DEFAULT_MS,
        }
    }
}
impl ConsumerSettings {
    pub fn clamped(mut self) -> Self {
        self.batch_size = self.batch_size.clamp(BATCH_SIZE_MIN, BATCH_SIZE_MAX);
        self.max_batch_timeout = self
            .max_batch_timeout
            .clamp(BATCH_TIMEOUT_MIN_SECS, BATCH_TIMEOUT_MAX_SECS);
        self.visibility_timeout_ms = self
            .visibility_timeout_ms
            .clamp(1000, VISIBILITY_TIMEOUT_MAX_MS);
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Consumer {
    pub consumer_id: String,
    pub queue_id: String,
    #[serde(rename = "type")]
    pub kind: ConsumerType,
    #[serde(default)]
    pub settings: ConsumerSettings,
    /// Worker-type only: which project+function this consumer dispatches to.
    /// Cloudflare's `script_name`.
    #[serde(default)]
    pub script_name: String,
    /// Another queue's `queue_id`, validated same-tenant at create time.
    #[serde(default)]
    pub dead_letter_queue: Option<String>,
    pub created_on: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueMessage {
    pub id: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub attempts: u32,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// `None` = available for lease/delivery. `Some((lease_id, expires_ms))` =
    /// currently leased — an expired lease is treated as available again by
    /// every reader, never eagerly swept (matches `PooledConn`'s Drop-release
    /// discipline: state is checked at use time, not by a separate janitor
    /// racing real acquisition).
    #[serde(default)]
    pub lease: Option<(String, u64)>,
}

/// A queue's live message working set. Node-local (the owner's), mirrored to
/// GuardianDB — never store_sync'd (unbounded volume, only the owner needs
/// the hot set, exactly `world_queue.rs`'s `WorldQueue.pending` precedent).
#[derive(Default)]
struct MessageLog {
    messages: RwLock<Vec<QueueMessage>>,
    /// Set once this log has been recovered from GuardianDB at least once —
    /// without this, a freshly-restarted node's message store, metrics, and
    /// pull-consumer reads all raced the periodic (~5s cadence) recovery
    /// tick: a queue with real durable messages read as an honestly-empty
    /// backlog for up to 5 seconds after every restart. Every read path now
    /// forces a synchronous recovery the FIRST time it touches a queue this
    /// process has not yet loaded, then relies on the periodic tick for
    /// ongoing convergence.
    synced_once: std::sync::atomic::AtomicBool,
}

pub struct QueueStore {
    queues: RwLock<Vec<Queue>>,
    consumers: RwLock<Vec<Consumer>>,
    queue_tombstones: RwLock<std::collections::BTreeMap<String, u64>>,
    consumer_tombstones: RwLock<std::collections::BTreeMap<String, u64>>,
    logs: RwLock<HashMap<String, Arc<MessageLog>>>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct SyncedQueues {
    pub queues: Vec<Queue>,
    pub consumers: Vec<Consumer>,
    #[serde(default)]
    pub queue_tombstones: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub consumer_tombstones: std::collections::BTreeMap<String, u64>,
}

fn guardian_key(queue_id: &str, message_id: &str) -> String {
    format!("queues/msg/{queue_id}/{message_id}")
}

impl QueueStore {
    pub fn new() -> Arc<QueueStore> {
        Arc::new(QueueStore {
            queues: RwLock::new(Vec::new()),
            consumers: RwLock::new(Vec::new()),
            queue_tombstones: RwLock::new(std::collections::BTreeMap::new()),
            consumer_tombstones: RwLock::new(std::collections::BTreeMap::new()),
            logs: RwLock::new(HashMap::new()),
        })
    }

    fn log_for(&self, queue_id: &str) -> Arc<MessageLog> {
        let mut logs = self.logs.write();
        logs.entry(queue_id.to_string())
            .or_insert_with(|| Arc::new(MessageLog::default()))
            .clone()
    }

    // ---- Queue CRUD ----

    pub fn list(&self, tenant: &str) -> Vec<Queue> {
        self.queues
            .read()
            .iter()
            .filter(|q| q.tenant == tenant)
            .cloned()
            .collect()
    }

    pub fn get(&self, tenant: &str, queue_id: &str) -> Option<Queue> {
        self.queues
            .read()
            .iter()
            .find(|q| q.tenant == tenant && q.queue_id == queue_id)
            .cloned()
    }

    pub fn find_by_name(&self, tenant: &str, queue_name: &str) -> Option<Queue> {
        self.queues
            .read()
            .iter()
            .find(|q| q.tenant == tenant && q.queue_name == queue_name)
            .cloned()
    }

    pub fn create(&self, tenant: &str, queue_name: &str) -> Queue {
        let now = now_ms();
        let queue = Queue {
            queue_id: format!("q_{}", uuid::Uuid::new_v4().simple()),
            queue_name: queue_name.to_string(),
            tenant: tenant.to_string(),
            created_on: now,
            modified_on: now,
            settings: QueueSettings::default(),
        };
        self.queues.write().push(queue.clone());
        queue
    }

    /// Returns `false` if `queue_id` does not belong to `tenant` (never
    /// distinguishable from "does not exist" — no existence leak).
    pub fn update_settings(
        &self,
        tenant: &str,
        queue_id: &str,
        settings: QueueSettings,
    ) -> Option<Queue> {
        let mut qs = self.queues.write();
        let q = qs
            .iter_mut()
            .find(|q| q.tenant == tenant && q.queue_id == queue_id)?;
        q.settings = settings.clamped();
        q.modified_on = now_ms();
        Some(q.clone())
    }

    /// Cascades to every consumer of this queue (Cloudflare's own delete
    /// behavior) and tombstones both the queue and its consumers so the
    /// deletion replicates instead of being silently re-adopted from a peer
    /// that has not heard about it yet.
    pub fn delete(&self, tenant: &str, queue_id: &str) -> bool {
        let existed = {
            let mut qs = self.queues.write();
            let before = qs.len();
            qs.retain(|q| !(q.tenant == tenant && q.queue_id == queue_id));
            qs.len() != before
        };
        if !existed {
            return false;
        }
        let now = now_ms();
        self.queue_tombstones
            .write()
            .insert(queue_id.to_string(), now);
        let removed_consumers: Vec<String> = {
            let mut cs = self.consumers.write();
            let (removed, kept): (Vec<_>, Vec<_>) =
                cs.drain(..).partition(|c| c.queue_id == queue_id);
            *cs = kept;
            removed.into_iter().map(|c| c.consumer_id).collect()
        };
        {
            let mut ct = self.consumer_tombstones.write();
            for id in removed_consumers {
                ct.insert(id, now);
            }
        }
        self.logs.write().remove(queue_id);
        true
    }

    // ---- Consumer CRUD ----

    pub fn list_consumers(&self, queue_id: &str) -> Vec<Consumer> {
        self.consumers
            .read()
            .iter()
            .filter(|c| c.queue_id == queue_id)
            .cloned()
            .collect()
    }

    pub fn consumers_total_count(&self, queue_id: &str) -> usize {
        self.consumers
            .read()
            .iter()
            .filter(|c| c.queue_id == queue_id)
            .count()
    }

    pub fn create_consumer(
        &self,
        queue_id: &str,
        kind: ConsumerType,
        settings: ConsumerSettings,
        script_name: String,
        dead_letter_queue: Option<String>,
    ) -> Consumer {
        let consumer = Consumer {
            consumer_id: format!("qc_{}", uuid::Uuid::new_v4().simple()),
            queue_id: queue_id.to_string(),
            kind,
            settings: settings.clamped(),
            script_name,
            dead_letter_queue,
            created_on: now_ms(),
        };
        self.consumers.write().push(consumer.clone());
        consumer
    }

    pub fn delete_consumer(&self, queue_id: &str, consumer_id: &str) -> bool {
        let mut cs = self.consumers.write();
        let before = cs.len();
        cs.retain(|c| !(c.queue_id == queue_id && c.consumer_id == consumer_id));
        let removed = cs.len() != before;
        drop(cs);
        if removed {
            self.consumer_tombstones
                .write()
                .insert(consumer_id.to_string(), now_ms());
        }
        removed
    }

    pub fn get_consumer(&self, queue_id: &str, consumer_id: &str) -> Option<Consumer> {
        self.consumers
            .read()
            .iter()
            .find(|c| c.queue_id == queue_id && c.consumer_id == consumer_id)
            .cloned()
    }

    // ---- Messages: send / pull / ack ----

    /// Enqueue one message. Mirrors to GuardianDB so a restart of this
    /// queue's owner recovers the message (the `world_queue.rs` durability
    /// precedent). Returns the assigned message id.
    pub fn send(
        &self,
        queue_id: &str,
        body: String,
        metadata: serde_json::Value,
        delay_secs: u32,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            body.len() <= MAX_MESSAGE_BODY_BYTES,
            "message body exceeds the {MAX_MESSAGE_BODY_BYTES}-byte limit"
        );
        let id = format!("qm_{}", uuid::Uuid::new_v4().simple());
        let msg = QueueMessage {
            id: id.clone(),
            body,
            timestamp_ms: now_ms() + (delay_secs as u64) * 1000,
            attempts: 0,
            metadata,
            lease: None,
        };
        let log = self.log_for(queue_id);
        log.messages.write().push(msg.clone());
        let key = guardian_key(queue_id, &id);
        if let Ok(bytes) = serde_json::to_vec(&msg) {
            tokio::spawn(async move { crate::guardian::put(&key, bytes).await });
        }
        Ok(id)
    }

    /// Recover `queue_id`'s messages from GuardianDB exactly once per process
    /// lifetime on first touch (a fresh restart otherwise reads an honestly-
    /// empty backlog for up to the periodic sync loop's ~5s cadence).
    /// Idempotent and safe to call redundantly — `sync_from_guardian` itself
    /// only adds messages absent from the local set.
    async fn ensure_synced(&self, queue_id: &str) {
        let log = self.log_for(queue_id);
        if log
            .synced_once
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.sync_from_guardian(queue_id).await;
    }

    /// Live backlog stats for the metrics endpoint — computed from the real
    /// in-memory log, never a stale counter.
    pub async fn backlog_stats(&self, queue_id: &str) -> (usize, usize, Option<u64>) {
        self.ensure_synced(queue_id).await;
        let log = self.log_for(queue_id);
        let msgs = log.messages.read();
        let now = now_ms();
        let available: Vec<&QueueMessage> = msgs
            .iter()
            .filter(|m| {
                m.timestamp_ms <= now
                    && m.lease
                        .as_ref()
                        .is_none_or(|(_, expires)| *expires <= now)
            })
            .collect();
        let count = available.len();
        let bytes = available.iter().map(|m| m.body.len()).sum();
        let oldest = available.iter().map(|m| m.timestamp_ms).min();
        (count, bytes, oldest)
    }

    /// Lease up to `batch_size` available messages. A message is "available"
    /// when its scheduled time has arrived AND it carries no lease, or its
    /// lease has expired — checked and re-leased atomically under one write
    /// lock so two concurrent pulls can never both lease the same message
    /// (the TOCTOU class `queues-adversarial-edge-cases` names explicitly).
    pub async fn pull(
        &self,
        queue_id: &str,
        batch_size: u32,
        visibility_timeout_ms: u64,
    ) -> Vec<QueueMessage> {
        self.ensure_synced(queue_id).await;
        let log = self.log_for(queue_id);
        let mut msgs = log.messages.write();
        let now = now_ms();
        let expires = now + visibility_timeout_ms;
        let mut leased = Vec::new();
        for m in msgs.iter_mut() {
            if leased.len() as u32 >= batch_size {
                break;
            }
            let available = m.timestamp_ms <= now
                && m.lease
                    .as_ref()
                    .is_none_or(|(_, exp)| *exp <= now);
            if !available {
                continue;
            }
            let lease_id = format!("ql_{}", uuid::Uuid::new_v4().simple());
            m.lease = Some((lease_id, expires));
            m.attempts += 1;
            let mut out = m.clone();
            out.lease = m.lease.clone();
            leased.push(out);
        }
        leased
    }

    /// Ack (permanently remove) a leased message. Returns `false` for an
    /// unknown or already-expired-then-reclaimed lease id — a typed refusal,
    /// never a silent success (an ack racing an expiry must not appear to
    /// have worked when the message was already re-leased to someone else).
    pub fn ack(&self, queue_id: &str, lease_id: &str) -> bool {
        let log = self.log_for(queue_id);
        let mut msgs = log.messages.write();
        let before = msgs.len();
        let now = now_ms();
        let mut acked_id = None;
        msgs.retain(|m| {
            let matches = m
                .lease
                .as_ref()
                .is_some_and(|(id, exp)| id == lease_id && *exp > now);
            if matches {
                acked_id = Some(m.id.clone());
            }
            !matches
        });
        let ok = msgs.len() != before;
        drop(msgs);
        if let Some(id) = acked_id {
            let key = guardian_key(queue_id, &id);
            tokio::spawn(async move { crate::guardian::delete(&key).await });
        }
        ok
    }

    /// Retry a leased message: clear its lease and push its next-eligible
    /// time out by `delay_secs`. Same not-found semantics as `ack`.
    pub fn retry(&self, queue_id: &str, lease_id: &str, delay_secs: u32) -> bool {
        let log = self.log_for(queue_id);
        let mut msgs = log.messages.write();
        let now = now_ms();
        let mut found = false;
        for m in msgs.iter_mut() {
            if m.lease
                .as_ref()
                .is_some_and(|(id, exp)| id == lease_id && *exp > now)
            {
                m.lease = None;
                m.timestamp_ms = now + (delay_secs as u64) * 1000;
                found = true;
                break;
            }
        }
        found
    }

    /// Due messages this node currently holds for `queue_id`, removed from
    /// the log (re-inserted by the caller on a retriable failure) — the
    /// worker-consumer batch source. Never leases: push delivery owns the
    /// message directly, unlike the pull-consumer's lease/ack/retry cycle.
    /// Increments `attempts` on take (matching `pull`'s own counting) so a
    /// message's attempt count reflects DISPATCH attempts consistently
    /// across both consumer types — the caller compares the RETURNED
    /// `attempts` against `max_retries` directly, no separate reap scan.
    pub fn take_due_for_push(&self, queue_id: &str, max_batch: u32) -> Vec<QueueMessage> {
        let log = self.log_for(queue_id);
        let mut msgs = log.messages.write();
        let now = now_ms();
        let mut taken = Vec::new();
        let mut remaining = Vec::new();
        for mut m in msgs.drain(..) {
            if taken.len() as u32 >= max_batch || m.timestamp_ms > now || m.lease.is_some() {
                remaining.push(m);
                continue;
            }
            m.attempts += 1;
            taken.push(m);
        }
        *msgs = remaining;
        taken
    }

    /// Dead-letter (or drop, with no DLQ configured) exactly the given
    /// messages — never a queue-wide reap scan, which would also catch
    /// unrelated pull-consumer messages sharing the same log.
    pub fn dead_letter(&self, queue_id: &str, dlq_id: Option<&str>, messages: Vec<QueueMessage>) {
        if messages.is_empty() {
            return;
        }
        for m in &messages {
            let key = guardian_key(queue_id, &m.id);
            tokio::spawn(async move { crate::guardian::delete(&key).await });
        }
        let Some(dlq) = dlq_id else { return };
        let now = now_ms();
        let dlq_log = self.log_for(dlq);
        let mut dlq_msgs = dlq_log.messages.write();
        for mut m in messages {
            m.attempts = 0;
            m.lease = None;
            m.timestamp_ms = now;
            let key = guardian_key(dlq, &m.id);
            if let Ok(bytes) = serde_json::to_vec(&m) {
                tokio::spawn(async move { crate::guardian::put(&key, bytes).await });
            }
            dlq_msgs.push(m);
        }
    }

    /// Re-queue a message after a failed push-delivery attempt. `attempts`
    /// was already incremented by `take_due_for_push` at dispatch time —
    /// incrementing it again here would double-count and let a message
    /// exceed `max_retries` after only half as many real attempts.
    pub fn reschedule(&self, queue_id: &str, mut msg: QueueMessage, backoff_ms: u64) {
        msg.timestamp_ms = now_ms() + backoff_ms;
        msg.lease = None;
        let key = guardian_key(queue_id, &msg.id);
        if let Ok(bytes) = serde_json::to_vec(&msg) {
            tokio::spawn(async move { crate::guardian::put(&key, bytes).await });
        }
        self.log_for(queue_id).messages.write().push(msg);
    }

    pub fn mark_delivered(&self, queue_id: &str, message_id: &str) {
        let key = guardian_key(queue_id, message_id);
        tokio::spawn(async move { crate::guardian::delete(&key).await });
    }

    /// Bounded TTL sweep against `message_retention_period` — never reap more
    /// than half a queue's backlog in one pass (the `gc_rootfs_images`
    /// blast-radius-guard precedent: a bug in the age computation must not
    /// silently empty an entire live queue in one tick).
    pub fn sweep_expired(&self, queue_id: &str, retention_secs: u32) -> usize {
        let log = self.log_for(queue_id);
        let now = now_ms();
        let max_age_ms = (retention_secs as u64) * 1000;
        let mut msgs = log.messages.write();
        let total = msgs.len();
        if total == 0 {
            return 0;
        }
        let expired_ids: Vec<String> = msgs
            .iter()
            .filter(|m| now.saturating_sub(m.timestamp_ms) > max_age_ms)
            .map(|m| m.id.clone())
            .collect();
        let max_reap = (total / 2).max(1);
        let to_reap: std::collections::HashSet<String> =
            expired_ids.into_iter().take(max_reap).collect();
        if to_reap.is_empty() {
            return 0;
        }
        msgs.retain(|m| !to_reap.contains(&m.id));
        drop(msgs);
        for id in &to_reap {
            let key = guardian_key(queue_id, id);
            let key2 = key.clone();
            tokio::spawn(async move { crate::guardian::delete(&key2).await });
        }
        to_reap.len()
    }

    /// Recover a queue's messages from GuardianDB into the local working set
    /// (adopt-if-absent, idempotent) — run when this node becomes/rejoins as
    /// the elected owner for `queue_id`.
    pub async fn sync_from_guardian(&self, queue_id: &str) {
        let log = self.log_for(queue_id);
        let prefix = format!("queues/msg/{queue_id}/");
        for key in crate::guardian::keys().await {
            if !key.starts_with(&prefix) {
                continue;
            }
            if let Some(bytes) = crate::guardian::get(&key).await {
                if let Ok(msg) = serde_json::from_slice::<QueueMessage>(&bytes) {
                    let mut msgs = log.messages.write();
                    if !msgs.iter().any(|m| m.id == msg.id) {
                        msgs.push(msg);
                    }
                }
            }
        }
    }

    // ---- Disk persistence (this node's own boot/restore, not cross-node merge) ----

    pub fn load(&self, synced: SyncedQueues) {
        *self.queues.write() = synced.queues;
        *self.consumers.write() = synced.consumers;
        *self.queue_tombstones.write() = synced.queue_tombstones;
        *self.consumer_tombstones.write() = synced.consumer_tombstones;
    }

    // ---- Replicated snapshot / merge (metadata only — messages never ride this) ----

    pub fn snapshot_synced(&self) -> SyncedQueues {
        let mut queues = self.queues.read().clone();
        queues.sort_by(|a, b| a.queue_id.cmp(&b.queue_id));
        let mut consumers = self.consumers.read().clone();
        consumers.sort_by(|a, b| a.consumer_id.cmp(&b.consumer_id));
        SyncedQueues {
            queues,
            consumers,
            queue_tombstones: self.queue_tombstones.read().clone(),
            consumer_tombstones: self.consumer_tombstones.read().clone(),
        }
    }

    pub fn merge_synced(&self, remote: SyncedQueues) -> usize {
        let now = now_ms();
        {
            let mut tombs = self.queue_tombstones.write();
            for (id, ms) in remote.queue_tombstones {
                let e = tombs.entry(id).or_insert(ms);
                if ms > *e {
                    *e = ms;
                }
            }
            tombs.retain(|_, ms| now.saturating_sub(*ms) < TOMBSTONE_RETENTION_MS);
        }
        {
            let mut tombs = self.consumer_tombstones.write();
            for (id, ms) in remote.consumer_tombstones {
                let e = tombs.entry(id).or_insert(ms);
                if ms > *e {
                    *e = ms;
                }
            }
            tombs.retain(|_, ms| now.saturating_sub(*ms) < TOMBSTONE_RETENTION_MS);
        }
        let queue_tombs = self.queue_tombstones.read().clone();
        let consumer_tombs = self.consumer_tombstones.read().clone();

        {
            let mut queues = self.queues.write();
            for r in remote.queues {
                match queues.iter_mut().find(|q| q.queue_id == r.queue_id) {
                    Some(local) if r.modified_on >= local.modified_on => *local = r,
                    Some(_) => {}
                    None => queues.push(r),
                }
            }
            queues.retain(|q| {
                queue_tombs
                    .get(&q.queue_id)
                    .is_none_or(|ts| *ts < q.created_on)
            });
        }
        {
            let mut consumers = self.consumers.write();
            for r in remote.consumers {
                if !consumers.iter().any(|c| c.consumer_id == r.consumer_id) {
                    consumers.push(r);
                }
            }
            consumers.retain(|c| {
                consumer_tombs
                    .get(&c.consumer_id)
                    .is_none_or(|ts| *ts < c.created_on)
            });
        }
        self.queues.read().len()
    }
}

/// Deterministic per-queue primary election, scoped to nodes currently
/// hosting the consumer's tenant — identical shape to
/// `world_queue::is_primary_for_team`, so delivery ownership cannot drift
/// between the two queue mechanisms this platform now runs side by side.
async fn is_primary_for_queue(cloud: &Arc<crate::state::CloudState>, tenant: &str) -> bool {
    let mut candidates: Vec<String> = Vec::new();
    if cloud
        .gw
        .list()
        .iter()
        .any(|d| crate::admin::record_tenant(&d.tenant) == tenant)
    {
        candidates.push(cloud.node_name.clone());
    }
    for (node, deps) in cloud.peer_deployments.read().iter() {
        if deps
            .iter()
            .any(|d| crate::admin::record_tenant(&d.tenant) == tenant)
            && !candidates.contains(node)
        {
            candidates.push(node.clone());
        }
    }
    if candidates.is_empty() {
        return true;
    }
    let nodes = cloud.registry.nodes();
    crate::cluster::Cluster::elect_among(&candidates, &nodes).as_deref()
        == Some(cloud.node_name.as_str())
}

/// Push delivery for Worker-type consumers: batches due messages up to
/// `max_batch_size`/`max_batch_timeout`, POSTs the batch to the consumer's
/// `script_name` project (a real deployed platform function reached over its
/// normal public invocation URL — the same `world_queue.rs` HTTP-dispatch
/// shape already proven live, just addressed at the tenant's own function
/// instead of an arbitrary webhook URL). On success the whole batch acks; on
/// failure every message's `attempts` increments and reschedules with
/// exponential backoff, dead-lettering at `max_retries`.
pub async fn spawn_delivery_loop(cloud: Arc<crate::state::CloudState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut sweep_tick: u32 = 0;
    loop {
        interval.tick().await;
        sweep_tick += 1;
        let queues = cloud.queues.snapshot_synced().queues;
        for queue in &queues {
            if queue.settings.delivery_paused {
                continue;
            }
            if !is_primary_for_queue(&cloud, &queue.tenant).await {
                continue;
            }
            // First-touch recovery, same rationale as `ensure_synced` on the
            // read paths: without this a push consumer's messages were
            // invisible for up to the full periodic-sweep cadence (5s) after
            // every restart, not just the pull-consumer/metrics paths.
            cloud.queues.ensure_synced(&queue.queue_id).await;
            // Periodic retention sweep + GuardianDB recovery, same cadence
            // idea as world_queue's ~5s sync tick (10 ticks * 500ms).
            if sweep_tick % 10 == 0 {
                cloud.queues.sync_from_guardian(&queue.queue_id).await;
                let reaped = cloud
                    .queues
                    .sweep_expired(&queue.queue_id, queue.settings.message_retention_period);
                if reaped > 0 {
                    tracing::debug!(queue = %queue.queue_id, reaped, "queues: retention sweep reaped expired messages");
                }
            }
            for consumer in cloud.queues.list_consumers(&queue.queue_id) {
                if consumer.kind != ConsumerType::Worker || consumer.script_name.is_empty() {
                    continue;
                }
                let due = cloud
                    .queues
                    .take_due_for_push(&queue.queue_id, consumer.settings.batch_size);
                if due.is_empty() {
                    continue;
                }
                let url = format!(
                    "{}/{}",
                    cloud.deploy_url(&consumer.script_name),
                    "__hive_queue_consumer"
                );
                let batch_json: Vec<serde_json::Value> = due
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "timestamp": m.timestamp_ms,
                            "body": m.body,
                            "attempts": m.attempts,
                        })
                    })
                    .collect();
                let req = cloud
                    .http
                    .post(&url)
                    .header("x-hive-queue-id", queue.queue_id.as_str())
                    .header("x-hive-queue-consumer", consumer.consumer_id.as_str())
                    .json(&serde_json::json!({ "messages": batch_json }));
                let outcome =
                    tokio::time::timeout(std::time::Duration::from_secs(30), req.send()).await;
                let ok = matches!(&outcome, Ok(Ok(resp)) if resp.status().is_success());
                if ok {
                    for m in &due {
                        cloud.queues.mark_delivered(&queue.queue_id, &m.id);
                    }
                } else {
                    let (exhausted, retriable): (Vec<_>, Vec<_>) = due
                        .into_iter()
                        .partition(|m| m.attempts > consumer.settings.max_retries);
                    if !exhausted.is_empty() {
                        cloud.queues.dead_letter(
                            &queue.queue_id,
                            consumer.dead_letter_queue.as_deref(),
                            exhausted,
                        );
                    }
                    for m in retriable {
                        let backoff_ms = if consumer.settings.retry_delay > 0 {
                            (consumer.settings.retry_delay as u64) * 1000
                        } else {
                            (1000u64.saturating_mul(1 << m.attempts.min(6))).min(60_000)
                        };
                        cloud.queues.reschedule(&queue.queue_id, m, backoff_ms);
                    }
                }
            }
        }
    }
}
