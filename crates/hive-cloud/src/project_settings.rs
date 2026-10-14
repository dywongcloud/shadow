//! Per-project settings (env vars, build config, function settings) keyed by project name, so they
//! persist across deployments: saved through `persist::persist` into `PlatformSnapshot.projects`,
//! restored on boot.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

/// The one writer gate for a project's lifecycle: admission, final deployment registration,
/// promotion and deletion take this keyed mutex before deciding which incarnation is active.
/// Entries are `Weak` so the registry stays bounded.
static LIFECYCLE_WRITERS: OnceLock<
    parking_lot::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();

pub(crate) async fn lifecycle_write(project: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let key = project.trim().to_ascii_lowercase();
    let lock = {
        let mut writers = LIFECYCLE_WRITERS
            .get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
            .lock();
        writers.retain(|_, lock| lock.strong_count() > 0);
        match writers.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                writers.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    /// "production" | "preview" | "development" | "all"
    #[serde(default = "default_target")]
    pub target: String,
    /// "runtime" | "build" | "all". Legacy rows lack it and default to runtime-only, so the field
    /// cannot expose stored runtime credentials to repository-controlled builds.
    #[serde(default = "default_env_scope")]
    pub scope: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub updated_ms: u64,
}
fn default_target() -> String {
    "production".into()
}
fn default_env_scope() -> String {
    "runtime".into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvExecutionScope {
    Build,
    Runtime,
}

/// Does this value look like a live credential? Forces `sensitive=true` when the caller did not
/// ask: an unticked "Sensitive" box must never leak a plaintext credential through the
/// settings/gitops reads. Precise prefix/shape matches, not entropy scoring, so ordinary config is
/// not masked.
fn looks_like_secret(v: &str) -> bool {
    let s = v.trim();
    if s.is_empty() {
        return false;
    }
    const GITHUB_TOKENS: &[&str] = &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];
    const AWS_ACCESS_KEY_IDS: &[&str] = &["AKIA", "ASIA"];
    const STRIPE_KEYS: &[&str] = &["sk_live_", "sk_test_", "rk_live_", "rk_test_"];
    const NPM_TOKENS: &[&str] = &["npm_"];
    const SLACK_TOKENS: &[&str] = &["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"];
    const GOOGLE_API_KEYS: &[&str] = &["AIza"];
    const ANTHROPIC_AND_OPENAI_KEYS: &[&str] = &["sk-ant-", "sk-proj-", "sk-"];
    const PEM_PRIVATE_KEY_BLOCKS: &[&str] = &["-----BEGIN "];
    const PREFIX_GROUPS: [&[&str]; 8] = [
        GITHUB_TOKENS,
        AWS_ACCESS_KEY_IDS,
        STRIPE_KEYS,
        NPM_TOKENS,
        SLACK_TOKENS,
        GOOGLE_API_KEYS,
        ANTHROPIC_AND_OPENAI_KEYS,
        PEM_PRIVATE_KEY_BLOCKS,
    ];
    if PREFIX_GROUPS
        .iter()
        .any(|group| group.iter().any(|p| s.starts_with(p)))
    {
        return true;
    }
    if looks_like_jwt(s) {
        return true;
    }
    false
}

/// A JWT is three base64url segments joined by dots, the header starting with `eyJ` (base64 of `{"`).
fn looks_like_jwt(s: &str) -> bool {
    s.starts_with("eyJ") && s.matches('.').count() == 2
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildConfig {
    pub framework: String,
    pub install_command: String,
    pub build_command: String,
    pub output_dir: String,
    pub root_dir: String,
    /// Explicit runtime override ("nodejs"/"bun"/empty = infer from the detected start command).
    /// Independent of package-manager detection: a `bun.lock` selects `bun install`, never the Bun
    /// RUNTIME.
    #[serde(default)]
    pub runtime: String,
    /// True when `framework` came from build-time auto-detection, not an explicit user choice: an
    /// auto-detected slug is never an override and detection re-runs every build, so a first-build
    /// misdetection cannot pin the wrong framework. Defaults to false, so already-persisted
    /// frameworks keep explicit-override behaviour.
    #[serde(default)]
    pub framework_auto: bool,
}
impl Default for BuildConfig {
    /// Every field empty: a non-empty value is an explicit override, so a default `npm install`
    /// would override detected pnpm/yarn and break monorepo workspaces.
    fn default() -> Self {
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
    /// Per-instance CPU tier; the default Standard tier is 1 vCPU / 2048 MiB (`memory_mib`).
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    pub memory_mib: u32,
    /// Serverless GPU: functions run only on nodes advertising `gpu_count > 0`, with the host GPUs
    /// passed through to the cell.
    #[serde(default)]
    pub gpu: bool,
    /// Whether this project's functions use its paid dedicated public IPv4
    /// ([`ProjectSettings::dedicated_ipv4`]). Unlike `gpu`, `git.rs` ASSIGNS it over a function's
    /// own fluid.json `dedicatedIpv4` instead of OR-merging, so a manifest cannot self-grant it.
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
            regions: vec![],
            failover: false,
            vcpus: 1,
            memory_mib: 2048,
            gpu: false,
            dedicated_ipv4: false,
        }
    }
}

/// The `fluid.json` `inference` block: `model` is a direct GGUF URL or an `org/repo/file.gguf`
/// HuggingFace path; `pool: true` lets the platform combine same-region GPU nodes (llama.cpp RPC)
/// when the model exceeds one node's free VRAM. The platform injects `HIVE_INFERENCE_URL` into the
/// project env.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InferenceSpec {
    pub model: String,
    #[serde(default)]
    pub pool: bool,
}

/// Dashboard-managed config of a CONTAINER-runtime project (a single-Dockerfile deploy): the
/// persisted equivalent of `fluid.json`'s `container` override block (`ContainerOverride`, same
/// fields and string formats), which always wins on the next build. A `None` field uses the
/// platform default, independently of the others.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContainerSettings {
    /// The port the container listens on inside the image.
    #[serde(default)]
    pub port: Option<u16>,
    /// Wire protocol: "http" (default), "tcp", "udp" or "grpc" (`fluid_core::ServiceProtocol`). A
    /// wrong one ("udp" for a TCP-only server) leaves the deployment unreachable although it builds
    /// and runs.
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
    /// Mount path of the automatic persistent volume inside the container (default `/data`,
    /// `container_volume_path()`), overridden for this project alone without a node-wide
    /// `HIVE_CONTAINER_VOLUME_PATH`.
    #[serde(default)]
    pub volume_mount_path: Option<String>,
}

/// A minted REST/Hrana credential for a project's `browser_db` replica: shown once at mint time,
/// stored only as its SHA-256 hash, checked constant-time by `browser_db_rest::credential_scope`. A
/// plain bearer token, separate from the QUIC admission lease `browser_admission` issues for the
/// CRR sync protocol.
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
    /// Non-zero only when this incarnation upgraded an already-live legacy row: it authorizes a
    /// one-time, exact-ID adoption of deployment records visible while the row had no identity.
    /// Fresh projects and ambiguous post-tombstone legacy rows leave it zero.
    #[serde(default)]
    pub legacy_incarnation_upgrade_ms: u64,
    /// Platform-issued deployment ids observed before or during the legacy-row upgrade: a
    /// `project_incarnation: None` record may be stamped only when its exact id is here; name or
    /// timestamp proximity is never proof.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_deployment_ids: Vec<String>,
    /// Exact platform-issued database ids admitted during this incarnation. Grow-only: deleting a
    /// database keeps this ownership fact so a delayed completion is never mistaken for another
    /// same-name project's resource. Pre-upgrade rows have an empty set and retain legacy databases
    /// on project deletion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_database_ids: Vec<String>,
    /// Exact incarnation-qualified persistent-volume names emitted by a build; survives deployment
    /// deletion so deleting the project can still reclaim a compose service's volume without
    /// deriving ownership from a name prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_volume_names: Vec<String>,
    /// Last write to THIS row on THIS node (ms epoch), the per-row freshness key `merge_synced`
    /// compares: the newest write wins a replication collision and a tombstone at-or-after it
    /// deletes the row. `0` merges fail-safe (never overwrites, never overwritten by another
    /// 0-row).
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
    /// Dashboard-managed browser-replicated database opt-in (the Storages page); presence is the
    /// opt-in. A SEPARATE source from the manifest's `browser_db`: `git.rs` syncs an explicit
    /// fluid.json block into this mirror and, when fluid.json declares none, merges THIS spec into
    /// the manifest at build time. An explicit fluid.json block always wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db: Option<fluid_core::BrowserDbPolicy>,
    /// Team-scope (read+write) REST/Hrana credential for this project's
    /// `browser_db` replica, if minted. See [`BrowserDbRestToken`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db_rest_team: Option<BrowserDbRestToken>,
    /// Public-scope (read-only) REST/Hrana credential, mintable only while `browser_db.public_read`
    /// is enabled and re-checked on every request, so disabling `public_read` stops it at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db_rest_public: Option<BrowserDbRestToken>,
    /// Dashboard-managed container config (port, protocol, resource ceilings, volume mount path),
    /// meaningful only while the current deployment runs a `container` function. Presence is the
    /// opt-in; an explicit fluid.json `container` block always wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerSettings>,
    /// The project's paid dedicated IPv4 allocation (`tencent_eip::provision_from_checkout`),
    /// attached to a function only while `FunctionSettings::dedicated_ipv4` is on (`git.rs`).
    /// `None` until purchased; kept across redeploys so the same address is re-adopted instead of a
    /// second one being bought.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedicated_ipv4: Option<fluid_core::DedicatedIpv4>,
    #[serde(default)]
    pub domains: Vec<String>,
    /// Team that owns this project (slug); an untagged row is UNOWNED, see `default_team`.
    #[serde(default = "default_team")]
    pub team: String,
    /// The Git branch whose deployments are PRODUCTION (Vercel's "Production Branch"), set to the
    /// imported branch on first deploy; every other branch or PR deploys a preview. Empty until the
    /// first git deploy classifies it.
    #[serde(default)]
    pub production_branch: String,
    /// When true (default), preview deployments are only reachable by team
    /// members — anonymous requests to a preview host get a 401.
    #[serde(default = "default_true")]
    pub preview_protection: bool,
    /// Explicit project-level opt-in for `sudo` inside this project's Sandboxes (default false);
    /// the ZeroTrust rule "sudo only if enabled by project policy" reads this flag.
    #[serde(default)]
    pub sandbox_allow_sudo: bool,
    /// When false this project's cron jobs stop firing (the scheduler skips invocation) while
    /// deployments keep creating, updating and deleting them (`vercel.json`) and they keep
    /// advancing their schedule: Vercel's project-level Cron Jobs kill switch. Default true.
    #[serde(default = "default_true")]
    pub cron_enabled: bool,
    /// Outcome of the auto-CI install attempted after the first git import
    /// (`/api/gitops/project-ci`): whether a GitHub webhook or the Actions-workflow fallback was
    /// installed on the source repo. Persisted so the dashboard can surface a project that got
    /// neither (no push would ever auto-deploy) and offer a retry.
    #[serde(default)]
    pub git_ci: Option<GitCiStatus>,
    /// Terminal outcome of the newest build the coordinator finished for this incarnation; an
    /// in-flight build is derived from `last_build_id` against the live `BuildStore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_build: Option<LastBuild>,
    /// Newest build admitted for this incarnation (empty = none).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_build_id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_build_started_ms: u64,
    /// Grow-only: when this incarnation's first deployment registered; `0` = never deployed, which is
    /// what makes the name reusable by its own tenant.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub first_deployed_ms: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Longest `LastBuild::error` kept in the replicated row.
pub const MAX_LAST_BUILD_ERROR_BYTES: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LastBuildState {
    Ready,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastBuild {
    pub id: String,
    pub state: LastBuildState,
    pub started_ms: u64,
    pub finished_ms: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
}

impl LastBuild {
    /// `None` while the build is still Queued/Building.
    pub fn from_build(build: &crate::git::Build) -> Option<Self> {
        let state = match build.state {
            fluid_core::DeployState::Ready => LastBuildState::Ready,
            fluid_core::DeployState::Error => LastBuildState::Error,
            fluid_core::DeployState::Cancelled => LastBuildState::Cancelled,
            fluid_core::DeployState::Queued | fluid_core::DeployState::Building => return None,
        };
        let mut error = build.error.clone().unwrap_or_default();
        if error.len() > MAX_LAST_BUILD_ERROR_BYTES {
            let mut cut = MAX_LAST_BUILD_ERROR_BYTES;
            while !error.is_char_boundary(cut) {
                cut -= 1;
            }
            error.truncate(cut);
            error.push('…');
        }
        Some(LastBuild {
            id: build.id.clone(),
            state,
            started_ms: build.started_ms,
            finished_ms: build.finished_ms.unwrap_or(build.started_ms),
            repo_url: build.repo_url.clone(),
            branch: build.branch.clone(),
            error,
            deployment_id: build.deployment_id.clone(),
        })
    }
}

/// See [`ProjectSettings::git_ci`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitCiStatus {
    pub webhook_installed: bool,
    pub workflow_installed: bool,
    /// Set only when neither is installed, e.g. "github-not-connected", "bad-repo",
    /// "composio-not-configured" (the `reason` values `/api/gitops/project-ci` returns).
    #[serde(default)]
    pub skipped_reason: String,
    pub checked_ms: u64,
}
/// A row with no explicit owner is UNOWNED (`admin::UNTAGGED_TENANT`), never the owner's "personal"
/// namespace.
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
            legacy_incarnation_upgrade_ms: 0,
            legacy_deployment_ids: Vec::new(),
            owned_database_ids: Vec::new(),
            owned_volume_names: Vec::new(),
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
            last_build: None,
            last_build_id: String::new(),
            last_build_started_ms: 0,
            first_deployed_ms: 0,
        }
    }
}

/// Replication payload for the projects store: rows plus the tombstones that explain absences,
/// because a wholesale-replace adoption makes "the sender lacks this row" and "this row was
/// deleted" the same event.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncedProjects {
    pub rows: std::collections::BTreeMap<String, ProjectSettings>,
    #[serde(default)]
    pub tombstones: std::collections::BTreeMap<String, u64>,
    /// Renamed-away names, replicated so EVERY node reserves the old name (see [`SyncedProjects`]).
    #[serde(default)]
    pub rename_tombstones: std::collections::BTreeMap<String, ProjectRenameTombstone>,
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

#[derive(Debug)]
pub enum ProjectClaimError {
    OwnedByOther,
    Incarnation(ProjectIncarnationError),
}

impl std::fmt::Display for ProjectClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectClaimError::OwnedByOther => write!(f, "project belongs to a different team"),
            ProjectClaimError::Incarnation(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ProjectClaimError {}

/// Old names a project was renamed FROM (old name -> new name), rebuilt at boot from the persisted
/// rename tombstones. A rename never moves a podman volume or checkout dir (live mounts, the fixed
/// 2048-lock pool, `rm` needing `-v`), so scanners that match on-disk NAME PREFIXES must also look
/// under the old names or those artifacts leak.
static RENAMED_FROM: std::sync::OnceLock<
    parking_lot::RwLock<std::collections::BTreeMap<String, String>>,
> = std::sync::OnceLock::new();

pub(crate) fn record_rename(from: &str, to: &str) {
    if from.is_empty() || to.is_empty() || from == to {
        return;
    }
    RENAMED_FROM
        .get_or_init(|| parking_lot::RwLock::new(Default::default()))
        .write()
        .insert(from.to_string(), to.to_string());
}

/// Every name `project` was previously known by, transitively (`a -> b -> c` yields `[a, b]` for
/// `c`), bounded by `MAX_HOPS` so a cycle in a corrupted map cannot spin.
pub(crate) fn previous_names(project: &str) -> Vec<String> {
    const MAX_HOPS: usize = 16;
    let Some(map) = RENAMED_FROM.get() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let mut cur = project.to_string();
    for _ in 0..MAX_HOPS {
        let prev = {
            let m = map.read();
            m.iter()
                .find(|(_, new)| new.as_str().eq_ignore_ascii_case(&cur))
                .map(|(old, _)| old.clone())
        };
        match prev {
            Some(old) => {
                if out.iter().any(|p| p == &old) {
                    break;
                }
                out.push(old.clone());
                cur = old;
            }
            None => break,
        }
    }
    out
}

/// Checkout-dir prefixes a project's artifacts may carry: [`crate::git::checkout_prefixes`] of its
/// CURRENT name plus of every name it was renamed FROM.
pub(crate) fn checkout_prefixes_including_previous(project: &str) -> Vec<String> {
    let mut v = crate::git::checkout_prefixes(project);
    for old in previous_names(project) {
        for prefix in crate::git::checkout_prefixes(&old) {
            if !v.contains(&prefix) {
                v.push(prefix);
            }
        }
    }
    v
}

/// Store keyed by project name.
pub struct ProjectStore {
    map: RwLock<HashMap<String, ProjectSettings>>,
    /// project -> deletion ms, so absence replicates EXPLICITLY (see [`SyncedProjects`]). Retained
    /// permanently: an offline node can return after any bounded GC window and would resurrect a
    /// deleted project.
    tombstones: RwLock<std::collections::BTreeMap<String, u64>>,
    /// old project name -> where it went. Permanent and platform-global: the old name stays
    /// reserved (no new project may inherit its traffic) and answers with a permanent redirect (see
    /// [`ProjectStore::rename_row`]); the newest rename wins, since a name can be renamed,
    /// re-created and renamed again.
    rename_tombstones: RwLock<std::collections::BTreeMap<String, ProjectRenameTombstone>>,
    incarnation_tombstones: RwLock<
        std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
        >,
    >,
}

/// Where a renamed-away project name went. See [`ProjectStore::rename_row`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectRenameTombstone {
    /// The project's name AFTER the rename.
    pub new: String,
    /// The incarnation the rename carried across (invariant under rename).
    #[serde(default)]
    pub incarnation: Option<fluid_core::ProjectIncarnation>,
    /// Generation of the rename — the causal floor the new row dominates.
    pub rename_ms: u64,
}

/// A delete issuer may lead by one skew allowance and a receiver trail by one.
const ISSUER_AND_RECEIVER_SKEW_ALLOWANCES: u64 = 2;

/// Receive-side ceiling for stored generations; `begin_delete` issues under the narrower one-skew
/// ceiling.
fn project_time_ceiling(now: u64) -> u64 {
    now.saturating_add(
        hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS * ISSUER_AND_RECEIVER_SKEW_ALLOWANCES,
    )
}

impl ProjectStore {
    pub fn new() -> ProjectStore {
        ProjectStore {
            map: RwLock::new(HashMap::new()),
            tombstones: RwLock::new(std::collections::BTreeMap::new()),
            rename_tombstones: RwLock::new(std::collections::BTreeMap::new()),
            incarnation_tombstones: RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    pub fn get(&self, project: &str) -> ProjectSettings {
        self.map.read().get(project).cloned().unwrap_or_default()
    }

    /// Return the active server-issued incarnation, minting one for a new project or upgrading a
    /// live legacy row in place (team, env, build settings and domains are preserved; only identity
    /// and causal timestamps change).
    pub fn ensure_incarnation(
        &self,
        project: &str,
    ) -> Result<fluid_core::ProjectIncarnation, ProjectIncarnationError> {
        let mut map = self.map.write();
        if let Some(row) = map.get_mut(project) {
            if let Some(incarnation) = row.incarnation {
                return Ok(incarnation);
            }
            let incarnation = fluid_core::ProjectIncarnation::mint();
            let legacy_floor = self.tombstones.read().get(project).copied().unwrap_or(0);
            let incarnation_floor = self
                .incarnation_tombstones
                .read()
                .get(project)
                .and_then(|tombstones| tombstones.values().copied().max())
                .unwrap_or(0);
            let legacy_adoption_unambiguous = incarnation_floor == 0;
            let created_ms = now_ms()
                .max(row.updated_ms.saturating_add(1))
                .max(legacy_floor.saturating_add(1))
                .max(incarnation_floor.saturating_add(1));
            row.incarnation = Some(incarnation);
            row.incarnation_created_ms = created_ms;
            row.legacy_incarnation_upgrade_ms = legacy_adoption_unambiguous
                .then_some(created_ms)
                .unwrap_or(0);
            row.legacy_deployment_ids.clear();
            row.owned_database_ids.clear();
            row.owned_volume_names.clear();
            row.updated_ms = created_ms;
            return Ok(incarnation);
        }
        drop(map);
        self.create(project)
    }

    /// Adopt the coordinator-issued incarnation on an authenticated internal fanout: a legacy row
    /// upgrades without losing settings, an identified row must match exactly, and a deleted
    /// incarnation is never reusable, even without a live row here.
    pub fn ensure_incarnation_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
    ) -> Result<fluid_core::ProjectIncarnation, ProjectIncarnationError> {
        if self.tombstone_of_incarnation(project, expected).is_some() {
            return Err(ProjectIncarnationError::Tombstoned);
        }
        let mut map = self.map.write();
        match map.get_mut(project) {
            Some(row) if row.incarnation == Some(expected) => Ok(expected),
            Some(row) if row.incarnation.is_none() => {
                let legacy_floor = self.tombstones.read().get(project).copied().unwrap_or(0);
                let incarnation_floor = self
                    .incarnation_tombstones
                    .read()
                    .get(project)
                    .and_then(|tombstones| tombstones.values().copied().max())
                    .unwrap_or(0);
                let legacy_adoption_unambiguous = incarnation_floor == 0;
                let created_ms = now_ms()
                    .max(row.updated_ms.saturating_add(1))
                    .max(legacy_floor.saturating_add(1))
                    .max(incarnation_floor.saturating_add(1));
                row.incarnation = Some(expected);
                row.incarnation_created_ms = created_ms;
                row.legacy_incarnation_upgrade_ms = legacy_adoption_unambiguous
                    .then_some(created_ms)
                    .unwrap_or(0);
                row.legacy_deployment_ids.clear();
                row.owned_database_ids.clear();
                row.owned_volume_names.clear();
                row.updated_ms = created_ms;
                Ok(expected)
            }
            Some(row) => Err(ProjectIncarnationError::Mismatch {
                active: row.incarnation.expect("matched None above"),
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
                        incarnation: Some(expected),
                        incarnation_created_ms: created_ms,
                        updated_ms: created_ms,
                        ..ProjectSettings::default()
                    },
                );
                Ok(expected)
            }
        }
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

    /// Bind the exact deployment ids observed while a row was still legacy to the incarnation that
    /// upgraded it. The caller holds [`lifecycle_write`]; later calls only add ids to the same
    /// migration window, and fresh rows or rows with prior incarnation tombstones refuse by
    /// returning an empty set.
    pub fn authorize_legacy_deployments(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        ids: impl IntoIterator<Item = String>,
    ) -> Result<Vec<String>, ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            if row.legacy_incarnation_upgrade_ms == 0
                || row.legacy_incarnation_upgrade_ms != row.incarnation_created_ms
            {
                return Vec::new();
            }
            row.legacy_deployment_ids.extend(ids);
            row.legacy_deployment_ids.sort();
            row.legacy_deployment_ids.dedup();
            row.legacy_deployment_ids.clone()
        })
    }

    pub fn legacy_deployment_ids_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
    ) -> Result<Vec<String>, ProjectIncarnationError> {
        self.with_active(project, expected, |row| {
            if row.legacy_incarnation_upgrade_ms == row.incarnation_created_ms {
                row.legacy_deployment_ids.clone()
            } else {
                Vec::new()
            }
        })
    }

    pub fn claim_database_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        id: String,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.owned_database_ids.push(id);
            row.owned_database_ids.sort();
            row.owned_database_ids.dedup();
        })
    }

    pub fn claim_volumes_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        names: impl IntoIterator<Item = String>,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.owned_volume_names.extend(names);
            row.owned_volume_names.sort();
            row.owned_volume_names.dedup();
        })
    }

    /// Full snapshot (project -> settings) for persistence.
    pub fn snapshot(&self) -> HashMap<String, ProjectSettings> {
        self.map.read().clone()
    }

    /// Every project carrying a `browser_db` opt-in, as (project, policy): one call for the Storage
    /// page's whole SQLite lane, cloning only the matching names and policies under a read lock
    /// (not `snapshot()`).
    pub fn browser_db_projects(&self) -> Vec<(String, fluid_core::BrowserDbPolicy)> {
        self.map
            .read()
            .iter()
            .filter_map(|(name, s)| s.browser_db.clone().map(|p| (name.clone(), p)))
            .collect()
    }

    /// Case-insensitive lookup of an existing project KEY under a read lock, cloning one string
    /// instead of the whole map: the hot deploy path runs it for every name-collision check.
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
            if row.legacy_incarnation_upgrade_ms != row.incarnation_created_ms {
                row.legacy_incarnation_upgrade_ms = 0;
                row.legacy_deployment_ids.clear();
            } else {
                row.legacy_deployment_ids.sort();
                row.legacy_deployment_ids.dedup();
            }
            row.owned_database_ids.sort();
            row.owned_database_ids.dedup();
            row.owned_volume_names.sort();
            row.owned_volume_names.dedup();
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
            rename_tombstones: self.rename_tombstones.read().clone(),
            incarnation_tombstones: self.incarnation_tombstones.read().clone(),
        }
    }

    /// Merge a peer's snapshot: NEWEST-PER-ROW wins (`updated_ms`; projects are written on many
    /// nodes), a tombstone at-or-after a row's last write drops it, and a row re-created AFTER its
    /// tombstone survives. Never a wholesale replace: a sender that lacks a row cannot erase it
    /// here.
    pub fn merge_synced(&self, remote: SyncedProjects) -> usize {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        {
            let mut tombs = self.tombstones.write();
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
        {
            let mut renames = self.rename_tombstones.write();
            for (old, remote_tomb) in remote.rename_tombstones {
                if remote_tomb.rename_ms > ceiling {
                    tracing::warn!(old = %old, new = %remote_tomb.new, rename_ms = remote_tomb.rename_ms, ceiling_ms = ceiling, "dropping relayed project rename tombstone with implausibly future generation");
                    continue;
                }
                match renames.get(&old) {
                    Some(local) if local.rename_ms >= remote_tomb.rename_ms => {}
                    _ => {
                        renames.insert(old, remote_tomb);
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
            if row.legacy_incarnation_upgrade_ms != row.incarnation_created_ms {
                row.legacy_incarnation_upgrade_ms = 0;
                row.legacy_deployment_ids.clear();
            } else {
                row.legacy_deployment_ids.sort();
                row.legacy_deployment_ids.dedup();
            }
            row.owned_database_ids.sort();
            row.owned_database_ids.dedup();
            row.owned_volume_names.sort();
            row.owned_volume_names.dedup();
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
            if remote_row.legacy_incarnation_upgrade_ms != remote_row.incarnation_created_ms {
                remote_row.legacy_incarnation_upgrade_ms = 0;
                remote_row.legacy_deployment_ids.clear();
            }
            remote_row.owned_database_ids.sort();
            remote_row.owned_database_ids.dedup();
            remote_row.owned_volume_names.sort();
            remote_row.owned_volume_names.dedup();
            if remote_row.incarnation_created_ms > ceiling {
                tracing::warn!(project = %name, created_ms = remote_row.incarnation_created_ms, ceiling_ms = ceiling, "dropping relayed project incarnation with implausibly future generation");
                continue;
            }
            if remote_row.updated_ms > ceiling {
                tracing::warn!(project = %name, updated_ms = remote_row.updated_ms, ceiling_ms = ceiling, "dropping relayed project row with implausibly future version");
                continue;
            }
            let same_incarnation = map.get(&name).is_some_and(|local| {
                local.incarnation.is_some()
                    && local.incarnation == remote_row.incarnation
                    && local.incarnation_created_ms == remote_row.incarnation_created_ms
            });
            if same_incarnation {
                if let Some(local) = map.get(&name) {
                    if local.legacy_incarnation_upgrade_ms == local.incarnation_created_ms {
                        remote_row.legacy_incarnation_upgrade_ms = local.incarnation_created_ms;
                        remote_row
                            .legacy_deployment_ids
                            .extend(local.legacy_deployment_ids.iter().cloned());
                    }
                    remote_row
                        .owned_database_ids
                        .extend(local.owned_database_ids.iter().cloned());
                    remote_row
                        .owned_volume_names
                        .extend(local.owned_volume_names.iter().cloned());
                }
                remote_row.legacy_deployment_ids.sort();
                remote_row.legacy_deployment_ids.dedup();
                remote_row.owned_database_ids.sort();
                remote_row.owned_database_ids.dedup();
                remote_row.owned_volume_names.sort();
                remote_row.owned_volume_names.dedup();
            }
            let take = match map.get(&name) {
                None => true,
                Some(local) => merge_take(local, &remote_row),
            };
            if take {
                if let Some(local) = map.get(&name) {
                    keep_framework_auto_marker(local, &mut remote_row);
                }
                map.insert(name, remote_row);
            } else if same_incarnation {
                if let Some(local) = map.get_mut(&name) {
                    absorb_grow_only_facts(local, remote_row);
                }
            }
        }
        {
            let renames = self.rename_tombstones.read().clone();
            drop_renamed_away_rows(&mut map, &renames);
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

    /// Tombstones for the durable snapshot: a restart must not forget deletions, or the node
    /// re-imports them from any peer.
    pub fn tombstones_snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        self.tombstones.read().clone()
    }

    /// The project's permanent deletion generation, if tombstoned: the causal floor every new
    /// incarnation's timestamps must dominate.
    pub fn tombstone_of(&self, project: &str) -> Option<u64> {
        self.tombstones.read().get(project).copied()
    }

    /// Load persisted project tombstones, clamping implausibly future ones, and drop the
    /// incarnation-less rows they dominate: a delete landing between a snapshot's row capture and
    /// tombstone capture wrote both, and the row must not revive at boot.
    pub fn tombstones_load(&self, mut data: std::collections::BTreeMap<String, u64>) {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        for (id, ms) in data.iter_mut() {
            if *ms > ceiling {
                tracing::warn!(project = %id, deleted_ms = *ms, ceiling_ms = ceiling, "normalizing implausibly future persisted project tombstone on load");
                *ms = now;
            }
        }
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

    /// Rename a project's settings row IN PLACE under the lifecycle writer. The INCARNATION is
    /// invariant (same project, new name: minting one would orphan every in-flight delete/reap path
    /// and break `legacy_deployment_ids` lineage) and the row's `updated_ms` is bumped past the
    /// rename generation so it dominates its own tombstone. Two permanent tombstones are recorded
    /// for the OLD name: `tombstones[old]`, so a sparse peer that still holds the old row cannot
    /// resurrect it, and `rename_tombstones[old]`, so the name stays RESERVED and old URLs get a
    /// permanent redirect instead of a hijackable 404. Returns the moved row, or `None` when this
    /// node held none.
    pub fn rename_row(&self, from: &str, to: &str, rename_ms: u64) -> Option<ProjectSettings> {
        if from == to {
            return None;
        }
        let ceiling = project_time_ceiling(now_ms());
        let rename_ms = rename_ms.min(ceiling);
        let mut map = self.map.write();
        let Some(mut row) = map.remove(from) else {
            return None;
        };
        row.updated_ms = row
            .updated_ms
            .max(row.incarnation_created_ms)
            .saturating_add(1)
            .max(rename_ms.saturating_add(1));
        map.insert(to.to_string(), row.clone());
        drop(map);
        {
            let mut tombs = self.tombstones.write();
            let entry = tombs.entry(from.to_string()).or_insert(rename_ms);
            if rename_ms > *entry {
                *entry = rename_ms;
            }
        }
        {
            let mut renames = self.rename_tombstones.write();
            renames.insert(
                from.to_string(),
                ProjectRenameTombstone {
                    new: to.to_string(),
                    incarnation: row.incarnation,
                    rename_ms,
                },
            );
        }
        record_rename(from, to);
        Some(row)
    }

    /// Record a rename this node has no row for (a sparse observer): the reservation is the point,
    /// the old name must never be claimable again on ANY node.
    pub fn record_rename_tombstone(&self, from: &str, to: &str, rename_ms: u64) {
        let rename_ms = rename_ms.min(project_time_ceiling(now_ms()));
        {
            let mut tombs = self.tombstones.write();
            let entry = tombs.entry(from.to_string()).or_insert(rename_ms);
            if rename_ms > *entry {
                *entry = rename_ms;
            }
        }
        {
            let mut renames = self.rename_tombstones.write();
            renames.insert(
                from.to_string(),
                ProjectRenameTombstone {
                    new: to.to_string(),
                    incarnation: None,
                    rename_ms,
                },
            );
        }
        record_rename(from, to);
    }

    /// Whether `name` was renamed away and is therefore RESERVED forever, case-insensitively like
    /// `find_key_ci`: project names are platform-unique case-insensitively because the name becomes
    /// the podman volume, the checkout dir and the browserdb file name.
    pub fn renamed_away(&self, name: &str) -> Option<ProjectRenameTombstone> {
        self.rename_tombstones
            .read()
            .iter()
            .find(|(old, _)| old.eq_ignore_ascii_case(name))
            .map(|(_, tomb)| tomb.clone())
    }

    /// Where `name` ultimately points today, following a chain of renames (`a -> b -> c` resolves
    /// `a` to `c`), bounded so a cycle cannot spin; `None` when it was never renamed.
    pub fn rename_target(&self, name: &str) -> Option<String> {
        const MAX_HOPS: usize = 16;
        let mut cur = name.to_string();
        let mut moved = false;
        for _ in 0..MAX_HOPS {
            let Some(next) = self.renamed_away(&cur) else {
                break;
            };
            if next.new.eq_ignore_ascii_case(&cur) {
                break;
            }
            cur = next.new;
            moved = true;
        }
        moved.then_some(cur)
    }

    /// The rename tombstone for this EXACT name, if any.
    pub fn rename_tombstone_of(&self, name: &str) -> Option<ProjectRenameTombstone> {
        self.rename_tombstones.read().get(name).cloned()
    }

    pub fn rename_tombstones_snapshot(
        &self,
    ) -> std::collections::BTreeMap<String, ProjectRenameTombstone> {
        self.rename_tombstones.read().clone()
    }

    /// Load persisted rename tombstones: MERGE, never replace, because `persist::restore` can run
    /// again on a live process (a Guardian restore replays an earlier snapshot) and a replace would
    /// forget every rename since; the newest rename wins.
    pub fn rename_tombstones_load(
        &self,
        mut data: std::collections::BTreeMap<String, ProjectRenameTombstone>,
    ) {
        let now = now_ms();
        let ceiling = project_time_ceiling(now);
        for (old, tomb) in data.iter_mut() {
            if tomb.rename_ms > ceiling {
                tracing::warn!(
                    renamed_to = %tomb.new,
                    rename_ms = tomb.rename_ms,
                    ceiling_ms = ceiling,
                    "normalizing implausibly future persisted project rename tombstone"
                );
                tomb.rename_ms = now;
            }
            record_rename(old, &tomb.new);
        }
        {
            let mut renames = self.rename_tombstones.write();
            for (old, tomb) in data {
                match renames.get(&old) {
                    Some(local) if local.rename_ms >= tomb.rename_ms => {}
                    _ => {
                        renames.insert(old, tomb);
                    }
                }
            }
        }
    }

    pub fn begin_delete_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        observed_ms: u64,
    ) -> Result<(ProjectSettings, u64), ProjectIncarnationError> {
        let now = now_ms();
        let ceiling = now.saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS);
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
        let row = map
            .remove(project)
            .ok_or(ProjectIncarnationError::Missing)?;
        let observed_ms = observed_ms.min(ceiling);
        let prior_project = self
            .tombstones
            .read()
            .get(project)
            .copied()
            .unwrap_or(0)
            .min(ceiling);
        let prior_incarnation = self
            .incarnation_tombstones
            .read()
            .get(project)
            .and_then(|tombstones| tombstones.get(&expected))
            .copied()
            .unwrap_or(0)
            .min(ceiling);
        let deleted_ms = now
            .max(observed_ms.saturating_add(1))
            .max(row.updated_ms.min(ceiling).saturating_add(1))
            .max(row.incarnation_created_ms.min(ceiling).saturating_add(1))
            .max(prior_project.saturating_add(1))
            .max(prior_incarnation.saturating_add(1))
            .min(ceiling);
        self.tombstones
            .write()
            .insert(project.to_string(), deleted_ms);
        self.incarnation_tombstones
            .write()
            .entry(project.to_string())
            .or_default()
            .insert(expected, deleted_ms);
        drop(map);
        Self::remove_relational(project, deleted_ms);
        Ok((row, deleted_ms))
    }

    pub fn remove_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
    ) -> Result<ProjectSettings, ProjectIncarnationError> {
        self.begin_delete_exact(project, expected, 0)
            .map(|(row, _)| row)
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
        let remove_relational = map
            .get(project)
            .is_none_or(|row| row.incarnation == Some(expected));
        let removed = if map
            .get(project)
            .is_some_and(|row| row.incarnation == Some(expected))
        {
            map.remove(project)
        } else {
            None
        };
        let mut project_tombstones = self.tombstones.write();
        let current = project_tombstones
            .entry(project.to_string())
            .or_insert(deleted_ms);
        if deleted_ms > *current {
            *current = deleted_ms;
        }
        drop(project_tombstones);
        let mut incarnation_tombstones = self.incarnation_tombstones.write();
        let current = incarnation_tombstones
            .entry(project.to_string())
            .or_default()
            .entry(expected)
            .or_insert(deleted_ms);
        if deleted_ms > *current {
            *current = deleted_ms;
        }
        drop(incarnation_tombstones);
        drop(map);
        if remove_relational {
            Self::remove_relational(project, deleted_ms);
        }
        removed
    }

    /// Start a NEW user-requested deletion generation, even when this node has no settings row.
    /// `observed_ms` is the newest deployment or settings version the coordinator saw fleet-wide;
    /// issuing strictly above it makes the deletion dominate every object it authorized against.
    /// The returned stamp must ride every retry and fanout of this one delete; receivers apply it
    /// idempotently with [`Self::apply_delete`].
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

    /// Apply one already-issued deletion generation. A retry with the same stamp cannot ratchet the
    /// tombstone forward and a row causally recreated after it survives. A stamp beyond the ceiling
    /// is clamped to the ceiling, never rewound to local `now`: that made the generation
    /// node-dependent and split the fleet on which records the delete dominates.
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
        let delete_dominates_local_row = self
            .map
            .read()
            .get(project)
            .map_or(true, |row| row.updated_ms <= deleted_ms);
        if delete_dominates_local_row {
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

    /// Every mutator funnels row access through this: fetch-or-create and stamp `updated_ms`, so
    /// the replication merge always sees a fresh key for the row that changed. The stamp is
    /// strictly monotonic across live rows and retained tombstones, so a causal recreation is newer
    /// than the delete it supersedes even when wall time moved backwards.
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

    /// See [`ProjectSettings::browser_db`]. `None` clears the dashboard-managed opt-in; an explicit
    /// fluid.json block still wins on the next build.
    pub fn set_browser_db(&self, project: &str, spec: Option<fluid_core::BrowserDbPolicy>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).browser_db = spec;
    }

    /// Mint, rotate or clear one of this project's `browser_db` REST credentials; `public` selects
    /// the public-scope slot, otherwise the team-scope one.
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

    /// See [`ProjectSettings::container`]. `None` clears the dashboard-managed config; an explicit
    /// fluid.json `container` block still wins on the next build.
    pub fn set_container(&self, project: &str, spec: Option<ContainerSettings>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).container = spec;
    }

    /// See [`ProjectSettings::dedicated_ipv4`]. Called once per project by
    /// `tencent_eip::provision_from_checkout`: this row IS the durable idempotency claim
    /// (replicated through `store_sync::REGISTRY`'s "projects" entry), so a second confirmation is
    /// a no-op read, never a second Tencent purchase.
    pub fn set_dedicated_ipv4(&self, project: &str, alloc: Option<fluid_core::DedicatedIpv4>) {
        let mut m = self.map.write();
        self.touch(&mut m, project).dedicated_ipv4 = alloc;
    }

    pub fn set_git_ci(&self, project: &str, status: GitCiStatus) {
        let mut m = self.map.write();
        self.touch(&mut m, project).git_ci = Some(status);
    }

    /// Add or update an env var (by key). Sensitive values are sealed (encrypted at rest) before
    /// storing, never written to disk or the replicated snapshot in plaintext.
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

    /// Set the owning team and mirror it best-effort into the fleet-replicated relational index
    /// (`relational::set_project_team`), so the project stays visible on a node that lacks this
    /// local row.
    pub fn set_team(&self, project: &str, team: &str) {
        let (root_dir, updated_ms) = {
            let mut m = self.map.write();
            let s = self.touch(&mut m, project);
            s.team = team.to_string();
            (s.build.root_dir.clone(), s.updated_ms)
        };
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
        let (root_dir, updated_ms) = self.mutate_exact(project, expected, |row| {
            row.team = team.to_string();
            (row.build.root_dir.clone(), row.updated_ms)
        })?;
        let (project, team) = (project.to_string(), team.to_string());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                crate::relational::set_project_team(&project, &team, &root_dir, updated_ms).await
            });
        }
        Ok(())
    }

    /// Compare-and-set ownership: an untagged row is claimed, the owner's own claim is a no-op,
    /// another tenant's row is refused — two tenants importing one new name can no longer both win.
    pub fn claim_team_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        team: &str,
    ) -> Result<(), ProjectClaimError> {
        let owner = {
            let map = self.map.read();
            map.get(project).map(|row| row.team.trim().to_string())
        };
        match owner.as_deref() {
            Some(current) if current == team => Ok(()),
            Some(current) if !current.is_empty() && current != crate::admin::UNTAGGED_TENANT => {
                Err(ProjectClaimError::OwnedByOther)
            }
            _ => self
                .set_team_exact(project, expected, team)
                .map_err(ProjectClaimError::Incarnation),
        }
    }

    /// Rows a tenant owns, by the stored tag alone (`record_tenant` semantics:
    /// an untagged row belongs to nobody), cloned under one read lock.
    pub fn rows_for_tenant(&self, tenant: &str) -> Vec<(String, ProjectSettings)> {
        self.map
            .read()
            .iter()
            .filter(|(_, row)| crate::admin::record_tenant(row.team.trim()) == tenant)
            .map(|(name, row)| (name.clone(), row.clone()))
            .collect()
    }

    /// A build was admitted for this incarnation; only the newest admission
    /// is remembered.
    pub fn record_build_started_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        build_id: &str,
        started_ms: u64,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            if started_ms >= row.last_build_started_ms {
                row.last_build_id = build_id.to_string();
                row.last_build_started_ms = started_ms;
            }
        })
    }

    /// The terminal outcome of a build; an older build never overwrites a
    /// newer one, and the same build's outcome is idempotent.
    pub fn record_last_build_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        outcome: LastBuild,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            let newer = row
                .last_build
                .as_ref()
                .map(|current| current.id == outcome.id || outcome.started_ms >= current.started_ms)
                .unwrap_or(true);
            if newer {
                if outcome.started_ms >= row.last_build_started_ms {
                    row.last_build_id = outcome.id.clone();
                    row.last_build_started_ms = outcome.started_ms;
                }
                row.last_build = Some(outcome);
            }
        })
    }

    /// Grow-only: the first deployment this incarnation ever registered.
    pub fn mark_first_deployed_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        at_ms: u64,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            if row.first_deployed_ms == 0 {
                row.first_deployed_ms = at_ms;
            }
        })
    }

    /// A never-deployed project is being imported again by its owner: the attempt-specific settings
    /// follow the new request, curated settings (env, domains, explicit build commands, owned
    /// resources) stay.
    pub fn reset_for_reuse_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        root_dir: &str,
        branch: &str,
    ) -> Result<(), ProjectIncarnationError> {
        self.mutate_exact(project, expected, |row| {
            row.build.root_dir = root_dir.to_string();
            row.production_branch = branch.to_string();
            row.inference = None;
        })
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

    /// Team slug owning a project. A project absent from this store, or whose stored team is empty
    /// (tag loss in a gossip/restore race), is UNOWNED: `UNTAGGED_TENANT`, never the owner's
    /// "personal" namespace.
    pub fn team_of(&self, project: &str) -> String {
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

    /// Runtime-scoped env vars without environment filtering, for callers whose environment is not
    /// deployment-specific; build-scoped values never escape through it.
    pub fn env_map(&self, project: &str) -> std::collections::BTreeMap<String, String> {
        self.get(project)
            .env
            .into_iter()
            .filter(|e| env_scope_matches(&e.scope, EnvExecutionScope::Runtime))
            .map(|e| (e.key, crate::secrets::decrypt(&e.value)))
            .collect()
    }

    /// Env for one deployment environment and execution phase. `allow_all_environment=false` is the
    /// fork boundary: only variables explicitly set to `preview` qualify, since an `all` value is
    /// not a grant to code from an unrelated repository.
    pub fn env_map_for_execution(
        &self,
        project: &str,
        environment: &str,
        scope: EnvExecutionScope,
        allow_all_environment: bool,
    ) -> std::collections::BTreeMap<String, String> {
        env_map_from_settings(
            &self.get(project),
            environment,
            scope,
            allow_all_environment,
        )
    }

    pub fn env_map_for_execution_exact(
        &self,
        project: &str,
        expected: fluid_core::ProjectIncarnation,
        environment: &str,
        scope: EnvExecutionScope,
        allow_all_environment: bool,
    ) -> Result<std::collections::BTreeMap<String, String>, ProjectIncarnationError> {
        Ok(env_map_from_settings(
            &self.get_exact(project, expected)?,
            environment,
            scope,
            allow_all_environment,
        ))
    }

    /// Runtime compatibility wrapper. Legacy rows default to runtime-only and
    /// therefore remain available exactly where they were intended to run.
    pub fn env_map_for(
        &self,
        project: &str,
        environment: &str,
    ) -> std::collections::BTreeMap<String, String> {
        self.env_map_for_execution(project, environment, EnvExecutionScope::Runtime, true)
    }
}

pub(crate) fn env_map_from_settings(
    settings: &ProjectSettings,
    environment: &str,
    scope: EnvExecutionScope,
    allow_all_environment: bool,
) -> std::collections::BTreeMap<String, String> {
    let want = environment.trim().to_ascii_lowercase();
    settings
        .env
        .iter()
        .filter(|entry| {
            let target = entry.target.trim().to_ascii_lowercase();
            let environment_matches = target == want
                || (allow_all_environment && target == "all")
                || (target.is_empty() && want == "production");
            environment_matches && env_scope_matches(&entry.scope, scope)
        })
        .map(|entry| (entry.key.clone(), crate::secrets::decrypt(&entry.value)))
        .collect()
}

/// Empty (the legacy representation) matches both scopes (Vercel parity: a variable added without a
/// scope choice is available during the build); an explicit "runtime" stays runtime-only, and
/// unknown values fail toward runtime-only so a typo never becomes a build grant.
fn env_scope_matches(raw: &str, want: EnvExecutionScope) -> bool {
    match raw.trim().to_ascii_lowercase().as_str() {
        "all" => true,
        "build" => want == EnvExecutionScope::Build,
        "" => true,
        "runtime" => want == EnvExecutionScope::Runtime,
        _ => want == EnvExecutionScope::Runtime,
    }
}

fn upsert_env(row: &mut ProjectSettings, mut value: EnvVar) {
    value.updated_ms = now_ms();
    value.scope = match value.scope.trim().to_ascii_lowercase().as_str() {
        "build" => "build",
        "all" => "all",
        _ => "runtime",
    }
    .to_string();
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

/// Deterministic winner for two equal-`updated_ms` rows so replication converges: compares the
/// canonical JSON of the WHOLE row (equal-generation writes can differ in any field), so both
/// observers pick the same lexical winner. If serialization ever fails the direction stays
/// deterministic (`(Err, Ok)` takes the remote) instead of restoring the equal-version split brain.
fn tie_break_take(local: &ProjectSettings, remote: &ProjectSettings) -> bool {
    match (serde_json::to_vec(local), serde_json::to_vec(remote)) {
        (Ok(local), Ok(remote)) => remote > local,
        (Err(_), Ok(_)) => true,
        _ => false,
    }
}

/// A pre-`framework_auto` binary drops the field on a round trip, so its newer row would demote our
/// AUTO marker to "explicit": keep the marker when the framework text is unchanged.
fn keep_framework_auto_marker(local: &ProjectSettings, remote_row: &mut ProjectSettings) {
    if local.build.framework_auto
        && !remote_row.build.framework_auto
        && remote_row.build.framework.trim() == local.build.framework.trim()
    {
        remote_row.build.framework_auto = true;
    }
}

/// Migration authority and ownership are grow-only exact-id sets: merged even when row freshness
/// keeps the local row, or a last-writer-wins replacement would forget ids another hosting node
/// learned independently.
fn absorb_grow_only_facts(local: &mut ProjectSettings, remote_row: ProjectSettings) {
    if remote_row.legacy_incarnation_upgrade_ms == remote_row.incarnation_created_ms {
        local.legacy_incarnation_upgrade_ms = local.incarnation_created_ms;
        local
            .legacy_deployment_ids
            .extend(remote_row.legacy_deployment_ids);
        local.legacy_deployment_ids.sort();
        local.legacy_deployment_ids.dedup();
    }
    local
        .owned_database_ids
        .extend(remote_row.owned_database_ids);
    local.owned_database_ids.sort();
    local.owned_database_ids.dedup();
    local
        .owned_volume_names
        .extend(remote_row.owned_volume_names);
    local.owned_volume_names.sort();
    local.owned_volume_names.dedup();
}

/// A renamed-away name never comes back, whether from a peer, a Guardian replay of a pre-rename
/// snapshot or this node's own restored rows. Run it AFTER the remote merge: a pre-rename row in
/// `remote.rows` would otherwise slip straight back in and the project would serve under its old
/// name again.
fn drop_renamed_away_rows(
    map: &mut HashMap<String, ProjectSettings>,
    renames: &std::collections::BTreeMap<String, ProjectRenameTombstone>,
) {
    map.retain(|name, row| {
        !renames.iter().any(|(old, tomb)| {
            old.eq_ignore_ascii_case(name) && row.updated_ms <= tomb.rename_ms.saturating_add(1)
        })
    });
}

impl Default for ProjectStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: &str, value: &str, sensitive: bool) -> EnvVar {
        EnvVar {
            key: key.into(),
            value: value.into(),
            target: "all".into(),
            scope: "runtime".into(),
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
