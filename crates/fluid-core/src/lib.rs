//! `fluid-core` — the deployment model for the serving layer.
//!
//! Where Hive builds code, Fluid *serves* it. A [`Deployment`] is the unit a
//! user ships: some static assets plus zero or more [`FunctionConfig`]s, with
//! [`Route`]s mapping request paths to either static files or a function.
//!
//! The function config carries the knobs that make compute "Fluid": an
//! in-function `max_concurrency` (many requests per instance), `min_instances`
//! (keep-warm), `max_instances` (autoscale ceiling), and an `idle_ttl` for
//! scale-to-zero.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeploymentId(pub String);

impl DeploymentId {
    pub fn new() -> Self {
        DeploymentId(format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for DeploymentId {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::fmt::Debug for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A serverless function within a deployment. The server process must listen on
/// `$PORT` and speak HTTP/1.1.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionConfig {
    pub name: String,
    /// Informational: "node", "python", "go", "command", ...
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// argv to start the server, e.g. ["node", "server.js"].
    pub start_cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    /// Fluid in-function concurrency: max simultaneous requests per instance.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    /// Keep at least this many instances warm (0 = scale fully to zero).
    #[serde(default)]
    pub min_instances: u32,
    /// Autoscaling ceiling.
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    /// Scale an idle instance down after this many seconds with no requests.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,
}

fn default_runtime() -> String {
    "command".into()
}
fn default_memory() -> u32 {
    512
}
fn default_max_concurrency() -> u32 {
    10
}
fn default_max_instances() -> u32 {
    10
}
fn default_idle_ttl() -> u64 {
    30
}

/// What a route serves.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Static,
    Function(String),
}

/// A path-prefix route. Longest matching prefix wins (computed at match time).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub pattern: String,
    pub target: RouteTarget,
}

/// `fluid.json` — what a user writes to describe their deployment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub project: String,
    /// Relative dir (within the deployment root) holding static assets.
    #[serde(default)]
    pub static_dir: Option<String>,
    #[serde(default)]
    pub functions: Vec<FunctionConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

impl Manifest {
    pub fn from_json(s: &str) -> Result<Manifest, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn function(&self, name: &str) -> Option<&FunctionConfig> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// Resolve a request path to a route target using longest-prefix match.
    /// Falls back to Static if nothing matches.
    pub fn resolve(&self, path: &str) -> RouteTarget {
        let mut best: Option<&Route> = None;
        for r in &self.routes {
            if path_matches(&r.pattern, path) {
                match best {
                    Some(b) if b.pattern.len() >= r.pattern.len() => {}
                    _ => best = Some(r),
                }
            }
        }
        best.map(|r| r.target.clone()).unwrap_or(RouteTarget::Static)
    }
}

/// Prefix match with `/` boundary awareness. `"/api"` matches `/api` and
/// `/api/x` but not `/apixyz`. `"/"` matches everything.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "/" {
        return true;
    }
    let pattern = pattern.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix(pattern) {
        rest.is_empty() || rest.starts_with('/')
    } else {
        false
    }
}

/// A registered deployment (manifest + where its files live).
#[derive(Clone, Debug)]
pub struct Deployment {
    pub id: DeploymentId,
    pub project: String,
    /// Host path to deployment files (mock backend serves from here).
    pub root: std::path::PathBuf,
    pub manifest: Manifest,
    pub created_at_ms: u64,
}

/// Admin API: request to create a deployment. For the mock backend the gateway
/// reads files directly from `root` (same host); a real deploy would upload a
/// tarball / build artifact instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployRequest {
    pub root: String,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub id: DeploymentId,
    pub project: String,
    pub functions: Vec<String>,
    pub created_at_ms: u64,
    /// Convenience: the Host alias that resolves to this deployment.
    pub alias: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        let m = Manifest {
            project: "p".into(),
            static_dir: Some("public".into()),
            functions: vec![],
            routes: vec![
                Route { pattern: "/".into(), target: RouteTarget::Static },
                Route { pattern: "/api".into(), target: RouteTarget::Function("api".into()) },
                Route { pattern: "/api/admin".into(), target: RouteTarget::Function("admin".into()) },
            ],
        };
        assert_eq!(m.resolve("/index.html"), RouteTarget::Static);
        assert_eq!(m.resolve("/api/users"), RouteTarget::Function("api".into()));
        assert_eq!(m.resolve("/api/admin/x"), RouteTarget::Function("admin".into()));
        assert!(!path_matches("/api", "/apixyz"));
    }
}
