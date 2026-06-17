//! The unit of work that flows through Hive: a build job.

use crate::ids::JobId;
use crate::time::now_ms;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Compute requested for a cell. Mirrors what Hive guarantees per cell:
/// dedicated CPU + memory, with disk/network rate-limited by the box.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceSpec {
    pub vcpus: u32,
    pub mem_mib: u32,
    pub disk_mib: u32,
    /// Hard wall-clock limit for the build before it is killed.
    pub timeout_secs: u64,
}

impl Default for ResourceSpec {
    fn default() -> Self {
        // A sensible "default cell" — small but real.
        ResourceSpec {
            vcpus: 2,
            mem_mib: 2048,
            disk_mib: 8192,
            timeout_secs: 900,
        }
    }
}

impl ResourceSpec {
    /// Two specs are placement-compatible if a warm cell built for `self`
    /// can satisfy a job asking for `req`.
    pub fn satisfies(&self, req: &ResourceSpec) -> bool {
        self.vcpus >= req.vcpus
            && self.mem_mib >= req.mem_mib
            && self.disk_mib >= req.disk_mib
    }
}

/// A build job submitted to a Hive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildJob {
    pub id: JobId,
    /// Logical name of the base image / rootfs the cell should boot with.
    /// Warm pools are keyed by this so we can hand out a pre-loaded cell.
    pub image: String,
    /// Git URL or local path to fetch. Empty = no fetch (commands only).
    pub repo: String,
    /// Branch, tag, or commit SHA.
    pub git_ref: String,
    /// Build steps, run sequentially in the cell's work dir.
    pub commands: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub resources: ResourceSpec,
    /// Build-cache key (e.g. a hash of the lockfile). When set, the cache_paths
    /// are restored before the build and saved after a successful build — the
    /// "npm install is nearly instant when package.json is unchanged" trick.
    #[serde(default)]
    pub cache_key: Option<String>,
    /// Directories (relative to the work dir) to cache, e.g. ["node_modules"].
    #[serde(default)]
    pub cache_paths: Vec<String>,
    pub submitted_at_ms: u64,
}

impl BuildJob {
    pub fn builder(image: impl Into<String>) -> BuildJobBuilder {
        BuildJobBuilder {
            image: image.into(),
            repo: String::new(),
            git_ref: "HEAD".to_string(),
            commands: Vec::new(),
            env: BTreeMap::new(),
            resources: ResourceSpec::default(),
            cache_key: None,
            cache_paths: Vec::new(),
        }
    }
}

pub struct BuildJobBuilder {
    image: String,
    repo: String,
    git_ref: String,
    commands: Vec<String>,
    env: BTreeMap<String, String>,
    resources: ResourceSpec,
    cache_key: Option<String>,
    cache_paths: Vec<String>,
}

impl BuildJobBuilder {
    pub fn repo(mut self, repo: impl Into<String>, git_ref: impl Into<String>) -> Self {
        self.repo = repo.into();
        self.git_ref = git_ref.into();
        self
    }
    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        self.commands.push(cmd.into());
        self
    }
    pub fn commands<I: IntoIterator<Item = String>>(mut self, cmds: I) -> Self {
        self.commands.extend(cmds);
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    pub fn resources(mut self, r: ResourceSpec) -> Self {
        self.resources = r;
        self
    }
    /// Enable build caching: restore/save `paths` keyed by `key`.
    pub fn cache(mut self, key: impl Into<String>, paths: Vec<String>) -> Self {
        self.cache_key = Some(key.into());
        self.cache_paths = paths;
        self
    }
    pub fn build(self) -> BuildJob {
        BuildJob {
            id: JobId::new(),
            image: self.image,
            repo: self.repo,
            git_ref: self.git_ref,
            commands: self.commands,
            env: self.env,
            resources: self.resources,
            cache_key: self.cache_key,
            cache_paths: self.cache_paths,
            submitted_at_ms: now_ms(),
        }
    }
}
