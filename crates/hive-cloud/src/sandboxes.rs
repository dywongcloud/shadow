#![allow(
    dead_code,
    reason = "The provider contract includes forward-compatible quota and source fields not yet consumed by the public sandbox routes."
)]

//! Platform-native Sandboxes — isolated, on-demand, tenant-scoped Linux
//! environments for running commands, testing code, and executing untrusted
//! workloads (Vercel Sandbox parity). This module owns the PURE model:
//! records, validation, secret redaction, and the [`SandboxProvider`]
//! abstraction. The concrete production backend
//! ([`crate::sandboxes_platform::PlatformSandboxProvider`]) is podman-backed —
//! this platform is a self-hosted cloud, not a Vercel customer, so "production
//! provider" means the platform's OWN isolated-container tech (the same
//! primitives functions/containers already use), not a call out to Vercel's
//! commercial API. [`MockSandboxProvider`] here is strictly for tests.
//!
//! SECURITY (ZeroTrust): every record is tenant + project scoped
//! (`SandboxRecord.tenant_id`/`project_id`); every mutation is authorized via
//! the same `require_project` guard the rest of the platform uses. Command argv
//! is never shell-interpolated by default (`shell: false`); secrets are stored
//! via `crate::secrets::encrypt` and redacted out of logs before they are
//! persisted or returned.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Pending,
    Running,
    Stopping,
    Stopped,
    Failed,
}
impl SandboxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxStatus::Pending => "pending",
            SandboxStatus::Running => "running",
            SandboxStatus::Stopping => "stopping",
            SandboxStatus::Stopped => "stopped",
            SandboxStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandStatus {
    Queued,
    Running,
    Exited,
    Failed,
    Killed,
}

/// "allow-all" | "deny-all" | "allowlist" (+ domains/subnets) | forward-proxy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// "allow-all" | "deny-all" | "allowlist".
    #[serde(default = "default_policy_mode")]
    pub mode: String,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub allowed_subnets: Vec<String>,
    #[serde(default)]
    pub denied_subnets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_proxy: Option<String>,
}
fn default_policy_mode() -> String {
    "allow-all".into()
}
// A manual impl, NOT `#[derive(Default)]`: derive would give `mode = ""` (plain
// `String::default()`), which fails validation — `#[serde(default = "...")]`
// only wires up (de)serialization, not `Default::default()`.
impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy {
            mode: default_policy_mode(),
            allowed_domains: vec![],
            allowed_subnets: vec![],
            denied_subnets: vec![],
            forward_proxy: None,
        }
    }
}

/// A referenced env var: `value` is the caller-supplied plaintext ONLY at write
/// time; what's stored is `EnvRef` with the value already sealed via
/// `crate::secrets::encrypt` when the caller marks it sensitive (or always, for
/// sandboxes we default env values to sensitive-safe handling since a sandbox
/// env var is functionally equivalent to a project secret to whatever process
/// runs inside it).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvRef {
    pub key: String,
    /// Sealed (`enc:v1:...`) at rest. Never returned in list/detail reads —
    /// use a dedicated reveal path exactly like `crate::project_settings`.
    pub value_enc: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxRecord {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    /// "platform" (production, podman-backed) | "mock" (tests only).
    pub provider: String,
    pub provider_sandbox_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sandbox_id: Option<String>,
    pub name: String,
    pub status: SandboxStatus,
    /// "node26" | "node24" | "node22" | "python3.13".
    pub runtime: String,
    pub region: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_expires_at: Option<u64>,
    pub persistent: bool,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub exposed_domains: HashMap<u16, String>,
    #[serde(default)]
    pub network_policy: NetworkPolicy,
    #[serde(default)]
    pub env_refs: Vec<EnvRef>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_expiration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_last_snapshots: Option<u32>,
    #[serde(default)]
    pub active_cpu_usage_ms: u64,
    #[serde(default)]
    pub total_duration_ms: u64,
    #[serde(default)]
    pub total_active_cpu_duration_ms: u64,
    #[serde(default)]
    pub total_ingress_bytes: u64,
    #[serde(default)]
    pub total_egress_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stopped_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
    /// Internal: the podman container name backing this sandbox (empty for
    /// simulated/unavailable-engine sandboxes). Never exposed to a different
    /// tenant/project since the whole record is scoped.
    #[serde(default)]
    pub container: String,
    /// Human-readable note for degraded states (e.g. "podman unavailable — this
    /// sandbox is simulated"), mirroring the DB-provisioning "simulated" idiom.
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxCommandRecord {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub sandbox_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_command_id: Option<String>,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env_refs: Vec<EnvRef>,
    #[serde(default)]
    pub sudo: bool,
    #[serde(default)]
    pub detached: bool,
    pub status: CommandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Redacted, size-bounded output — the log lines themselves ARE the
    /// "stdoutRef"/"stderrRef" (no separate blob store exists in this repo; this
    /// mirrors the `BuildStore`/`Vec<LogLine>` idiom exactly).
    #[serde(default)]
    pub stdout: Vec<LogLine>,
    #[serde(default)]
    pub stderr: Vec<LogLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    pub ts_ms: u64,
    pub line: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxSnapshotRecord {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub sandbox_id: String,
    /// The podman image tag this snapshot committed to (e.g.
    /// `hive-sandbox-snap:<id>`).
    pub provider_snapshot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxMountRecord {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub sandbox_id: String,
    pub mount_path: String,
    /// "drive" | "remote-fuse".
    #[serde(rename = "type")]
    pub kind: String,
    /// "read-only" | "read-write".
    pub mode: String,
    /// "s3" | "r2" | "gcs" | "custom" (remote-fuse) or "platform" (drive).
    pub provider: String,
    /// Non-secret + sealed-secret config, e.g. bucket/region/endpoint plus
    /// `access_key_enc`/`secret_key_enc` (never plaintext).
    #[serde(default)]
    pub config_refs: HashMap<String, String>,
    /// "mounted" | "pending" | "unavailable" | "error" — honest capability
    /// status (e.g. no FUSE driver installed on this node), never faked.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub note: String,
    pub created_at: u64,
    pub updated_at: u64,
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxError {
    NotFound(String),
    Unauthorized(String),
    InvalidName(String),
    InvalidRuntime(String),
    InvalidArgv(String),
    InvalidNetworkPolicy(String),
    InvalidMount(String),
    QuotaExceeded(String),
    EngineUnavailable(String),
    CommandNotFound(String),
    SnapshotNotFound(String),
    MountNotFound(String),
    AlreadyExists(String),
}
impl SandboxError {
    pub fn code(&self) -> &'static str {
        match self {
            SandboxError::NotFound(_) => "SANDBOX_NOT_FOUND",
            SandboxError::Unauthorized(_) => "SANDBOX_UNAUTHORIZED",
            SandboxError::InvalidName(_) => "SANDBOX_INVALID_NAME",
            SandboxError::InvalidRuntime(_) => "SANDBOX_INVALID_RUNTIME",
            SandboxError::InvalidArgv(_) => "SANDBOX_INVALID_COMMAND",
            SandboxError::InvalidNetworkPolicy(_) => "SANDBOX_INVALID_NETWORK_POLICY",
            SandboxError::InvalidMount(_) => "SANDBOX_INVALID_MOUNT",
            SandboxError::QuotaExceeded(_) => "SANDBOX_QUOTA_EXCEEDED",
            SandboxError::EngineUnavailable(_) => "SANDBOX_ENGINE_UNAVAILABLE",
            SandboxError::CommandNotFound(_) => "SANDBOX_COMMAND_NOT_FOUND",
            SandboxError::SnapshotNotFound(_) => "SANDBOX_SNAPSHOT_NOT_FOUND",
            SandboxError::MountNotFound(_) => "SANDBOX_MOUNT_NOT_FOUND",
            SandboxError::AlreadyExists(_) => "SANDBOX_ALREADY_EXISTS",
        }
    }
    pub fn message(&self) -> String {
        match self {
            SandboxError::NotFound(s)
            | SandboxError::Unauthorized(s)
            | SandboxError::InvalidName(s)
            | SandboxError::InvalidRuntime(s)
            | SandboxError::InvalidArgv(s)
            | SandboxError::InvalidNetworkPolicy(s)
            | SandboxError::InvalidMount(s)
            | SandboxError::QuotaExceeded(s)
            | SandboxError::EngineUnavailable(s)
            | SandboxError::CommandNotFound(s)
            | SandboxError::SnapshotNotFound(s)
            | SandboxError::MountNotFound(s)
            | SandboxError::AlreadyExists(s) => s.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Validation (pure — no I/O)
// ---------------------------------------------------------------------------

pub const RUNTIMES: &[&str] = &["node26", "node24", "node22", "python3.13"];

pub fn validate_runtime(runtime: &str) -> Result<(), SandboxError> {
    if RUNTIMES.contains(&runtime) {
        Ok(())
    } else {
        Err(SandboxError::InvalidRuntime(format!(
            "unsupported runtime '{runtime}' — must be one of {RUNTIMES:?}"
        )))
    }
}

/// Sandbox names follow the same DNS-label-safe shape used for projects/
/// containers elsewhere in the platform: lowercase alnum + hyphen, 1-63 chars,
/// not starting/ending with a hyphen.
pub fn validate_name(name: &str) -> Result<(), SandboxError> {
    let n = name.trim();
    if n.is_empty() || n.len() > 63 {
        return Err(SandboxError::InvalidName(
            "name must be 1-63 characters".into(),
        ));
    }
    let ok = n
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !n.starts_with('-')
        && !n.ends_with('-');
    if !ok {
        return Err(SandboxError::InvalidName(
            "name must be lowercase alphanumeric + hyphens, not starting/ending with a hyphen"
                .into(),
        ));
    }
    Ok(())
}

/// argv validation: every command runs as argv (`cmd`, `args: Vec<String>`),
/// NEVER shell-interpolated — this is what makes shell injection structurally
/// impossible for the default path. Rejects empty commands and embedded NUL
/// bytes (the one byte that can desync argv parsing/exec syscalls).
pub fn validate_argv(cmd: &str, args: &[String]) -> Result<(), SandboxError> {
    if cmd.trim().is_empty() {
        return Err(SandboxError::InvalidArgv(
            "command must not be empty".into(),
        ));
    }
    if cmd.contains('\0') || args.iter().any(|a| a.contains('\0')) {
        return Err(SandboxError::InvalidArgv(
            "command/args must not contain NUL bytes".into(),
        ));
    }
    Ok(())
}

/// `sudo` is a project-policy opt-in, never implicit — this enforces the
/// "only if explicitly enabled" invariant at the validation layer so every
/// caller (API, tests) shares one gate.
pub fn validate_sudo(requested: bool, project_allows_sudo: bool) -> Result<(), SandboxError> {
    if requested && !project_allows_sudo {
        return Err(SandboxError::Unauthorized(
            "sudo is not enabled for this project — enable it in project settings first".into(),
        ));
    }
    Ok(())
}

fn is_valid_domain(d: &str) -> bool {
    let d = d.trim();
    !d.is_empty()
        && d.len() <= 253
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

fn is_valid_cidr(s: &str) -> bool {
    let Some((ip, mask)) = s.split_once('/') else {
        return false;
    };
    let Ok(bits): Result<u8, _> = mask.parse() else {
        return false;
    };
    if bits > 32 {
        return false;
    }
    ip.split('.').count() == 4 && ip.split('.').all(|o| o.parse::<u8>().is_ok())
}

pub fn validate_network_policy(p: &NetworkPolicy) -> Result<(), SandboxError> {
    match p.mode.as_str() {
        "allow-all" | "deny-all" => Ok(()),
        "allowlist" => {
            if p.allowed_domains.is_empty() && p.allowed_subnets.is_empty() {
                return Err(SandboxError::InvalidNetworkPolicy(
                    "allowlist mode requires at least one allowed domain or subnet".into(),
                ));
            }
            for d in &p.allowed_domains {
                if !is_valid_domain(d) {
                    return Err(SandboxError::InvalidNetworkPolicy(format!(
                        "invalid domain '{d}'"
                    )));
                }
            }
            for s in p.allowed_subnets.iter().chain(p.denied_subnets.iter()) {
                if !is_valid_cidr(s) {
                    return Err(SandboxError::InvalidNetworkPolicy(format!(
                        "invalid CIDR subnet '{s}'"
                    )));
                }
            }
            Ok(())
        }
        other => Err(SandboxError::InvalidNetworkPolicy(format!(
            "unknown network policy mode '{other}' — must be allow-all, deny-all, or allowlist"
        ))),
    }
}

pub fn validate_mount(
    mount_path: &str,
    kind: &str,
    mode: &str,
    provider: &str,
) -> Result<(), SandboxError> {
    if !mount_path.starts_with('/') || mount_path.contains("..") {
        return Err(SandboxError::InvalidMount(
            "mountPath must be an absolute path with no '..' segments".into(),
        ));
    }
    if !matches!(kind, "drive" | "remote-fuse") {
        return Err(SandboxError::InvalidMount(format!(
            "mount type must be 'drive' or 'remote-fuse', got '{kind}'"
        )));
    }
    if !matches!(mode, "read-only" | "read-write") {
        return Err(SandboxError::InvalidMount(format!(
            "mount mode must be 'read-only' or 'read-write', got '{mode}'"
        )));
    }
    if kind == "remote-fuse" && !matches!(provider, "s3" | "r2" | "gcs" | "custom") {
        return Err(SandboxError::InvalidMount(format!(
            "remote-fuse provider must be one of s3|r2|gcs|custom, got '{provider}'"
        )));
    }
    Ok(())
}

/// Redact every occurrence of each secret's PLAINTEXT value out of `text`,
/// replacing with `[REDACTED]`. Applied to command output before it is
/// persisted or returned to the client — secrets never reach logs/telemetry.
/// Longest-first so a secret that is a substring of another doesn't leave a
/// partial, still-sensitive fragment behind.
pub fn redact_secrets(text: &str, secret_values: &[String]) -> String {
    if secret_values.is_empty() {
        return text.to_string();
    }
    let mut ordered: Vec<&String> = secret_values.iter().filter(|s| !s.is_empty()).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for s in ordered {
        if !s.trim().is_empty() {
            out = out.replace(s.as_str(), "[REDACTED]");
        }
    }
    out
}

/// Bound a log line to a maximum byte length (prevents one runaway line from
/// blowing the output-size budget); truncates on a UTF-8-safe boundary.
pub fn bound_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &line[..end])
}

// ---------------------------------------------------------------------------
// Quotas
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct SandboxQuota {
    pub max_sandboxes: u32,
    pub max_running_sandboxes: u32,
    pub max_command_runtime_secs: u64,
    pub max_output_bytes: usize,
    pub max_exposed_ports: u32,
    pub max_mounts: u32,
    pub max_env_vars: u32,
}

pub fn check_quota(current: u32, max: u32, resource: &str) -> Result<(), SandboxError> {
    if max > 0 && current >= max {
        return Err(SandboxError::QuotaExceeded(format!(
            "{resource} limit reached ({current}/{max}) on this plan — upgrade to add more"
        )));
    }
    Ok(())
}

/// Persisted snapshot of all sandbox-related records — mirrors the
/// `EnterpriseSnapshot`/`DatabaseStore` idiom (`#[serde(default)]` on every
/// field so schema evolution is backward-compatible).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SandboxesSnapshot {
    #[serde(default)]
    pub sandboxes: Vec<SandboxRecord>,
    #[serde(default)]
    pub commands: Vec<SandboxCommandRecord>,
    #[serde(default)]
    pub snapshots: Vec<SandboxSnapshotRecord>,
    #[serde(default)]
    pub mounts: Vec<SandboxMountRecord>,
}

// ---------------------------------------------------------------------------
// Provider abstraction
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct CreateSandboxInput {
    pub name: String,
    pub runtime: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub timeout_ms: u64,
    pub persistent: bool,
    pub ports: Vec<u16>,
    pub network_policy: NetworkPolicy,
    pub env: Vec<(String, String, bool)>, // (key, value, sensitive)
    pub tags: Vec<String>,
    /// "empty" | "git" | "tarball" | "snapshot" | "project".
    pub source_kind: String,
    pub source_ref: String,
}

#[derive(Clone, Debug, Default)]
pub struct RunCommandInput {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub sudo: bool,
    pub detached: bool,
    pub shell: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CreateSnapshotInput {
    pub keep_last: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct MountConfigInput {
    pub mount_path: String,
    pub kind: String,
    pub mode: String,
    pub provider: String,
    pub bucket: String,
    pub region: String,
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
    pub extra_args: HashMap<String, String>,
}

/// Provider-neutral sandbox backend. The UI/API talk to this trait, never to a
/// concrete engine directly — [`crate::sandboxes_platform::PlatformSandboxProvider`]
/// (podman) is the production implementation; [`MockSandboxProvider`] exists
/// ONLY for unit tests.
#[async_trait::async_trait]
pub trait SandboxProvider: Send + Sync {
    async fn list_sandboxes(&self, project_id: &str) -> Result<Vec<SandboxRecord>, SandboxError>;
    async fn create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError>;
    async fn get_sandbox(
        &self,
        project_id: &str,
        id_or_name: &str,
    ) -> Result<SandboxRecord, SandboxError>;
    async fn get_or_create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError>;
    async fn stop_sandbox(&self, project_id: &str, id: &str)
    -> Result<SandboxRecord, SandboxError>;
    async fn delete_sandbox(&self, project_id: &str, id: &str) -> Result<(), SandboxError>;
    async fn run_command(
        &self,
        project_id: &str,
        id: &str,
        input: RunCommandInput,
    ) -> Result<SandboxCommandRecord, SandboxError>;
    async fn list_commands(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxCommandRecord>, SandboxError>;
    async fn get_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError>;
    async fn kill_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError>;
    async fn write_files(
        &self,
        project_id: &str,
        id: &str,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<(), SandboxError>;
    async fn read_file(
        &self,
        project_id: &str,
        id: &str,
        path: &str,
    ) -> Result<Vec<u8>, SandboxError>;
    async fn create_snapshot(
        &self,
        project_id: &str,
        id: &str,
        input: CreateSnapshotInput,
    ) -> Result<SandboxSnapshotRecord, SandboxError>;
    async fn list_snapshots(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxSnapshotRecord>, SandboxError>;
    async fn delete_snapshot(
        &self,
        project_id: &str,
        snapshot_id: &str,
    ) -> Result<(), SandboxError>;
    async fn domain(&self, project_id: &str, id: &str, port: u16) -> Result<String, SandboxError>;
    async fn update_network_policy(
        &self,
        project_id: &str,
        id: &str,
        policy: NetworkPolicy,
    ) -> Result<SandboxRecord, SandboxError>;
    async fn mount_storage(
        &self,
        project_id: &str,
        id: &str,
        input: MountConfigInput,
    ) -> Result<SandboxMountRecord, SandboxError>;
    async fn list_mounts(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxMountRecord>, SandboxError>;
    async fn delete_mount(&self, project_id: &str, mount_id: &str) -> Result<(), SandboxError>;
}

// ---------------------------------------------------------------------------
// MockSandboxProvider — strictly for tests. Never the production default.
// ---------------------------------------------------------------------------

/// In-memory provider used ONLY by unit/integration tests exercising the
/// service layer (authz, quotas, validation) without spinning up real
/// containers. `ProductionProvider ≡ PlatformSandboxProvider` — this type must
/// never be constructed outside `#[cfg(test)]`.
#[cfg(test)]
pub struct MockSandboxProvider {
    sandboxes: parking_lot::Mutex<Vec<SandboxRecord>>,
    commands: parking_lot::Mutex<Vec<SandboxCommandRecord>>,
    snapshots: parking_lot::Mutex<Vec<SandboxSnapshotRecord>>,
    mounts: parking_lot::Mutex<Vec<SandboxMountRecord>>,
    seq: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl MockSandboxProvider {
    pub fn new() -> Self {
        MockSandboxProvider {
            sandboxes: parking_lot::Mutex::new(Vec::new()),
            commands: parking_lot::Mutex::new(Vec::new()),
            snapshots: parking_lot::Mutex::new(Vec::new()),
            mounts: parking_lot::Mutex::new(Vec::new()),
            seq: std::sync::atomic::AtomicU64::new(1),
        }
    }
    fn next_id(&self, prefix: &str) -> String {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("{prefix}_{n:012x}")
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SandboxProvider for MockSandboxProvider {
    async fn list_sandboxes(&self, project_id: &str) -> Result<Vec<SandboxRecord>, SandboxError> {
        Ok(self
            .sandboxes
            .lock()
            .iter()
            .filter(|s| s.project_id == project_id && s.deleted_at.is_none())
            .cloned()
            .collect())
    }
    async fn create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        validate_name(&input.name)?;
        validate_runtime(&input.runtime)?;
        validate_network_policy(&input.network_policy)?;
        let id = self.next_id("sbx");
        let now = 1;
        let rec = SandboxRecord {
            id: id.clone(),
            tenant_id: tenant_id.into(),
            project_id: project_id.into(),
            provider: "mock".into(),
            provider_sandbox_name: input.name.clone(),
            provider_sandbox_id: Some(id.clone()),
            name: input.name,
            status: SandboxStatus::Running,
            runtime: input.runtime,
            region: "test".into(),
            vcpus: input.vcpus.max(1),
            memory_mb: input.memory_mb.max(512),
            timeout_ms: input.timeout_ms,
            timeout_expires_at: Some(now + input.timeout_ms),
            persistent: input.persistent,
            ports: input.ports,
            exposed_domains: HashMap::new(),
            network_policy: input.network_policy,
            env_refs: input
                .env
                .into_iter()
                .map(|(k, v, _)| EnvRef {
                    key: k,
                    value_enc: v,
                })
                .collect(),
            tags: input.tags,
            current_snapshot_id: None,
            snapshot_expiration_ms: None,
            keep_last_snapshots: None,
            active_cpu_usage_ms: 0,
            total_duration_ms: 0,
            total_active_cpu_duration_ms: 0,
            total_ingress_bytes: 0,
            total_egress_bytes: 0,
            last_started_at: Some(now),
            last_stopped_at: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            container: String::new(),
            note: String::new(),
        };
        self.sandboxes.lock().push(rec.clone());
        Ok(rec)
    }
    async fn get_sandbox(
        &self,
        project_id: &str,
        id_or_name: &str,
    ) -> Result<SandboxRecord, SandboxError> {
        self.sandboxes
            .lock()
            .iter()
            .find(|s| {
                s.project_id == project_id
                    && s.deleted_at.is_none()
                    && (s.id == id_or_name || s.name == id_or_name)
            })
            .cloned()
            .ok_or_else(|| SandboxError::NotFound(id_or_name.to_string()))
    }
    async fn get_or_create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        if let Ok(existing) = self.get_sandbox(project_id, &input.name).await {
            return Ok(existing);
        }
        self.create_sandbox(tenant_id, project_id, input).await
    }
    async fn stop_sandbox(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<SandboxRecord, SandboxError> {
        let mut m = self.sandboxes.lock();
        let s = m
            .iter_mut()
            .find(|s| s.project_id == project_id && s.id == id)
            .ok_or_else(|| SandboxError::NotFound(id.to_string()))?;
        s.status = SandboxStatus::Stopped;
        s.last_stopped_at = Some(1);
        if s.persistent {
            s.current_snapshot_id = Some(format!("snap_{id}"));
        }
        Ok(s.clone())
    }
    async fn delete_sandbox(&self, project_id: &str, id: &str) -> Result<(), SandboxError> {
        let mut m = self.sandboxes.lock();
        let s = m
            .iter_mut()
            .find(|s| s.project_id == project_id && s.id == id)
            .ok_or_else(|| SandboxError::NotFound(id.to_string()))?;
        s.deleted_at = Some(1);
        Ok(())
    }
    async fn run_command(
        &self,
        project_id: &str,
        id: &str,
        input: RunCommandInput,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        validate_argv(&input.cmd, &input.args)?;
        self.get_sandbox(project_id, id).await?;
        let cid = self.next_id("cmd");
        let rec = SandboxCommandRecord {
            id: cid,
            tenant_id: "t".into(),
            project_id: project_id.into(),
            sandbox_id: id.into(),
            provider_command_id: None,
            cmd: input.cmd,
            args: input.args,
            cwd: input.cwd,
            env_refs: vec![],
            sudo: input.sudo,
            detached: input.detached,
            status: if input.detached {
                CommandStatus::Running
            } else {
                CommandStatus::Exited
            },
            exit_code: if input.detached { None } else { Some(0) },
            stdout: vec![LogLine {
                ts_ms: 1,
                line: "mock output".into(),
            }],
            stderr: vec![],
            started_at: Some(1),
            finished_at: if input.detached { None } else { Some(2) },
            created_at: 1,
            updated_at: 2,
        };
        self.commands.lock().push(rec.clone());
        Ok(rec)
    }
    async fn list_commands(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxCommandRecord>, SandboxError> {
        Ok(self
            .commands
            .lock()
            .iter()
            .filter(|c| c.project_id == project_id && c.sandbox_id == id)
            .cloned()
            .collect())
    }
    async fn get_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        self.commands
            .lock()
            .iter()
            .find(|c| c.project_id == project_id && c.sandbox_id == id && c.id == command_id)
            .cloned()
            .ok_or_else(|| SandboxError::CommandNotFound(command_id.to_string()))
    }
    async fn kill_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        let mut m = self.commands.lock();
        let c = m
            .iter_mut()
            .find(|c| c.project_id == project_id && c.sandbox_id == id && c.id == command_id)
            .ok_or_else(|| SandboxError::CommandNotFound(command_id.to_string()))?;
        c.status = CommandStatus::Killed;
        c.finished_at = Some(3);
        Ok(c.clone())
    }
    async fn write_files(
        &self,
        _project_id: &str,
        _id: &str,
        _files: Vec<(String, Vec<u8>)>,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn read_file(
        &self,
        _project_id: &str,
        _id: &str,
        _path: &str,
    ) -> Result<Vec<u8>, SandboxError> {
        Ok(b"mock file contents".to_vec())
    }
    async fn create_snapshot(
        &self,
        project_id: &str,
        id: &str,
        _input: CreateSnapshotInput,
    ) -> Result<SandboxSnapshotRecord, SandboxError> {
        self.get_sandbox(project_id, id).await?;
        let sid = self.next_id("snap");
        let rec = SandboxSnapshotRecord {
            id: sid.clone(),
            tenant_id: "t".into(),
            project_id: project_id.into(),
            sandbox_id: id.into(),
            provider_snapshot_id: sid,
            source_session_id: None,
            status: "ready".into(),
            size_bytes: Some(1024),
            created_at: 1,
            expires_at: None,
        };
        self.snapshots.lock().push(rec.clone());
        Ok(rec)
    }
    async fn list_snapshots(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxSnapshotRecord>, SandboxError> {
        Ok(self
            .snapshots
            .lock()
            .iter()
            .filter(|s| s.project_id == project_id && s.sandbox_id == id)
            .cloned()
            .collect())
    }
    async fn delete_snapshot(
        &self,
        project_id: &str,
        snapshot_id: &str,
    ) -> Result<(), SandboxError> {
        let mut m = self.snapshots.lock();
        let before = m.len();
        m.retain(|s| !(s.project_id == project_id && s.id == snapshot_id));
        if m.len() == before {
            return Err(SandboxError::SnapshotNotFound(snapshot_id.to_string()));
        }
        Ok(())
    }
    async fn domain(&self, project_id: &str, id: &str, port: u16) -> Result<String, SandboxError> {
        self.get_sandbox(project_id, id).await?;
        Ok(format!("http://mock.local:{port}"))
    }
    async fn update_network_policy(
        &self,
        project_id: &str,
        id: &str,
        policy: NetworkPolicy,
    ) -> Result<SandboxRecord, SandboxError> {
        validate_network_policy(&policy)?;
        let mut m = self.sandboxes.lock();
        let s = m
            .iter_mut()
            .find(|s| s.project_id == project_id && s.id == id)
            .ok_or_else(|| SandboxError::NotFound(id.to_string()))?;
        s.network_policy = policy;
        Ok(s.clone())
    }
    async fn mount_storage(
        &self,
        project_id: &str,
        id: &str,
        input: MountConfigInput,
    ) -> Result<SandboxMountRecord, SandboxError> {
        validate_mount(&input.mount_path, &input.kind, &input.mode, &input.provider)?;
        self.get_sandbox(project_id, id).await?;
        let mid = self.next_id("mnt");
        let rec = SandboxMountRecord {
            id: mid,
            tenant_id: "t".into(),
            project_id: project_id.into(),
            sandbox_id: id.into(),
            mount_path: input.mount_path,
            kind: input.kind,
            mode: input.mode,
            provider: input.provider,
            config_refs: HashMap::new(),
            status: "mounted".into(),
            note: String::new(),
            created_at: 1,
            updated_at: 1,
        };
        self.mounts.lock().push(rec.clone());
        Ok(rec)
    }
    async fn list_mounts(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxMountRecord>, SandboxError> {
        Ok(self
            .mounts
            .lock()
            .iter()
            .filter(|m| m.project_id == project_id && m.sandbox_id == id)
            .cloned()
            .collect())
    }
    async fn delete_mount(&self, project_id: &str, mount_id: &str) -> Result<(), SandboxError> {
        let mut m = self.mounts.lock();
        let before = m.len();
        m.retain(|x| !(x.project_id == project_id && x.id == mount_id));
        if m.len() == before {
            return Err(SandboxError::MountNotFound(mount_id.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_validation() {
        for r in RUNTIMES {
            assert!(validate_runtime(r).is_ok());
        }
        assert!(validate_runtime("ruby3.2").is_err());
        assert_eq!(
            validate_runtime("bogus").unwrap_err().code(),
            "SANDBOX_INVALID_RUNTIME"
        );
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("my-sandbox-1").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-leading").is_err());
        assert!(validate_name("trailing-").is_err());
        assert!(validate_name("Has_Upper").is_err());
        assert!(validate_name(&"x".repeat(64)).is_err());
    }

    #[test]
    fn argv_validation_rejects_empty_and_nul() {
        assert!(validate_argv("node", &["--version".into()]).is_ok());
        assert!(validate_argv("", &[]).is_err());
        assert!(validate_argv("node\0rm", &[]).is_err());
        assert!(validate_argv("node", &["--eval\0malicious".into()]).is_err());
    }

    #[test]
    fn sudo_requires_explicit_project_policy() {
        assert!(validate_sudo(false, false).is_ok());
        assert!(validate_sudo(true, true).is_ok());
        assert_eq!(
            validate_sudo(true, false).unwrap_err().code(),
            "SANDBOX_UNAUTHORIZED"
        );
    }

    #[test]
    fn network_policy_validation() {
        assert!(
            validate_network_policy(&NetworkPolicy {
                mode: "allow-all".into(),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate_network_policy(&NetworkPolicy {
                mode: "deny-all".into(),
                ..Default::default()
            })
            .is_ok()
        );
        assert_eq!(
            validate_network_policy(&NetworkPolicy {
                mode: "allowlist".into(),
                ..Default::default()
            })
            .unwrap_err()
            .code(),
            "SANDBOX_INVALID_NETWORK_POLICY"
        );
        assert!(
            validate_network_policy(&NetworkPolicy {
                mode: "allowlist".into(),
                allowed_domains: vec!["api.example.com".into()],
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate_network_policy(&NetworkPolicy {
                mode: "allowlist".into(),
                allowed_subnets: vec!["not-a-cidr".into()],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_network_policy(&NetworkPolicy {
                mode: "allowlist".into(),
                allowed_subnets: vec!["10.0.0.0/8".into()],
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate_network_policy(&NetworkPolicy {
                mode: "bogus".into(),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn mount_validation() {
        assert!(validate_mount("/data", "drive", "read-write", "platform").is_ok());
        assert!(validate_mount("relative/path", "drive", "read-write", "platform").is_err());
        assert!(validate_mount("/data/../etc", "drive", "read-write", "platform").is_err());
        assert!(validate_mount("/data", "bogus", "read-write", "platform").is_err());
        assert!(validate_mount("/data", "drive", "bogus", "platform").is_err());
        assert!(validate_mount("/data", "remote-fuse", "read-only", "azure").is_err());
        assert!(validate_mount("/data", "remote-fuse", "read-only", "s3").is_ok());
    }

    #[test]
    fn secret_redaction_masks_values_and_handles_substrings() {
        let secrets = vec!["sk_live_abc123".to_string(), "sk_live_abc".to_string()];
        let text = "Authorization: Bearer sk_live_abc123 request failed";
        let redacted = redact_secrets(text, &secrets);
        assert!(!redacted.contains("sk_live_abc123"));
        assert!(
            !redacted.contains("sk_live_abc"),
            "shorter substring secret must not survive: {redacted}"
        );
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn secret_redaction_noop_without_secrets() {
        assert_eq!(redact_secrets("hello world", &[]), "hello world");
    }

    #[test]
    fn bound_line_truncates_on_utf8_boundary() {
        let s = "a".repeat(100);
        let b = bound_line(&s, 10);
        assert!(b.starts_with(&"a".repeat(10)));
        assert!(b.ends_with("[truncated]"));
        // Multi-byte safe: truncating mid-character must not panic.
        let multi = "é".repeat(20); // 2 bytes each
        let _ = bound_line(&multi, 5);
    }

    #[test]
    fn quota_checks() {
        assert!(check_quota(4, 5, "sandboxes").is_ok());
        assert!(check_quota(5, 5, "sandboxes").is_err());
        assert!(check_quota(100, 0, "sandboxes").is_ok(), "0 = unlimited");
    }

    #[tokio::test]
    async fn mock_provider_create_list_get_lifecycle() {
        let p = MockSandboxProvider::new();
        let input = CreateSandboxInput {
            name: "test-1".into(),
            runtime: "node22".into(),
            vcpus: 1,
            memory_mb: 1024,
            timeout_ms: 60_000,
            ..Default::default()
        };
        let rec = p.create_sandbox("t1", "proj-a", input).await.unwrap();
        assert_eq!(rec.status, SandboxStatus::Running);
        let listed = p.list_sandboxes("proj-a").await.unwrap();
        assert_eq!(listed.len(), 1);
        let got = p.get_sandbox("proj-a", &rec.id).await.unwrap();
        assert_eq!(got.id, rec.id);
        // Cross-project isolation.
        assert!(p.get_sandbox("proj-b", &rec.id).await.is_err());
    }

    #[tokio::test]
    async fn mock_provider_persistent_stop_creates_snapshot_marker() {
        let p = MockSandboxProvider::new();
        let input = CreateSandboxInput {
            name: "persist-1".into(),
            runtime: "node22".into(),
            persistent: true,
            ..Default::default()
        };
        let rec = p.create_sandbox("t1", "proj-a", input).await.unwrap();
        let stopped = p.stop_sandbox("proj-a", &rec.id).await.unwrap();
        assert_eq!(stopped.status, SandboxStatus::Stopped);
        assert!(
            stopped.current_snapshot_id.is_some(),
            "persistent sandbox must record snapshot/persistent state on stop"
        );
    }

    #[tokio::test]
    async fn mock_provider_run_command_blocking_and_detached() {
        let p = MockSandboxProvider::new();
        let rec = p
            .create_sandbox(
                "t1",
                "proj-a",
                CreateSandboxInput {
                    name: "cmd-1".into(),
                    runtime: "node22".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let blocking = p
            .run_command(
                "proj-a",
                &rec.id,
                RunCommandInput {
                    cmd: "node".into(),
                    args: vec!["--version".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(blocking.status, CommandStatus::Exited);
        assert_eq!(blocking.exit_code, Some(0));
        let detached = p
            .run_command(
                "proj-a",
                &rec.id,
                RunCommandInput {
                    cmd: "sleep".into(),
                    args: vec!["100".into()],
                    detached: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(detached.status, CommandStatus::Running);
        assert!(detached.exit_code.is_none());
        let killed = p
            .kill_command("proj-a", &rec.id, &detached.id)
            .await
            .unwrap();
        assert_eq!(killed.status, CommandStatus::Killed);

        let all = p.list_commands("proj-a", &rec.id).await.unwrap();
        assert_eq!(
            all.len(),
            2,
            "both the blocking and detached commands must be listed"
        );
        assert!(
            p.list_commands("proj-b", &rec.id).await.unwrap().is_empty(),
            "cross-project list must be empty, not error or leak"
        );
    }

    #[tokio::test]
    async fn mock_provider_snapshot_lifecycle() {
        let p = MockSandboxProvider::new();
        let rec = p
            .create_sandbox(
                "t1",
                "proj-a",
                CreateSandboxInput {
                    name: "snap-1".into(),
                    runtime: "node22".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let snap = p
            .create_snapshot("proj-a", &rec.id, CreateSnapshotInput::default())
            .await
            .unwrap();
        assert_eq!(p.list_snapshots("proj-a", &rec.id).await.unwrap().len(), 1);
        p.delete_snapshot("proj-a", &snap.id).await.unwrap();
        assert_eq!(p.list_snapshots("proj-a", &rec.id).await.unwrap().len(), 0);
        assert!(
            p.delete_snapshot("proj-a", &snap.id).await.is_err(),
            "deleting twice must error, not silently succeed"
        );
    }

    #[tokio::test]
    async fn mock_provider_domain_and_network_policy() {
        let p = MockSandboxProvider::new();
        let rec = p
            .create_sandbox(
                "t1",
                "proj-a",
                CreateSandboxInput {
                    name: "net-1".into(),
                    runtime: "node22".into(),
                    ports: vec![3000],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let url = p.domain("proj-a", &rec.id, 3000).await.unwrap();
        assert!(url.contains("3000"));
        let updated = p
            .update_network_policy(
                "proj-a",
                &rec.id,
                NetworkPolicy {
                    mode: "allowlist".into(),
                    allowed_domains: vec!["api.example.com".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.network_policy.mode, "allowlist");
        assert!(
            p.update_network_policy(
                "proj-a",
                &rec.id,
                NetworkPolicy {
                    mode: "bogus".into(),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn mock_provider_mount_lifecycle() {
        let p = MockSandboxProvider::new();
        let rec = p
            .create_sandbox(
                "t1",
                "proj-a",
                CreateSandboxInput {
                    name: "mnt-1".into(),
                    runtime: "node22".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mount = p
            .mount_storage(
                "proj-a",
                &rec.id,
                MountConfigInput {
                    mount_path: "/mnt/data".into(),
                    kind: "remote-fuse".into(),
                    mode: "read-only".into(),
                    provider: "s3".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(p.list_mounts("proj-a", &rec.id).await.unwrap().len(), 1);
        p.delete_mount("proj-a", &mount.id).await.unwrap();
        assert!(p.list_mounts("proj-a", &rec.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn tenant_and_project_authorization_boundaries() {
        let p = MockSandboxProvider::new();
        let rec = p
            .create_sandbox(
                "tenant-a",
                "proj-a",
                CreateSandboxInput {
                    name: "authz-1".into(),
                    runtime: "node22".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // A different project can never see or act on this sandbox, even by exact id.
        assert!(matches!(
            p.get_sandbox("proj-b", &rec.id).await,
            Err(SandboxError::NotFound(_))
        ));
        assert!(matches!(
            p.stop_sandbox("proj-b", &rec.id).await,
            Err(SandboxError::NotFound(_))
        ));
        assert!(matches!(
            p.delete_sandbox("proj-b", &rec.id).await,
            Err(SandboxError::NotFound(_))
        ));
        assert!(matches!(
            p.run_command(
                "proj-b",
                &rec.id,
                RunCommandInput {
                    cmd: "id".into(),
                    ..Default::default()
                }
            )
            .await,
            Err(SandboxError::NotFound(_))
        ));
    }
}
