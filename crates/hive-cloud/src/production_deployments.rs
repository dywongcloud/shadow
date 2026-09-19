//! Durable, fleet-replicated record of which node hosts each project's
//! current PRODUCTION deployment — the missing piece for node-death self-heal.
//!
//! `gw.deployment_records()` (node-local) and `peer_deployments` (a gossip
//! CACHE, see `state.rs`) both describe "what's deployed where" today, but
//! neither survives the deploying node's death: `merge_deployments_ttl`
//! (main.rs) drops a peer's entire deployment list the instant that peer ages
//! out of `registry.nodes()` — the SAME 30s no-gossip window that marks it
//! dead. So the one moment this platform most needs to know "project X's
//! source was `repo@commit`, last hosted on node Y" is exactly the moment
//! that fact disappears. This store exists to survive past that point.
//!
//! Shape mirrors `project_settings::SyncedProjects` (the `store_sync.rs`
//! `"projects"` entry): a `BTreeMap` keyed by project, `updated_ms`
//! newest-wins merge, permanent tombstones so an absent row replicates as an
//! explicit deletion rather than "this peer hasn't synced yet". Deliberately
//! narrower than `DeployRecord`/`ProjectSettings` — only the fields a
//! from-scratch rebuild via `git::start_build` actually needs (see
//! `admin::redeploy_request`, the human "Redeploy" button's own request
//! shape, which this record's fields are chosen to reconstruct).

use hive_core::now_ms;
use parking_lot::RwLock;
use std::collections::BTreeMap;

/// One project's last-known production deployment, durable across the host
/// node's death.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProductionDeploymentRecord {
    pub project: String,
    pub git: fluid_core::GitSource,
    /// The node this deployment was actually running on as of `updated_ms`.
    /// Read, never trusted blindly — the reconciler re-checks THIS node's
    /// live health via `registry.nodes()` before treating the record as
    /// needing relocation, since the record itself only proves "true then".
    pub host_node: String,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncedProductionDeployments {
    pub rows: BTreeMap<String, ProductionDeploymentRecord>,
    #[serde(default)]
    pub tombstones: BTreeMap<String, u64>,
}

/// A generation timestamp further in the future than plausible clock skew
/// explains is a poisoned write, not a real future fact — same discipline as
/// `project_settings::project_time_ceiling`.
const MAX_FUTURE_SKEW_MS: u64 = 60_000;

fn time_ceiling(now: u64) -> u64 {
    now.saturating_add(MAX_FUTURE_SKEW_MS)
}

pub struct ProductionDeploymentStore {
    map: RwLock<BTreeMap<String, ProductionDeploymentRecord>>,
    tombstones: RwLock<BTreeMap<String, u64>>,
}

impl ProductionDeploymentStore {
    pub fn new() -> Self {
        Self {
            map: RwLock::new(BTreeMap::new()),
            tombstones: RwLock::new(BTreeMap::new()),
        }
    }

    /// Record (or update) `project`'s current production host + source. Called
    /// wherever a production build actually lands — see `git::run_build`'s
    /// post-success path. A re-record after a tombstone revives the row (the
    /// project was legitimately redeployed after being deleted-and-recreated),
    /// matching `ProjectStore`'s own tombstone-vs-recreation precedent.
    pub fn record(&self, project: &str, git: fluid_core::GitSource, host_node: &str) {
        let now = now_ms();
        self.map.write().insert(
            project.to_string(),
            ProductionDeploymentRecord {
                project: project.to_string(),
                git,
                host_node: host_node.to_string(),
                updated_ms: now,
            },
        );
    }

    /// Explicit removal (project deleted). Permanent tombstone — same
    /// reasoning as `ProjectStore::tombstones`: an offline node returning
    /// after any bounded window must not resurrect a deleted project's last
    /// known host.
    pub fn remove(&self, project: &str) {
        let now = now_ms();
        self.map.write().remove(project);
        let mut tombs = self.tombstones.write();
        let e = tombs.entry(project.to_string()).or_insert(now);
        if now > *e {
            *e = now;
        }
    }

    pub fn get(&self, project: &str) -> Option<ProductionDeploymentRecord> {
        self.map.read().get(project).cloned()
    }

    pub fn snapshot(&self) -> Vec<ProductionDeploymentRecord> {
        self.map.read().values().cloned().collect()
    }

    pub fn snapshot_synced(&self) -> SyncedProductionDeployments {
        SyncedProductionDeployments {
            rows: self.map.read().clone(),
            tombstones: self.tombstones.read().clone(),
        }
    }

    /// Merge a peer's snapshot. NEWEST-`updated_ms`-per-row wins; a tombstone
    /// at-or-after a row's last write drops it; a row re-created after its own
    /// tombstone survives. Never a wholesale replace — a sender simply
    /// missing a row can never erase it here (the `projects`/`teams`
    /// precedent this mirrors).
    pub fn merge_synced(&self, remote: SyncedProductionDeployments) -> usize {
        let now = now_ms();
        let ceiling = time_ceiling(now);
        {
            let mut tombs = self.tombstones.write();
            for (project, ms) in remote.tombstones {
                if ms > ceiling {
                    tracing::warn!(project = %project, deleted_ms = ms, ceiling_ms = ceiling, "dropping relayed production-deployment tombstone with implausibly future generation");
                    continue;
                }
                let e = tombs.entry(project).or_insert(ms);
                if ms > *e {
                    *e = ms;
                }
            }
        }
        let tombs = self.tombstones.read().clone();
        let mut map = self.map.write();
        for row in map.values_mut() {
            if row.updated_ms > ceiling {
                tracing::warn!(project = %row.project, updated_ms = row.updated_ms, ceiling_ms = ceiling, "normalizing implausibly future local production-deployment row");
                row.updated_ms = now;
            }
        }
        let mut adopted = 0usize;
        for (project, mut remote_row) in remote.rows {
            if remote_row.updated_ms > ceiling {
                tracing::warn!(project = %project, updated_ms = remote_row.updated_ms, ceiling_ms = ceiling, "dropping relayed production-deployment row with implausibly future generation");
                continue;
            }
            if let Some(&deleted_ms) = tombs.get(&project) {
                if deleted_ms >= remote_row.updated_ms {
                    map.remove(&project);
                    continue;
                }
            }
            match map.get(&project) {
                Some(local) if local.updated_ms >= remote_row.updated_ms => {}
                _ => {
                    remote_row.project = project.clone();
                    map.insert(project, remote_row);
                    adopted += 1;
                }
            }
        }
        // Apply tombstones to any locally-held row they cover (a tombstone
        // that arrived before its row would otherwise leave a deleted
        // project's row stranded until the next sync tick).
        for (project, &deleted_ms) in tombs.iter() {
            if let Some(local) = map.get(project) {
                if deleted_ms >= local.updated_ms {
                    map.remove(project);
                }
            }
        }
        adopted
    }

    /// Load from a durable snapshot at boot (persist.rs), replacing local
    /// state wholesale — this is the ONE path where replace-not-merge is
    /// correct, since it runs before any peer sync could race it.
    pub fn load(&self, rows: Vec<ProductionDeploymentRecord>, tombstones: BTreeMap<String, u64>) {
        *self.map.write() = rows.into_iter().map(|r| (r.project.clone(), r)).collect();
        *self.tombstones.write() = tombstones;
    }
}

impl Default for ProductionDeploymentStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Node-death reconciler: detect a project's host going unhealthy, redeploy it
// elsewhere via the SAME mechanism the dashboard's own "Redeploy" button uses
// (`git::start_build`), then leave an incident recording what happened.
// ---------------------------------------------------------------------------

use std::sync::Arc;

/// A demoted node must stay demoted for this long, with no re-record from it
/// (a re-record means the node is back and rebuilt — see `record`'s call
/// site in `git::run_build`), before a row is treated as needing relocation.
/// Matches the reasoning documented fleet-wide for owner-chain/DNS
/// engage-disengage damping (AGENTS.md: a flapping node otherwise drives
/// wasted rebuilds on every reconvergence) — a transport blip or a brief
/// restart must not trigger a full redeploy-elsewhere.
const RELOCATION_GRACE_MS: u64 = 5 * 60 * 1000;

/// How often the reconciler sweeps `production_deployments` for rows whose
/// host has been down past the grace period. Cheap per tick (no network I/O
/// unless something actually needs relocating), so a short interval costs
/// little and keeps the detect-to-heal latency close to the grace period
/// itself rather than grace-period-plus-a-long-poll.
const RECONCILE_TICK_SECS: u64 = 30;

pub fn spawn_node_death_reconcile(cloud: Arc<crate::state::CloudState>) {
    crate::supervise::spawn_supervised("production-deployment-node-death-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            // Let boot-time gossip/store_sync settle before the first sweep —
            // otherwise a freshly-booted leader would see every OTHER node as
            // "not yet in my registry" and treat their live deployments as
            // needing relocation. Mirrors `spawn_git_poll_reconcile`'s own
            // 45s settle sleep.
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(RECONCILE_TICK_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                crate::supervise::beat("production-deployment-node-death-reconcile");
                // LEADER ONLY — exactly one node redeploys a given dead host's
                // projects, mirroring `git_poll_cycle`'s own gate. Without
                // this, every node in the fleet would independently detect
                // the same dead host and race to redeploy the same project
                // N times.
                if !cloud.is_control_plane_leader() {
                    continue;
                }
                reconcile_once(&cloud).await;
            }
        }
    });
}

async fn reconcile_once(cloud: &Arc<crate::state::CloudState>) {
    let now = now_ms();
    let rows = cloud.production_deployments.snapshot();
    if rows.is_empty() {
        return;
    }
    let live_nodes = cloud.registry.nodes();
    for row in rows {
        // Re-resolve the host's CURRENT health on every sweep — never cache
        // across ticks, since a node's health is exactly the kind of fact
        // that legitimately flips back to true (AGENTS.md's per-observer
        // health section: this leader's own registry is the one view that
        // actually drives placement/DNS, so it is the correct one to key
        // off here).
        let host_healthy = live_nodes
            .iter()
            .find(|n| n.name == row.host_node)
            .map(|n| n.healthy)
            // Absent from the registry entirely means the same thing
            // `admin::nodes`'s own doc says it means: no gossip for 30s,
            // i.e. offline — treat identically to an explicit `healthy: false`.
            .unwrap_or(false);
        if host_healthy {
            continue;
        }
        // Grace period: the row itself is only re-written by a SUCCESSFUL
        // build landing on a node (see `record`'s call site) — so
        // `updated_ms` doubles as "last time we know this project was
        // genuinely served from `host_node`". A host that died 4 minutes ago
        // does not yet warrant relocating; one dead 6+ minutes does.
        if now.saturating_sub(row.updated_ms) < RELOCATION_GRACE_MS {
            continue;
        }
        relocate_one(cloud, row).await;
    }
}

async fn relocate_one(cloud: &Arc<crate::state::CloudState>, row: ProductionDeploymentRecord) {
    let project = row.project.clone();
    // A concurrent build already in flight for this project (e.g. a genuine
    // push landed around the same time the host died) means relocation is
    // redundant — whichever finishes first will re-record a fresh row via
    // `record`'s normal call site, and starting a second build here would
    // just race it.
    let already_building = cloud.builds.list().iter().any(|b| {
        b.project == project
            && matches!(
                b.state,
                fluid_core::DeployState::Queued | fluid_core::DeployState::Building
            )
    });
    if already_building {
        return;
    }

    let incident = cloud.incidents.open(crate::incidents::OpenReq {
        title: format!("Redeploying '{project}' — its host node went offline"),
        severity: crate::incidents::Severity::Minor,
        affected: vec![row.host_node.clone()],
        message: format!(
            "Node '{}' has been unreachable for over {} minutes with '{project}' still recorded \
             as hosted there. Automatically redeploying commit {} of {} to a healthy node.",
            row.host_node,
            RELOCATION_GRACE_MS / 60_000,
            short_commit(&row.git.commit),
            row.git.repo_url,
        ),
    });

    let req = fluid_core::GitDeployRequest {
        repo_url: row.git.repo_url.clone(),
        branch: Some(row.git.branch.clone()).filter(|b| !b.is_empty()),
        // Pin the EXACT last-known-good commit — this is a recovery action,
        // not a chance to pick up whatever the branch has advanced to since.
        // A genuine new push is handled by the ordinary git-poll/webhook
        // path, which will supersede this build the normal way if one lands.
        commit: Some(row.git.commit.clone()),
        head_repo_url: None,
        project: Some(project.clone()),
        project_incarnation: None,
        creator: Some("node-death-self-heal".into()),
        production: true,
        target: Some("production".into()),
        use_cache: true,
        root_dir: None,
        env: None,
        no_fanout: false,
        fanout_secondary: false,
        build_config: None,
        function_settings: None,
        redeploy: false,
        zip_b64: None,
        image_ref: None,
        image_port: None,
        image_protocol: None,
        image_memory: None,
        image_cpus: None,
        image_pids: None,
        image_ports: None,
        git_token: None,
        // This restores a known-good Git source after its host died; it does
        // not reapply a Marketplace placement-policy snapshot.
        marketplace_placement: None,
        source_deployment_ids: Vec::new(),
    };

    let outcome = crate::git::start_build(cloud.clone(), req, None, None).await;
    match outcome {
        Ok(build_id) => {
            tracing::warn!(
                project = %project,
                dead_host = %row.host_node,
                build_id = %build_id,
                "node-death self-heal: redeploy started"
            );
            cloud.incidents.update(
                &incident.id,
                crate::incidents::UpdateReq {
                    status: crate::incidents::IncidentStatus::Monitoring,
                    message: format!(
                        "Redeploy started as build {build_id}. This incident resolves once the \
                         new deployment reports ready; see the build log for progress."
                    ),
                },
            );
            monitor_relocation(cloud.clone(), incident.id, project, build_id);
        }
        Err(error) => {
            tracing::error!(
                project = %project,
                dead_host = %row.host_node,
                error = %error,
                "node-death self-heal: redeploy FAILED to start"
            );
            cloud.incidents.update(
                &incident.id,
                crate::incidents::UpdateReq {
                    status: crate::incidents::IncidentStatus::Identified,
                    message: format!(
                        "Automatic redeploy could not even start: {error}. '{project}' remains \
                         recorded as hosted on the offline node '{}' — manual intervention is \
                         needed (redeploy from the dashboard, or check placement capacity).",
                        row.host_node
                    ),
                },
            );
        }
    }
}

/// Watch the started build to a terminal state and resolve (or re-flag) the
/// incident opened for it — bounded, so a build that never terminates for
/// some unrelated reason cannot leave this task running forever.
fn monitor_relocation(
    cloud: Arc<crate::state::CloudState>,
    incident_id: String,
    project: String,
    build_id: String,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
        let mut poll = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            poll.tick().await;
            if tokio::time::Instant::now() >= deadline {
                cloud.incidents.update(
                    &incident_id,
                    crate::incidents::UpdateReq {
                        status: crate::incidents::IncidentStatus::Identified,
                        message: format!(
                            "Redeploy of '{project}' (build {build_id}) did not reach a terminal \
                             state within 30 minutes — check the build log directly."
                        ),
                    },
                );
                return;
            }
            let Some(build) = cloud.builds.get(&build_id) else {
                // The build record itself is gone (e.g. superseded and
                // pruned) — nothing left to watch; leave the incident as-is
                // for an operator to notice rather than guessing an outcome.
                return;
            };
            match build.state {
                fluid_core::DeployState::Ready => {
                    cloud.incidents.update(
                        &incident_id,
                        crate::incidents::UpdateReq {
                            status: crate::incidents::IncidentStatus::Resolved,
                            message: format!(
                                "'{project}' is back up — build {build_id} completed and is now serving."
                            ),
                        },
                    );
                    return;
                }
                fluid_core::DeployState::Error | fluid_core::DeployState::Cancelled => {
                    cloud.incidents.update(
                        &incident_id,
                        crate::incidents::UpdateReq {
                            status: crate::incidents::IncidentStatus::Identified,
                            message: format!(
                                "Automatic redeploy of '{project}' (build {build_id}) did not \
                                 succeed — check the build log. Manual redeploy may be needed."
                            ),
                        },
                    );
                    return;
                }
                fluid_core::DeployState::Queued | fluid_core::DeployState::Building => {}
            }
        }
    });
}

fn short_commit(commit: &str) -> &str {
    commit.get(0..8).unwrap_or(commit)
}
