//! GitOps — the link between a tenant (team/personal account) and a GitHub repo
//! that holds the declarative OpenEdge config (`openedge.yaml`).
//!
//! Two halves:
//! * **Outbound** — the dashboard generates the org's config as YAML and commits
//!   it to the linked repo via Composio. This store only persists *where* to push
//!   (repo/branch/path) plus the last-sync metadata (commit + content hash) so a
//!   re-sync can skip a redundant commit when nothing changed.
//! * **Inbound** — a GitHub push/PR webhook (see `admin::git_webhook`) matches the
//!   pushed repo against existing projects and triggers a fresh build+deploy.
//!
//! The store is in-memory + persisted via the platform snapshot, keyed by tenant.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
        GitOpsStore { map: RwLock::new(HashMap::new()) }
    }

    /// The link for a tenant (a disconnected default if none exists yet).
    pub fn get(&self, tenant: &str) -> GitOpsLink {
        self.map.read().get(tenant).cloned().unwrap_or_else(|| GitOpsLink::empty(tenant))
    }

    /// Connect/point a tenant at a config repo.
    pub fn set_link(&self, tenant: &str, repo: &str, branch: &str, path: &str, scope: &str) -> GitOpsLink {
        let mut m = self.map.write();
        let link = m.entry(tenant.to_string()).or_insert_with(|| GitOpsLink::empty(tenant));
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
        let link = m.entry(tenant.to_string()).or_insert_with(|| GitOpsLink::empty(tenant));
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
        assert_eq!(norm_repo("  HTTPS://GitHub.com/Owner/Repo.git "), "owner/repo");
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
