//! Hive's own native Queue service for the Vercel Workflow SDK `World` interface
//! -- a first-party, self-hosted, MULTI-REGION replacement for the `Queue`
//! third of `World = Storage & Queue & Streamer` (no Zeplo, no external
//! managed-queue dependency of any kind).
//!
//! Multi-region model: every enqueue is mirrored into GuardianDB (the
//! replicated, content-addressed mesh KV already used for the roster/TLS
//! bundles) under a tenant-scoped key, so any node can see a tenant's pending
//! jobs. Exactly one node runs the delivery loop for a given tenant at a
//! time -- the lowest-healthy-peer_id node among those currently hosting that
//! tenant's deployments (the SAME deterministic election shape as
//! `Cluster::billing_leader`, scoped to the tenant's hosting set instead of
//! the whole fleet). On a hosting-node failure, health flips within the probe
//! interval and the next-lowest-identity hosting node's next tick picks the
//! job set back up from GuardianDB -- no separate promotion RPC needed.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use hive_core::now_ms;

use crate::state::CloudState;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueJob {
    pub id: String,
    pub team: String,
    pub target_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub payload: Value,
    pub run_at_ms: u64,
    pub attempt: u32,
    pub max_attempts: u32,
}

fn guardian_key(team: &str, id: &str) -> String {
    format!("wqueue/pending/{team}/{id}")
}

/// Python WDK detection: `vercel.json`'s `experimentalServices` declares a
/// worker whose `topics` includes a `__wkf_*` pattern (the fixed Workflow SDK
/// queue-name prefix, confirmed against vercel.com/docs/workflows/python --
/// Python has no build-time `.well-known/workflow/v1/manifest.json` artifact
/// the way JS/TS does, so this static pre-build config is the only available
/// signal). Tolerant parse: a malformed/missing vercel.json is simply "not
/// detected", never a build error.
pub fn vercel_json_declares_workflow_worker(dir: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("vercel.json")) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return false };
    let Some(services) = v.get("experimentalServices").and_then(|s| s.as_object()) else { return false };
    services.values().any(|svc| {
        svc.get("topics")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().any(|t| t.as_str().is_some_and(|s| s.starts_with("__wkf_"))))
            .unwrap_or(false)
    })
}

/// Opt-out signals for the managed World Queue auto-wiring: the project
/// already brought its own Upstash/Redis world config (BYO -- never silently
/// override or duplicate a tenant's own external connection), or fluid.json
/// explicitly declares `{"workflow":{"world":"external"}}` (tolerant parse,
/// same style as the existing container-override block).
pub fn workflow_world_opted_out(cloud: &Arc<CloudState>, project: &str, dir: &std::path::Path) -> bool {
    let env = cloud.projects.env_map(project);
    let byo = env.get("WORKFLOW_REDIS_REST_URL").is_some_and(|s| !s.trim().is_empty())
        || env.get("UPSTASH_REDIS_REST_URL").is_some_and(|s| !s.trim().is_empty());
    if byo {
        return true;
    }
    let Ok(text) = std::fs::read_to_string(dir.join("fluid.json")) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return false };
    v.get("workflow").and_then(|w| w.get("world")).and_then(|w| w.as_str()) == Some("external")
}

/// Local working set (every node holds a copy once it learns of a job, but
/// only the current tenant-primary actually dispatches it). `delivered`/`dead`
/// are small ring-buffered result logs for observability, not a durability
/// guarantee -- GuardianDB is the durability layer.
pub struct WorldQueue {
    pending: RwLock<BTreeMap<String, QueueJob>>,
    delivered: RwLock<Vec<String>>,
    dead: RwLock<Vec<String>>,
}

impl WorldQueue {
    pub fn new() -> Arc<WorldQueue> {
        Arc::new(WorldQueue { pending: RwLock::new(BTreeMap::new()), delivered: RwLock::new(Vec::new()), dead: RwLock::new(Vec::new()) })
    }

    /// Enqueue locally and kick off the (best-effort, async) GuardianDB mirror
    /// so every other node hosting `team` can see this job even if THIS node
    /// never becomes primary for it (e.g. it only received the enqueue call
    /// because the app's HTTP client happened to resolve here).
    pub fn enqueue(
        &self,
        team: String,
        target_url: String,
        headers: BTreeMap<String, String>,
        payload: Value,
        delay_seconds: u64,
        max_attempts: u32,
    ) -> String {
        let id = format!("wq_{}", uuid::Uuid::new_v4().simple());
        let job = QueueJob {
            id: id.clone(),
            team,
            target_url,
            headers,
            payload,
            run_at_ms: now_ms() + delay_seconds.saturating_mul(1000),
            attempt: 0,
            max_attempts: max_attempts.max(1),
        };
        self.pending.write().insert(id.clone(), job.clone());
        let key = guardian_key(&job.team, &job.id);
        if let Ok(bytes) = serde_json::to_vec(&job) {
            tokio::spawn(async move { crate::guardian::put(&key, bytes).await });
        }
        id
    }

    /// Every due job THIS node currently holds, removed from `pending`
    /// (re-inserted by the caller on a retriable failure). Never holds the
    /// lock across the network call.
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
        let key = guardian_key(&job.team, &job.id);
        if let Ok(bytes) = serde_json::to_vec(&job) {
            tokio::spawn(async move { crate::guardian::put(&key, bytes).await });
        }
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

    /// Adopt a job recovered from GuardianDB (another node's mirror, or this
    /// node's own after a restart) into the local working set if not already
    /// tracked here. Idempotent.
    fn adopt(&self, job: QueueJob) {
        self.pending.write().entry(job.id.clone()).or_insert(job);
    }

    pub fn stats(&self) -> Value {
        json!({
            "pending": self.pending.read().len(),
            "delivered_recent": self.delivered.read().len(),
            "dead_recent": self.dead.read().len(),
        })
    }
}

/// Deterministic tenant-scoped primary election, the SAME shape as
/// `Cluster::billing_leader` (lowest healthy peer_id wins) but scoped to only
/// the nodes currently hosting `team`'s deployments -- never the whole fleet,
/// since a queue's delivery target (the app's own callback URL) is only
/// meaningfully reachable/relevant from a node actually serving that tenant.
/// Fails open (this node is primary) when hosting info isn't available yet
/// (fresh gossip, single-node dev) rather than stranding jobs undelivered.
async fn is_primary_for_team(cloud: &Arc<CloudState>, team: &str) -> bool {
    let mut candidates: Vec<String> = Vec::new();
    if cloud.gw.list().iter().any(|d| crate::admin::norm(&d.tenant) == team) {
        candidates.push(cloud.node_name.clone());
    }
    for (node, deps) in cloud.peer_deployments.read().iter() {
        if deps.iter().any(|d| crate::admin::norm(&d.tenant) == team) && !candidates.contains(node) {
            candidates.push(node.clone());
        }
    }
    if candidates.is_empty() {
        return true;
    }
    let nodes = cloud.registry.nodes();
    let mut healthy_ids: Vec<(String, String)> = nodes
        .iter()
        .filter(|n| candidates.contains(&n.name) && n.healthy)
        .filter_map(|n| n.peer_id.clone().map(|pid| (n.name.clone(), pid)))
        .collect();
    if healthy_ids.is_empty() {
        // No election-eligible identity known for any candidate yet (e.g. iroh
        // not bound) -- fall back to node-name ordering so a primary still
        // exists deterministically rather than every node deferring forever.
        let mut names = candidates.clone();
        names.sort();
        return names.first() == Some(&cloud.node_name);
    }
    healthy_ids.sort_by(|a, b| a.1.cmp(&b.1));
    healthy_ids.first().map(|(name, _)| name.as_str()) == Some(cloud.node_name.as_str())
}

/// Pull every job mirrored in GuardianDB for tenants THIS node currently hosts
/// (or is otherwise a candidate for) into the local working set. Run
/// periodically so a newly-elected primary recovers jobs enqueued while
/// another node held the job (or before this node existed / after a restart).
async fn sync_from_guardian(cloud: &Arc<CloudState>, queue: &Arc<WorldQueue>) {
    let mut hosted_teams: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in cloud.gw.list() {
        hosted_teams.insert(crate::admin::norm(&d.tenant).to_string());
    }
    for deps in cloud.peer_deployments.read().values() {
        for d in deps {
            hosted_teams.insert(crate::admin::norm(&d.tenant).to_string());
        }
    }
    if hosted_teams.is_empty() {
        return;
    }
    for key in crate::guardian::keys().await {
        let Some(rest) = key.strip_prefix("wqueue/pending/") else { continue };
        let Some((team, _id)) = rest.split_once('/') else { continue };
        if !hosted_teams.contains(team) {
            continue;
        }
        if let Some(bytes) = crate::guardian::get(&key).await {
            if let Ok(job) = serde_json::from_slice::<QueueJob>(&bytes) {
                queue.adopt(job);
            }
        }
    }
}

/// The delivery loop: poll for due jobs THIS node currently holds, but only
/// actually dispatch the ones for a tenant this node is elected primary for
/// right now (checked per-job, per-tick -- cheap, and correct across a
/// mid-tick failover since the check is the last thing before the network
/// call). Non-primary due jobs are simply left in `pending` for the elected
/// node (which will `adopt` them from GuardianDB on its own next sync) to
/// pick up -- never dispatched twice by construction of the election being
/// deterministic over the same live membership view.
pub async fn run_delivery_loop(cloud: Arc<CloudState>, queue: Arc<WorldQueue>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
    let mut sync_tick: u32 = 0;
    loop {
        interval.tick().await;
        sync_tick += 1;
        if sync_tick % 25 == 0 {
            // ~5s cadence at the 200ms poll interval.
            sync_from_guardian(&cloud, &queue).await;
        }
        for job in queue.take_due() {
            if !is_primary_for_team(&cloud, &job.team).await {
                // Not ours to deliver right now -- leave it out of our local
                // pending set; the elected node re-adopts it from GuardianDB.
                continue;
            }
            let mut req = cloud.http.post(&job.target_url).json(&job.payload);
            for (k, v) in &job.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req = req.header("x-hive-queue-attempt", (job.attempt + 1).to_string());
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), req.send()).await;
            let ok = matches!(&outcome, Ok(Ok(resp)) if resp.status().is_success());
            if ok {
                queue.mark_delivered(&job.id);
                let key = guardian_key(&job.team, &job.id);
                tokio::spawn(async move { crate::guardian::delete(&key).await });
            } else if job.attempt + 1 >= job.max_attempts {
                tracing::warn!(job_id = %job.id, target = %job.target_url, attempts = job.attempt + 1, "world_queue: job dead-lettered");
                queue.mark_dead(&job.id);
                let key = guardian_key(&job.team, &job.id);
                tokio::spawn(async move { crate::guardian::delete(&key).await });
            } else {
                queue.reschedule(job);
            }
        }
    }
}
