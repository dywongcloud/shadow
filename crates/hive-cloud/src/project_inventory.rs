//! The tenant's project inventory: every `ProjectSettings` row it owns with deployment counts and
//! the newest build outcome, so a project whose first build failed (a row, no deployment) is listed
//! too.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::admin::{record_tenant, UNTAGGED_TENANT};
use crate::project_settings::{LastBuild, LastBuildState, ProjectSettings};
use crate::state::CloudState;

/// An admitted build with no record anywhere after this long is reported lost.
const STALE_BUILD_MS: u64 = 15 * 60_000;
/// Per-tenant ceiling on rows that never deployed (`HIVE_MAX_EMPTY_PROJECTS`).
const DEFAULT_MAX_EMPTY_PROJECTS: usize = 25;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct DeploymentFacts {
    count: usize,
    newest_ms: u64,
    production_alias: Option<String>,
}

/// One pass over this node's gateway plus the gossiped peer view; local untagged records fall back
/// to the row's team, peer records are trusted only on their own tag (as `dep_list`).
fn deployment_facts(c: &Arc<CloudState>, tenant: &str) -> HashMap<String, DeploymentFacts> {
    let mut facts: HashMap<String, DeploymentFacts> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut absorb =
        |project: &str, id: String, created_at_ms: u64, production: bool, alias: &str| {
            if !seen.insert(id) {
                return;
            }
            let entry = facts.entry(project.to_string()).or_default();
            entry.count += 1;
            entry.newest_ms = entry.newest_ms.max(created_at_ms);
            if production {
                entry.production_alias = Some(alias.to_string());
            }
        };
    for d in c.gw.list() {
        let own = record_tenant(&d.tenant);
        let effective = if own == UNTAGGED_TENANT {
            record_tenant(&c.projects.team_of(&d.project)).to_string()
        } else {
            own.to_string()
        };
        if effective == tenant {
            absorb(
                &d.project,
                d.id.to_string(),
                d.created_at_ms,
                d.production,
                &d.alias,
            );
        }
    }
    for d in c.peer_deployments.read().values().flatten() {
        if record_tenant(&d.tenant) == tenant {
            absorb(
                &d.project,
                d.id.to_string(),
                d.created_at_ms,
                d.production,
                &d.alias,
            );
        }
    }
    facts
}

fn repo_url_of(c: &Arc<CloudState>, project: &str, row: &ProjectSettings) -> String {
    row.last_build
        .as_ref()
        .map(|b| b.repo_url.clone())
        .filter(|url| !url.is_empty())
        .or_else(|| c.gw.git_for_project(project).map(|g| g.repo_url))
        .unwrap_or_default()
}

/// The newest outcome the tenant should see: the stored terminal stub, the live record of a newer
/// admitted build, or a lost build once `STALE_BUILD_MS` has passed.
fn build_state(c: &Arc<CloudState>, row: &ProjectSettings) -> (Option<LastBuild>, Option<Value>) {
    let stored = row.last_build.clone();
    let pointer = row.last_build_id.as_str();
    if pointer.is_empty() || stored.as_ref().is_some_and(|b| b.id == pointer) {
        return (stored, None);
    }
    let in_flight = |started_ms: u64| json!({ "id": pointer, "started_ms": started_ms });
    match c.builds.get(pointer) {
        Some(build) => match LastBuild::from_build(&build) {
            Some(done) => (Some(done), None),
            None => (stored, Some(in_flight(build.started_ms))),
        },
        None if now_ms().saturating_sub(row.last_build_started_ms) < STALE_BUILD_MS => {
            (stored, Some(in_flight(row.last_build_started_ms)))
        }
        None => (
            Some(LastBuild {
                id: pointer.to_string(),
                state: LastBuildState::Error,
                started_ms: row.last_build_started_ms,
                finished_ms: row.last_build_started_ms,
                repo_url: String::new(),
                branch: String::new(),
                error: "build record lost before it reported a result (node restarted or record evicted)".into(),
                deployment_id: None,
            }),
            None,
        ),
    }
}

pub fn summary(c: &Arc<CloudState>, tenant: &str) -> Vec<Value> {
    let facts = deployment_facts(c, tenant);
    let mut rows = c.projects.rows_for_tenant(tenant);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .map(|(name, row)| {
            let f = facts.get(&name);
            let (last_build, building) = build_state(c, &row);
            json!({
                "project": name,
                "team": row.team,
                "created_ms": row.incarnation_created_ms,
                "updated_ms": row.updated_ms,
                "framework": row.build.framework,
                "root_dir": row.build.root_dir,
                "repo_url": repo_url_of(c, &name, &row),
                "production_branch": row.production_branch,
                "deployment_count": f.map(|f| f.count).unwrap_or(0),
                "newest_deployment_ms": f.map(|f| f.newest_ms).unwrap_or(0),
                "production_alias": f.and_then(|f| f.production_alias.clone()),
                "first_deployed_ms": row.first_deployed_ms,
                "last_build": last_build,
                "building": building,
            })
        })
        .collect()
}

/// What a same-tenant import of an existing project name may do.
#[derive(Debug, PartialEq, Eq)]
pub enum NameReuse {
    /// The row never deployed and its last build pulled the same repository:
    /// the import is a retry of that project.
    ReuseEmpty,
    /// A build for the row is still running.
    InFlight { build_id: String },
    /// Taken for real: the ordinary name-collision answer applies.
    Taken,
}

pub fn name_reuse(c: &Arc<CloudState>, project: &str, tenant: &str, repo_url: &str) -> NameReuse {
    let Some(row) = c.projects.get_if_set(project) else {
        return NameReuse::Taken;
    };
    if record_tenant(row.team.trim()) != tenant || row.first_deployed_ms != 0 {
        return NameReuse::Taken;
    }
    if deployment_facts(c, tenant).contains_key(project) {
        return NameReuse::Taken;
    }
    if let Some(build) = c.builds.get(&row.last_build_id) {
        if LastBuild::from_build(&build).is_none() {
            return NameReuse::InFlight { build_id: build.id };
        }
    }
    let same_repo = row
        .last_build
        .as_ref()
        .map(|b| b.repo_url.as_str())
        .filter(|stored| !stored.is_empty())
        .is_some_and(|stored| {
            crate::gitops::norm_repo(stored) == crate::gitops::norm_repo(repo_url)
        });
    if same_repo && !repo_url.starts_with("upload://") && !repo_url.starts_with("image://") {
        NameReuse::ReuseEmpty
    } else {
        NameReuse::Taken
    }
}

pub fn max_empty_projects() -> usize {
    std::env::var("HIVE_MAX_EMPTY_PROJECTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_EMPTY_PROJECTS)
}

/// Rows this tenant created through a build that never produced a deployment.
pub fn empty_project_count(c: &Arc<CloudState>, tenant: &str) -> usize {
    let facts = deployment_facts(c, tenant);
    c.projects
        .rows_for_tenant(tenant)
        .iter()
        .filter(|(name, row)| {
            row.first_deployed_ms == 0 && !row.last_build_id.is_empty() && !facts.contains_key(name)
        })
        .count()
}
