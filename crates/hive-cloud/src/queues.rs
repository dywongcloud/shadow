//! A durable, tenant-scoped message queue service shaped like Cloudflare Queues: the
//! `/accounts/{account_id}/queues` REST contract, Worker push and HTTP pull consumers, and the
//! metrics surface.
//!
//! Two storage tiers:
//! * **Metadata** (queue and consumer records) is small and store_sync-replicated, so every node
//! holds a fresh copy. `merge_synced` merges per row with tombstones, as `databases.rs`'s
//! `SyncedDatabases` does, and never adopts by replace: a leader killed before its debounced save
//! ran must not erase every follower's copy of a record it never saved.
//! * **Messages** are node-local to the queue's elected owner (a deterministic per-queue election,
//! the same `Cluster::elect_among` shape as `world_queue.rs`'s `is_primary_for_team`) and mirrored
//! to GuardianDB for durability across a restart. They are never replicated mesh-wide: volume is
//! unbounded and only the owner needs the working set.

use hive_core::now_ms;
use hmac::{Hmac, Mac};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;

pub const TOMBSTONE_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Minimum delivery delay in seconds. Every bound in this block is Cloudflare Queues' documented
/// limit (developers.cloudflare.com/queues).
pub const DELIVERY_DELAY_MIN_SECS: u32 = 0;
pub const DELIVERY_DELAY_MAX_SECS: u32 = 86_400;
pub const RETENTION_MIN_SECS: u32 = 60;
pub const RETENTION_MAX_SECS: u32 = 1_209_600;
pub const RETENTION_DEFAULT_SECS: u32 = 4 * 24 * 60 * 60;
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
/// Cloudflare's per-message body cap: refused loudly, never silently truncated (truncation in a
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
    /// Clamps to Cloudflare's documented bounds: an out-of-range request is clamped, never
    /// rejected, and the caller sees the effective value in the response (defaulted fields and
    /// clamped ceilings, never a silent drop).
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
    /// Server-resolved tenant (the platform's account_id equivalent), never trusted from the
    /// request body.
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
    /// `None` = available for lease and delivery; `Some((lease_id, expires_ms))` = currently
    /// leased. An expired lease counts as available again for every reader and is never eagerly
    /// swept: state is checked at use time, not by a janitor racing real acquisition.
    #[serde(default)]
    pub lease: Option<(String, u64)>,
}

/// A queue's live message working set: node-local (the owner's) and mirrored to GuardianDB, never
/// store_sync'd (unbounded volume, and only the owner needs the hot set).
#[derive(Default)]
struct MessageLog {
    messages: RwLock<Vec<QueueMessage>>,
    /// Set once this log has been recovered from GuardianDB at least once. Every read path forces a
    /// synchronous recovery the FIRST time it touches a queue this process has not loaded, then
    /// relies on the periodic ~5 s tick for ongoing convergence; without it a freshly restarted
    /// node read an empty backlog for up to a tick.
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

    /// Cascades to every consumer of the queue (as Cloudflare's delete does) and tombstones the
    /// queue and its consumers, so the deletion replicates instead of being re-adopted from a peer
    /// that has not heard about it.
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

    /// Enqueues one message and mirrors it to GuardianDB so a restart of this queue's owner
    /// recovers it. Returns the assigned message id.
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

    /// Recovers `queue_id`'s messages from GuardianDB exactly once per process lifetime, on first
    /// touch. Idempotent and safe to call redundantly: `sync_from_guardian` only adds messages
    /// absent from the local set.
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
                m.timestamp_ms <= now && m.lease.as_ref().is_none_or(|(_, expires)| *expires <= now)
            })
            .collect();
        let count = available.len();
        let bytes = available.iter().map(|m| m.body.len()).sum();
        let oldest = available.iter().map(|m| m.timestamp_ms).min();
        (count, bytes, oldest)
    }

    /// Leases up to `batch_size` available messages. A message is available when its scheduled time
    /// has arrived AND it carries no lease, or its lease has expired; this is checked and re-leased
    /// atomically under one write lock, so two concurrent pulls can never lease the same message.
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
            let available =
                m.timestamp_ms <= now && m.lease.as_ref().is_none_or(|(_, exp)| *exp <= now);
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

    /// Acks (permanently removes) a leased message. Returns `false` for an unknown, or expired and
    /// since re-leased, lease id: a typed refusal, never a silent success (an ack racing an expiry
    /// must not look like it worked when the message went to someone else).
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

    /// Due messages this node holds for `queue_id`, removed from the log (the caller re-inserts
    /// them on a retriable failure): the Worker-consumer batch source. Never leases: push delivery
    /// owns the message directly, unlike the pull consumer's lease/ack/retry cycle. Increments
    /// `attempts` on take, as `pull` does, so the count means DISPATCH attempts for both consumer
    /// types and the caller compares the returned `attempts` against `max_retries` directly, with
    /// no separate reap scan.
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

    /// Dead-letters (or drops, when no DLQ is configured) exactly the given messages, never a
    /// queue-wide reap scan, which would also catch unrelated pull-consumer messages sharing the
    /// log.
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

    /// Re-queues a message after a failed push-delivery attempt. `attempts` was already incremented
    /// by `take_due_for_push` at dispatch time; incrementing it again here would double-count and
    /// let a message exceed `max_retries` after half as many real attempts.
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

    /// Bounded TTL sweep against `message_retention_period`: reaps at most half of a queue's
    /// backlog per pass (at least one message), so a bug in the age computation cannot silently
    /// empty a live queue in one tick. Returns the count reaped.
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

    /// Recovers a queue's messages from GuardianDB into the local working set (adopt-if-absent,
    /// idempotent); run when this node becomes, or rejoins as, the elected owner for `queue_id`.
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

    /// Restores this node's own persisted copy at boot: a disk restore, not a cross-node merge.
    pub fn load(&self, synced: SyncedQueues) {
        *self.queues.write() = synced.queues;
        *self.consumers.write() = synced.consumers;
        *self.queue_tombstones.write() = synced.queue_tombstones;
        *self.consumer_tombstones.write() = synced.consumer_tombstones;
    }

    /// The replicated snapshot: queue and consumer metadata plus tombstones, sorted by id. Messages
    /// never ride it.
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

    /// Merges a peer's snapshot and returns the resulting queue count. Tombstones union keeping the
    /// LATEST deletion time and expire after [`TOMBSTONE_RETENTION_MS`]; a queue takes the copy
    /// with the newer `modified_on` (the remote wins ties); consumers are add-only, a local
    /// consumer is never overwritten; a record whose tombstone is at or after its `created_on` is
    /// dropped.
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

/// Deterministic per-queue primary election among the nodes currently hosting the consumer's
/// tenant, the same shape as `world_queue::is_primary_for_team`, so delivery ownership cannot drift
/// between the two queue mechanisms. A tenant hosted nowhere makes every node primary.
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

type HmacSha256 = Hmac<Sha256>;

/// Signing secret of one push consumer, derived from the fleet-shared at-rest key (the shape of
/// `zkauth::derive_secret`): every node computes the same value, nothing is stored or replicated,
/// and the consumer's owner reads it from the consumer view to verify the `x-hive-signature` of
/// every batch. Each field is length-prefixed so ("ab", "c") and ("a", "bc") cannot collide.
pub fn consumer_signing_secret(queue_id: &str, consumer_id: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(&crate::secrets::key_material())
        .expect("HMAC accepts any key length");
    mac.update(b"hive-queue-consumer-v1");
    for part in [queue_id, consumer_id] {
        mac.update(&(part.len() as u64).to_le_bytes());
        mac.update(part.as_bytes());
    }
    let digest = mac.finalize().into_bytes();
    format!(
        "qsec_{}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// The project the host label `<label>.{apps_domain}` reaches: a deployment alias of this node's
/// gateway or of any peer's gossiped record, else the label is the project name itself.
fn label_project(cloud: &Arc<crate::state::CloudState>, label: &str) -> String {
    if let Some((_, project)) = cloud.gw.attribution_for_host(label) {
        return project;
    }
    let names_label = |alias: &str| {
        alias
            .split('.')
            .next()
            .unwrap_or("")
            .eq_ignore_ascii_case(label)
    };
    cloud
        .peer_deployments
        .read()
        .values()
        .flatten()
        .find(|d| {
            [&d.alias, &d.commit_alias, &d.branch_alias, &d.id_alias]
                .into_iter()
                .any(|alias| !alias.is_empty() && names_label(alias))
        })
        .map(|d| d.project.clone())
        .unwrap_or_else(|| label.to_string())
}

/// A label this platform will ever put in front of its own apps/deploy-zone domain: lowercase
/// ASCII letters, digits and hyphens ONLY. `script_name` is tenant input that reaches
/// [`crate::state::CloudState::deploy_url`], which builds the delivery authority from it, so
/// anything outside this set is refused outright here rather than trusted to survive URL
/// construction unharmed — the userinfo/fragment/port-override class of bug a raw `format!` into a
/// URL STRING is vulnerable to: a label containing `@` becomes userinfo and the REAL destination
/// host, one containing `#` swallows the rest of the string as a fragment, and a literal `:`
/// changes the port. `deploy_url` itself is hardened the same way (`host_scoped_https_url`), so
/// this is defense in depth, not the only gate.
fn valid_script_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The ONLY part of a consumer's `script_name` that may reach [`crate::state::CloudState::deploy_url`]:
/// its first label, the one [`consumer_target_owned`] resolves to a project and checks the owner of.
/// `deploy_url` formats the WHOLE alias under `HIVE_INGRESS=ngrok`, so handing it the tenant's string
/// let `own-app.<any-host>` choose the delivery host (the unfiltered internal client then connects to
/// it) while the ownership check had read only `own-app`. Check and use read this one value.
fn delivery_label(script_name: &str) -> String {
    script_name
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Whether `script_name` addresses an app of `tenant`, requiring POSITIVE evidence. A push
/// consumer's batches are POSTed to `cloud.deploy_url(script_name)`, i.e. the host label
/// `<script_name's first label>.{apps_domain}` of whatever project or deployment alias that label
/// names, so the owner of that project must be the queue's own tenant; otherwise a tenant could aim
/// its queue at another tenant's handler. Unlike the general `admin::project_owned_by` (which
/// trusts the caller optimistically for a project nobody has ever heard of, useful when claiming a
/// brand-new name), a queue consumer must never target a not-yet-existing name: the only way such a
/// name becomes real later is if SOME tenant deploys it, and an attacker who predicts or squats it
/// would then receive the batches a trusting victim's queue was already addressed there. Checked
/// when a consumer is registered AND before every delivery pass (ownership, and existence, can both
/// change after registration).
pub fn consumer_target_owned(
    cloud: &Arc<crate::state::CloudState>,
    tenant: &str,
    script_name: &str,
) -> bool {
    // A malformed name is refused outright, tail labels included: a port, a userinfo or an empty
    // label after the first dot is never a project name, and refusing it here keeps a typo from
    // registering as if it worked (delivery reads only `delivery_label`, never the tail).
    if !script_name
        .to_ascii_lowercase()
        .split('.')
        .all(valid_script_label)
    {
        return false;
    }
    let label = delivery_label(script_name);
    let project = label_project(cloud, &label);
    let owner = cloud.projects.team_of(&project);
    if crate::admin::norm(&owner) == tenant {
        return true;
    }
    cloud.gw.deployment_records().iter().any(|r| {
        r.project.eq_ignore_ascii_case(&project) && crate::admin::record_tenant(&r.tenant) == tenant
    }) || cloud.peer_deployments.read().values().flatten().any(|d| {
        d.project.eq_ignore_ascii_case(&project) && crate::admin::record_tenant(&d.tenant) == tenant
    })
}

/// Logs a foreign consumer target at most once a minute per consumer: the delivery loop revisits
/// every consumer twice a second.
fn warn_foreign_target(queue_id: &str, consumer: &Consumer) {
    static LAST_WARNED: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, u64>>> =
        std::sync::LazyLock::new(Default::default);
    let now = now_ms();
    let mut last = LAST_WARNED.lock();
    let due = last
        .get(&consumer.consumer_id)
        .is_none_or(|at| now.saturating_sub(*at) >= 60_000);
    if due {
        last.insert(consumer.consumer_id.clone(), now);
        tracing::warn!(
            queue = %queue_id,
            consumer = %consumer.consumer_id,
            target = %consumer.script_name,
            "queues: consumer target is not an app of the queue's tenant, delivery skipped"
        );
    }
}

const DELIVERY_TICK_MS: u64 = 500;
/// GuardianDB recovery and the retention sweep run on every Nth delivery tick (about 5 s).
const MAINTENANCE_EVERY_TICKS: u32 = 10;

/// Push delivery for Worker-type consumers, ticking every `DELIVERY_TICK_MS`. For each unpaused
/// queue this node is primary for it first recovers the queue once (`ensure_synced`, so a restarted
/// node does not skip messages it has not loaded yet), runs GuardianDB recovery and the retention
/// sweep every `MAINTENANCE_EVERY_TICKS` ticks, then takes due messages up to the consumer's
/// `batch_size` and POSTs the batch to the consumer's `script_name` project over its normal public
/// invocation URL (the `world_queue.rs` HTTP-dispatch shape), only while that project is the
/// queue's own tenant's ([`consumer_target_owned`]) and signed with the consumer's own secret
/// (`x-hive-signature`, [`consumer_signing_secret`]), so the handler can tell a delivery from a
/// request anyone on the internet could forge. On success the whole batch acks; on failure every message's `attempts` increments and
/// it reschedules with exponential backoff, dead-lettering at `max_retries`.
pub async fn spawn_delivery_loop(cloud: Arc<crate::state::CloudState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(DELIVERY_TICK_MS));
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
            cloud.queues.ensure_synced(&queue.queue_id).await;
            if sweep_tick % MAINTENANCE_EVERY_TICKS == 0 {
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
                if !consumer_target_owned(
                    &cloud,
                    crate::admin::record_tenant(&queue.tenant),
                    &consumer.script_name,
                ) {
                    warn_foreign_target(&queue.queue_id, &consumer);
                    continue;
                }
                let due = cloud
                    .queues
                    .take_due_for_push(&queue.queue_id, consumer.settings.batch_size);
                if due.is_empty() {
                    continue;
                }
                // The destination host is built from the ONE label `consumer_target_owned` just
                // resolved and ownership-checked (`delivery_label`, then `deploy_url` via
                // `host_scoped_https_url`), never from the tenant's whole `script_name`: this is the
                // platform's OWN apps-domain namespace, never an arbitrary tenant-supplied host, so
                // the full `hive_outbound` SSRF guard (which would also refuse a dev node whose apps
                // domain is itself a loopback/internal name) does not apply here — only the
                // no-redirect client does.
                let target = format!(
                    "{}/__hive_queue_consumer",
                    cloud.deploy_url(&delivery_label(&consumer.script_name))
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
                let body = serde_json::json!({ "messages": batch_json }).to_string();
                // Signed over `timestamp.nonce.body` (never body alone), so a captured batch cannot
                // be replayed verbatim: the receiver recomputes the HMAC over the SAME three parts
                // (reading them back from the x-hive-timestamp/x-hive-nonce headers) and rejects a
                // stale timestamp or a reused nonce itself, exactly as it already must recompute the
                // signature.
                let ts = now_ms().to_string();
                let nonce = uuid::Uuid::new_v4().simple().to_string();
                let signed_message = format!("{ts}.{nonce}.{body}");
                let signature = crate::webhooks::sign(
                    &consumer_signing_secret(&queue.queue_id, &consumer.consumer_id),
                    &signed_message,
                );
                // The destination is this platform's own apps domain (hardened host construction in
                // `deploy_url`/`consumer_target_owned`), never an arbitrary tenant-supplied URL, so
                // the no-redirect internal client is the right guard: a redirect here could only come
                // from the target app itself, and following it anywhere is never required to deliver
                // a batch to that app's own fixed consumer route.
                let req = hive_outbound::internal_client()
                    .post(target)
                    .header("content-type", "application/json")
                    .header("x-hive-queue-id", queue.queue_id.as_str())
                    .header("x-hive-queue-consumer", consumer.consumer_id.as_str())
                    .header("x-hive-timestamp", ts)
                    .header("x-hive-nonce", nonce)
                    .header("x-hive-signature", signature)
                    .body(body);
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
