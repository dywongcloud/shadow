//! Hive's own native Queue service for the Vercel Workflow SDK `World` interface
//! -- a first-party, self-hosted replacement for the `Queue` third of
//! `World = Storage & Queue & Streamer`, matching the real contract (enqueue,
//! at-least-once delivery via HTTP callback, retry with backoff, no external
//! managed-queue dependency of any kind).
//!
//! Prototype scope (Phase 0 spike): in-memory job store + a single delivery
//! loop on this node. Production (later phases) moves the store to the
//! project's own managed Redis and elects a per-project primary the same way
//! the rest of the managed-world design does -- this module's enqueue/deliver
//! contract does not change when that happens.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use hive_core::now_ms;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueJob {
    pub id: String,
    pub target_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub payload: Value,
    pub run_at_ms: u64,
    pub attempt: u32,
    pub max_attempts: u32,
}

/// In-memory (prototype) job store + delivery bookkeeping. `pending` holds
/// every not-yet-delivered job; `done`/`dead` are small ring-buffered result
/// logs for observability (dashboard/debugging), not a durability guarantee.
pub struct WorldQueue {
    pending: RwLock<BTreeMap<String, QueueJob>>,
    delivered: RwLock<Vec<String>>,
    dead: RwLock<Vec<String>>,
}

impl WorldQueue {
    pub fn new() -> Arc<WorldQueue> {
        Arc::new(WorldQueue { pending: RwLock::new(BTreeMap::new()), delivered: RwLock::new(Vec::new()), dead: RwLock::new(Vec::new()) })
    }

    pub fn enqueue(
        &self,
        target_url: String,
        headers: BTreeMap<String, String>,
        payload: Value,
        delay_seconds: u64,
        max_attempts: u32,
    ) -> String {
        let id = format!("wq_{}", uuid::Uuid::new_v4().simple());
        let job = QueueJob {
            id: id.clone(),
            target_url,
            headers,
            payload,
            run_at_ms: now_ms() + delay_seconds.saturating_mul(1000),
            attempt: 0,
            max_attempts: max_attempts.max(1),
        };
        self.pending.write().insert(id.clone(), job);
        id
    }

    /// Every due job, removed from `pending` (re-inserted by the caller on a
    /// retriable failure). Never holds the lock across the network call.
    fn take_due(&self) -> Vec<QueueJob> {
        let now = now_ms();
        let mut m = self.pending.write();
        let due_ids: Vec<String> = m.iter().filter(|(_, j)| j.run_at_ms <= now).map(|(id, _)| id.clone()).collect();
        due_ids.into_iter().filter_map(|id| m.remove(&id)).collect()
    }

    fn reschedule(&self, mut job: QueueJob) {
        job.attempt += 1;
        // Exponential backoff, capped at 60s, matching the base design's
        // documented default (retryBaseMs-equivalent) for the reference world.
        let backoff_ms = (1000u64.saturating_mul(1 << job.attempt.min(6))).min(60_000);
        job.run_at_ms = now_ms() + backoff_ms;
        self.pending.write().insert(job.id.clone(), job);
    }

    fn mark_delivered(&self, id: &str) {
        let mut v = self.delivered.write();
        v.push(id.to_string());
        if v.len() > 200 {
            let excess = v.len() - 200;
            v.drain(0..excess);
        }
    }

    fn mark_dead(&self, id: &str) {
        let mut v = self.dead.write();
        v.push(id.to_string());
        if v.len() > 200 {
            let excess = v.len() - 200;
            v.drain(0..excess);
        }
    }

    pub fn stats(&self) -> Value {
        json!({
            "pending": self.pending.read().len(),
            "delivered_recent": self.delivered.read().len(),
            "dead_recent": self.dead.read().len(),
        })
    }
}

/// The delivery loop: poll for due jobs, POST each to its target URL, retry
/// with backoff on failure up to `max_attempts`, else dead-letter. Mirrors the
/// exact HTTP-callback shape `wf_invoker`/`StepInvoker` already uses to durably
/// invoke a deployment endpoint from this node's own always-on process --
/// applied here to third-party WDK apps' own queued step/workflow invocations
/// instead of hive's internal WorkflowStep records.
pub async fn run_delivery_loop(http: reqwest::Client, queue: Arc<WorldQueue>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        interval.tick().await;
        for job in queue.take_due() {
            let mut req = http.post(&job.target_url).json(&job.payload);
            for (k, v) in &job.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req = req.header("x-hive-queue-attempt", (job.attempt + 1).to_string());
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), req.send()).await;
            let ok = matches!(&outcome, Ok(Ok(resp)) if resp.status().is_success());
            if ok {
                queue.mark_delivered(&job.id);
            } else if job.attempt + 1 >= job.max_attempts {
                tracing::warn!(job_id = %job.id, target = %job.target_url, attempts = job.attempt + 1, "world_queue: job dead-lettered");
                queue.mark_dead(&job.id);
            } else {
                queue.reschedule(job);
            }
        }
    }
}
