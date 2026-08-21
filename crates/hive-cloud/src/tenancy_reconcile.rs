//! Project-tenancy REPAIR loop — the restore half of the disappearing-projects
//! fix.
//!
//! The prevention half (the scoped relocation reaper, the merge+tombstone
//! projects store, the sticky tenant stamp) stops NEW losses of the node-local
//! team tag, but a tag already destroyed before those landed stays destroyed:
//! the project's deployments keep serving while every tenant-scoped listing on
//! the affected node fail-closed-hides them — witnessed repeatedly as "the
//! Minecraft server vanished from my account" / "projects missing from the
//! thoth division". Reinserting rows by hand fixes one node once; this loop
//! makes the repair automatic, continuous, and fleet-wide.
//!
//! REPAIR-ONLY, by construction: it writes a team tag ONLY where the local row
//! is currently untagged, from two surviving sources of truth — never untags,
//! never overwrites an existing tag, never deletes anything (an index/settings
//! disagreement must never destroy canonical data; deleting because an index
//! disagrees is the exact anti-pattern that caused the losses).
//!
//! Sources, in precedence order:
//!  1. The relational `project_teams` replica (GuardianDB, replicated to every
//!     node) — written on every `set_team`, and the only source that still
//!     knows a project with ZERO live deployment records (a stopped game
//!     server between sessions).
//!  2. The newest TAGGED deployment record, local or gossiped
//!     (`peer_deployments`) — covers a project actively serving somewhere even
//!     if the relational replica is cold on this node.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::state::CloudState;

fn interval_secs() -> u64 {
    std::env::var("HIVE_TENANCY_RECONCILE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// One repair pass. Returns how many project rows were repaired.
pub async fn reconcile_once(cloud: &Arc<CloudState>) -> usize {
    // Desired (project -> team) from the two surviving sources. Records first,
    // then the relational replica OVERWRITES on collision — set_team writes it
    // on every explicit ownership change, so it carries operator intent, while
    // a record's tag is a stamp of whatever was believed at deploy time.
    let mut desired: HashMap<String, String> = HashMap::new();
    let tagged = |t: &str| crate::admin::record_tenant(t) != crate::admin::UNTAGGED_TENANT;

    let mut newest: HashMap<String, (u64, String)> = HashMap::new();
    for d in cloud.gw.list() {
        if tagged(&d.tenant) {
            let e = newest
                .entry(d.project.clone())
                .or_insert((0, String::new()));
            if d.created_at_ms >= e.0 {
                *e = (d.created_at_ms, d.tenant.clone());
            }
        }
    }
    for deps in cloud.peer_deployments.read().values() {
        for d in deps {
            if tagged(&d.tenant) {
                let e = newest
                    .entry(d.project.clone())
                    .or_insert((0, String::new()));
                if d.created_at_ms >= e.0 {
                    *e = (d.created_at_ms, d.tenant.clone());
                }
            }
        }
    }
    for (project, (_, team)) in newest {
        desired.insert(project, team);
    }
    for (project, team) in crate::relational::all_project_teams().await {
        if tagged(&team) {
            desired.insert(project, team);
        }
    }

    let mut repaired = 0usize;
    for (project, team) in desired {
        // A tombstoned project is DELETED, not untagged: the relational
        // `project_teams` row can outlive the delete (its `deleted_ms` write
        // rides guardian replication, which lags or fails behind the projects
        // store_sync that already carried the tombstone here). Repairing such
        // a project via the touch-stamping setter RESURRECTS it with
        // `updated_ms = tombstone+1` — a row the merge then keeps forever as a
        // post-delete "recreation" and re-gossips fleet-wide, which is exactly
        // how a deleted project reappeared in listings minutes after its
        // delete (witnessed live on `meinkampf`, 2026-08-21: rows re-stamped
        // 1-4 min after the tombstone on three nodes). Never repair what was
        // deliberately deleted; a genuine recreation returns with its own
        // tagged row and never reaches this guard.
        if cloud.projects.tombstone_of(&project).is_some() {
            continue;
        }
        let own = cloud.projects.team_of(&project);
        if tagged(&own) {
            continue; // already tagged — NEVER overwritten, even on disagreement
        }
        cloud.projects.set_team(&project, &team);
        repaired += 1;
        tracing::warn!(
            project,
            team,
            "tenancy reconcile: repaired a missing project→team tag (the project was \
             invisible in that tenant's listings on this node)"
        );
    }
    if repaired > 0 {
        crate::persist::persist(cloud);
    }
    repaired
}

/// Every node, periodic. First pass runs ~60s after boot so gossip and the
/// GuardianDB replica have warmed; then every `HIVE_TENANCY_RECONCILE_SECS`
/// (default 300).
pub fn spawn(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            let n = reconcile_once(&cloud).await;
            if n > 0 {
                tracing::info!(repaired = n, "tenancy reconcile: pass complete");
            }
            tokio::time::sleep(Duration::from_secs(interval_secs())).await;
        }
    });
}
