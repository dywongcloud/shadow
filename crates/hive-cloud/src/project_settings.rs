//! Per-project settings — Environment Variables, Build & Development config, and
//! Function settings (Fluid, max duration, regions, failover). Keyed by project
//! name so they persist across deployments. In-memory for this study build.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    /// "production" | "preview" | "development" | "all"
    #[serde(default = "default_target")]
    pub target: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub updated_ms: u64,
}
fn default_target() -> String {
    "production".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildConfig {
    pub framework: String,
    pub install_command: String,
    pub build_command: String,
    pub output_dir: String,
    pub root_dir: String,
}
impl Default for BuildConfig {
    fn default() -> Self {
        BuildConfig {
            framework: "Other".into(),
            install_command: "npm install".into(),
            build_command: "npm run build".into(),
            output_dir: "dist".into(),
            root_dir: "./".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionSettings {
    pub fluid_enabled: bool,
    pub default_max_duration_secs: u64,
    pub regions: Vec<String>,
    pub failover: bool,
    pub memory_mib: u32,
}
impl Default for FunctionSettings {
    fn default() -> Self {
        FunctionSettings {
            fluid_enabled: true,
            default_max_duration_secs: 300,
            regions: vec!["iad1".into()],
            failover: false,
            memory_mib: 512,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSettings {
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub build: BuildConfig,
    #[serde(default)]
    pub functions: FunctionSettings,
    #[serde(default)]
    pub domains: Vec<String>,
    /// Team that owns this project (slug). Defaults to "personal".
    #[serde(default = "default_team")]
    pub team: String,
    /// When true (default), preview deployments are only reachable by team
    /// members — anonymous requests to a preview host get a 401.
    #[serde(default = "default_true")]
    pub preview_protection: bool,
}
fn default_team() -> String {
    "personal".into()
}
fn default_true() -> bool {
    true
}

impl Default for ProjectSettings {
    fn default() -> Self {
        ProjectSettings {
            env: Vec::new(),
            build: BuildConfig::default(),
            functions: FunctionSettings::default(),
            domains: Vec::new(),
            team: default_team(),
            preview_protection: true,
        }
    }
}

/// Store keyed by project name.
pub struct ProjectStore {
    map: RwLock<HashMap<String, ProjectSettings>>,
}

impl ProjectStore {
    pub fn new() -> ProjectStore {
        ProjectStore { map: RwLock::new(HashMap::new()) }
    }

    pub fn get(&self, project: &str) -> ProjectSettings {
        self.map.read().get(project).cloned().unwrap_or_default()
    }

    /// Full snapshot (project -> settings) for persistence.
    pub fn snapshot(&self) -> HashMap<String, ProjectSettings> {
        self.map.read().clone()
    }

    /// Replace the whole store (used on boot).
    pub fn load(&self, data: HashMap<String, ProjectSettings>) {
        *self.map.write() = data;
    }

    /// Get with sensitive env values masked (for display).
    pub fn get_masked(&self, project: &str) -> ProjectSettings {
        let mut s = self.get(project);
        for e in s.env.iter_mut() {
            if e.sensitive {
                e.value = String::new();
            }
        }
        s
    }

    pub fn set_build(&self, project: &str, build: BuildConfig) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().build = build;
    }

    pub fn set_functions(&self, project: &str, f: FunctionSettings) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().functions = f;
    }

    /// Add or update an env var (by key+target).
    pub fn put_env(&self, project: &str, mut v: EnvVar) {
        v.updated_ms = now_ms();
        let mut m = self.map.write();
        let s = m.entry(project.to_string()).or_default();
        if let Some(existing) = s.env.iter_mut().find(|e| e.key == v.key && e.target == v.target) {
            *existing = v;
        } else {
            s.env.push(v);
        }
    }

    pub fn delete_env(&self, project: &str, key: &str) {
        if let Some(s) = self.map.write().get_mut(project) {
            s.env.retain(|e| e.key != key);
        }
    }

    pub fn set_team(&self, project: &str, team: &str) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().team = team.to_string();
    }

    pub fn set_preview_protection(&self, project: &str, on: bool) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().preview_protection = on;
    }

    /// Team slug owning a project (defaults to "personal").
    pub fn team_of(&self, project: &str) -> String {
        self.map.read().get(project).map(|s| s.team.clone()).unwrap_or_else(|| "personal".into())
    }

    /// Whether previews for a project are protected (defaults to true).
    pub fn preview_protected(&self, project: &str) -> bool {
        self.map.read().get(project).map(|s| s.preview_protection).unwrap_or(true)
    }

    pub fn add_domain(&self, project: &str, domain: String) {
        let mut m = self.map.write();
        let s = m.entry(project.to_string()).or_default();
        if !s.domains.contains(&domain) {
            s.domains.push(domain);
        }
    }

    /// All (project, domain) pairs across projects.
    pub fn all_domains(&self) -> Vec<(String, String)> {
        let m = self.map.read();
        m.iter()
            .flat_map(|(p, s)| s.domains.iter().map(move |d| (p.clone(), d.clone())))
            .collect()
    }

    /// Env vars to inject into a function at deploy time (decrypted values).
    pub fn env_map(&self, project: &str) -> std::collections::BTreeMap<String, String> {
        self.get(project)
            .env
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect()
    }
}

impl Default for ProjectStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Static catalog of Hive regions (Vercel-style), grouped by continent, for the
/// Function Regions selector.
pub fn region_catalog() -> serde_json::Value {
    serde_json::json!({
        "North America": [
            {"id": "iad1", "label": "Washington, D.C., USA (East)", "aws": "us-east-1"},
            {"id": "cle1", "label": "Cleveland, USA (East)", "aws": "us-east-2"},
            {"id": "sfo1", "label": "San Francisco, USA (West)", "aws": "us-west-1"},
            {"id": "pdx1", "label": "Portland, USA (West)", "aws": "us-west-2"},
            {"id": "yul1", "label": "Montréal, Canada (East)", "aws": "ca-central-1"}
        ],
        "South America": [
            {"id": "gru1", "label": "São Paulo, Brazil (East)", "aws": "sa-east-1"}
        ],
        "Europe": [
            {"id": "fra1", "label": "Frankfurt, Germany", "aws": "eu-central-1"},
            {"id": "lhr1", "label": "London, UK", "aws": "eu-west-2"},
            {"id": "cdg1", "label": "Paris, France", "aws": "eu-west-3"},
            {"id": "arn1", "label": "Stockholm, Sweden", "aws": "eu-north-1"}
        ],
        "Asia": [
            {"id": "hnd1", "label": "Tokyo, Japan", "aws": "ap-northeast-1"},
            {"id": "sin1", "label": "Singapore", "aws": "ap-southeast-1"},
            {"id": "bom1", "label": "Mumbai, India", "aws": "ap-south-1"}
        ],
        "Oceania": [
            {"id": "syd1", "label": "Sydney, Australia", "aws": "ap-southeast-2"}
        ]
    })
}
