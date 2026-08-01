//! GitOps — the link between a tenant (team/personal account) and a GitHub repo
//! that holds the declarative OpenEdge config (`openedge.yaml`).
//!
//! Two halves:
//! * **Outbound** — the dashboard generates the org's config as YAML and commits
//!   it to the linked repo via Composio. This store only persists *where* to push
//!   (repo/branch/path) plus the last-sync metadata (commit + content hash) so a
//!   re-sync can skip a redundant commit when nothing changed.
//! * **Inbound** — a GitHub push/PR webhook (see `admin::git_webhook`) matches the
//!   pushed repo against existing projects and triggers a fresh build+deploy. This
//!   matching is accelerated by [`GitRepoIndex`], an in-memory reverse index
//!   (normalized repo -> project names) so a webhook delivery is an O(1) lookup
//!   instead of an O(projects) fleet-wide scan.
//!
//! The store is in-memory + persisted via the platform snapshot, keyed by tenant.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

fn default_branch() -> String {
    "main".into()
}
fn default_path() -> String {
    "openedge.yaml".into()
}

/// A tenant's link to its GitOps config repo.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitOpsLink {
    /// Tenant (team slug / "personal").
    pub tenant: String,
    /// `owner/repo` the config is committed to.
    #[serde(default)]
    pub repo: String,
    /// Branch the config lives on.
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Path of the config file in the repo.
    #[serde(default = "default_path")]
    pub path: String,
    /// "personal" | "org" — how the user scoped the GitHub connection.
    #[serde(default)]
    pub scope: String,
    /// Whether GitHub is connected for this tenant (set by the dashboard once
    /// Composio reports an active connection).
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub connected_ms: u64,
    #[serde(default)]
    pub last_sync_ms: u64,
    /// Short SHA of the last config commit (for display).
    #[serde(default)]
    pub last_commit: String,
    /// Content hash of the last-pushed YAML (so an identical re-sync is a no-op).
    #[serde(default)]
    pub last_hash: String,
}

impl GitOpsLink {
    fn empty(tenant: &str) -> GitOpsLink {
        GitOpsLink {
            tenant: tenant.to_string(),
            repo: String::new(),
            branch: default_branch(),
            path: default_path(),
            scope: String::new(),
            connected: false,
            connected_ms: 0,
            last_sync_ms: 0,
            last_commit: String::new(),
            last_hash: String::new(),
        }
    }
}

pub struct GitOpsStore {
    map: RwLock<HashMap<String, GitOpsLink>>,
}

impl GitOpsStore {
    pub fn new() -> GitOpsStore {
        GitOpsStore {
            map: RwLock::new(HashMap::new()),
        }
    }

    /// The link for a tenant (a disconnected default if none exists yet).
    pub fn get(&self, tenant: &str) -> GitOpsLink {
        self.map
            .read()
            .get(tenant)
            .cloned()
            .unwrap_or_else(|| GitOpsLink::empty(tenant))
    }

    /// Connect/point a tenant at a config repo.
    pub fn set_link(
        &self,
        tenant: &str,
        repo: &str,
        branch: &str,
        path: &str,
        scope: &str,
    ) -> GitOpsLink {
        let mut m = self.map.write();
        let link = m
            .entry(tenant.to_string())
            .or_insert_with(|| GitOpsLink::empty(tenant));
        link.repo = repo.trim().to_string();
        if !branch.trim().is_empty() {
            link.branch = branch.trim().to_string();
        }
        if !path.trim().is_empty() {
            link.path = path.trim().to_string();
        }
        if !scope.trim().is_empty() {
            link.scope = scope.trim().to_string();
        }
        link.connected = true;
        if link.connected_ms == 0 {
            link.connected_ms = now_ms();
        }
        link.clone()
    }

    /// Record the result of a successful config push.
    pub fn record_sync(&self, tenant: &str, commit: &str, hash: &str) -> GitOpsLink {
        let mut m = self.map.write();
        let link = m
            .entry(tenant.to_string())
            .or_insert_with(|| GitOpsLink::empty(tenant));
        link.last_sync_ms = now_ms();
        link.last_commit = commit.to_string();
        link.last_hash = hash.to_string();
        link.clone()
    }

    pub fn unlink(&self, tenant: &str) {
        self.map.write().remove(tenant);
    }

    pub fn snapshot(&self) -> Vec<GitOpsLink> {
        self.map.read().values().cloned().collect()
    }

    pub fn load(&self, data: Vec<GitOpsLink>) {
        let mut m = self.map.write();
        for link in data {
            m.insert(link.tenant.clone(), link);
        }
    }
}

impl Default for GitOpsStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a git URL/`owner/repo` to a canonical `owner/repo` (lowercased) so
/// an inbound webhook can match a push against a project's stored clone URL.
pub fn norm_repo(url: &str) -> String {
    let s = url.trim().to_lowercase();
    let s = s.strip_prefix("git@github.com:").unwrap_or(&s);
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .or_else(|| s.strip_prefix("ssh://"))
        .unwrap_or(s);
    let s = s.strip_prefix("github.com/").unwrap_or(s);
    let s = s.strip_suffix(".git").unwrap_or(s);
    // Drop any leftover host (e.g. "github.com/" not stripped above) and creds.
    let s = s.rsplit('@').next().unwrap_or(s);
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        let n = parts.len();
        format!("{}/{}", parts[n - 2], parts[n - 1])
    } else {
        s.to_string()
    }
}

/// Reverse index for GitHub-webhook repo -> project matching (see
/// `admin::git_webhook`): normalized repo full_name -> the project names
/// currently connected to it. Turns the webhook's per-delivery project lookup
/// from an O(projects) fleet-wide scan into an O(1) lookup for the common case.
///
/// **In-memory only, deliberately not persisted.** It is a pure derived cache
/// over each project's current real-git source — the SAME source
/// `admin::git_for_project_fleet` already derives from deployment history (this
/// node's local gateway, falling back to gossiped fleet deployments) — so
/// persisting it would create a second, independently-driftable source of
/// truth for something already durable via the deployment/project stores.
/// Rebuilt from scratch at boot (`rebuild`) and kept incrementally in sync
/// afterward at every point a project's git connection changes: git-import at
/// project creation and project deletion (both in `admin.rs`), a future
/// explicit connect/reconnect settings action (also `admin.rs` — see
/// `set_project_repo`'s doc), and freshly-gossiped fleet deployments arriving
/// via the anti-entropy loop (`main.rs`, so a project first deployed via a
/// DIFFERENT node than the one that later receives a webhook delivery — the
/// `webhook.<platform_domain>` DNS root round-robins across every gateway
/// node — still gets indexed here without waiting for its OWN next deploy).
///
/// A `git_webhook` caller must treat an index that has NEVER been populated at
/// all (`is_empty()`, e.g. a fresh boot before the rebuild pass ran, or a bug)
/// as "uninitialized" and fall back to the original full fleet scan
/// defensively — that's distinct from "populated, but this particular repo
/// genuinely has zero connected projects" (an empty `Vec` from `projects_for`).
pub struct GitRepoIndex {
    /// normalized repo full_name -> project names currently connected to it.
    by_repo: RwLock<HashMap<String, HashSet<String>>>,
    /// project -> its current normalized repo, so `set_project_repo` can evict
    /// the OLD repo entry on a reconnect without the caller having to track
    /// (or separately look up) what the previous repo was.
    by_project: RwLock<HashMap<String, String>>,
}

impl GitRepoIndex {
    pub fn new() -> GitRepoIndex {
        GitRepoIndex {
            by_repo: RwLock::new(HashMap::new()),
            by_project: RwLock::new(HashMap::new()),
        }
    }

    /// Record/update `project`'s git connection to `repo_url` (any URL form —
    /// normalized internally, same as `norm_repo`). Evicts the project's
    /// previous repo entry first, so a reconnect to a different repo never
    /// leaves a stale/duplicate reverse-mapping behind.
    ///
    /// A no-op for an empty, or synthetic non-git (`upload://`/`image://`,
    /// see `fluid_core::GitSource::is_real_git`), `repo_url` — a zip-upload or
    /// prebuilt-image deploy for an already-git-connected project must NOT
    /// evict that project's real repo entry, mirroring `git_for_project_fleet`
    /// / `Gateway::git_for_project`'s own real-git-only filtering exactly (a
    /// caller passing a pseudo-source here is almost always a redeploy of an
    /// existing git-connected project via a different source, not a genuine
    /// disconnect).
    pub fn set_project_repo(&self, project: &str, repo_url: &str) {
        let repo_url = repo_url.trim();
        if repo_url.is_empty()
            || repo_url.starts_with("upload://")
            || repo_url.starts_with("image://")
        {
            return;
        }
        let norm = norm_repo(repo_url);
        if norm.is_empty() {
            return;
        }
        let mut by_project = self.by_project.write();
        if by_project.get(project).is_some_and(|prev| prev == &norm) {
            return; // already correctly indexed
        }
        let mut by_repo = self.by_repo.write();
        if let Some(prev) = by_project.insert(project.to_string(), norm.clone()) {
            if let Some(set) = by_repo.get_mut(&prev) {
                set.remove(project);
                if set.is_empty() {
                    by_repo.remove(&prev);
                }
            }
        }
        by_repo.entry(norm).or_default().insert(project.to_string());
    }

    /// Forget a project entirely (deletion) — evicts it from its repo's set too.
    pub fn remove_project(&self, project: &str) {
        let mut by_project = self.by_project.write();
        if let Some(prev) = by_project.remove(project) {
            let mut by_repo = self.by_repo.write();
            if let Some(set) = by_repo.get_mut(&prev) {
                set.remove(project);
                if set.is_empty() {
                    by_repo.remove(&prev);
                }
            }
        }
    }

    /// Projects currently connected to `repo_url` (any URL form — normalized
    /// internally). Empty when the repo has no connected projects; see the
    /// type doc for why that's distinct from "index uninitialized."
    pub fn projects_for(&self, repo_url: &str) -> Vec<String> {
        let norm = norm_repo(repo_url);
        self.by_repo
            .read()
            .get(&norm)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// True when the index holds no rows at all — the defensive "never
    /// populated" signal `git_webhook` uses to fall back to a full scan.
    pub fn is_empty(&self) -> bool {
        self.by_repo.read().is_empty()
    }

    /// Rebuild the whole index from scratch (boot-time). `entries` yields each
    /// project's current real-git repo URL — the caller resolves this
    /// fleet-aware (`admin::git_for_project_fleet`), since doing so needs
    /// `CloudState`, which this module deliberately doesn't depend on. Entries
    /// with an empty/pseudo-source `repo_url` are skipped, same as
    /// `set_project_repo`.
    pub fn rebuild<I: IntoIterator<Item = (String, String)>>(&self, entries: I) {
        let mut by_repo: HashMap<String, HashSet<String>> = HashMap::new();
        let mut by_project: HashMap<String, String> = HashMap::new();
        for (project, repo_url) in entries {
            let repo_url = repo_url.trim();
            if repo_url.is_empty()
                || repo_url.starts_with("upload://")
                || repo_url.starts_with("image://")
            {
                continue;
            }
            let norm = norm_repo(repo_url);
            if norm.is_empty() {
                continue;
            }
            by_repo
                .entry(norm.clone())
                .or_default()
                .insert(project.clone());
            by_project.insert(project, norm);
        }
        *self.by_repo.write() = by_repo;
        *self.by_project.write() = by_project;
    }
}

impl Default for GitRepoIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_repo_canonicalizes_all_url_forms() {
        let want = "owner/repo";
        for input in [
            "https://github.com/owner/repo",
            "https://github.com/owner/repo.git",
            "http://github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
            "github.com/owner/repo",
            "owner/repo",
            "https://x-access-token:TOKEN@github.com/owner/repo.git",
        ] {
            assert_eq!(norm_repo(input), want, "failed for {input}");
        }
    }

    #[test]
    fn norm_repo_is_case_insensitive_and_trims() {
        assert_eq!(
            norm_repo("  HTTPS://GitHub.com/Owner/Repo.git "),
            "owner/repo"
        );
    }

    #[test]
    fn gitops_store_link_and_sync_roundtrip() {
        let s = GitOpsStore::new();
        // Disconnected default before linking.
        let empty = s.get("acme");
        assert!(!empty.connected);
        assert_eq!(empty.branch, "main");
        assert_eq!(empty.path, "openedge.yaml");

        let linked = s.set_link("acme", "acme/config", "trunk", "org.yaml", "org");
        assert!(linked.connected);
        assert_eq!(linked.repo, "acme/config");
        assert_eq!(linked.branch, "trunk");
        assert_eq!(linked.path, "org.yaml");

        let synced = s.record_sync("acme", "abc123", "deadbeef");
        assert_eq!(synced.last_commit, "abc123");
        assert_eq!(synced.last_hash, "deadbeef");
        assert!(synced.last_sync_ms > 0);

        // Persistence roundtrip.
        let snap = s.snapshot();
        let s2 = GitOpsStore::new();
        s2.load(snap);
        assert_eq!(s2.get("acme").repo, "acme/config");

        s.unlink("acme");
        assert!(!s.get("acme").connected);
    }
}
