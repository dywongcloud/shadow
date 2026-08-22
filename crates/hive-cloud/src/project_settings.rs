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

/// Does this value look like a real, live credential? Used to force
/// `sensitive=true` even when the caller didn't ask for it — a user leaving the
/// "Sensitive" checkbox unticked must never turn into a plaintext credential leak
/// through the settings/gitops read paths. Deliberately precise prefix/shape
/// matches (not generic entropy scoring) to avoid false-positive-masking a
/// normal, non-secret config value.
fn looks_like_secret(v: &str) -> bool {
    let s = v.trim();
    if s.is_empty() {
        return false;
    }
    const PREFIXES: &[&str] = &[
        // GitHub personal/app/OAuth/refresh tokens + fine-grained PATs.
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        // AWS access key IDs (both long-term and STS-issued).
        "AKIA",
        "ASIA",
        // Stripe secret/restricted keys (live and test).
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "rk_test_",
        // npm publish tokens.
        "npm_",
        // Slack bot/user/app/config tokens.
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxs-",
        // Google API keys.
        "AIza",
        // Anthropic / OpenAI API keys.
        "sk-ant-",
        "sk-proj-",
        "sk-",
        // PEM-encoded private key blocks.
        "-----BEGIN ",
    ];
    if PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    // JWTs: three base64url segments separated by dots, header segment starts
    // with the near-universal `eyJ` (base64 of `{"`).
    if s.starts_with("eyJ") && s.matches('.').count() == 2 {
        return true;
    }
    false
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
    /// True when `framework` was written by build-time auto-detection rather
    /// than an explicit user choice. An auto-detected slug must never be
    /// treated as an override: detection re-runs on every build while this is
    /// set, so a first-build misdetection (e.g. a monorepo scanned at the
    /// wrong root reading as "static") cannot permanently pin the project to
    /// the wrong framework. `#[serde(default)]` = false, so every
    /// already-persisted framework (which may have been user-set) keeps its
    /// explicit-override behavior.
    #[serde(default)]
    pub framework_auto: bool,
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
            framework_auto: false,
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
    /// Serverless GPU: this project's functions run on GPU-equipped nodes with
    /// the host GPUs passed through to the cell. Placement then only targets
    /// nodes advertising `gpu_count > 0`. Default off; absent in stored
    /// settings from before this field existed deserializes to off.
    #[serde(default)]
    pub gpu: bool,
    /// Dedicated public IPv4: this project's functions get a real Tencent
    /// Cloud EIP purchased/associated at deploy time (`hive-cloud::tencent_eip`),
    /// mirroring `gpu`'s shape exactly. Default off; absent in stored settings
    /// from before this field existed deserializes to off. Settings can only
    /// turn this ON, never strip a function's own declared
    /// `fluid.json functions[].dedicatedIpv4` need (the `gpu` OR-merge
    /// precedent, `git.rs`).
    #[serde(default)]
    pub dedicated_ipv4: bool,
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
            gpu: false,
            dedicated_ipv4: false,
        }
    }
}

/// `fluid.json` top-level `inference` block — the developer-facing serverless
/// GPU inference convention. `model` is a real model ref (direct GGUF URL, or
/// an `org/repo/file.gguf` HuggingFace path); `pool: true` allows the platform
/// to combine multiple same-region GPU nodes (llama.cpp RPC layer-distribution)
/// when the model does not fit a single node's free VRAM. The platform
/// provisions the real backend and injects `HIVE_INFERENCE_URL` (an
/// OpenAI-compatible endpoint) into the project env — the app just fetches it,
/// same precedent as DB env auto-injection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InferenceSpec {
    pub model: String,
    #[serde(default)]
    pub pool: bool,
}

/// Dashboard-managed configuration for a CONTAINER-runtime project (a
/// single-Dockerfile deploy, not a compose service — compose already
/// configures per-service via its own YAML) — the persisted equivalent of
/// `git.rs`'s `fluid.json` `container` override block (`ContainerOverride`:
/// same field set, same string formats for `memory`/`cpus`). Follows the
/// `inference`/`browser_db` sync discipline exactly: an explicit fluid.json
/// `container` block always wins over this on the next build; this is what
/// lets a container project be configured from the dashboard alone with no
/// git push. All fields optional — `None` means "use the platform default"
/// for that one field, independent of the others.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContainerSettings {
    /// The port the container listens on inside the image.
    #[serde(default)]
    pub port: Option<u16>,
    /// Wire protocol: "http" (default), "tcp", "udp", or "grpc" — see
    /// `fluid_core::ServiceProtocol`. Get this wrong (e.g. "udp" for a
    /// TCP-only game server) and the deployment silently never becomes
    /// reachable, even though it builds and runs successfully.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Memory ceiling, e.g. "4g", "2048m" — same format as `ContainerOverride::memory`.
    #[serde(default)]
    pub memory: Option<String>,
    /// CPU quota, e.g. "2.0", "0.5" — same format as `ContainerOverride::cpus`.
    #[serde(default)]
    pub cpus: Option<String>,
    /// Max-PIDs ceiling (fork-bomb guard) — same as `ContainerOverride::pids`.
    #[serde(default)]
    pub pids: Option<u32>,
    /// Mount path for the automatic persistent volume INSIDE the container
    /// (platform default: `/data`, see `container_volume_path()`). Lets a
    /// project whose image expects data somewhere else (not every image
    /// follows the `/data` convention `itzg/minecraft-server` uses) redirect
    /// the durable volume without a node-wide `HIVE_CONTAINER_VOLUME_PATH`
    /// override affecting every OTHER project on the node too.
    #[serde(default)]
    pub volume_mount_path: Option<String>,
}

/// A minted REST/Hrana credential for a project's `browser_db` replica
/// (`browser_db_rest`, bn-browser-db-rest) — the `drive_api::webdav_token_mint`
/// pattern applied to this surface: shown once at mint time, stored here only
/// as its SHA-256 hash, checked constant-time by
/// `browser_db_rest::credential_scope`. Structurally separate from the
/// QUIC-endpoint-identity admission lease `browser_admission` issues for the
/// CRR sync protocol — this is a plain bearer token for the REST/Hrana HTTP
/// surface, mirroring `db_rest`'s per-database `DB_REST_TOKEN` credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserDbRestToken {
    /// SHA-256 hex of the plaintext token — the plaintext itself is never
    /// stored anywhere, shown to the caller exactly once at mint time.
    pub hash: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<fluid_core::ProjectIncarnation>,
    #[serde(default)]
    pub incarnation_created_ms: u64,
    /// Last write to THIS row on THIS node (ms epoch). The per-row freshness
    /// key `merge_synced` compares — newest write wins a replication collision,
    /// and a tombstone at-or-after it deletes the row. `0` = a row never
    /// touched since the field existed: it merges fail-safe (never overwrites,
    /// never overwritten by another 0-row).
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub build: BuildConfig,
    #[serde(default)]
    pub functions: FunctionSettings,
    /// Managed inference backend request (fluid.json `inference`), if any.
    /// Synced from the deploy path; consumed by `inference::spawn_reconcile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<InferenceSpec>,
    /// Dashboard-managed browser-replicated database opt-in (the Storages
    /// page "Deploy a replicated SQLite database" flow). Presence is the
    /// opt-in, same discipline as `fluid_core::Manifest::browser_db` — this
    /// is a SEPARATE opt-in source, not a copy of the manifest field: `git.rs`
    /// syncs an explicit fluid.json block into this mirror (read side, the
    /// `inference` precedent) and, when fluid.json declares NO block, merges
    /// THIS spec into the manifest at build time instead (write side, the
    /// `FunctionSettings::gpu` OR precedent applied to an `Option` — an
    /// explicit fluid.json block always wins). Lets a project opt in from the
    /// dashboard alone, with no git push required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db: Option<fluid_core::BrowserDbPolicy>,
    /// Team-scope (read+write) REST/Hrana credential for this project's
    /// `browser_db` replica, if minted. See [`BrowserDbRestToken`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db_rest_team: Option<BrowserDbRestToken>,
    /// Public-scope (read-only) REST/Hrana credential — mintable only while
    /// `browser_db.public_read` is enabled (mirrors the CRR admission's own
    /// Public-scope gate); re-checked live on every request, not just at
    /// mint time, so disabling `public_read` later immediately stops this
    /// token from reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db_rest_public: Option<BrowserDbRestToken>,
    /// Dashboard-managed container config (port/protocol/resource ceilings +
    /// volume mount path) for a container-runtime project. Only meaningful
    /// when the project's current deployment actually runs a `container`
    /// function — see [`ContainerSettings`]. Same `Option` discipline as
    /// `inference`/`browser_db`: presence is the opt-in, an explicit
    /// fluid.json `container` block always wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerSettings>,
    /// The project's paid dedicated public IPv4 allocation, if the addon was
    /// purchased (`tencent_eip::provision_from_checkout`). This — not a
    /// fluid.json flag — is now the ONLY source that can turn
    /// `FunctionConfig::dedicated_ipv4` on for a deploy (`git.rs`'s merge):
    /// a tenant declaring `functions[].dedicatedIpv4` in fluid.json can no
    /// longer self-grant the feature for free. `None` until purchased; stays
    /// `Some` across redeploys so the manifest re-adopts the same address
    /// instead of buying a second one (`Manifest::dedicated_ipv4_binding`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedicated_ipv4: Option<fluid_core::DedicatedIpv4>,
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
    /// Outcome of the auto-CI install attempted right after this project's
    /// first git import (`/api/gitops/project-ci`): did a real GitHub webhook
    /// or the Actions-workflow fallback actually get installed on the source
    /// repo? Previously this result was fire-and-forget from the UI and
    /// discarded entirely — a project imported without a completed GitHub
    /// OAuth connection (the common "paste a public repo URL" flow) silently
    /// got NEITHER installed, so no future push ever auto-deployed, with zero
    /// visible error anywhere (not even a failed GitHub delivery, since no
    /// webhook object was ever created). Persisting it here lets the
    /// dashboard surface the gap and offer a retry.
    #[serde(default)]
    pub git_ci: Option<GitCiStatus>,
}

/// See [`ProjectSettings::git_ci`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitCiStatus {
    pub webhook_installed: bool,
    pub workflow_installed: bool,
    /// Set only when neither installed — e.g. "github-not-connected",
    /// "bad-repo", "composio-not-configured" (the exact `reason` values
    /// `/api/gitops/project-ci` already returns).
    #[serde(default)]
    pub skipped_reason: String,
    pub checked_ms: u64,
}
/// A project settings row with no explicit owner (never `set_team`'d, or
/// reloaded from a snapshot/serde-default that lost the tag) is UNOWNED, never
/// the platform owner's "personal" namespace — that literal string is itself a
/// live tenant, so defaulting into it used to make an untagged project silently
/// visible under the owner's personal project list (the multitenancy leak).
/// Mirrors `admin::UNTAGGED_TENANT`; `team_of()` relies on this same value for
/// its own "row absent" fallback so both paths agree.
fn default_team() -> String {
    crate::admin::UNTAGGED_TENANT.into()
}
fn default_true() -> bool {
    true
}

impl Default for ProjectSettings {
    fn default() -> Self {
        ProjectSettings {
            incarnation: None,
            incarnation_created_ms: 0,
            updated_ms: 0,
            env: Vec::new(),
            build: BuildConfig::default(),
            functions: FunctionSettings::default(),
            inference: None,
            browser_db: None,
            browser_db_rest_team: None,
            browser_db_rest_public: None,
            container: None,
            dedicated_ipv4: None,
            domains: Vec::new(),
            team: default_team(),
            production_branch: String::new(),
            preview_protection: true,
            sandbox_allow_sudo: false,
            cron_enabled: true,
            git_ci: None,
        }
    }
}

/// Store keyed by project name.
/// Replication payload for the projects store: rows plus the tombstones that
/// explain absences — the `SyncedDatabases` shape applied to projects, for the
/// same witnessed reason: a wholesale-replace adoption makes "the sender has
/// not got this row" and "this row was deleted" the same event, so one node's
/// row loss (leader OOM before its debounced save, a since-fixed reaper wiping
/// settings) replicated fleet-wide within a sync tick and the project
/// VANISHED from its account everywhere.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncedProjects {
    pub rows: std::collections::BTreeMap<String, ProjectSettings>,
    #[serde(default)]
    pub tombstones: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub incarnation_tombstones: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
    >,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectIncarnationError {
    Missing,
    Legacy,
    Mismatch {
        active: fluid_core::ProjectIncarnation,
    },
    AlreadyExists,
    Tombstoned,
}

impl std::fmt::Display for ProjectIncarnationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("project does not exist"),
            Self::Legacy => f.write_str("project has no incarnation"),
            Self::Mismatch { active } => write!(f, "project incarnation changed to {active}"),
            Self::AlreadyExists => f.write_str("project already exists"),
            Self::Tombstoned => f.write_str("project incarnation was deleted"),
        }
    }
}

impl std::error::Error for ProjectIncarnationError {}

pub struct ProjectStore {
    map: RwLock<HashMap<String, ProjectSettings>>,
    /// project → deletion ms. Records deletions so absence can replicate
    /// EXPLICITLY (see [`SyncedProjects`]). Retained permanently: an offline
    /// node can return after any bounded GC window and otherwise resurrect a
    /// deleted project, while this map grows only once per deleted project.
    tombstones: RwLock<std::collections::BTreeMap<String, u64>>,
    incarnation_tombstones: RwLock<
        std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
        >,
    >,
}

fn project_time_ceiling(now: u64) -> u64 {
    // A delete issuer may intentionally lead by one skew allowance; a receiver
    // can be the same allowance behind it. Locally-issued generations still use
    // the narrower one-skew ceiling in `begin_delete`.
    now.saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS * 2)
}

impl ProjectStore {
    pub fn new() -> ProjectStore {
        ProjectStore {
            map: RwLock::new(HashMap::new()),
            tombstones: RwLock::new(std::collections::BTreeMap::new()),
            incarnation_tombstones: RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    pub fn get(&self, project: &str) -> ProjectSettings {
        self.map.read().get(project).cloned().unwrap_or_default()
    }

    pub fn create(
        &self,
        project: &str,
    ) -> Result<fluid_core::ProjectIncarnation, ProjectIncarnationError> {
        let mut map = self.map.write();
        if map.contains_key(project) {
            return Err(ProjectIncarnationError::AlreadyExists);
        }
        let legacy_floor = self.tombstones.read().get(project).copied().unwrap_or(0);
        let incarnation_floor = self
            .incarnation_tombstones
            .read()
            .get(project)
            .and_then(|tombstones| tombstones.values().copied().max())
            .unwrap_or(0);
        let incarnation = fluid_core::ProjectIncarnation::mint();
        let created_ms = now_ms()
            .max(legacy_floor.saturating_add(1))
            .max(incarnation_floor.saturating_add(1));
        let row = ProjectSettings {
            incarnation: Some(incarnation),
            incarnation_created_ms: created_ms,
            updated_ms: created_ms,
            ..ProjectSettings::default()
        };
        map.insert(project.to_string(), row);
        Ok(incarnation)
    }

    pub fn adopt_incarnation(
        &self,
        project: &str,
        incarnation: fluid_core::ProjectIncarnation,
    ) -> Result<(), ProjectIncarnationError> {
        let mut map = self.map.write();
        if self
            .incarnation_tombstones
            .read()
            .get(project)
            .is_some_and(|tombstones| tombstones.contains_key(&incarnation))
        {
            return Err(ProjectIncarnationError::Tombstoned);
        }
        match map.get(project) {
            Some(row) if row.incarnation == Some(incarnation) => Ok(()),
            Some(row) => Err(match row.incarnation {
                Some(active) => ProjectIncarnationError::Mismatch { active },
                None => ProjectIncarnationError::Legacy,
            }),
            None => {
                let legacy_floor = self.tombstones.read().get(project).copied().unwrap_or(0);
                let incarnation_floor = self
                    .incarnation_tombstones
                    .read()
                    .get(project)
                    .and_then(|tombstones| tombstones.values().copied().max())
                    .unwrap_or(0);
                let created_ms = now_ms()
                    .max(legacy_floor.saturating_add(1))
                    .max(incarnation_floor.saturating_add(1));
                map.insert(
                    project.to_string(),
                    ProjectSettings {
                        incarnation: Some(incarnation),
                        incarnation_created_ms: created_ms,
                        updated_ms: created_ms,
                        ..ProjectSettings::default()
                    },
                );
                Ok(())
            }
        }
    }

    pub fn active_incarnation(
        &self,
        project: &str,
    ) -> Result<fluid_core::ProjectIncarnation, ProjectIncarnationError> {
        match self.map.read().get(project) {
            None => Err(ProjectIncarnationError::Missing),
            Some(row) => row.incarnation.ok_or(ProjectIncarnationError::Legacy),
        }
    }

    pub fn with_active<R>(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        apply: impl FnOnce(&ProjectSettings) -> R,
    ) -> Result<R, ProjectIncarnationError> {
        let map = self.map.read();
        let row = map.get(project).ok_or(ProjectIncarnationError::Missing)?;
        match row.incarnation {
            Some(active) if active == expected => Ok(apply(row)),
            Some(active) => Err(ProjectIncarnationError::Mismatch { active }),
            None => Err(ProjectIncarnationError::Legacy),
        }
    }

    pub fn get_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
    ) -> Result<ProjectSettings, ProjectIncarnationError> {
        self.with_active(project, expected, Clone::clone)
    }

    /// Full snapshot (project -> settings) for persistence.
    pub fn snapshot(&self) -> HashMap<String, ProjectSettings> {
        self.map.read().clone()
    }

    /// Every project carrying a `browser_db` opt-in, as (project, policy).
    ///
    /// Exists so the Storage page can learn its whole SQLite lane in ONE call.
    /// It previously issued one `/v1/projects/<p>/settings` request PER project
    /// and assembled the lane client-side, which made a database's presence in
    /// the list depend on N independent requests all succeeding promptly — the
    /// managed half is one endpoint and cannot partially fail, and the two
    /// halves of one list should not have different failure modes.
    ///
    /// Clones only the matching project names and their (small) policies under
    /// a read lock — deliberately NOT `snapshot()`, which copies every
    /// project's env/build/function config (the `find_key_ci` precedent).
    pub fn browser_db_projects(&self) -> Vec<(String, fluid_core::BrowserDbPolicy)> {
        self.map
            .read()
            .iter()
            .filter_map(|(name, s)| s.browser_db.clone().map(|p| (name.clone(), p)))
            .collect()
    }

    /// Case-insensitive lookup of an existing project KEY, holding only a read
    /// lock and cloning a single string — NOT the whole map + every project's
    /// env/build/function config. Used on the hot deploy path (name-collision
    /// checks run on every deploy/create) where the old `snapshot()` clone was
    /// O(all projects × settings size). Semantics-preserving.
    pub fn find_key_ci(&self, name: &str) -> Option<String> {
        self.map
            .read()
            .keys()
            .find(|k| k.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// Replace the whole store (used on boot).
    pub fn load(&self, mut data: HashMap<String, ProjectSettings>) {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        for (id, row) in data.iter_mut() {
            if row.incarnation.is_some() && row.incarnation_created_ms == 0 {
                row.incarnation_created_ms = row.updated_ms;
            }
            if row.incarnation_created_ms > ceiling {
                tracing::warn!(project = %id, created_ms = row.incarnation_created_ms, ceiling_ms = ceiling, "normalizing implausibly future persisted project incarnation on load");
                row.incarnation_created_ms = now;
            }
            if row.updated_ms > ceiling {
                tracing::warn!(project = %id, updated_ms = row.updated_ms, ceiling_ms = ceiling, "normalizing implausibly future persisted project row on load");
                row.updated_ms = now;
            }
        }
        *self.map.write() = data;
    }

    /// Snapshot for replication: rows plus the tombstones explaining absences.
    pub fn snapshot_synced(&self) -> SyncedProjects {
        SyncedProjects {
            rows: self.map.read().clone().into_iter().collect(),
            tombstones: self.tombstones.read().clone(),
            incarnation_tombstones: self.incarnation_tombstones.read().clone(),
        }
    }

    // (helper below `merge_synced`)
    /// Merge a peer's snapshot. NEWEST-PER-ROW wins (`updated_ms` — projects
    /// are legitimately written on many nodes: set_team on the ingress node,
    /// env saves through the leader, production_branch on the build node), a
    /// tombstone at-or-after a row's last write drops it, and a row
    /// re-created AFTER its tombstone survives. Never a wholesale replace:
    /// a sender that simply lacks a row cannot erase it here.
    pub fn merge_synced(&self, remote: SyncedProjects) -> usize {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        {
            let mut tombs = self.tombstones.write();
            // Repair poison persisted by pre-boundary binaries. Without this, one
            // `u64::MAX` tombstone makes every future recreation overflow and
            // stay deleted permanently.
            for (id, ms) in tombs.iter_mut() {
                if *ms > ceiling {
                    tracing::warn!(project = %id, deleted_ms = *ms, ceiling_ms = ceiling, "normalizing implausibly future project tombstone");
                    *ms = now;
                }
            }
            for (id, ms) in remote.tombstones {
                if ms > ceiling {
                    tracing::warn!(project = %id, deleted_ms = ms, ceiling_ms = ceiling, "dropping relayed project tombstone with implausibly future generation");
                    continue;
                }
                let e = tombs.entry(id).or_insert(ms);
                if ms > *e {
                    *e = ms;
                }
            }
            // Project deletions are permanent anti-entropy facts. Expiring this
            // set lets a node offline longer than the window return with an old
            // row that every peer now mistakes for a recreation.
        }
        {
            let mut incarnation_tombstones = self.incarnation_tombstones.write();
            for (project, remote_tombstones) in remote.incarnation_tombstones {
                let local_tombstones = incarnation_tombstones.entry(project).or_default();
                for (incarnation, deleted_ms) in remote_tombstones {
                    if deleted_ms > ceiling {
                        tracing::warn!(
                            %incarnation,
                            deleted_ms,
                            ceiling_ms = ceiling,
                            "dropping relayed project-incarnation tombstone with implausibly future generation"
                        );
                        continue;
                    }
                    let current = local_tombstones.entry(incarnation).or_insert(deleted_ms);
                    if deleted_ms > *current {
                        *current = deleted_ms;
                    }
                }
            }
        }
        let tombs = self.tombstones.read().clone();
        let incarnation_tombstones = self.incarnation_tombstones.read().clone();
        let mut map = self.map.write();
        for (name, row) in map.iter_mut() {
            if row.incarnation.is_some() && row.incarnation_created_ms == 0 {
                row.incarnation_created_ms = row.updated_ms;
            }
            if row.incarnation_created_ms > ceiling {
                tracing::warn!(project = %name, created_ms = row.incarnation_created_ms, ceiling_ms = ceiling, "normalizing implausibly future local project incarnation");
                row.incarnation_created_ms = now;
            }
            if row.updated_ms > ceiling {
                tracing::warn!(project = %name, updated_ms = row.updated_ms, ceiling_ms = ceiling, "normalizing implausibly future local project row");
                row.updated_ms = now.max(row.incarnation_created_ms);
            }
        }
        for (name, mut remote_row) in remote.rows {
            if remote_row.incarnation.is_some() && remote_row.incarnation_created_ms == 0 {
                remote_row.incarnation_created_ms = remote_row.updated_ms;
            }
            if remote_row.incarnation_created_ms > ceiling {
                tracing::warn!(project = %name, created_ms = remote_row.incarnation_created_ms, ceiling_ms = ceiling, "dropping relayed project incarnation with implausibly future generation");
                continue;
            }
            if remote_row.updated_ms > ceiling {
                tracing::warn!(project = %name, updated_ms = remote_row.updated_ms, ceiling_ms = ceiling, "dropping relayed project row with implausibly future version");
                continue;
            }
            let take = match map.get(&name) {
                None => true,
                Some(local) => merge_take(local, &remote_row),
            };
            if take {
                // Mixed-version strip guard: a pre-`framework_auto` binary
                // deserializes the row dropping the field (serde default =
                // false), and any local mutation there re-serializes a newer
                // row that would demote our AUTO marker to "explicit" —
                // permanently freezing framework re-detection. When the
                // framework string itself is unchanged, the auto marker is a
                // fact about how that string was WRITTEN and cannot be
                // legitimately revoked by a row that didn't change it.
                if let Some(local) = map.get(&name) {
                    if local.build.framework_auto
                        && !remote_row.build.framework_auto
                        && remote_row.build.framework.trim() == local.build.framework.trim()
                    {
                        remote_row.build.framework_auto = true;
                    }
                }
                map.insert(name, remote_row);
            }
        }
        map.retain(|name, row| match row.incarnation {
            Some(incarnation) => !incarnation_tombstones
                .get(name)
                .is_some_and(|tombstones| tombstones.contains_key(&incarnation)),
            None => match tombs.get(name) {
                Some(deleted_ms) => row.updated_ms > *deleted_ms,
                None => true,
            },
        });
        map.len()
    }

    /// Tombstones for the durable snapshot — a restart must not forget
    /// deletions, or the node re-imports them from any peer (the
    /// `database_tombstones` precedent in `persist.rs`).
    pub fn tombstones_snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        self.tombstones.read().clone()
    }

    /// The project's permanent deletion generation, if tombstoned — the
    /// causal floor every new incarnation's timestamps must dominate
    /// (deployments register with `created_at_ms` floored at this +1, or the
    /// next delete's `remove_project_through` sweeps the fresh incarnation
    /// as "old": adversarial finding).
    pub fn tombstone_of(&self, project: &str) -> Option<u64> {
        self.tombstones.read().get(project).copied()
    }

    pub fn tombstones_load(&self, mut data: std::collections::BTreeMap<String, u64>) {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        for (id, ms) in data.iter_mut() {
            if *ms > ceiling {
                tracing::warn!(project = %id, deleted_ms = *ms, ceiling_ms = ceiling, "normalizing implausibly future persisted project tombstone on load");
                *ms = now;
            }
        }
        // A persisted row dominated by a tombstone in the SAME snapshot dies
        // here (a delete landing between the row-capture and the
        // tombstone-capture wrote both) — the `teams.rs` retain pass this
        // loader previously lacked, letting a deleted project's row briefly
        // revive at boot (adversarial finding).
        {
            let mut m = self.map.write();
            m.retain(|project, row| {
                row.incarnation.is_some()
                    || data
                        .get(project)
                        .is_none_or(|tombstone| row.updated_ms > *tombstone)
            });
        }
        *self.tombstones.write() = data;
    }

    pub fn incarnation_tombstones_snapshot(
        &self,
    ) -> std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
    > {
        self.incarnation_tombstones.read().clone()
    }

    pub fn incarnation_tombstones_load(
        &self,
        mut data: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
        >,
    ) {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        for tombstones in data.values_mut() {
            for deleted_ms in tombstones.values_mut() {
                if *deleted_ms > ceiling {
                    *deleted_ms = now;
                }
            }
        }
        {
            let mut map = self.map.write();
            map.retain(|project, row| match row.incarnation {
                Some(incarnation) => !data
                    .get(project)
                    .is_some_and(|tombstones| tombstones.contains_key(&incarnation)),
                None => true,
            });
        }
        *self.incarnation_tombstones.write() = data;
    }

    // (see merge_synced's equal-updated_ms arm)

    pub fn tombstone_of_incarnation(
        &self,
        project: &str,
        incarnation: fluid_core::ProjectIncarnation,
    ) -> Option<u64> {
        self.incarnation_tombstones
            .read()
            .get(project)
            .and_then(|tombstones| tombstones.get(&incarnation))
            .copied()
    }

    pub fn remove_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
    ) -> Result<ProjectSettings, ProjectIncarnationError> {
        let mut map = self.map.write();
        let active = map.get(project).ok_or_else(|| {
            if self.tombstone_of_incarnation(project, expected).is_some() {
                ProjectIncarnationError::Tombstoned
            } else {
                ProjectIncarnationError::Missing
            }
        })?;
        match active.incarnation {
            Some(incarnation) if incarnation == expected => {}
            Some(active) => return Err(ProjectIncarnationError::Mismatch { active }),
            None => return Err(ProjectIncarnationError::Legacy),
        }
        let row = map.remove(project).ok_or(ProjectIncarnationError::Missing)?;
        let now = now_ms();
        let ceiling = now.saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS);
        let mut all_tombstones = self.incarnation_tombstones.write();
        let tombstones = all_tombstones.entry(project.to_string()).or_default();
        let prior = tombstones.get(&expected).copied().unwrap_or(0).min(ceiling);
        let deleted_ms = now
            .max(row.updated_ms.min(ceiling).saturating_add(1))
            .max(row.incarnation_created_ms.min(ceiling).saturating_add(1))
            .max(prior.saturating_add(1))
            .min(ceiling);
        tombstones.insert(expected, deleted_ms);
        Ok(row)
    }

    pub fn apply_delete_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        deleted_ms: u64,
    ) -> Option<ProjectSettings> {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        let deleted_ms = deleted_ms.min(ceiling);
        let mut map = self.map.write();
        let removed = if map
            .get(project)
            .is_some_and(|row| row.incarnation == Some(expected))
        {
            map.remove(project)
        } else {
            None
        };
        let mut all_tombstones = self.incarnation_tombstones.write();
        let current = all_tombstones
            .entry(project.to_string())
            .or_default()
            .entry(expected)
            .or_insert(deleted_ms);
        if deleted_ms > *current {
            *current = deleted_ms;
        }
        removed
    }

    /// Start a NEW user-requested deletion generation, even when this node has
    /// no settings row. `observed_ms` is the newest deployment/settings version
    /// the coordinator saw anywhere in the fleet; issuing strictly above it
    /// makes this deletion dominate every object it authorized against. The
    /// returned stamp must ride every retry/fanout of this one delete; receivers
    /// apply it idempotently with [`Self::apply_delete`].
    pub fn begin_delete(&self, project: &str, observed_ms: u64) -> u64 {
        let now = now_ms();
        let ceiling = now.saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS);
        let removed_ms = self
            .map
            .write()
            .remove(project)
            .map(|row| row.updated_ms.min(ceiling))
            .unwrap_or(0);
        let observed_ms = observed_ms.min(ceiling);
        let mut tombs = self.tombstones.write();
        let prior = tombs.get(project).copied().unwrap_or(0);
        let prior = if prior > ceiling {
            tracing::warn!(project, deleted_ms = prior, ceiling_ms = ceiling, "discarding implausibly future prior project tombstone before issuing delete generation");
            now
        } else {
            prior
        };
        let deleted_ms = now
            .max(observed_ms.saturating_add(1))
            .max(removed_ms.saturating_add(1))
            .max(prior.saturating_add(1));
        tombs.insert(project.to_string(), deleted_ms);
        drop(tombs);
        Self::remove_relational(project, deleted_ms);
        deleted_ms
    }

    /// Apply one already-issued deletion generation. A retry with the same
    /// stamp cannot ratchet the tombstone forward; a row causally recreated
    /// after that stamp survives.
    pub fn apply_delete(&self, project: &str, deleted_ms: u64) -> bool {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        let deleted_ms = if deleted_ms > ceiling {
            tracing::warn!(
                project,
                deleted_ms,
                ceiling_ms = ceiling,
                "clamping implausibly future project delete generation to the ceiling"
            );
            // Monotone clamp: rewinding to local `now` made the "immutable
            // generation" node-dependent (issuer stored T+11s, receiver
            // stored T) and split the fleet on which records the delete
            // dominates (adversarial finding). The boundary gate
            // `valid_delete_generation` already rejects these loudly; this is
            // the store-layer match for any caller that skips it.
            ceiling
        } else {
            deleted_ms
        };
        let removed = {
            let mut map = self.map.write();
            match map.get(project) {
                Some(row) if row.updated_ms > deleted_ms => false,
                Some(_) => {
                    map.remove(project);
                    true
                }
                None => false,
            }
        };
        let mut tombs = self.tombstones.write();
        let current = tombs.entry(project.to_string()).or_insert(deleted_ms);
        if deleted_ms > *current {
            *current = deleted_ms;
        }
        drop(tombs);
        // An older delete must not erase the relational identity of a row that
        // already proves a later recreation on this node.
        if self
            .map
            .read()
            .get(project)
            .map_or(true, |row| row.updated_ms <= deleted_ms)
        {
            Self::remove_relational(project, deleted_ms);
        }
        removed
    }

    fn remove_relational(project: &str, deleted_ms: u64) {
        let project = project.to_string();
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move { crate::relational::remove_project(&project, deleted_ms).await });
        }
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

    /// Every mutator funnels row access through this: fetch-or-create AND
    /// stamp `updated_ms`, so the replication merge always sees a fresh key
    /// for the row that actually changed.
    fn touch<'a>(
        &self,
        m: &'a mut HashMap<String, ProjectSettings>,
        project: &str,
    ) -> &'a mut ProjectSettings {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        let tombstone_ms = {
            let mut tombstones = self.tombstones.write();
            let ms = tombstones.get(project).copied().unwrap_or(0);
            if ms > ceiling {
                tracing::warn!(
                    project,
                    deleted_ms = ms,
                    ceiling_ms = ceiling,
                    "normalizing implausibly future project tombstone before mutation"
                );
                tombstones.insert(project.to_string(), now);
                now
            } else {
                ms
            }
        };
        let s = m.entry(project.to_string()).or_default();
        if s.updated_ms > ceiling {
            tracing::warn!(
                project,
                updated_ms = s.updated_ms,
                ceiling_ms = ceiling,
                "normalizing implausibly future project row before mutation"
            );
            s.updated_ms = now;
        }
        // STRICTLY monotonic across both live rows and retained tombstones. A
        // causal recreation must be newer than the delete it supersedes, even
        // when wall time has moved backwards or the tombstone came from a peer.
        s.updated_ms = now
            .max(s.updated_ms.saturating_add(1))
            .max(tombstone_ms.saturating_add(1));
        s
    }

    fn mutate_exact<R>(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        apply: impl FnOnce(&mut ProjectSettings) -> R,
    ) -> Result<R, ProjectIncarnationError> {
        let mut map = self.map.write();
        let row = map.get_mut(project).ok_or_else(|| {
            if self.tombstone_of_incarnation(project, expected).is_some() {
                ProjectIncarnationError::Tombstoned
            } else {
                ProjectIncarnationError::Missing
            }
        })?;
        match row.incarnation {
            Some(active) if active == expected => {
                row.updated_ms = now_ms().max(row.updated_ms.saturating_add(1));
                Ok(apply(row))
            }
            Some(active) => Err(ProjectIncarnationError::Mismatch { active }),
            None => Err(ProjectIncarnationError::Legacy),
        }
    }

    pub fn set_build_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        build: BuildConfig,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.build = build)
    }

    pub fn set_functions_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        functions: FunctionSettings,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.functions = functions)
    }

    pub fn set_inference_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        spec: Option<InferenceSpec>,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.inference = spec)
    }

    pub fn set_browser_db_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        spec: Option<fluid_core::BrowserDbPolicy>,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.browser_db = spec)
    }

    pub fn set_container_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        spec: Option<ContainerSettings>,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.container = spec)
    }

    pub fn set_dedicated_ipv4_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        allocation: Option<fluid_core::DedicatedIpv4>,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.dedicated_ipv4 = allocation)
    }

    pub fn set_git_ci_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        status: GitCiStatus,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.git_ci = Some(status))
    }

    pub fn set_build(&self, project: &str, build: BuildConfig) {
        let mut m = self.map.write();
        self.touch(&mut m, project).build = build;
    }

    pub fn set_functions(&self, project: &str, f: FunctionSettings) {
        let mut m = self.map.write();
        self.touch(&mut m, project).functions = f;
    }

    pub fn set_inference(&self, project: &str, spec: Option<InferenceSpec>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).inference = spec;
    }

    /// See [`ProjectSettings::browser_db`]. `None` clears the dashboard-managed
    /// opt-in (the Storages page's delete/disable action) — it does not touch
    /// an explicit fluid.json block, which still wins on the next build.
    pub fn set_browser_db(&self, project: &str, spec: Option<fluid_core::BrowserDbPolicy>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).browser_db = spec;
    }

    /// Mint/rotate/clear one of this project's `browser_db` REST credentials.
    /// `public` selects which of the two independent slots (team vs public
    /// scope) is written — the `drive_api::webdav_token_mint` pattern,
    /// doubled for the two admission scopes this surface must respect.
    pub fn set_browser_db_rest_token(
        &self,
        project: &str,
        public: bool,
        token: Option<BrowserDbRestToken>,
    ) {
        let mut m = self.map.write();
        let s = self.touch(&mut m, project);
        if public {
            s.browser_db_rest_public = token;
        } else {
            s.browser_db_rest_team = token;
        }
    }

    pub fn set_browser_db_rest_token_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        public: bool,
        token: Option<BrowserDbRestToken>,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            if public {
                row.browser_db_rest_public = token;
            } else {
                row.browser_db_rest_team = token;
            }
        })
    }

    /// See [`ProjectSettings::container`]. `None` clears the dashboard-managed
    /// config — it does not touch an explicit fluid.json `container` block,
    /// which still wins on the next build (the `browser_db`/`set_browser_db`
    /// precedent).
    pub fn set_container(&self, project: &str, spec: Option<ContainerSettings>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).container = spec;
    }

    /// See [`ProjectSettings::dedicated_ipv4`]. Called exactly once per
    /// project by `tencent_eip::provision_from_checkout` on a successful
    /// allocation — this row IS the durable idempotency claim (replicated
    /// fleet-wide via `store_sync::REGISTRY`'s "projects" entry), so a
    /// second confirmation firing for the same project is a no-op read, not
    /// a second Tencent purchase.
    pub fn set_dedicated_ipv4(&self, project: &str, alloc: Option<fluid_core::DedicatedIpv4>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).dedicated_ipv4 = alloc;
    }

    pub fn set_git_ci(&self, project: &str, status: GitCiStatus) {
        let mut m = self.map.write();
        self.touch(&mut m, project).git_ci = Some(status);
    }

    /// Add or update an env var (by key+target). Sensitive values are sealed
    /// (encrypted at rest) before storing, so they're persisted as secrets — never
    /// written to disk / the replicated snapshot in plaintext.
    pub fn put_env(&self, project: &str, value: EnvVar) {
        let mut map = self.map.write();
        upsert_env(self.touch(&mut map, project), value);
    }

    pub fn put_env_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        value: EnvVar,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| upsert_env(row, value))
    }

    pub fn delete_env(&self, project: &str, key: &str) {
        let mut m = self.map.write();
        let Some(s) = m.get(project) else {
            return;
        };
        if !s.env.iter().any(|e| e.key == key) {
            return;
        }
        self.touch(&mut m, project).env.retain(|e| e.key != key);
    }

    pub fn delete_env_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        key: &str,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.env.retain(|entry| entry.key != key)
        })
    }

    pub fn set_team(&self, project: &str, team: &str) {
        let (root_dir, updated_ms) = {
            let mut m = self.map.write();
            let s = self.touch(&mut m, project);
            s.team = team.to_string();
            (s.build.root_dir.clone(), s.updated_ms)
        };
        // Best-effort fleet-replicated mirror (see relational.rs's module doc):
        // the durable fix for a project being invisible on any node lacking
        // this LOCAL row — every node's own replica converges within seconds,
        // regardless of which node actually owns this in-memory row.
        let (project, team, root_dir) = (project.to_string(), team.to_string(), root_dir);
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                crate::relational::set_project_team(&project, &team, &root_dir, updated_ms).await
            });
        }
    }

    pub fn set_team_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        team: &str,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.team = team.to_string())
    }

    /// Persist the monorepo subdirectory so redeploys keep building it.
    pub fn set_root_dir(&self, project: &str, root_dir: &str) {
        let mut m = self.map.write();
        self.touch(&mut m, project).build.root_dir = root_dir.to_string();
    }

    pub fn set_root_dir_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        root_dir: &str,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.build.root_dir = root_dir.to_string()
        })
    }

    /// The configured root/subdirectory for a project ("" if none).
    pub fn root_dir_of(&self, project: &str) -> String {
        self.map
            .read()
            .get(project)
            .map(|s| s.build.root_dir.clone())
            .unwrap_or_default()
    }

    /// The project's production branch ("" if not yet classified).
    pub fn production_branch_of(&self, project: &str) -> String {
        self.map
            .read()
            .get(project)
            .map(|s| s.production_branch.clone())
            .unwrap_or_default()
    }

    /// Set the production branch (Vercel "Production Branch"). Called once on the
    /// project's first git deploy, or explicitly from project settings.
    pub fn set_production_branch(&self, project: &str, branch: &str) {
        let mut m = self.map.write();
        self.touch(&mut m, project).production_branch = branch.to_string();
    }

    pub fn set_production_branch_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        branch: &str,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.production_branch = branch.to_string()
        })
    }

    pub fn set_preview_protection(&self, project: &str, on: bool) {
        let mut m = self.map.write();
        self.touch(&mut m, project).preview_protection = on;
    }

    pub fn set_preview_protection_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        on: bool,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.preview_protection = on)
    }

    pub fn set_cron_enabled(&self, project: &str, on: bool) {
        let mut m = self.map.write();
        self.touch(&mut m, project).cron_enabled = on;
    }

    pub fn set_cron_enabled_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        on: bool,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| row.cron_enabled = on)
    }

    /// Whether this project's cron jobs are allowed to fire (default true —
    /// an unknown/never-configured project is not disabled).
    pub fn cron_enabled(&self, project: &str) -> bool {
        self.map
            .read()
            .get(project)
            .map(|s| s.cron_enabled)
            .unwrap_or(true)
    }

    /// Team slug owning a project. A project absent from this store (never
    /// `set_team`'d) is UNOWNED — resolves to `UNTAGGED_TENANT`, never the
    /// owner's real "personal" namespace (see `default_team`).
    pub fn team_of(&self, project: &str) -> String {
        // A row whose team was stored EMPTY (tag loss during a gossip/restore
        // race) is just as unowned as a missing row — returning "" verbatim
        // let downstream consumers (preview-unlock's team query param, JS's
        // falsy-string fallback) silently collapse it into the owner-only
        // "personal" namespace and deny the real owner.
        self.map
            .read()
            .get(project)
            .map(|s| s.team.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(default_team)
    }

    /// Count of projects owned by a team/tenant (for plan-quota enforcement).
    pub fn count_for_team(&self, team: &str) -> usize {
        let t = team.trim().to_lowercase();
        self.map
            .read()
            .values()
            .filter(|s| s.team.trim().to_lowercase() == t)
            .count()
    }

    /// Whether previews for a project are protected (defaults to true).
    pub fn preview_protected(&self, project: &str) -> bool {
        self.map
            .read()
            .get(project)
            .map(|s| s.preview_protection)
            .unwrap_or(true)
    }

    pub fn add_domain(&self, project: &str, domain: String) {
        let mut m = self.map.write();
        let s = self.touch(&mut m, project);
        if !s.domains.contains(&domain) {
            s.domains.push(domain);
        }
    }

    pub fn add_domain_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        domain: String,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            if !row.domains.contains(&domain) {
                row.domains.push(domain);
            }
        })
    }

    /// Detach a domain from a project (idempotent).
    pub fn remove_domain(&self, project: &str, domain: &str) {
        let mut m = self.map.write();
        let s = self.touch(&mut m, project);
        s.domains.retain(|d| d != domain);
    }

    pub fn remove_domain_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        domain: &str,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.domains.retain(|candidate| candidate != domain)
        })
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

    /// Env for ONE environment — `"production"` | `"preview"` |
    /// `"development"`. A var applies when its `target` is `"all"` or matches;
    /// an empty/unknown target is treated as `"production"` (the field's own
    /// serde default), so nothing silently widens.
    ///
    /// `EnvVar::target` and the dashboard's Production/Preview/Development
    /// selector existed, but every consumer used the unfiltered [`env_map`] —
    /// so a preview deployment (any non-production branch, any PR) launched
    /// with the project's PRODUCTION secrets. The isolation the UI promised
    /// was never enforced anywhere.
    pub fn env_map_for(
        &self,
        project: &str,
        environment: &str,
    ) -> std::collections::BTreeMap<String, String> {
        let want = environment.trim().to_ascii_lowercase();
        self.get(project)
            .env
            .into_iter()
            .filter(|e| {
                let t = e.target.trim().to_ascii_lowercase();
                t == "all" || t == want || (t.is_empty() && want == "production")
            })
            .map(|e| (e.key, crate::secrets::decrypt(&e.value)))
            .collect()
    }
}

fn upsert_env(row: &mut ProjectSettings, mut value: EnvVar) {
    value.updated_ms = now_ms();
    if !value.sensitive && looks_like_secret(&value.value) {
        value.sensitive = true;
    }
    if value.value.is_empty() {
        if let Some(previous) = row.env.iter().find(|entry| entry.key == value.key) {
            value.value = previous.value.clone();
            if value.sensitive && !previous.sensitive && !value.value.is_empty() {
                value.value = crate::secrets::encrypt(&value.value);
            }
        }
    } else if value.sensitive {
        value.value = crate::secrets::encrypt(&value.value);
    }
    if let Some(existing) = row.env.iter_mut().find(|entry| entry.key == value.key) {
        *existing = value;
    } else {
        row.env.push(value);
    }
}

fn merge_take(local: &ProjectSettings, remote: &ProjectSettings) -> bool {
    match (local.incarnation, remote.incarnation) {
        (Some(_), None) => false,
        (None, Some(_)) => true,
        (Some(local_incarnation), Some(remote_incarnation))
            if local_incarnation != remote_incarnation =>
        {
            (remote.incarnation_created_ms, remote_incarnation)
                > (local.incarnation_created_ms, local_incarnation)
        }
        _ if remote.updated_ms != local.updated_ms => remote.updated_ms > local.updated_ms,
        _ => tie_break_take(local, remote),
    }
}

/// Deterministic winner for two equal-`updated_ms` rows so replication
/// converges instead of each node keeping its own. Compare the canonical JSON
/// encoding of the WHOLE row, not one field: equal-generation writes can differ
/// in team, build config, domains, function policy, inference, database grants,
/// or env. Both observers deserialize the same two rows and compute the same
/// lexical winner, regardless of which one is local.
fn tie_break_take(local: &ProjectSettings, remote: &ProjectSettings) -> bool {
    match (serde_json::to_vec(local), serde_json::to_vec(remote)) {
        (Ok(local), Ok(remote)) => remote > local,
        // These structs contain no serialization-fallible values. Keep a
        // deterministic direction even if a future field violates that
        // invariant, rather than restoring the equal-version split brain.
        (Err(_), Ok(_)) => true,
        _ => false,
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
        EnvVar {
            key: key.into(),
            value: value.into(),
            target: "all".into(),
            sensitive,
            updated_ms: 0,
        }
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
        assert_eq!(
            s.get("app").env.iter().filter(|e| e.key == "FOO").count(),
            1
        );
        assert_eq!(s.env_map("app").get("FOO").map(|v| v.as_str()), Some("baz"));
    }

    #[test]
    fn sensitive_value_is_encrypted_at_rest_masked_and_decrypted_for_runtime() {
        let s = ProjectStore::new();
        s.put_env("app", ev("API_TOKEN", "supersecret", true));
        // Stored at rest: not plaintext (encrypted blob).
        let stored = s
            .get("app")
            .env
            .iter()
            .find(|e| e.key == "API_TOKEN")
            .unwrap()
            .value
            .clone();
        assert_ne!(stored, "supersecret");
        assert!(
            crate::secrets::is_encrypted(&stored),
            "sensitive env sealed at rest"
        );
        // Masked view blanks the value.
        let masked = s
            .get_masked("app")
            .env
            .iter()
            .find(|e| e.key == "API_TOKEN")
            .unwrap()
            .value
            .clone();
        assert_eq!(masked, "");
        // Runtime injection decrypts back to plaintext.
        assert_eq!(
            s.env_map("app").get("API_TOKEN").map(|v| v.as_str()),
            Some("supersecret")
        );
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
        // Unset team is UNOWNED, never the owner's real "personal" namespace
        // (see UNTAGGED_TENANT / default_team's doc comment — the multitenancy
        // leak this default used to cause).
        assert_eq!(s.team_of("app"), crate::admin::UNTAGGED_TENANT);
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
            assert_eq!(
                s.find_key_ci(probe),
                old,
                "find_key_ci disagrees for {probe}"
            );
        }
    }

    #[test]
    fn snapshot_load_roundtrip_preserves_sealed_env() {
        let s = ProjectStore::new();
        s.put_env("app", ev("SECRET", "v", true));
        let snap = s.snapshot();
        let s2 = ProjectStore::new();
        s2.load(snap);
        assert_eq!(
            s2.env_map("app").get("SECRET").map(|v| v.as_str()),
            Some("v")
        );
    }
}
