//! Per-project settings — Environment Variables, Build & Development config, and
//! Function settings (Fluid, max duration, regions, failover). Keyed by project
//! name so they persist across deployments: changes are saved via `persist::persist`
//! into `PlatformSnapshot.projects` and restored on boot.

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
    /// Explicit runtime override ("nodejs"/"bun"/empty). Empty = infer from the
    /// detected start command (today's behavior, unchanged). Distinct from
    /// package-manager detection — a `bun.lock` in the repo picks `bun install`
    /// as the installer without this field ever being set; only an explicit
    /// choice here (or `vercel.json`'s `runtime`/`bunVersion`) selects the Bun
    /// RUNTIME. `#[serde(default)]` so every already-persisted project setting
    /// (written before this field existed) still deserializes.
    #[serde(default)]
    pub runtime: String,
}
impl Default for BuildConfig {
    fn default() -> Self {
        // Empty = "use framework/package-manager detection". Non-empty values are
        // treated as explicit overrides by the builder, so defaults MUST be empty
        // (otherwise a freshly-created project would force `npm install` and
        // override detected pnpm/yarn — and break monorepo workspaces).
        BuildConfig {
            framework: String::new(),
            install_command: String::new(),
            build_command: String::new(),
            output_dir: String::new(),
            root_dir: String::new(),
            runtime: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionSettings {
    pub fluid_enabled: bool,
    pub default_max_duration_secs: u64,
    pub regions: Vec<String>,
    pub failover: bool,
    /// Per-instance CPU/memory tier (one microVM per instance):
    ///   Standard    = 1 vCPU  / 2048 MiB  (default)
    ///   Performance = 2 vCPUs / 4096 MiB
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    pub memory_mib: u32,
}
fn default_vcpus() -> u32 {
    1
}
impl Default for FunctionSettings {
    fn default() -> Self {
        FunctionSettings {
            fluid_enabled: true,
            default_max_duration_secs: 300,
            // No hard-coded region. Empty = "run on the nearest available node"
            // (anycast). Real regions come from the live mesh; the dashboard's
            // region picker is populated from where nodes actually are.
            regions: vec![],
            failover: false,
            // Standard serverless tier: 1 vCPU / 2 GB.
            vcpus: 1,
            memory_mib: 2048,
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
    /// The Git branch whose deployments are PRODUCTION (Vercel's "Production
    /// Branch"). Set to the imported branch on first deploy. Pushes to this branch
    /// deploy to production; every other branch / PR deploys a preview. Empty until
    /// the project's first git deploy classifies it.
    #[serde(default)]
    pub production_branch: String,
    /// When true (default), preview deployments are only reachable by team
    /// members — anonymous requests to a preview host get a 401.
    #[serde(default = "default_true")]
    pub preview_protection: bool,
    /// Explicit project-level opt-in for `sudo` inside this project's
    /// Sandboxes (default false) — the ZeroTrust invariant "sudo only if
    /// explicitly enabled by project policy" reads this flag.
    #[serde(default)]
    pub sandbox_allow_sudo: bool,
    /// When false, this project's cron jobs stop firing (the scheduler's tick
    /// loop skips invocation) without touching the jobs themselves — they
    /// keep being created/updated/deleted on every deployment via
    /// `vercel.json` and keep advancing their own schedule, matching Vercel's
    /// project-level Cron Jobs kill switch. Default true (on).
    #[serde(default = "default_true")]
    pub cron_enabled: bool,
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
            production_branch: String::new(),
            preview_protection: true,
            sandbox_allow_sudo: false,
            cron_enabled: true,
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

    /// Case-insensitive lookup of an existing project KEY, holding only a read
    /// lock and cloning a single string — NOT the whole map + every project's
    /// env/build/function config. Used on the hot deploy path (name-collision
    /// checks run on every deploy/create) where the old `snapshot()` clone was
    /// O(all projects × settings size). Semantics-preserving.
    pub fn find_key_ci(&self, name: &str) -> Option<String> {
        self.map.read().keys().find(|k| k.eq_ignore_ascii_case(name)).cloned()
    }

    /// Replace the whole store (used on boot).
    pub fn load(&self, data: HashMap<String, ProjectSettings>) {
        *self.map.write() = data;
    }

    /// Forget a project's settings (used when a project is deleted).
    pub fn remove(&self, project: &str) {
        self.map.write().remove(project);
    }

    /// Settings only if the project was explicitly configured (None otherwise).
    /// Distinguishes "user set a build command" from the generic defaults.
    pub fn get_if_set(&self, project: &str) -> Option<ProjectSettings> {
        self.map.read().get(project).cloned()
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

    /// Add or update an env var (by key+target). Sensitive values are sealed
    /// (encrypted at rest) before storing, so they're persisted as secrets — never
    /// written to disk / the replicated snapshot in plaintext.
    pub fn put_env(&self, project: &str, mut v: EnvVar) {
        v.updated_ms = now_ms();
        let mut m = self.map.write();
        let s = m.entry(project.to_string()).or_default();
        // EDIT semantics: sensitive values are masked to "" in every read, so the
        // dashboard's editor can't echo them back. An upsert of an EXISTING key
        // with an EMPTY value means "keep the stored value" (retarget/re-flag
        // only) — never silently blank a secret. The kept value is already
        // encrypted when the previous entry was sensitive (don't re-encrypt).
        if v.value.is_empty() {
            if let Some(prev) = s.env.iter().find(|e| e.key == v.key) {
                v.value = prev.value.clone();
                if v.sensitive && !prev.sensitive && !v.value.is_empty() {
                    v.value = crate::secrets::encrypt(&v.value);
                }
            }
        } else if v.sensitive {
            v.value = crate::secrets::encrypt(&v.value);
        }
        // Upsert by KEY — one row per key, matching `delete_env` and the
        // dashboard's list model. (Previously keyed by (key, target): editing a
        // var's environment DUPLICATED the row, and the duplicate persisted —
        // the "my variable disappeared/duplicated after I left the page" bug.)
        if let Some(existing) = s.env.iter_mut().find(|e| e.key == v.key) {
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

    /// Persist the monorepo subdirectory so redeploys keep building it.
    pub fn set_root_dir(&self, project: &str, root_dir: &str) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().build.root_dir = root_dir.to_string();
    }

    /// The configured root/subdirectory for a project ("" if none).
    pub fn root_dir_of(&self, project: &str) -> String {
        self.map.read().get(project).map(|s| s.build.root_dir.clone()).unwrap_or_default()
    }

    /// The project's production branch ("" if not yet classified).
    pub fn production_branch_of(&self, project: &str) -> String {
        self.map.read().get(project).map(|s| s.production_branch.clone()).unwrap_or_default()
    }

    /// Set the production branch (Vercel "Production Branch"). Called once on the
    /// project's first git deploy, or explicitly from project settings.
    pub fn set_production_branch(&self, project: &str, branch: &str) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().production_branch = branch.to_string();
    }

    pub fn set_preview_protection(&self, project: &str, on: bool) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().preview_protection = on;
    }

    pub fn set_cron_enabled(&self, project: &str, on: bool) {
        let mut m = self.map.write();
        m.entry(project.to_string()).or_default().cron_enabled = on;
    }

    /// Whether this project's cron jobs are allowed to fire (default true —
    /// an unknown/never-configured project is not disabled).
    pub fn cron_enabled(&self, project: &str) -> bool {
        self.map.read().get(project).map(|s| s.cron_enabled).unwrap_or(true)
    }

    /// Team slug owning a project (defaults to "personal").
    pub fn team_of(&self, project: &str) -> String {
        self.map.read().get(project).map(|s| s.team.clone()).unwrap_or_else(|| "personal".into())
    }

    /// Count of projects owned by a team/tenant (for plan-quota enforcement).
    pub fn count_for_team(&self, team: &str) -> usize {
        let t = team.trim().to_lowercase();
        self.map.read().values().filter(|s| s.team.trim().to_lowercase() == t).count()
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
    /// Sealed (sensitive) values are opened here; plaintext passes through.
    pub fn env_map(&self, project: &str) -> std::collections::BTreeMap<String, String> {
        self.get(project)
            .env
            .into_iter()
            .map(|e| (e.key, crate::secrets::decrypt(&e.value)))
            .collect()
    }
}

impl Default for ProjectStore {
    fn default() -> Self {
        Self::new()
    }
}

// NOTE: the Function Regions catalog is no longer a static Vercel-style table.
// It is built dynamically in `admin::region_catalog` from the live mesh — the
// real regions where P2P nodes report their lat/lon, auto-grouped by continent.

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: &str, value: &str, sensitive: bool) -> EnvVar {
        EnvVar { key: key.into(), value: value.into(), target: "all".into(), sensitive, updated_ms: 0 }
    }

    #[test]
    fn cron_enabled_defaults_true_and_is_toggleable() {
        let s = ProjectStore::new();
        // An unconfigured/unknown project is never treated as disabled.
        assert!(s.cron_enabled("never-touched"));
        s.set_cron_enabled("app", false);
        assert!(!s.cron_enabled("app"));
        s.set_cron_enabled("app", true);
        assert!(s.cron_enabled("app"));
    }

    #[test]
    fn put_env_then_read_and_replace() {
        let s = ProjectStore::new();
        s.put_env("app", ev("FOO", "bar", false));
        let m = s.env_map("app");
        assert_eq!(m.get("FOO").map(|v| v.as_str()), Some("bar"));
        // Same key+target replaces rather than duplicates.
        s.put_env("app", ev("FOO", "baz", false));
        assert_eq!(s.get("app").env.iter().filter(|e| e.key == "FOO").count(), 1);
        assert_eq!(s.env_map("app").get("FOO").map(|v| v.as_str()), Some("baz"));
    }

    #[test]
    fn sensitive_value_is_encrypted_at_rest_masked_and_decrypted_for_runtime() {
        let s = ProjectStore::new();
        s.put_env("app", ev("API_TOKEN", "supersecret", true));
        // Stored at rest: not plaintext (encrypted blob).
        let stored = s.get("app").env.iter().find(|e| e.key == "API_TOKEN").unwrap().value.clone();
        assert_ne!(stored, "supersecret");
        assert!(crate::secrets::is_encrypted(&stored), "sensitive env sealed at rest");
        // Masked view blanks the value.
        let masked = s.get_masked("app").env.iter().find(|e| e.key == "API_TOKEN").unwrap().value.clone();
        assert_eq!(masked, "");
        // Runtime injection decrypts back to plaintext.
        assert_eq!(s.env_map("app").get("API_TOKEN").map(|v| v.as_str()), Some("supersecret"));
    }

    #[test]
    fn delete_env_removes_key() {
        let s = ProjectStore::new();
        s.put_env("app", ev("A", "1", false));
        s.put_env("app", ev("B", "2", false));
        s.delete_env("app", "A");
        let keys: Vec<_> = s.get("app").env.iter().map(|e| e.key.clone()).collect();
        assert_eq!(keys, vec!["B"]);
    }

    #[test]
    fn root_dir_production_branch_and_team_accessors() {
        let s = ProjectStore::new();
        assert_eq!(s.root_dir_of("app"), "");
        s.set_root_dir("app", "examples/nextjs");
        assert_eq!(s.root_dir_of("app"), "examples/nextjs");
        s.set_production_branch("app", "main");
        assert_eq!(s.production_branch_of("app"), "main");
        // default team is "personal" until set.
        assert_eq!(s.team_of("app"), "personal");
        s.set_team("app", "acme");
        assert_eq!(s.team_of("app"), "acme");
    }

    #[test]
    fn find_key_ci_matches_snapshot_collision_semantics() {
        // find_key_ci must return the SAME collision decision the old
        // snapshot()+eq_ignore_ascii_case scan produced — the hot-path clone
        // replacement is purely a performance change, not a behavior change.
        let s = ProjectStore::new();
        s.set_team("MyApp", "acme");
        // exact + case-insensitive hits return the actual stored key
        assert_eq!(s.find_key_ci("MyApp").as_deref(), Some("MyApp"));
        assert_eq!(s.find_key_ci("myapp").as_deref(), Some("MyApp"));
        assert_eq!(s.find_key_ci("MYAPP").as_deref(), Some("MyApp"));
        // miss
        assert_eq!(s.find_key_ci("other"), None);
        // equivalence with the previous snapshot-based check across the map
        let snap = s.snapshot();
        for probe in ["MyApp", "myapp", "nope", "my-app"] {
            let old = snap.keys().find(|k| k.eq_ignore_ascii_case(probe)).cloned();
            assert_eq!(s.find_key_ci(probe), old, "find_key_ci disagrees for {probe}");
        }
    }

    #[test]
    fn snapshot_load_roundtrip_preserves_sealed_env() {
        let s = ProjectStore::new();
        s.put_env("app", ev("SECRET", "v", true));
        let snap = s.snapshot();
        let s2 = ProjectStore::new();
        s2.load(snap);
        assert_eq!(s2.env_map("app").get("SECRET").map(|v| v.as_str()), Some("v"));
    }
}
