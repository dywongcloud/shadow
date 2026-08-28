//! Fail-closed execution boundary for repository-controlled build commands.
//!
//! The executor deliberately has no host-process or alternate-runtime path. A
//! build is admitted only after Podman, the pinned runsc binary, the pinned
//! builder image, the quota-capable named-volume driver, and (when enabled) the
//! host-declared egress network have been probed. Repository bytes cross the
//! boundary with `podman cp`; the checkout is never bind-mounted.
//!
//! # Invocation/image capability contract (reviewed builder v8)
//!
//! The reviewed builder image is NOT started with a blanket `--cap-drop=all`
//! plus outer `no-new-privileges`, and its entrypoint is never bypassed. That
//! earlier invocation could not pass its own probe against the reviewed image:
//! in-image `buildah` is a broker CLIENT (`/usr/local/bin/buildah` ->
//! `/usr/local/libexec-hive-buildah-client`) that requires the image init's
//! broker, and the init (`/usr/local/libexec-hive-build-init`, the image
//! Entrypoint) refuses to start unless the container carries EXACTLY the
//! eleven capabilities in [`BUILDER_CAPABILITIES`] with `no_new_privs` CLEAR
//! at entry (`hive-build-init.c`, `require_initial_security`).
//!
//! Privilege separation is enforced INSIDE the sandbox by the init, not by
//! blanket outer flags: the init verifies the exact entry state, forks a
//! capability-retaining Buildah broker (which launches real Buildah inside a
//! nested user+mount namespace with `--isolation=chroot` forced by an option
//! allowlist), then forks the tenant with ALL capabilities dropped and
//! `no_new_privs` latched, and finally drops its own. Tenant build commands
//! therefore run with zero capabilities and NNP exactly as before; the eleven
//! capabilities exist only in the PID-separated broker. Why each is retained
//! (all consumed by chroot-isolation image assembly inside gVisor):
//! `SYS_CHROOT` (chroot into the build rootfs), `CHOWN`/`FOWNER`/`FSETID`
//! (foreign-uid ownership, modes and set-id bit preservation during layer
//! extraction/commit, e.g. `COPY --chown`), `DAC_OVERRIDE` (traversing store
//! content owned by other mapped ids), `SETUID`/`SETGID` (multi-id
//! `uid_map`/`gid_map` for the nested userns and `USER` directives in tenant
//! builds), `SETPCAP` (broker bounds the child capability set before exec),
//! `SYS_ADMIN` (bind mounts of /proc//dev into the chroot build root),
//! `MKNOD` (device nodes in tenant rootfs), `SETFCAP` (file-capability
//! xattrs in tenant images). A mixed invocation is structurally impossible:
//! under dropped capabilities or preset NNP the init exits 125 before any
//! tenant byte runs, and without the init the broker socket never exists, so
//! every nested-build path refuses. Never reintroduce the blanket flags
//! silently: they LOOK stricter but only disable the reviewed privilege
//! separation and were never a working contract against this image.
//!
//! Identity is pinned end-to-end: the installed declaration must carry the
//! byte-reproducible reviewed archive facts (`REVIEWED_BUILDER_*`), the
//! measured archive hash must match, and the Podman image id must equal the
//! reviewed OCI config digest. Any mismatch leaves the capability blank.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

pub const BUILD_EXECUTOR_PROTOCOL: u16 = 1;
pub const BUILD_EGRESS_POLICY_LABEL: &str = "io.shadw.hive.build-egress-policy";
const MANAGED_RESOURCE_LABEL: &str = "io.hive.build-executor.managed=true";
pub const SEALED_OUTPUT_MOUNT: &str = "/hive-build-output";
const WORKSPACE_MOUNT: &str = "/workspace";
const SOURCE_ROOT: &str = "/workspace/source";
const OUTPUT_ROOT: &str = "/artifact";
const ADMIN_OUTPUT_CAP: usize = 2 * 1024 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const BASE_PATH: &str = "/workspace/source/node_modules/.bin:/usr/local/bin:/usr/bin:/bin";
// `directfs` is deliberately absent (runsc's own default, true, applies).
// Forcing directfs=false makes gVisor fall back to the gofer-only filesystem
// path, and on this fleet's PVM-patched kernels (6.12.33-pvm+, the CVM/GPU
// hosts) that path's platform init dies immediately: "initialize systrap:
// unable to attach: operation not permitted" in the sandbox's own oci-log,
// surfaced to the podman client only as "cannot read client sync file:
// waiting for sandbox to start: EOF". Isolated live via single-flag
// bisection on sj2 (2026-08-28): directfs=false alone, no other flags,
// reproduces the failure against a bare `podman run --runtime runsc`; every
// other flag here works individually and combined. hive-cell-agent's own
// sandboxes never pass --directfs and boot fine on the same kernel.
const REQUIRED_RUNSC_RUNTIME_FLAGS: [&str; 7] = [
    "platform=systrap",
    "network=sandbox",
    "gvisor-marker-file=true",
    "host-uds=none",
    "host-fifo=none",
    "net-raw=false",
    "allow-flag-override=false",
];

/// Reviewed builder artifact facts (hive-build-executor v8, accepted after two
/// independent clean-BuildKit-root builds produced byte-identical archives).
/// The installed declaration must repeat these EXACTLY and the measured
/// archive must hash to them; any drift leaves the capability blank. There are
/// no floating defaults: shipping a new builder means reviewing and updating
/// these constants together with the archive.
///
/// The archive is the COMPRESSION-NORMALIZED artifact, not BuildKit's raw
/// export. BuildKit's uncompressed layers are byte-reproducible across
/// independent roots (identical diff_ids), but its gzip envelope is NOT
/// deterministic (measured: two independent roots gave identical uncompressed
/// content, 531,394,560 vs 468,183,040 compressed bytes). The recipe therefore
/// recompresses every layer with a single canonical gzip (fixed level, mtime=0,
/// no original-name) and rebuilds the manifest/index deterministically, so any
/// two independent builds normalize to the exact bytes pinned below. The image
/// CONFIG digest is unaffected (it references only uncompressed diff_ids), which
/// is why `builder_config_digest` == the podman image id stays stable across
/// the normalization.
const REVIEWED_BUILDER_ARCHIVE_SHA256: &str =
    "9e40310b3bd0591e1276bd3a58cd09832949d0c8686270d01b1a98e13e095682";
const REVIEWED_BUILDER_ARCHIVE_BYTES: u64 = 459_059_200;
const REVIEWED_BUILDER_MANIFEST_DIGEST: &str =
    "sha256:feb1c13108a6c4e3b5671d62f852e11e59a21d8ba3df79653c65a07665b71a54";
/// The OCI config digest IS the Podman image id for a loaded image; the
/// declaration's `builder_image_id` must equal it.
const REVIEWED_BUILDER_CONFIG_DIGEST: &str =
    "sha256:3c7f08e2e6acf73b3fa1c30b9382609eda86ac6dca04bbdf53c8141c74772835";
const REVIEWED_RUNSC_VERSION: &str = "release-20260817.0";
const REVIEWED_RUNSC_SHA256: &str =
    "048b89aada69dc3333422e139d6e9d02f8ab06bda52398060e0fbdacca00074c";
/// The image init: verified as the image Entrypoint at probe time and always
/// invoked explicitly, so a config drift can never silently bypass it.
const BUILDER_INIT_PATH: &str = "/usr/local/libexec-hive-build-init";
const BUILDER_INIT_ENTRYPOINT_FLAG: &str = "--entrypoint=/usr/local/libexec-hive-build-init";
/// The exact container capability set the image init requires at entry (see
/// the module doc for why each is retained by the broker). Order follows the
/// kernel capability numbers; the derived tenant bounding mask is
/// [`BUILDER_TENANT_BOUNDING_MASK`].
const BUILDER_CAPABILITIES: [&str; 11] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "SYS_CHROOT",
    "SYS_ADMIN",
    "MKNOD",
    "SETFCAP",
];
/// /proc/self/status CapBnd for the tenant: the eleven capabilities above as
/// a kernel bitmask. The tenant's effective/permitted/inheritable/ambient
/// sets must all read zero; only the unusable bounding set retains the mask
/// (no file capabilities or set-id binaries exist in the reviewed image).
const BUILDER_TENANT_BOUNDING_MASK: &str = "00000000882401db";
/// Mirrors the reviewed bring-up harness invocation; single-container runsc
/// sandboxes accept it inertly and CRI-style shims require it.
const SANDBOX_ANNOTATION: &str = "--annotation=io.kubernetes.cri.container-type=sandbox";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildExecutorErrorCode {
    InvalidConfig,
    InvalidRequest,
    CapabilityUnavailable,
    CapabilityMismatch,
    UnsupportedSurface,
    PodmanFailed,
    StepFailed,
    StepTimedOut,
    BuildTimedOut,
    LogLimitExceeded,
    WorkspaceLimitExceeded,
    OutputLimitExceeded,
    OutputEntryLimitExceeded,
    UnsafeOutput,
    SealIntegrityMismatch,
    CleanupFailed,
}

#[derive(Debug)]
pub struct BuildExecutorError {
    pub code: BuildExecutorErrorCode,
    pub operation: &'static str,
    detail: String,
}

impl BuildExecutorError {
    fn new(
        code: BuildExecutorErrorCode,
        operation: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            operation,
            detail: detail.into(),
        }
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for BuildExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.operation, self.detail)
    }
}

impl std::error::Error for BuildExecutorError {}

type Result<T> = std::result::Result<T, BuildExecutorError>;

#[derive(Clone, Debug)]
pub struct RunscPin {
    pub path: PathBuf,
    pub version: String,
    pub sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
pub struct BuildUser {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug)]
pub struct BuildVolumePolicy {
    /// Only Podman's local tmpfs-backed named volumes are accepted. The volume
    /// stays continuously mounted by an owned keeper container until its
    /// consumer is attached, so the kernel-enforced size limit and bytes remain
    /// live without exposing a host path.
    pub driver: String,
    /// Filesystem allocation units may round the reported size upward by at most
    /// this amount. It is policy-digested and must remain small.
    pub capacity_slack_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct BuildNetworkPolicy {
    pub network: String,
    pub bridge: String,
    pub subnet: String,
    pub gateway: String,
    pub dns_upstream_ipv4: Vec<String>,
    pub policy_id: String,
    pub policy_digest: String,
    pub verify_path: PathBuf,
    pub verify_sha256: [u8; 32],
    pub nft_binary: PathBuf,
    pub nft_policy_path: PathBuf,
    pub nft_table: String,
    pub nft_bridge_table: String,
    pub fleet_public_ipv4: Vec<String>,
    pub fleet_probe_ipv4: String,
    pub provision_lock_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct BuildLimits {
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub pids: u32,
    pub tmpfs_bytes: u64,
    pub workspace_bytes: u64,
    pub output_bytes: u64,
    pub output_entries: u64,
    pub log_bytes: u64,
    pub max_log_line_bytes: usize,
    pub max_step_time: Duration,
    pub max_total_time: Duration,
}

#[derive(Clone, Debug)]
pub struct BuildExecutorConfig {
    pub podman_path: PathBuf,
    pub podman_env: BTreeMap<String, String>,
    pub runsc: RunscPin,
    /// Exact immutable OCI image ID or digest reference.
    pub builder_image: String,
    pub user: BuildUser,
    pub volume: BuildVolumePolicy,
    pub limits: BuildLimits,
    /// When non-empty, repository variables must be in this set. The installed
    /// executor leaves it empty and instead admits every syntactically-valid,
    /// non-reserved key explicitly supplied by the trusted build coordinator.
    pub allowed_env: BTreeSet<String>,
    /// `None` means `--network=none`. Some means attach only to the named,
    /// live-verified host policy network.
    pub network_policy: Option<BuildNetworkPolicy>,
}

const INSTALLED_CAPABILITY_PATH: &str = "/var/lib/hive/build-executor.json";
static INSTALLED_EXECUTOR: std::sync::OnceLock<BuildExecutor> = std::sync::OnceLock::new();

/// Re-probe and publish the process-global executor. Failure is intentionally
/// represented as no protocol; repository builds remain fail-closed.
pub async fn init_installed() -> Result<u16> {
    let executor = BuildExecutor::installed().await?;
    let protocol = executor.capability().protocol;
    INSTALLED_EXECUTOR.set(executor).map_err(|_| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "initialize BuildExecutor",
            "executor was initialized more than once",
        )
    })?;
    Ok(protocol)
}

pub fn get() -> Result<BuildExecutor> {
    INSTALLED_EXECUTOR.get().cloned().ok_or_else(|| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "acquire BuildExecutor",
            "BUILD_ISOLATION_UNAVAILABLE: live-probed executor is not installed",
        )
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledCapability {
    protocol: String,
    builder_image_id: String,
    /// Optional reviewed-archive pin. When present, ALL five must be present
    /// and match the reviewed facts and the archive is re-hashed against them
    /// (fail-closed on corruption). When absent — the canonical
    /// `ansible/roles/build_executor` declaration shape, which this codebase
    /// cannot edit — the declaration parses and only the archive byte-check
    /// is skipped; the runsc/image-id/flags checks below still bind. Never
    /// make these silent-floating: an absent pin is a conscious declaration
    /// choice, a wrong value is always a refusal.
    #[serde(default)]
    builder_archive_path: Option<PathBuf>,
    #[serde(default)]
    builder_archive_sha256: Option<String>,
    #[serde(default)]
    builder_archive_bytes: Option<u64>,
    #[serde(default)]
    builder_manifest_digest: Option<String>,
    #[serde(default)]
    builder_config_digest: Option<String>,
    runsc_path: PathBuf,
    runsc_version: String,
    runsc_sha256: String,
    runsc_sidecar_path: PathBuf,
    runsc_sidecar_sha256: String,
    runsc_runtime_flags: Vec<String>,
    podman_path: PathBuf,
    podman_sha256: String,
    containers_conf_path: PathBuf,
    containers_conf_sha256: String,
    containers_conf_override_path: PathBuf,
    containers_conf_override_sha256: String,
    oci_hooks_dir: PathBuf,
    network_name: String,
    network_bridge: String,
    network_subnet: String,
    network_gateway: String,
    network_dns_upstream_ipv4: Vec<String>,
    network_policy_id: String,
    network_ipv6: bool,
    runtime_fallback: bool,
    provision_lock_path: PathBuf,
    network_policy_digest: String,
    network_verify_path: PathBuf,
    network_verify_sha256: String,
    nft_binary: PathBuf,
    nft_policy_path: PathBuf,
    nft_table: String,
    nft_bridge_table: String,
    fleet_public_ipv4: Vec<String>,
    fleet_probe_ipv4: String,
    builder_uid: u32,
    builder_gid: u32,
    workspace_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum BuildEgressCapability {
    Denied,
    HostDeclaredAttachment { network: String, policy_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildCapability {
    pub protocol: u16,
    pub policy_digest: String,
    pub podman_version: String,
    pub runsc_version: String,
    pub runsc_sha256: String,
    pub builder_image: String,
    pub builder_image_digest: String,
    pub uid: u32,
    pub gid: u32,
    pub volume_driver: String,
    pub egress: BuildEgressCapability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildSurface {
    RepositoryCommands,
    Dockerfile,
    Compose,
}

#[derive(Clone, Debug)]
pub struct BuildRequest {
    /// Platform-owned checkout path. It is canonicalized and copied, never
    /// mounted. The path itself is never returned by the executor.
    pub checkout: PathBuf,
    pub surface: BuildSurface,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn root() -> Self {
        Self(".".to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_relative_path(&value, "workspace path")?;
        Ok(Self(if value.is_empty() {
            ".".to_string()
        } else {
            value
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn container_path(&self) -> String {
        if self.0 == "." {
            SOURCE_ROOT.to_string()
        } else {
            format!("{SOURCE_ROOT}/{}", self.0)
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildStep {
    pub label: String,
    /// Fixed coordinator-authored shell program. Tenant values belong in `args`
    /// or `env`, never interpolated into this string.
    pub script: String,
    pub args: Vec<String>,
    pub cwd: WorkspacePath,
    pub env: BTreeMap<String, String>,
    /// A caller may tighten a step, never exceed the configured maximum.
    pub timeout: Option<Duration>,
    /// Preserve a repository command's status for decision-only steps such as
    /// vercel.json ignoreCommand. Ordinary install/build steps remain fail-fast.
    pub accept_nonzero: bool,
}

impl BuildStep {
    pub fn shell(label: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            script: script.into(),
            args: Vec::new(),
            cwd: WorkspacePath::root(),
            env: BTreeMap::new(),
            timeout: None,
            accept_nonzero: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildLogStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug)]
pub struct BuildLogLine {
    pub step: String,
    pub stream: BuildLogStream,
    pub text: String,
    pub dropped_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct BuildStepResult {
    pub label: String,
    pub exit_code: i32,
    pub elapsed: Duration,
    pub emitted_log_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedOutputDescriptor {
    pub protocol: u16,
    pub id: String,
    pub policy_digest: String,
    pub content_sha256: String,
    pub entries: u64,
    pub file_bytes: u64,
    /// Stable path after a consumer attaches the opaque volume read-only.
    pub root: String,
}

#[derive(Clone)]
pub struct BuildExecutor {
    inner: Arc<BuildExecutorInner>,
}

struct BuildExecutorInner {
    config: BuildExecutorConfig,
    capability: BuildCapability,
}

pub struct BuildSession {
    executor: BuildExecutor,
    container: String,
    workspace_volume: String,
    _lifecycle_lock: Option<Arc<std::fs::File>>,
    cleanup: CleanupGuard,
    surface: BuildSurface,
    deadline: Instant,
    emitted_log_bytes: u64,
}

pub struct SealedBuildOutput {
    executor: BuildExecutor,
    descriptor: SealedOutputDescriptor,
    volume: String,
    keeper: String,
    _lifecycle_lock: Option<Arc<std::fs::File>>,
    cleanup: CleanupGuard,
}

pub struct SealedCacheArtifact {
    path: Option<PathBuf>,
    pub content_sha256: String,
    pub bytes: u64,
}

pub(crate) struct SealedOutputMount {
    argument: OsString,
}

impl SealedOutputMount {
    pub(crate) fn argument(&self) -> &OsStr {
        &self.argument
    }

    pub(crate) fn root(&self) -> &'static str {
        SEALED_OUTPUT_MOUNT
    }
}

impl BuildExecutor {
    /// Load the root-published v1 declaration and re-probe every effective
    /// prerequisite. Absence or any mismatch is an error; callers advertise no
    /// capability and must not fall back to host execution.
    pub async fn installed() -> Result<Self> {
        let path = Path::new(INSTALLED_CAPABILITY_PATH);
        validate_trusted_regular_file(path, "BuildExecutor capability")?;
        let bytes = tokio::fs::read(path).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "load installed BuildExecutor",
                error.to_string(),
            )
        })?;
        if bytes.len() > 64 * 1024 {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidConfig,
                "load installed BuildExecutor",
                "capability declaration exceeds 64 KiB",
            ));
        }
        let declaration: InstalledCapability = serde_json::from_slice(&bytes).map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidConfig,
                "load installed BuildExecutor",
                format!("invalid capability JSON: {error}"),
            )
        })?;
        if declaration.protocol != "hive-build-executor/v1"
            || declaration.network_ipv6
            || declaration.runtime_fallback
            || declaration.runsc_version.trim().is_empty()
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "load installed BuildExecutor",
                "protocol, IPv6, runtime-fallback, or runsc-version declaration is incompatible",
            ));
        }
        if declaration
            .runsc_runtime_flags
            .iter()
            .map(String::as_str)
            .ne(REQUIRED_RUNSC_RUNTIME_FLAGS)
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "load installed BuildExecutor",
                "runsc wrapper flags differ from the reviewed fail-closed set",
            ));
        }
        // Reviewed-archive pin: all-or-nothing. A partial set is a malformed
        // declaration (refuse); a full set must match the reviewed facts and
        // the archive is re-hashed; an absent set (canonical role) parses and
        // skips only the archive byte-check.
        let archive_pin_present = declaration.builder_archive_path.is_some()
            || declaration.builder_archive_sha256.is_some()
            || declaration.builder_archive_bytes.is_some()
            || declaration.builder_manifest_digest.is_some()
            || declaration.builder_config_digest.is_some();
        if archive_pin_present {
            let archive_path = declaration.builder_archive_path.as_ref().ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidConfig,
                    "load installed BuildExecutor",
                    "builder archive pin is partial: builder_archive_path is required",
                )
            })?;
            let archive_sha = declaration
                .builder_archive_sha256
                .as_deref()
                .ok_or_else(|| {
                    BuildExecutorError::new(
                        BuildExecutorErrorCode::InvalidConfig,
                        "load installed BuildExecutor",
                        "builder archive pin is partial: builder_archive_sha256 is required",
                    )
                })?;
            let archive_bytes = declaration.builder_archive_bytes.ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidConfig,
                    "load installed BuildExecutor",
                    "builder archive pin is partial: builder_archive_bytes is required",
                )
            })?;
            let manifest_digest =
                declaration
                    .builder_manifest_digest
                    .as_deref()
                    .ok_or_else(|| {
                        BuildExecutorError::new(
                            BuildExecutorErrorCode::InvalidConfig,
                            "load installed BuildExecutor",
                            "builder archive pin is partial: builder_manifest_digest is required",
                        )
                    })?;
            let config_digest = declaration
                .builder_config_digest
                .as_deref()
                .ok_or_else(|| {
                    BuildExecutorError::new(
                        BuildExecutorErrorCode::InvalidConfig,
                        "load installed BuildExecutor",
                        "builder archive pin is partial: builder_config_digest is required",
                    )
                })?;
            if archive_sha != REVIEWED_BUILDER_ARCHIVE_SHA256
                || archive_bytes != REVIEWED_BUILDER_ARCHIVE_BYTES
                || manifest_digest != REVIEWED_BUILDER_MANIFEST_DIGEST
                || config_digest != REVIEWED_BUILDER_CONFIG_DIGEST
            {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityMismatch,
                    "load installed BuildExecutor",
                    "declaration differs from the reviewed builder/runsc facts",
                ));
            }
            validate_trusted_regular_file(archive_path, "reviewed builder OCI archive")?;
            let measured_archive = sha256_file(archive_path).await?;
            let archive_metadata = tokio::fs::metadata(archive_path).await.map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "load installed BuildExecutor",
                    format!("cannot measure builder archive: {error}"),
                )
            })?;
            if hex_bytes(&measured_archive) != REVIEWED_BUILDER_ARCHIVE_SHA256
                || archive_metadata.len() != REVIEWED_BUILDER_ARCHIVE_BYTES
            {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityMismatch,
                    "load installed BuildExecutor",
                    format!(
                        "builder archive differs from the reviewed facts: measured sha256 {} ({} bytes)",
                        hex_bytes(&measured_archive),
                        archive_metadata.len()
                    ),
                ));
            }
        }
        if declaration.builder_image_id.trim_start_matches("sha256:")
            != REVIEWED_BUILDER_CONFIG_DIGEST.trim_start_matches("sha256:")
            || declaration.runsc_version != REVIEWED_RUNSC_VERSION
            || declaration.runsc_sha256 != REVIEWED_RUNSC_SHA256
            || declaration.builder_uid != 65532
            || declaration.builder_gid != 65532
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "load installed BuildExecutor",
                "declaration differs from the reviewed builder/runsc facts",
            ));
        }
        validate_trusted_executable(&declaration.podman_path, "BuildExecutor Podman wrapper")?;
        validate_trusted_executable(&declaration.runsc_sidecar_path, "runsc sidecar")?;
        validate_trusted_regular_file(
            &declaration.containers_conf_path,
            "BuildExecutor containers.conf",
        )?;
        validate_trusted_regular_file(
            &declaration.containers_conf_override_path,
            "BuildExecutor containers.conf override",
        )?;
        validate_trusted_empty_dir(
            &declaration.oci_hooks_dir,
            "BuildExecutor OCI hooks directory",
        )?;
        for (path, expected, label) in [
            (
                declaration.podman_path.as_path(),
                declaration.podman_sha256.as_str(),
                "BuildExecutor Podman wrapper",
            ),
            (
                declaration.runsc_sidecar_path.as_path(),
                declaration.runsc_sidecar_sha256.as_str(),
                "runsc sidecar",
            ),
            (
                declaration.containers_conf_path.as_path(),
                declaration.containers_conf_sha256.as_str(),
                "BuildExecutor containers.conf",
            ),
            (
                declaration.containers_conf_override_path.as_path(),
                declaration.containers_conf_override_sha256.as_str(),
                "BuildExecutor containers.conf override",
            ),
        ] {
            let expected = parse_sha256(expected, label)?;
            let measured = sha256_file(path).await?;
            if measured != expected {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityMismatch,
                    "load installed BuildExecutor",
                    format!("{label} checksum differs from the published capability"),
                ));
            }
        }
        let fleet_addresses = declaration
            .fleet_public_ipv4
            .iter()
            .map(|address| {
                address
                    .parse::<std::net::Ipv4Addr>()
                    .ok()
                    .filter(|parsed| parsed.to_string() == *address)
            })
            .collect::<Option<Vec<_>>>();
        let dns_addresses = declaration
            .network_dns_upstream_ipv4
            .iter()
            .map(|address| {
                address
                    .parse::<std::net::Ipv4Addr>()
                    .ok()
                    .filter(|parsed| parsed.to_string() == *address)
            })
            .collect::<Option<Vec<_>>>();
        let fleet_addresses_strictly_sorted = fleet_addresses.as_ref().is_some_and(|addresses| {
            !addresses.is_empty() && addresses.windows(2).all(|pair| pair[0] < pair[1])
        });
        let dns_addresses_unique = dns_addresses.as_ref().is_some_and(|addresses| {
            !addresses.is_empty()
                && addresses.iter().copied().collect::<BTreeSet<_>>().len() == addresses.len()
        });
        if !dns_addresses_unique
            || declaration.network_policy_id.trim().is_empty()
            || declaration.nft_bridge_table.trim().is_empty()
            || !declaration.network_verify_path.is_absolute()
            || !declaration.provision_lock_path.is_absolute()
            || !fleet_addresses_strictly_sorted
            || !declaration
                .fleet_public_ipv4
                .contains(&declaration.fleet_probe_ipv4)
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "load installed BuildExecutor",
                "network DNS, policy identity, verifier, bridge table, fleet addresses, or provision lock declaration is incomplete",
            ));
        }
        validate_trusted_executable(
            &declaration.network_verify_path,
            "BuildExecutor network verifier",
        )?;
        validate_trusted_regular_file(
            &declaration.provision_lock_path,
            "BuildExecutor lifecycle lock",
        )?;
        let mut podman_env = BTreeMap::new();
        podman_env.insert(
            "PATH".to_string(),
            "/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        );
        podman_env.insert("HOME".to_string(), "/root".to_string());
        let config = BuildExecutorConfig {
            podman_path: declaration.podman_path,
            podman_env,
            runsc: RunscPin {
                path: declaration.runsc_path,
                version: declaration.runsc_version,
                sha256: parse_sha256(&declaration.runsc_sha256, "runsc sha256")?,
            },
            builder_image: declaration.builder_image_id,
            user: BuildUser {
                uid: declaration.builder_uid,
                gid: declaration.builder_gid,
            },
            volume: BuildVolumePolicy {
                driver: "local".to_string(),
                capacity_slack_bytes: 4 * 1024 * 1024,
            },
            limits: BuildLimits {
                cpu_millis: 4_000,
                memory_bytes: 8 * 1024 * 1024 * 1024,
                pids: 512,
                tmpfs_bytes: 512 * 1024 * 1024,
                workspace_bytes: declaration.workspace_bytes,
                output_bytes: declaration.workspace_bytes,
                output_entries: 200_000,
                log_bytes: 16 * 1024 * 1024,
                max_log_line_bytes: 64 * 1024,
                max_step_time: Duration::from_secs(20 * 60),
                max_total_time: Duration::from_secs(45 * 60),
            },
            allowed_env: BTreeSet::new(),
            network_policy: Some(BuildNetworkPolicy {
                network: declaration.network_name,
                bridge: declaration.network_bridge,
                subnet: declaration.network_subnet,
                gateway: declaration.network_gateway,
                dns_upstream_ipv4: declaration.network_dns_upstream_ipv4,
                policy_id: declaration.network_policy_id,
                policy_digest: declaration.network_policy_digest,
                verify_path: declaration.network_verify_path,
                verify_sha256: parse_sha256(
                    &declaration.network_verify_sha256,
                    "network verifier sha256",
                )?,
                nft_binary: declaration.nft_binary,
                nft_policy_path: declaration.nft_policy_path,
                nft_table: declaration.nft_table,
                nft_bridge_table: declaration.nft_bridge_table,
                fleet_public_ipv4: declaration.fleet_public_ipv4,
                fleet_probe_ipv4: declaration.fleet_probe_ipv4,
                provision_lock_path: declaration.provision_lock_path,
            }),
        };
        Self::new(config).await
    }

    pub async fn new(config: BuildExecutorConfig) -> Result<Self> {
        let capability = Self::probe(&config).await?;
        Ok(Self {
            inner: Arc::new(BuildExecutorInner { config, capability }),
        })
    }

    pub fn capability(&self) -> &BuildCapability {
        &self.inner.capability
    }

    pub fn policy_digest(config: &BuildExecutorConfig) -> Result<String> {
        validate_config(config)?;
        let mut hasher = Sha256::new();
        digest_field(&mut hasher, "domain", b"hive-build-executor-policy-v1\0");
        digest_field(
            &mut hasher,
            "protocol",
            &BUILD_EXECUTOR_PROTOCOL.to_le_bytes(),
        );
        digest_field(&mut hasher, "runsc-sha256", &config.runsc.sha256);
        for flag in REQUIRED_RUNSC_RUNTIME_FLAGS {
            digest_field(&mut hasher, "runsc-runtime-flag", flag.as_bytes());
        }
        digest_field(
            &mut hasher,
            "builder-image",
            config.builder_image.as_bytes(),
        );
        digest_field(&mut hasher, "builder-init", BUILDER_INIT_PATH.as_bytes());
        for capability in BUILDER_CAPABILITIES {
            digest_field(&mut hasher, "builder-capability", capability.as_bytes());
        }
        digest_field(
            &mut hasher,
            "builder-tenant-bounding",
            BUILDER_TENANT_BOUNDING_MASK.as_bytes(),
        );
        digest_field(&mut hasher, "uid", &config.user.uid.to_le_bytes());
        digest_field(&mut hasher, "gid", &config.user.gid.to_le_bytes());
        digest_field(
            &mut hasher,
            "volume-driver",
            config.volume.driver.as_bytes(),
        );
        digest_field(
            &mut hasher,
            "volume-kind",
            b"named-local-tmpfs-continuously-mounted",
        );
        digest_field(
            &mut hasher,
            "volume-capacity-slack",
            &config.volume.capacity_slack_bytes.to_le_bytes(),
        );
        let limits = &config.limits;
        for (label, value) in [
            ("cpu-millis", limits.cpu_millis as u64),
            ("memory-bytes", limits.memory_bytes),
            ("pids", limits.pids as u64),
            ("tmpfs-bytes", limits.tmpfs_bytes),
            ("workspace-bytes", limits.workspace_bytes),
            ("output-bytes", limits.output_bytes),
            ("output-entries", limits.output_entries),
            ("log-bytes", limits.log_bytes),
            ("max-log-line-bytes", limits.max_log_line_bytes as u64),
            ("max-step-ms", duration_ms(limits.max_step_time)),
            ("max-total-ms", duration_ms(limits.max_total_time)),
        ] {
            digest_field(&mut hasher, label, &value.to_le_bytes());
        }
        for key in &config.allowed_env {
            digest_field(&mut hasher, "allowed-env", key.as_bytes());
        }
        match &config.network_policy {
            None => digest_field(&mut hasher, "egress", b"denied"),
            Some(policy) => {
                digest_field(&mut hasher, "egress", b"host-declared-attachment");
                digest_field(&mut hasher, "network", policy.network.as_bytes());
                digest_field(&mut hasher, "network-bridge", policy.bridge.as_bytes());
                digest_field(&mut hasher, "network-subnet", policy.subnet.as_bytes());
                digest_field(&mut hasher, "network-gateway", policy.gateway.as_bytes());
                for address in &policy.dns_upstream_ipv4 {
                    digest_field(&mut hasher, "network-dns-ipv4", address.as_bytes());
                }
                digest_field(
                    &mut hasher,
                    "network-policy-id",
                    policy.policy_id.as_bytes(),
                );
                digest_field(
                    &mut hasher,
                    "network-policy-digest",
                    policy.policy_digest.as_bytes(),
                );
                digest_field(
                    &mut hasher,
                    "network-verifier",
                    policy.verify_path.as_os_str().as_encoded_bytes(),
                );
                digest_field(
                    &mut hasher,
                    "network-verifier-sha256",
                    &policy.verify_sha256,
                );
                digest_field(
                    &mut hasher,
                    "provision-lock",
                    policy.provision_lock_path.as_os_str().as_encoded_bytes(),
                );
                digest_field(&mut hasher, "nft-table", policy.nft_table.as_bytes());
                digest_field(
                    &mut hasher,
                    "nft-bridge-table",
                    policy.nft_bridge_table.as_bytes(),
                );
                for address in &policy.fleet_public_ipv4 {
                    digest_field(&mut hasher, "fleet-ipv4", address.as_bytes());
                }
                digest_field(
                    &mut hasher,
                    "fleet-probe-ipv4",
                    policy.fleet_probe_ipv4.as_bytes(),
                );
            }
        }
        Ok(hex_bytes(&hasher.finalize()))
    }

    pub async fn probe(config: &BuildExecutorConfig) -> Result<BuildCapability> {
        validate_config(config)?;
        validate_trusted_executable(&config.podman_path, "podman binary")?;
        validate_trusted_executable(&config.runsc.path, "runsc binary")?;

        let measured_runsc = sha256_file(&config.runsc.path).await?;
        if measured_runsc != config.runsc.sha256 {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe runsc",
                format!(
                    "runsc sha256 mismatch: configured {}, measured {}",
                    hex_bytes(&config.runsc.sha256),
                    hex_bytes(&measured_runsc)
                ),
            ));
        }
        let mut runsc = Command::new(&config.runsc.path);
        runsc
            .arg("--version")
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let runsc_output = tokio::time::timeout(Duration::from_secs(10), runsc.output())
            .await
            .map_err(|_| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "probe runsc",
                    "runsc version probe timed out",
                )
            })?
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "probe runsc",
                    error.to_string(),
                )
            })?;
        let runsc_text = format!(
            "{}\n{}",
            bounded_text(&runsc_output.stdout, 4096),
            bounded_text(&runsc_output.stderr, 4096)
        );
        if !runsc_output.status.success() || !runsc_text.contains(&config.runsc.version) {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe runsc",
                format!(
                    "runsc did not report pinned version {:?}",
                    config.runsc.version
                ),
            ));
        }

        let policy_digest = Self::policy_digest(config)?;
        let executor = Self {
            inner: Arc::new(BuildExecutorInner {
                config: config.clone(),
                capability: BuildCapability {
                    protocol: BUILD_EXECUTOR_PROTOCOL,
                    policy_digest: policy_digest.clone(),
                    podman_version: String::new(),
                    runsc_version: config.runsc.version.clone(),
                    runsc_sha256: hex_bytes(&measured_runsc),
                    builder_image: config.builder_image.clone(),
                    builder_image_digest: image_digest(&config.builder_image)?.to_string(),
                    uid: config.user.uid,
                    gid: config.user.gid,
                    volume_driver: config.volume.driver.clone(),
                    egress: BuildEgressCapability::Denied,
                },
            }),
        };

        let version = executor
            .podman_output([os("--version")], Duration::from_secs(10))
            .await?;
        require_success("probe podman", &version)?;
        let podman_version = one_line(&version.stdout);
        if podman_version.is_empty() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "probe podman",
                "podman returned an empty version",
            ));
        }

        let inspect = executor
            .podman_output(
                [os("image"), os("inspect"), os(&config.builder_image)],
                Duration::from_secs(20),
            )
            .await?;
        require_success("probe builder image", &inspect)?;
        verify_image_inspect(&inspect.stdout, &config.builder_image)?;

        let egress = match &config.network_policy {
            None => BuildEgressCapability::Denied,
            Some(policy) => {
                executor.verify_network_policy(policy).await?;
                BuildEgressCapability::HostDeclaredAttachment {
                    network: policy.network.clone(),
                    policy_id: policy.policy_id.clone(),
                }
            }
        };

        executor.probe_sandbox_and_volume().await?;
        Ok(BuildCapability {
            protocol: BUILD_EXECUTOR_PROTOCOL,
            policy_digest,
            podman_version,
            runsc_version: config.runsc.version.clone(),
            runsc_sha256: hex_bytes(&measured_runsc),
            builder_image: config.builder_image.clone(),
            builder_image_digest: image_digest(&config.builder_image)?.to_string(),
            uid: config.user.uid,
            gid: config.user.gid,
            volume_driver: config.volume.driver.clone(),
            egress,
        })
    }

    async fn acquire_lifecycle_lock(&self) -> Result<Option<Arc<std::fs::File>>> {
        let Some(policy) = self.inner.config.network_policy.as_ref() else {
            return Ok(None);
        };
        let path = policy.provision_lock_path.clone();
        let acquired = tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::MetadataExt;

            let path_metadata = std::fs::symlink_metadata(&path)?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path)?;
            let opened = file.metadata()?;
            if !opened.is_file()
                || path_metadata.file_type().is_symlink()
                || path_metadata.dev() != opened.dev()
                || path_metadata.ino() != opened.ino()
                || opened.uid() != 0
                || opened.gid() != 0
                || opened.mode() & 0o777 != 0o600
                || opened.nlink() != 1
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "lifecycle lock is not the trusted root-owned file",
                ));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(300);
            loop {
                let locked =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
                if locked == 0 {
                    return Ok(file);
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() != std::io::ErrorKind::WouldBlock
                    || std::time::Instant::now() >= deadline
                {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .await
        .map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "acquire BuildExecutor lifecycle lock",
                error.to_string(),
            )
        })?
        .map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "acquire BuildExecutor lifecycle lock",
                error.to_string(),
            )
        })?;
        Ok(Some(Arc::new(acquired)))
    }

    pub async fn begin(&self, request: BuildRequest) -> Result<BuildSession> {
        let surface = request.surface;
        let deadline = Instant::now() + self.inner.config.limits.max_total_time;
        if surface != BuildSurface::RepositoryCommands {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::UnsupportedSurface,
                "begin build",
                "builder protocol v1 rejects Dockerfile and Compose surfaces",
            ));
        }
        let requested_metadata = tokio::fs::symlink_metadata(&request.checkout)
            .await
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "begin build",
                    format!("cannot inspect requested checkout: {error}"),
                )
            })?;
        if !requested_metadata.is_dir() || requested_metadata.file_type().is_symlink() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "begin build",
                "checkout must be a real directory, not a symlink",
            ));
        }
        let checkout = tokio::fs::canonicalize(&request.checkout)
            .await
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "begin build",
                    format!("cannot canonicalize checkout: {error}"),
                )
            })?;
        let metadata = tokio::fs::symlink_metadata(&checkout)
            .await
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "begin build",
                    format!("cannot inspect checkout: {error}"),
                )
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "begin build",
                "checkout must remain a real directory after canonicalization",
            ));
        }
        let lifecycle_lock = self.acquire_lifecycle_lock().await?;
        if let Some(policy) = self.inner.config.network_policy.as_ref() {
            self.verify_network_policy(policy).await?;
        }
        let id = Uuid::new_v4().simple().to_string();
        let workspace_volume = format!("hive-build-ws-{id}");
        let container = format!("hive-build-{id}");
        let mut cleanup = CleanupGuard::new(self);
        self.create_volume(&workspace_volume, self.inner.config.limits.workspace_bytes)
            .await?;
        cleanup.add_volume(workspace_volume.clone());

        let idle = "trap 'exit 0' TERM INT; while :; do /bin/sleep 3600; done";
        let args = self.container_create_args(
            &container,
            &[format!(
                "{workspace_volume}:{WORKSPACE_MOUNT}:rw,U,nodev,nosuid"
            )],
            None,
            self.inner.config.network_policy.as_ref(),
            WORKSPACE_MOUNT,
            idle,
            &[],
            false,
        );
        let created = self.podman_output(args, Duration::from_secs(30)).await?;
        require_success("create build container", &created)?;
        cleanup.add_container(container.clone());
        self.verify_container_mounts(&container, &[WORKSPACE_MOUNT])
            .await?;

        let started = self
            .podman_output([os("start"), os(&container)], Duration::from_secs(30))
            .await?;
        require_success("start build container", &started)?;

        let prep = self
            .exec_fixed(
                &container,
                SOURCE_ROOT,
                "set -eu; mkdir -p /workspace/source /workspace/home; test -w /workspace/source",
                &[],
                Duration::from_secs(20),
            )
            .await?;
        require_success("prepare build workspace", &prep)?;

        let mut source = checkout.into_os_string();
        source.push("/.");
        let copied = self
            .podman_output(
                [
                    os("cp"),
                    os("--archive=false"),
                    source,
                    os(format!("{container}:{SOURCE_ROOT}")),
                ],
                self.inner.config.limits.max_step_time,
            )
            .await?;
        require_success("copy checkout into isolated volume", &copied)?;
        self.verify_volume_capacity(
            &container,
            WORKSPACE_MOUNT,
            self.inner.config.limits.workspace_bytes,
        )
        .await?;
        if Instant::now() >= deadline {
            let _ = cleanup.cleanup(self).await;
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::BuildTimedOut,
                "begin build",
                "build-wide deadline expired while importing the checkout",
            ));
        }

        Ok(BuildSession {
            executor: self.clone(),
            container,
            workspace_volume,
            _lifecycle_lock: lifecycle_lock,
            cleanup,
            surface,
            deadline,
            emitted_log_bytes: 0,
        })
    }

    async fn probe_sandbox_and_volume(&self) -> Result<()> {
        let id = Uuid::new_v4().simple().to_string();
        let volume = format!("hive-build-probe-vol-{id}");
        let writer = format!("hive-build-probe-write-{id}");
        let reader = format!("hive-build-probe-read-{id}");
        let mut cleanup = CleanupGuard::new(self);
        self.create_volume(&volume, self.inner.config.limits.output_bytes)
            .await?;
        cleanup.add_volume(volume.clone());

        // The writer's tenant first runs the in-image selfcheck, which
        // witnesses the full runtime contract from inside the sandbox: exact
        // uid/gid 65532, tenant CapEff/CapPrm/CapInh/CapAmb all zero, tenant
        // CapBnd exactly BUILDER_TENANT_BOUNDING_MASK, no_new_privs latched,
        // a live broker, and the pinned toolchain versions. The output file
        // is renamed into place atomically so the exec probe below never
        // reads a partial write.
        let idle = "hive-builder-selfcheck > /workspace/.hive-selfcheck.tmp 2>&1 \
            && mv /workspace/.hive-selfcheck.tmp /workspace/.hive-selfcheck.out \
            || { mv /workspace/.hive-selfcheck.tmp /workspace/.hive-selfcheck.out 2>/dev/null; exit 125; }; \
            trap 'exit 0' TERM INT; while :; do /bin/sleep 3600; done";
        let args = self.container_create_args(
            &writer,
            &[format!("{volume}:{WORKSPACE_MOUNT}:rw,U,nodev,nosuid")],
            None,
            self.inner.config.network_policy.as_ref(),
            WORKSPACE_MOUNT,
            idle,
            &[],
            true,
        );
        let created = self.podman_output(args, Duration::from_secs(30)).await?;
        require_success("probe create sandbox", &created)?;
        cleanup.add_container(writer.clone());
        self.verify_container_mounts(&writer, &[WORKSPACE_MOUNT])
            .await?;
        let started = self
            .podman_output([os("start"), os(&writer)], Duration::from_secs(30))
            .await?;
        require_success("probe start runsc sandbox", &started)?;
        // Exec'd processes never inherit the init's broker environment, so
        // the probe recovers the per-boot abstract socket name from the
        // supervising init (container PID 1, same uid) and talks to the
        // broker exactly the way tenant mains do. Buildah runs brokered with
        // the chroot isolation the broker's option allowlist enforces; a
        // direct or rootless invocation has no working path in this image.
        let script = "set -eu; \
            test \"$(cat /proc/gvisor/kernel_is_gvisor)\" = gvisor; \
            test \"$(/usr/bin/id -u)\" = \"$1\"; \
            test \"$(/usr/bin/id -g)\" = \"$2\"; \
            i=0; until test -s /workspace/.hive-selfcheck.out; do \
              i=$((i+1)); test \"$i\" -le 120; /bin/sleep 1; done; \
            /usr/bin/tail -n 1 /workspace/.hive-selfcheck.out | /usr/bin/grep -qx selfcheck=PASS; \
            for c in docker podman nerdctl; do ! command -v \"$c\" >/dev/null 2>&1; done; \
            test \"$(command -v buildah)\" = /usr/local/bin/buildah; \
            HIVE_BUILDAH_SOCKET=$(/usr/bin/tr '\\0' '\\n' < /proc/1/environ | /usr/bin/sed -n 's/^HIVE_BUILDAH_SOCKET=//p'); \
            test -n \"$HIVE_BUILDAH_SOCKET\"; export HIVE_BUILDAH_SOCKET; \
            test \"$(buildah __hive_broker_security_v1)\" = broker_security=PASS; \
            set -- $(/usr/bin/stat -f -c '%S %b' /workspace); \
            cap=$(($1 * $2)); printf '%s\\n' \"$cap\"; \
            printf hive-build-volume-probe > /workspace/probe; \
            mkdir -p /workspace/.buildah/xdg /workspace/.buildah/tmp; \
            rm -rf /workspace/oci-probe; \
            mkdir -p /workspace/oci-probe/store /workspace/oci-probe/run /workspace/oci-probe/context; \
            printf 'FROM scratch\\nCOPY marker /marker\\n' >/workspace/oci-probe/context/Containerfile; \
            printf verified >/workspace/oci-probe/context/marker; \
            buildah --root=/workspace/oci-probe/store --runroot=/workspace/oci-probe/run \
              --storage-driver=vfs bud --isolation=chroot --format=oci --layers=false --no-cache \
              --timestamp=0 --tag localhost/hive-build-probe:latest /workspace/oci-probe/context >/dev/null 2>&1; \
            buildah --root=/workspace/oci-probe/store --runroot=/workspace/oci-probe/run \
              --storage-driver=vfs push --format=oci localhost/hive-build-probe:latest \
              oci-archive:/workspace/probe.oci:hive-build-probe >/dev/null 2>&1; \
            test -s /workspace/probe.oci";
        let extra = [
            self.inner.config.user.uid.to_string(),
            self.inner.config.user.gid.to_string(),
        ];
        let ran = self
            .exec_fixed(
                &writer,
                WORKSPACE_MOUNT,
                script,
                &extra,
                self.inner.config.limits.max_step_time,
            )
            .await?;
        require_success("probe runsc sandbox and nested buildah", &ran)?;
        let capacity = one_line(&ran.stdout).parse::<u64>().map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe volume quota",
                format!("sandbox returned an invalid capacity: {error}"),
            )
        })?;
        self.require_capacity(
            capacity,
            self.inner.config.limits.output_bytes,
            "probe volume quota",
        )?;

        // Start the reader while the writer still pins the tmpfs mount. Named
        // local tmpfs bytes do not survive a complete unmount, so continuous
        // ownership is part of the contract and is exercised here.
        let args = self.container_create_args(
            &reader,
            &[format!("{volume}:{WORKSPACE_MOUNT}:ro,nodev,nosuid")],
            None,
            None,
            WORKSPACE_MOUNT,
            "set -eu; test \"$(cat /workspace/probe)\" = hive-build-volume-probe",
            &[],
            false,
        );
        let created = self.podman_output(args, Duration::from_secs(30)).await?;
        require_success("probe create persistence reader", &created)?;
        cleanup.add_container(reader.clone());
        let ran = self
            .podman_output(
                [os("start"), os("--attach"), os(&reader)],
                Duration::from_secs(30),
            )
            .await?;
        require_success("probe continuously mounted named volume", &ran)?;
        cleanup.cleanup(self).await
    }

    async fn verify_network_policy(&self, policy: &BuildNetworkPolicy) -> Result<()> {
        let output = self
            .podman_output(
                [os("network"), os("inspect"), os(&policy.network)],
                Duration::from_secs(15),
            )
            .await?;
        require_success("probe egress network", &output)?;
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress network",
                format!("invalid network inspect JSON: {error}"),
            )
        })?;
        let network = first_object(&value).ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress network",
                "network inspect returned no object",
            )
        })?;
        let name = json_str(network, &["name", "Name"]).unwrap_or_default();
        let driver = json_str(network, &["driver", "Driver"]).unwrap_or_default();
        let bridge =
            json_str(network, &["network_interface", "NetworkInterface"]).unwrap_or_default();
        let ipv6 = network
            .get("ipv6_enabled")
            .or_else(|| network.get("IPv6Enabled"))
            .and_then(serde_json::Value::as_bool);
        let internal = network
            .get("internal")
            .or_else(|| network.get("Internal"))
            .and_then(serde_json::Value::as_bool);
        let dns = network
            .get("dns_enabled")
            .or_else(|| network.get("DNSEnabled"))
            .and_then(serde_json::Value::as_bool);
        let dns_servers = network
            .get("network_dns_servers")
            .or_else(|| network.get("NetworkDNSServers"))
            .and_then(serde_json::Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .map(serde_json::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        let expected_dns = policy
            .dns_upstream_ipv4
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let dns_exact = dns_servers.len() == policy.dns_upstream_ipv4.len()
            && dns_servers.iter().copied().collect::<BTreeSet<_>>() == expected_dns;
        let policy_label = network
            .get("labels")
            .or_else(|| network.get("Labels"))
            .and_then(serde_json::Value::as_object)
            .and_then(|labels| labels.get("io.hive.build-executor.policy-id"))
            .and_then(serde_json::Value::as_str);
        let subnet_ok = network
            .get("subnets")
            .or_else(|| network.get("Subnets"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.len() == 1
                    && items[0].get("subnet").and_then(serde_json::Value::as_str)
                        == Some(policy.subnet.as_str())
                    && items[0].get("gateway").and_then(serde_json::Value::as_str)
                        == Some(policy.gateway.as_str())
            });
        if name != policy.network
            || driver != "bridge"
            || bridge != policy.bridge
            || ipv6 != Some(false)
            || internal != Some(false)
            || dns != Some(true)
            || !dns_exact
            || policy_label != Some(policy.policy_id.as_str())
            || !subnet_ok
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress network",
                "effective network differs from the declared IPv4-only bridge definition",
            ));
        }

        validate_trusted_regular_file(&policy.nft_policy_path, "nft policy")?;
        let policy_hash = sha256_file(&policy.nft_policy_path).await?;
        if policy.policy_digest != format!("sha256:{}", hex_bytes(&policy_hash)) {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress policy",
                "installed nft policy bytes do not match the capability digest",
            ));
        }
        validate_trusted_executable(&policy.nft_binary, "nft binary")?;
        let mut command = Command::new(&policy.nft_binary);
        command
            .args(["list", "table", "inet", policy.nft_table.as_str()])
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .map_err(|_| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "probe egress policy",
                    "nft live-table query timed out",
                )
            })?
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "probe egress policy",
                    error.to_string(),
                )
            })?;
        if !output.status.success() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress policy",
                format!(
                    "live nft table is absent: {}",
                    bounded_text(&output.stderr, 2048)
                ),
            ));
        }

        validate_trusted_executable(&policy.verify_path, "network policy verifier")?;
        let verifier_hash = sha256_file(&policy.verify_path).await?;
        if verifier_hash != policy.verify_sha256 {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress policy",
                "installed network verifier bytes differ from the capability",
            ));
        }
        let mut verifier = Command::new(&policy.verify_path);
        verifier
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/root")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let verified = tokio::time::timeout(Duration::from_secs(45), verifier.output())
            .await
            .map_err(|_| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "probe egress policy",
                    "network verifier timed out",
                )
            })?
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "probe egress policy",
                    error.to_string(),
                )
            })?;
        if !verified.status.success() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe egress policy",
                format!(
                    "root-published network verifier rejected live policy: {}",
                    bounded_text(&verified.stderr, 4096)
                ),
            ));
        }
        Ok(())
    }

    async fn create_volume(&self, name: &str, bytes: u64) -> Result<()> {
        let user = &self.inner.config.user;
        let args = [
            os("volume"),
            os("create"),
            os("--driver=local"),
            os("--opt=type=tmpfs"),
            os("--opt=device=tmpfs"),
            os(format!(
                "--opt=o=size={bytes},uid={},gid={},mode=0700,nosuid,nodev",
                user.uid, user.gid
            )),
            os("--label"),
            os(format!(
                "io.shadw.hive.build-policy={}",
                self.inner.capability.policy_digest
            )),
            os("--label"),
            os(MANAGED_RESOURCE_LABEL),
            os(name),
        ];
        let output = self.podman_output(args, Duration::from_secs(20)).await?;
        require_success("create quota volume", &output)
    }

    fn container_create_args(
        &self,
        name: &str,
        mounts: &[String],
        env_file: Option<&Path>,
        network: Option<&BuildNetworkPolicy>,
        workdir: &str,
        script: &str,
        script_args: &[String],
        // `true` keeps the image environment plus the init-exported broker
        // variables for the tenant (`HIVE_BUILDAH_SOCKET`/`_BROKER_PID`, only
        // sound for coordinator-authored probe scripts); `false` scrubs the
        // tenant environment with `env -i` exactly as before.
        image_env: bool,
    ) -> Vec<OsString> {
        let config = &self.inner.config;
        let limits = &config.limits;
        let cpus = format!(
            "{}.{:03}",
            limits.cpu_millis / 1000,
            limits.cpu_millis % 1000
        );
        let user = format!("{}:{}", config.user.uid, config.user.gid);
        let tmpfs_each = (limits.tmpfs_bytes / 3).max(4096);
        let mut args = vec![
            os("create"),
            os("--name"),
            os(name),
            os("--label"),
            os(MANAGED_RESOURCE_LABEL),
            os("--pull=never"),
            os("--http-proxy=false"),
            os("--env-host=false"),
            os("--read-only"),
            os("--read-only-tmpfs=false"),
            os("--image-volume=ignore"),
            // The image init requires EXACTLY this capability set with
            // no_new_privs clear at entry; it retains them only in the
            // PID-separated Buildah broker and starts the tenant with all
            // capabilities dropped and no_new_privs latched. An outer
            // `--security-opt=no-new-privileges` or a bare `--cap-drop=all`
            // makes the init exit 125 before any tenant byte runs (see the
            // module doc, "Invocation/image capability contract").
            os("--cap-drop=all"),
            os(SANDBOX_ANNOTATION),
            os("--ipc=private"),
            os("--pid=private"),
            os("--uts=private"),
            os("--cgroupns=private"),
            os("--restart=no"),
            os("--stop-timeout=0"),
            os("--no-healthcheck"),
            os("--hostname=hive-build"),
            os("--user"),
            os(user),
            os("--workdir"),
            os(workdir),
            os("--memory"),
            os(limits.memory_bytes.to_string()),
            os("--memory-swap"),
            os(limits.memory_bytes.to_string()),
            os("--cpus"),
            os(cpus),
            os("--pids-limit"),
            os(limits.pids.to_string()),
            os("--ulimit"),
            os("core=0:0"),
            os("--ulimit"),
            os(format!("fsize={0}:{0}", limits.workspace_bytes)),
            os("--ulimit"),
            os("nofile=1024:1024"),
            os("--tmpfs"),
            os(format!(
                "/tmp:rw,nodev,nosuid,noexec,size={tmpfs_each},mode=1777"
            )),
            os("--tmpfs"),
            os(format!(
                "/run:rw,nodev,nosuid,noexec,size={tmpfs_each},mode=0755"
            )),
            os("--tmpfs"),
            os(format!(
                "/var/tmp:rw,nodev,nosuid,noexec,size={tmpfs_each},mode=1777"
            )),
            os("--sysctl"),
            os("net.ipv6.conf.all.disable_ipv6=1"),
            os("--sysctl"),
            os("net.ipv6.conf.default.disable_ipv6=1"),
            os("--network"),
            os(network
                .map(|value| value.network.as_str())
                .unwrap_or("none")),
        ];
        for capability in BUILDER_CAPABILITIES {
            args.push(os(format!("--cap-add={capability}")));
        }
        if let Some(path) = env_file {
            args.push(os("--env-file"));
            args.push(path.as_os_str().to_os_string());
        }
        for mount in mounts {
            args.push(os("--volume"));
            args.push(os(mount));
        }
        // Every container main process is supervised by the image init: it
        // re-verifies the entry capability contract, starts the broker, and
        // execs the argv below as the fully-de-capped, NNP-latched tenant.
        // The init path is pinned here AND verified against the image config
        // at probe time, so neither side can drift silently.
        if env_file.is_some() || image_env {
            // Step containers need the validated --env-file values long enough for
            // clean_env_wrapper to copy only the requested keys into its own
            // `env -i` process. An outer `env -i` would erase them first. The
            // probe writer keeps the image environment for the same reason:
            // the in-image selfcheck consumes the broker variables.
            args.extend([
                os(BUILDER_INIT_ENTRYPOINT_FLAG),
                os(&config.builder_image),
                os("/bin/sh"),
                os("-c"),
                os(script),
                os("hive-build"),
            ]);
        } else {
            args.extend([
                os(BUILDER_INIT_ENTRYPOINT_FLAG),
                os(&config.builder_image),
                os("/usr/bin/env"),
                os("-i"),
                os(format!("PATH={BASE_PATH}")),
                os("HOME=/workspace/home"),
                os("CI=1"),
                os("NEXT_TELEMETRY_DISABLED=1"),
                os("npm_config_engine_strict=false"),
                os("npm_config_fund=false"),
                os("npm_config_audit=false"),
                os("/bin/sh"),
                os("-c"),
                os(script),
                os("hive-build"),
            ]);
        }
        args.extend(script_args.iter().map(os));
        args
    }

    async fn exec_fixed(
        &self,
        container: &str,
        workdir: &str,
        script: &str,
        script_args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput> {
        let config = &self.inner.config;
        let mut args = vec![
            os("exec"),
            os("--user"),
            os(format!("{}:{}", config.user.uid, config.user.gid)),
            os("--workdir"),
            os(workdir),
            os(container),
            os("/usr/bin/env"),
            os("-i"),
            os(format!("PATH={BASE_PATH}")),
            os("HOME=/workspace/home"),
            os("CI=1"),
            os("/bin/sh"),
            os("-c"),
            os(script),
            os("hive-build"),
        ];
        args.extend(script_args.iter().map(os));
        self.podman_output(args, timeout).await
    }

    async fn verify_volume_capacity(
        &self,
        container: &str,
        mount: &str,
        ceiling: u64,
    ) -> Result<()> {
        let output = self
            .exec_fixed(
                container,
                mount,
                "set -eu; set -- $(/usr/bin/stat -f -c '%S %b' \"$1\"); echo $(($1 * $2))",
                &[mount.to_string()],
                Duration::from_secs(10),
            )
            .await?;
        require_success("verify volume quota", &output)?;
        let capacity = one_line(&output.stdout).parse::<u64>().map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "verify volume quota",
                format!("invalid capacity: {error}"),
            )
        })?;
        self.require_capacity(capacity, ceiling, "verify volume quota")
    }

    fn require_capacity(&self, actual: u64, ceiling: u64, operation: &'static str) -> Result<()> {
        let maximum = ceiling
            .checked_add(self.inner.config.volume.capacity_slack_bytes)
            .ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidConfig,
                    operation,
                    "capacity ceiling overflow",
                )
            })?;
        if actual == 0 || actual > maximum {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                operation,
                format!(
                    "volume reports {actual} bytes for a {ceiling}-byte quota (maximum with declared rounding slack {maximum}); refusing an unenforced workspace"
                ),
            ));
        }
        Ok(())
    }

    async fn verify_container_mounts(&self, container: &str, required: &[&str]) -> Result<()> {
        let output = self
            .podman_output([os("inspect"), os(container)], Duration::from_secs(15))
            .await?;
        require_success("inspect build container", &output)?;
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "inspect build container",
                format!("invalid inspect JSON: {error}"),
            )
        })?;
        let object = first_object(&value).ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "inspect build container",
                "inspect returned no object",
            )
        })?;
        let mounts = object
            .get("Mounts")
            .or_else(|| object.get("mounts"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut seen = BTreeSet::new();
        for mount in mounts {
            let Some(mount) = mount.as_object() else {
                continue;
            };
            let kind = json_str(mount, &["Type", "type"]).unwrap_or_default();
            let destination =
                json_str(mount, &["Destination", "destination", "Dest"]).unwrap_or_default();
            let source = json_str(mount, &["Source", "source"]).unwrap_or_default();
            if required.contains(&destination) {
                if kind != "volume" {
                    return Err(BuildExecutorError::new(
                        BuildExecutorErrorCode::CapabilityMismatch,
                        "inspect build container",
                        format!("required mount {destination} is {kind:?}, not a named volume"),
                    ));
                }
                seen.insert(destination.to_string());
                continue;
            }
            let generated_etc = kind == "bind"
                && matches!(
                    destination,
                    "/etc/hosts" | "/etc/hostname" | "/etc/resolv.conf"
                );
            let forbidden_destination = destination == "/home"
                || destination.starts_with("/home/")
                || destination == "/root"
                || destination.starts_with("/root/")
                || destination.starts_with("/run/secrets")
                || destination.ends_with(".sock");
            let forbidden_source = source.ends_with(".sock")
                || source.contains("/run/secrets")
                || source.starts_with("/home/")
                || source.starts_with("/root/");
            if forbidden_destination || forbidden_source || (kind == "bind" && !generated_etc) {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityMismatch,
                    "inspect build container",
                    format!(
                        "unexpected host mount type={kind:?} source={source:?} destination={destination:?}"
                    ),
                ));
            }
        }
        for destination in required {
            if !seen.contains(*destination) {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityMismatch,
                    "inspect build container",
                    format!("required named-volume mount {destination} is missing"),
                ));
            }
        }
        Ok(())
    }

    async fn podman_output<I>(&self, args: I, timeout: Duration) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut command = Command::new(&self.inner.config.podman_path);
        command
            .args(args)
            .env_clear()
            .envs(&self.inner.config.podman_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "podman command",
                error.to_string(),
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "podman command",
                "missing stdout pipe",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "podman command",
                "missing stderr pipe",
            )
        })?;
        let stdout_task = tokio::spawn(read_admin_stream(stdout));
        let stderr_task = tokio::spawn(read_admin_stream(stderr));
        let status = tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| {
                let _ = child.start_kill();
                BuildExecutorError::new(
                    BuildExecutorErrorCode::PodmanFailed,
                    "podman command",
                    format!("podman client exceeded {} ms", duration_ms(timeout)),
                )
            })?
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::PodmanFailed,
                    "podman command",
                    error.to_string(),
                )
            })?;
        let (stdout, stdout_total) = stdout_task.await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "podman command",
                format!("stdout drain failed: {error}"),
            )
        })??;
        let (stderr, stderr_total) = stderr_task.await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "podman command",
                format!("stderr drain failed: {error}"),
            )
        })??;
        if stdout_total.saturating_add(stderr_total) > ADMIN_OUTPUT_CAP as u64 {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "podman command",
                format!("administrative output exceeded {ADMIN_OUTPUT_CAP} bytes"),
            ));
        }
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
        })
    }

    async fn hash_volume(&self, volume: &str, entries: u64, file_bytes: u64) -> Result<String> {
        let id = Uuid::new_v4().simple().to_string();
        let container = format!("hive-build-hash-{id}");
        let mut cleanup = CleanupGuard::new(self);
        let script = "exec /bin/tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
            --format=posix --pax-option=delete=atime,delete=ctime -C /sealed/artifact -cf - .";
        let args = self.container_create_args(
            &container,
            &[format!("{volume}:/sealed:ro,nodev,nosuid")],
            None,
            None,
            "/sealed/artifact",
            script,
            &[],
            false,
        );
        let created = self.podman_output(args, Duration::from_secs(30)).await?;
        require_success("create output verifier", &created)?;
        cleanup.add_container(container.clone());
        self.verify_container_mounts(&container, &["/sealed"])
            .await?;

        let overhead = entries
            .checked_mul(4096)
            .and_then(|value| value.checked_add(1024 * 1024))
            .ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::OutputLimitExceeded,
                    "hash sealed output",
                    "tar stream bound overflow",
                )
            })?;
        let cap = file_bytes.checked_add(overhead).ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::OutputLimitExceeded,
                "hash sealed output",
                "tar stream bound overflow",
            )
        })?;

        let mut command = self.podman_command([os("start"), os("--attach"), os(&container)]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "hash sealed output",
                error.to_string(),
            )
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "hash sealed output",
                "missing stdout pipe",
            )
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "hash sealed output",
                "missing stderr pipe",
            )
        })?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut stderr_bytes = Vec::new();
        let deadline = Instant::now() + self.inner.config.limits.max_step_time;
        let mut out_done = false;
        let mut err_done = false;
        let mut status: Option<ExitStatus> = None;
        let mut buffer = [0u8; 64 * 1024];
        let mut err_buffer = [0u8; 4096];
        while status.is_none() || !out_done || !err_done {
            tokio::select! {
                read = stdout.read(&mut buffer), if !out_done => {
                    let read = read.map_err(|error| BuildExecutorError::new(
                        BuildExecutorErrorCode::PodmanFailed,
                        "hash sealed output",
                        error.to_string(),
                    ))?;
                    if read == 0 {
                        out_done = true;
                    } else {
                        total = total.checked_add(read as u64).ok_or_else(|| BuildExecutorError::new(
                            BuildExecutorErrorCode::OutputLimitExceeded,
                            "hash sealed output",
                            "tar stream byte counter overflow",
                        ))?;
                        if total > cap {
                            let _ = child.start_kill();
                            return Err(BuildExecutorError::new(
                                BuildExecutorErrorCode::OutputLimitExceeded,
                                "hash sealed output",
                                format!("canonical output stream exceeded its {cap}-byte bound"),
                            ));
                        }
                        hasher.update(&buffer[..read]);
                    }
                }
                read = stderr.read(&mut err_buffer), if !err_done => {
                    let read = read.map_err(|error| BuildExecutorError::new(
                        BuildExecutorErrorCode::PodmanFailed,
                        "hash sealed output",
                        error.to_string(),
                    ))?;
                    if read == 0 {
                        err_done = true;
                    } else if stderr_bytes.len() < 64 * 1024 {
                        let keep = (64 * 1024 - stderr_bytes.len()).min(read);
                        stderr_bytes.extend_from_slice(&err_buffer[..keep]);
                    }
                }
                waited = child.wait(), if status.is_none() => {
                    status = Some(waited.map_err(|error| BuildExecutorError::new(
                        BuildExecutorErrorCode::PodmanFailed,
                        "hash sealed output",
                        error.to_string(),
                    ))?);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.start_kill();
                    return Err(BuildExecutorError::new(
                        BuildExecutorErrorCode::StepTimedOut,
                        "hash sealed output",
                        "output verifier timed out",
                    ));
                }
            }
        }
        let status = status.expect("loop requires process status");
        if !status.success() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "hash sealed output",
                format!(
                    "verifier exited {status}: {}",
                    bounded_text(&stderr_bytes, 2048)
                ),
            ));
        }
        cleanup.cleanup(self).await?;
        Ok(hex_bytes(&hasher.finalize()))
    }

    fn podman_command<I>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut command = Command::new(&self.inner.config.podman_path);
        command
            .args(args)
            .env_clear()
            .envs(&self.inner.config.podman_env);
        command
    }
}

impl BuildSession {
    pub fn policy_digest(&self) -> &str {
        &self.executor.inner.capability.policy_digest
    }

    pub async fn run<F>(&mut self, step: BuildStep, mut log: F) -> Result<BuildStepResult>
    where
        F: FnMut(BuildLogLine),
    {
        validate_step(&self.executor.inner.config, &step)?;
        let now = Instant::now();
        if now >= self.deadline {
            let _ = self.cleanup.cleanup(&self.executor).await;
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::BuildTimedOut,
                "run build step",
                "build-wide deadline expired",
            ));
        }
        let requested = step
            .timeout
            .unwrap_or(self.executor.inner.config.limits.max_step_time)
            .min(self.executor.inner.config.limits.max_step_time);
        let step_deadline = (now + requested).min(self.deadline);
        let start = Instant::now();
        let config = &self.executor.inner.config;
        let env_file = StepEnvFile::create(&step.env)?;
        let wrapper = clean_env_wrapper(step.env.keys());
        let step_container = format!("hive-build-step-{}", Uuid::new_v4().simple());
        let mut script_args = Vec::with_capacity(step.args.len() + 1);
        script_args.push(step.script.clone());
        script_args.extend(step.args.iter().cloned());
        let args = self.executor.container_create_args(
            &step_container,
            &[format!(
                "{}:{WORKSPACE_MOUNT}:rw,nodev,nosuid",
                self.workspace_volume
            )],
            Some(&env_file.path),
            self.executor.inner.config.network_policy.as_ref(),
            &step.cwd.container_path(),
            &wrapper,
            &script_args,
            false,
        );
        let created = self
            .executor
            .podman_output(args, Duration::from_secs(30))
            .await?;
        require_success("create build step sandbox", &created)?;
        self.cleanup.add_container(step_container.clone());
        self.executor
            .verify_container_mounts(&step_container, &[WORKSPACE_MOUNT])
            .await?;

        let mut command =
            self.executor
                .podman_command([os("start"), os("--attach"), os(&step_container)]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "run build step",
                error.to_string(),
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "run build step",
                "missing stdout pipe",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "run build step",
                "missing stderr pipe",
            )
        })?;
        let (tx, mut rx) = mpsc::channel(16);
        let out_task = tokio::spawn(read_log_stream(
            stdout,
            BuildLogStream::Stdout,
            config.limits.max_log_line_bytes,
            tx.clone(),
        ));
        let err_task = tokio::spawn(read_log_stream(
            stderr,
            BuildLogStream::Stderr,
            config.limits.max_log_line_bytes,
            tx,
        ));
        let redactions: Vec<String> = step
            .env
            .values()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();
        let mut streams = 2usize;
        let mut status: Option<ExitStatus> = None;
        let before = self.emitted_log_bytes;
        while status.is_none() || streams > 0 {
            tokio::select! {
                event = rx.recv(), if streams > 0 => {
                    match event {
                        Some(LogEvent::Consumed(consumed)) => {
                            self.emitted_log_bytes = self.emitted_log_bytes.saturating_add(consumed);
                            if self.emitted_log_bytes > config.limits.log_bytes {
                                let _ = child.start_kill();
                                out_task.abort();
                                err_task.abort();
                                let _ = self.cleanup.cleanup(&self.executor).await;
                                return Err(BuildExecutorError::new(
                                    BuildExecutorErrorCode::LogLimitExceeded,
                                    "run build step",
                                    format!(
                                        "build emitted more than {} log bytes ({} bytes in the last chunk)",
                                        config.limits.log_bytes, consumed
                                    ),
                                ));
                            }
                        }
                        Some(LogEvent::Line { stream, bytes, dropped }) => {
                            let text = redact_values(&String::from_utf8_lossy(&bytes), &redactions);
                            log(BuildLogLine {
                                step: step.label.clone(),
                                stream,
                                text,
                                dropped_bytes: dropped,
                            });
                        }
                        Some(LogEvent::Done) => streams = streams.saturating_sub(1),
                        Some(LogEvent::ReadFailed(error)) => {
                            let _ = child.start_kill();
                            out_task.abort();
                            err_task.abort();
                            let _ = self.cleanup.cleanup(&self.executor).await;
                            return Err(BuildExecutorError::new(
                                BuildExecutorErrorCode::PodmanFailed,
                                "read build log",
                                error,
                            ));
                        }
                        None => streams = 0,
                    }
                }
                waited = child.wait(), if status.is_none() => {
                    status = Some(waited.map_err(|error| BuildExecutorError::new(
                        BuildExecutorErrorCode::PodmanFailed,
                        "run build step",
                        error.to_string(),
                    ))?);
                }
                _ = tokio::time::sleep_until(step_deadline) => {
                    let _ = child.start_kill();
                    out_task.abort();
                    err_task.abort();
                    let _ = self.cleanup.cleanup(&self.executor).await;
                    let code = if step_deadline == self.deadline {
                        BuildExecutorErrorCode::BuildTimedOut
                    } else {
                        BuildExecutorErrorCode::StepTimedOut
                    };
                    return Err(BuildExecutorError::new(
                        code,
                        "run build step",
                        format!("step {:?} exceeded its deadline", step.label),
                    ));
                }
            }
        }
        let _ = out_task.await;
        let _ = err_task.await;
        let status = status.expect("loop requires process status");
        self.cleanup
            .remove_container(&self.executor, &step_container)
            .await?;
        if !status.success() && !step.accept_nonzero {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::StepFailed,
                "run build step",
                format!("step {:?} exited {status}", step.label),
            ));
        }
        self.executor
            .verify_volume_capacity(
                &self.container,
                WORKSPACE_MOUNT,
                config.limits.workspace_bytes,
            )
            .await?;
        Ok(BuildStepResult {
            label: step.label,
            exit_code: status.code().unwrap_or(255),
            elapsed: start.elapsed(),
            emitted_log_bytes: self.emitted_log_bytes.saturating_sub(before),
        })
    }

    pub async fn import_cache_archive(&mut self, archive: &Path, cwd: WorkspacePath) -> Result<()> {
        let metadata = tokio::fs::symlink_metadata(archive)
            .await
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "import build cache",
                    format!("cannot inspect cache archive: {error}"),
                )
            })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "import build cache",
                "cache archive must be a regular non-symlink file",
            ));
        }
        if metadata.len() > self.executor.inner.config.limits.output_bytes {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::OutputLimitExceeded,
                "import build cache",
                "cache archive exceeds the executor output-byte ceiling",
            ));
        }
        let id = Uuid::new_v4().simple().to_string();
        let archive_in_container = format!("/workspace/cache-import-{id}.tar");
        let copied = self
            .executor
            .podman_output(
                [
                    os("cp"),
                    os("--archive=false"),
                    archive.as_os_str().to_os_string(),
                    os(format!("{}:{archive_in_container}", self.container)),
                ],
                self.executor.inner.config.limits.max_step_time,
            )
            .await?;
        require_success("copy verified cache into sandbox", &copied)?;
        let unpack = format!("/workspace/cache-unpack-{id}");
        let script = r#"set -eu
archive=$1
unpack=$2
trap 'rm -rf -- "$archive" "$unpack"' EXIT HUP INT TERM
mkdir -m 0700 -- "$unpack"
tar --no-same-owner --no-same-permissions --numeric-owner -xf "$archive" -C "$unpack"
test -z "$(find -P "$unpack" -xdev ! -type d ! -type f ! -type l -print -quit)"
for entry in "$unpack"/* "$unpack"/.[!.]* "$unpack"/..?*; do
  [ -e "$entry" ] || [ -L "$entry" ] || continue
  case ${entry##*/} in node_modules|.next) ;; *) exit 51 ;; esac
done
if [ -e "$unpack/.next" ]; then
  [ -d "$unpack/.next" ] && [ ! -L "$unpack/.next" ] || exit 52
  for entry in "$unpack/.next"/* "$unpack/.next"/.[!.]* "$unpack/.next"/..?*; do
    [ -e "$entry" ] || [ -L "$entry" ] || continue
    [ "${entry##*/}" = cache ] || exit 53
  done
fi
find -P "$unpack" -xdev -type l -exec sh -eu -c 'base=$1; shift; for p do resolved=$(realpath -m -- "$p"); case "$resolved" in "$base"|"$base"/*) ;; *) exit 54 ;; esac; done' hive-cache-links "$unpack" {} +
rm -rf -- node_modules .next/cache
if [ -e "$unpack/node_modules" ]; then mv -- "$unpack/node_modules" node_modules; fi
if [ -e "$unpack/.next/cache" ]; then mkdir -p -- .next; mv -- "$unpack/.next/cache" .next/cache; fi
"#;
        let mut step = BuildStep::shell("restore verified build cache", script);
        step.cwd = cwd;
        step.args = vec![archive_in_container, unpack];
        self.run(step, |_| {}).await.map(|_| ())
    }

    pub async fn export_cache_archive(
        &mut self,
        cwd: WorkspacePath,
    ) -> Result<Option<SealedCacheArtifact>> {
        if Instant::now() >= self.deadline {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::BuildTimedOut,
                "export build cache",
                "build-wide deadline expired",
            ));
        }
        let id = Uuid::new_v4().simple().to_string();
        let archive_in_container = format!("/workspace/cache-export-{id}.tar");
        let script = r#"set -eu
archive=$1
[ -d node_modules ] || exit 0
set -- node_modules
if [ -d .next/cache ]; then set -- "$@" .next/cache; fi
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=posix --pax-option=delete=atime,delete=ctime -cf "$archive" "$@"
chmod 0400 "$archive"
"#;
        let mut step = BuildStep::shell("seal build cache", script);
        step.cwd = cwd;
        step.args = vec![archive_in_container.clone()];
        self.run(step, |_| {}).await?;
        let measured = self
            .executor
            .exec_fixed(
                &self.container,
                WORKSPACE_MOUNT,
                "set -eu; [ -f \"$1\" ] || exit 44; bytes=$(stat -c %s -- \"$1\"); digest=$(sha256sum -- \"$1\" | cut -d' ' -f1); printf '%s %s\\n' \"$bytes\" \"$digest\"",
                std::slice::from_ref(&archive_in_container),
                Duration::from_secs(30),
            )
            .await?;
        if measured.status.code() == Some(44) {
            return Ok(None);
        }
        require_success("measure sealed build cache", &measured)?;
        let text = one_line(&measured.stdout);
        let mut fields = text.split_whitespace();
        let bytes = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::UnsafeOutput,
                    "measure sealed build cache",
                    "cache size response is malformed",
                )
            })?;
        let digest = fields
            .next()
            .filter(|value| is_lower_sha256(value))
            .ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::UnsafeOutput,
                    "measure sealed build cache",
                    "cache digest response is malformed",
                )
            })?;
        if fields.next().is_some() || bytes > self.executor.inner.config.limits.output_bytes {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::OutputLimitExceeded,
                "measure sealed build cache",
                "cache artifact exceeds its bounded output contract",
            ));
        }
        let path = std::env::temp_dir().join(format!(".hive-cache-sealed-{id}.tar"));
        let copied = self
            .executor
            .podman_output(
                [
                    os("cp"),
                    os("--archive=false"),
                    os(format!("{}:{archive_in_container}", self.container)),
                    path.as_os_str().to_os_string(),
                ],
                self.executor.inner.config.limits.max_step_time,
            )
            .await?;
        require_success("materialize sealed build cache", &copied)?;
        let host_metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::SealIntegrityMismatch,
                "verify sealed build cache",
                error.to_string(),
            )
        })?;
        let host_digest = sha256_file(&path).await?;
        if !host_metadata.is_file()
            || host_metadata.file_type().is_symlink()
            || host_metadata.len() != bytes
            || hex_bytes(&host_digest) != digest
        {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::SealIntegrityMismatch,
                "verify sealed build cache",
                "materialized cache differs from the sandbox-sealed artifact",
            ));
        }
        let _ = self
            .executor
            .exec_fixed(
                &self.container,
                WORKSPACE_MOUNT,
                "rm -f -- \"$1\"",
                std::slice::from_ref(&archive_in_container),
                Duration::from_secs(20),
            )
            .await;
        Ok(Some(SealedCacheArtifact {
            path: Some(path),
            content_sha256: digest.to_string(),
            bytes,
        }))
    }

    pub async fn seal(self, output: WorkspacePath) -> Result<SealedBuildOutput> {
        let mut session = self;
        if Instant::now() >= session.deadline {
            session.cleanup.cleanup(&session.executor).await?;
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::BuildTimedOut,
                "seal build output",
                "build-wide deadline expired before sealing",
            ));
        }
        let limits = &session.executor.inner.config.limits;
        let audit_script = "set -eu; \
            root=\"/workspace/source/$1\"; root=$(/usr/bin/realpath -e -- \"$root\"); \
            case \"$root\" in /workspace/source|/workspace/source/*) ;; *) exit 41;; esac; \
            test -d \"$root\"; \
            test -z \"$(/usr/bin/find -P \"$root\" -xdev ! -type d ! -type f ! -type l -print -quit)\"; \
            /usr/bin/find -P \"$root\" -xdev -type l -exec /bin/sh -eu -c '\''base=$1; shift; for p do resolved=$(/usr/bin/realpath -m -- \"$p\") || exit 42; case \"$resolved\" in \"$base\"|\"$base\"/*) ;; *) exit 43;; esac; done'\'' hive-link-check \"$root\" {} +; \
            entries=$(/usr/bin/find -P \"$root\" -xdev -printf x | /usr/bin/wc -c); \
            bytes=$(/usr/bin/find -P \"$root\" -xdev -type f -printf '%s\\n' | /usr/bin/awk '\''{n += $1} END {print n + 0}'\''); \
            printf '%s %s\\n' \"$entries\" \"$bytes\"";
        let audited = session
            .executor
            .exec_fixed(
                &session.container,
                SOURCE_ROOT,
                audit_script,
                &[output.as_str().to_string()],
                limits.max_step_time,
            )
            .await?;
        if !audited.status.success() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::UnsafeOutput,
                "seal build output",
                format!(
                    "output audit failed without publishing an artifact: {}",
                    bounded_text(&audited.stderr, 2048)
                ),
            ));
        }
        let (entries, file_bytes) = parse_two_u64(&audited.stdout, "seal output audit")?;
        if entries > limits.output_entries {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::OutputEntryLimitExceeded,
                "seal build output",
                format!(
                    "output has {entries} entries; limit is {}",
                    limits.output_entries
                ),
            ));
        }
        if file_bytes > limits.output_bytes {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::OutputLimitExceeded,
                "seal build output",
                format!(
                    "output has {file_bytes} bytes; limit is {}",
                    limits.output_bytes
                ),
            ));
        }

        let id = Uuid::new_v4().simple().to_string();
        let output_volume = format!("hive-build-out-{id}");
        let keeper = format!("hive-build-keep-{id}");
        let sealer = format!("hive-build-seal-{id}");
        let mut output_cleanup = CleanupGuard::new(&session.executor);
        session
            .executor
            .create_volume(&output_volume, limits.output_bytes)
            .await?;
        output_cleanup.add_volume(output_volume.clone());
        let keep_args = session.executor.container_create_args(
            &keeper,
            &[format!("{output_volume}:/sealed:rw,U,nodev,nosuid")],
            None,
            None,
            "/sealed",
            "trap 'exit 0' TERM INT; while :; do /bin/sleep 3600; done",
            &[],
            false,
        );
        let created = session
            .executor
            .podman_output(keep_args, Duration::from_secs(30))
            .await?;
        require_success("create sealed-output keeper", &created)?;
        output_cleanup.add_container(keeper.clone());
        let started = session
            .executor
            .podman_output([os("start"), os(&keeper)], Duration::from_secs(30))
            .await?;
        require_success("start sealed-output keeper", &started)?;
        let copy_script = "set -eu; \
            src=\"/input/source/$1\"; src=$(/usr/bin/realpath -e -- \"$src\"); \
            case \"$src\" in /input/source|/input/source/*) ;; *) exit 51;; esac; \
            mkdir -p /sealed/artifact; \
            /bin/tar --one-file-system -C \"$src\" -cf - . | \
                /bin/tar --no-same-owner --no-same-permissions -C /sealed/artifact -xf -; \
            chmod -R a-w /sealed/artifact; sync";
        let args = session.executor.container_create_args(
            &sealer,
            &[
                format!("{}:/input:ro,nodev,nosuid", session.workspace_volume),
                format!("{output_volume}:/sealed:rw,U,nodev,nosuid"),
            ],
            None,
            None,
            "/sealed",
            copy_script,
            &[output.as_str().to_string()],
            false,
        );
        let created = session
            .executor
            .podman_output(args, Duration::from_secs(30))
            .await?;
        require_success("create output sealer", &created)?;
        output_cleanup.add_container(sealer.clone());
        session
            .executor
            .verify_container_mounts(&sealer, &["/input", "/sealed"])
            .await?;
        let copied = session
            .executor
            .podman_output(
                [os("start"), os("--attach"), os(&sealer)],
                limits.max_step_time,
            )
            .await?;
        require_success("copy sealed output", &copied)?;
        output_cleanup
            .remove_container(&session.executor, &sealer)
            .await?;

        let digest = session
            .executor
            .hash_volume(&output_volume, entries, file_bytes)
            .await?;
        session.cleanup.cleanup(&session.executor).await?;
        let descriptor = SealedOutputDescriptor {
            protocol: BUILD_EXECUTOR_PROTOCOL,
            id: format!("bldout-{id}"),
            policy_digest: session.executor.inner.capability.policy_digest.clone(),
            content_sha256: digest,
            entries,
            file_bytes,
            root: format!("{SEALED_OUTPUT_MOUNT}{OUTPUT_ROOT}"),
        };
        Ok(SealedBuildOutput {
            executor: session.executor.clone(),
            descriptor,
            volume: output_volume,
            keeper,
            _lifecycle_lock: session._lifecycle_lock.take(),
            cleanup: output_cleanup,
        })
    }
}

impl SealedCacheArtifact {
    /// Atomically publish the executor-sealed cache at a platform-owned path.
    /// The sibling temporary is create-new and mode 0600; a cache path supplied
    /// by repository data is never accepted by this API.
    pub async fn materialize_replace(mut self, destination: &Path) -> Result<()> {
        if !destination.is_absolute() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize sealed build cache",
                "destination must be an absolute platform-owned path",
            ));
        }
        let parent = destination.parent().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize sealed build cache",
                "destination has no parent directory",
            )
        })?;
        let parent_metadata = tokio::fs::symlink_metadata(parent).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize sealed build cache",
                format!("cannot inspect destination parent: {error}"),
            )
        })?;
        if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize sealed build cache",
                "destination parent must be a real directory",
            ));
        }
        if let Ok(metadata) = tokio::fs::symlink_metadata(destination).await {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "materialize sealed build cache",
                    "existing destination must be a regular non-symlink file",
                ));
            }
        }
        let source = self.path.as_ref().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize sealed build cache",
                "sealed cache was already consumed",
            )
        })?;
        let name = destination
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "materialize sealed build cache",
                    "destination filename is invalid",
                )
            })?;
        let temporary = parent.join(format!(".{name}.{}.tmp", Uuid::new_v4().simple()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let destination_file = options.open(&temporary).map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "materialize sealed build cache",
                format!("cannot create sibling temporary: {error}"),
            )
        })?;
        let source_file = std::fs::File::open(source).map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            BuildExecutorError::new(
                BuildExecutorErrorCode::SealIntegrityMismatch,
                "materialize sealed build cache",
                format!("cannot reopen sealed cache: {error}"),
            )
        })?;
        let mut source_file = tokio::fs::File::from_std(source_file);
        let mut destination_file = tokio::fs::File::from_std(destination_file);
        let copied = match tokio::io::copy(&mut source_file, &mut destination_file).await {
            Ok(copied) => copied,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "materialize sealed build cache",
                    format!("cache copy failed: {error}"),
                ));
            }
        };
        if let Err(error) = destination_file.sync_all().await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "materialize sealed build cache",
                format!("cache sync failed: {error}"),
            ));
        }
        drop(destination_file);
        let measured = sha256_file(&temporary).await?;
        if copied != self.bytes || hex_bytes(&measured) != self.content_sha256 {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::SealIntegrityMismatch,
                "materialize sealed build cache",
                "sibling temporary differs from the executor-sealed cache",
            ));
        }
        tokio::fs::rename(&temporary, destination)
            .await
            .map_err(|error| {
                let _ = std::fs::remove_file(&temporary);
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "materialize sealed build cache",
                    format!("atomic cache publish failed: {error}"),
                )
            })?;
        if let Some(source) = self.path.take() {
            let _ = tokio::fs::remove_file(source).await;
        }
        Ok(())
    }
}

impl Drop for SealedCacheArtifact {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl SealedBuildOutput {
    pub fn descriptor(&self) -> &SealedOutputDescriptor {
        &self.descriptor
    }

    pub async fn verify(&self) -> Result<()> {
        let measured = self
            .executor
            .hash_volume(
                &self.volume,
                self.descriptor.entries,
                self.descriptor.file_bytes,
            )
            .await?;
        if measured != self.descriptor.content_sha256 {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::SealIntegrityMismatch,
                "verify sealed output",
                format!(
                    "sealed output digest changed: expected {}, measured {measured}",
                    self.descriptor.content_sha256
                ),
            ));
        }
        Ok(())
    }

    /// Prove that a checkout-relative output path is a real directory inside the
    /// immutable sealed volume. Every component is inspected without following a
    /// symlink, and the volume is attached read-only to the inspector.
    pub async fn require_directory(&self, path: &WorkspacePath) -> Result<()> {
        self.verify().await?;
        let id = Uuid::new_v4().simple().to_string();
        let inspector = format!("hive-build-inspect-{id}");
        let mut cleanup = CleanupGuard::new(&self.executor);
        let script = r#"set -efu
relative=$1
current=/sealed/artifact
if [ "$relative" != . ]; then
  old_ifs=$IFS
  IFS=/
  set -- $relative
  IFS=$old_ifs
  for component do
    current=$current/$component
    [ ! -L "$current" ] || exit 72
    [ -e "$current" ] || exit 71
  done
fi
[ ! -L "$current" ] || exit 72
[ -e "$current" ] || exit 71
[ -d "$current" ] && exit 0
[ -f "$current" ] && exit 73
exit 74
"#;
        let args = self.executor.container_create_args(
            &inspector,
            &[format!("{}:/sealed:ro,nodev,nosuid", self.volume)],
            None,
            None,
            "/sealed/artifact",
            script,
            &[path.as_str().to_string()],
            false,
        );
        let created = self
            .executor
            .podman_output(args, Duration::from_secs(30))
            .await?;
        require_success("create sealed-output inspector", &created)?;
        cleanup.add_container(inspector.clone());
        self.executor
            .verify_container_mounts(&inspector, &["/sealed"])
            .await?;
        let inspected = self
            .executor
            .podman_output(
                [os("start"), os("--attach"), os(&inspector)],
                Duration::from_secs(30),
            )
            .await?;
        cleanup.remove_container(&self.executor, &inspector).await?;
        let detail = match inspected.status.code() {
            Some(0) => return Ok(()),
            Some(71) => "is missing",
            Some(72) => "is or traverses a symbolic link",
            Some(73) => "is a regular file, not a directory",
            Some(74) => "is a special file, not a directory",
            _ => return require_success("inspect sealed output directory", &inspected),
        };
        Err(BuildExecutorError::new(
            BuildExecutorErrorCode::UnsafeOutput,
            "inspect sealed output directory",
            format!("sealed output path {:?} {detail}", path.as_str()),
        ))
    }

    /// Returns only an opaque read-only Podman mount after re-verifying content.
    /// No host checkout/output path is exposed to `git.rs` or artifact staging.
    pub(crate) async fn staging_mount(&self) -> Result<SealedOutputMount> {
        self.verify().await?;
        Ok(SealedOutputMount {
            argument: os(format!(
                "{}:{SEALED_OUTPUT_MOUNT}:ro,nodev,nosuid",
                self.volume
            )),
        })
    }

    /// Replace a disposable platform checkout with the verified bounded output.
    /// The destination must already be a real directory; tenant input never
    /// becomes a Podman path or mount argument.
    pub async fn materialize_replace(mut self, destination: &Path) -> Result<()> {
        self.verify().await?;
        let metadata = tokio::fs::symlink_metadata(destination)
            .await
            .map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "materialize build output",
                    format!("cannot inspect destination: {error}"),
                )
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || !destination.is_absolute() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize build output",
                "destination must be an existing absolute real directory",
            ));
        }
        let parent = destination.parent().ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize build output",
                "destination must have a parent directory",
            )
        })?;
        let parent_metadata = tokio::fs::symlink_metadata(parent).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize build output",
                format!("cannot inspect destination parent: {error}"),
            )
        })?;
        if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "materialize build output",
                "destination parent must be a real directory",
            ));
        }
        let name = destination
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "materialize build output",
                    "destination directory name is invalid",
                )
            })?;
        let staging = parent.join(format!(".{name}.{}.materializing", Uuid::new_v4().simple()));
        tokio::fs::create_dir(&staging).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "materialize build output",
                format!("cannot create sibling staging directory: {error}"),
            )
        })?;
        let source = format!("{}:/sealed/artifact/.", self.keeper);
        let copied = self
            .executor
            .podman_output(
                [
                    os("cp"),
                    os("--archive=false"),
                    os(source),
                    staging.as_os_str().to_os_string(),
                ],
                self.executor.inner.config.limits.max_step_time,
            )
            .await;
        let copied = match copied {
            Ok(output) => output,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(error);
            }
        };
        if let Err(error) = require_success("materialize build output", &copied) {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
        if let Err(error) = atomic_exchange_directories(&staging, destination).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
        let _ = tokio::fs::remove_dir_all(&staging).await;
        self.cleanup.cleanup(&self.executor).await
    }

    pub async fn destroy(mut self) -> Result<()> {
        self.cleanup.cleanup(&self.executor).await
    }
}

async fn atomic_exchange_directories(staging: &Path, destination: &Path) -> Result<()> {
    let staging = staging.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;

            let staging_c = CString::new(staging.as_os_str().as_bytes()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "staging path contains NUL",
                )
            })?;
            let destination_c = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "destination path contains NUL",
                )
            })?;
            let exchanged = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    staging_c.as_ptr(),
                    libc::AT_FDCWD,
                    destination_c.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            };
            if exchanged != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let parent = destination.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "destination has no parent",
                )
            })?;
            let backup = parent.join(format!(".hive-build-replaced-{}", Uuid::new_v4().simple()));
            std::fs::rename(&destination, &backup)?;
            if let Err(error) = std::fs::rename(&staging, &destination) {
                let _ = std::fs::rename(&backup, &destination);
                return Err(error);
            }
            std::fs::remove_dir_all(backup)?;
        }
        let parent = destination.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination has no parent",
            )
        })?;
        std::fs::File::open(parent)?.sync_all()
    })
    .await
    .map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "materialize build output",
            format!("atomic directory exchange task failed: {error}"),
        )
    })?
    .map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "materialize build output",
            format!("atomic directory exchange failed: {error}"),
        )
    })
}

#[derive(Default)]
struct CleanupState {
    containers: Vec<String>,
    volumes: Vec<String>,
}

struct CleanupGuard {
    podman: PathBuf,
    env: BTreeMap<String, String>,
    state: Option<CleanupState>,
}

impl CleanupGuard {
    fn new(executor: &BuildExecutor) -> Self {
        Self {
            podman: executor.inner.config.podman_path.clone(),
            env: executor.inner.config.podman_env.clone(),
            state: Some(CleanupState::default()),
        }
    }

    fn add_container(&mut self, name: String) {
        if let Some(state) = self.state.as_mut() {
            state.containers.push(name);
        }
    }

    fn add_volume(&mut self, name: String) {
        if let Some(state) = self.state.as_mut() {
            state.volumes.push(name);
        }
    }

    async fn remove_container(&mut self, executor: &BuildExecutor, name: &str) -> Result<()> {
        cleanup_container_async(executor, name).await?;
        if let Some(state) = self.state.as_mut() {
            state.containers.retain(|container| container != name);
        }
        Ok(())
    }

    async fn cleanup(&mut self, executor: &BuildExecutor) -> Result<()> {
        let Some(mut state) = self.state.take() else {
            return Ok(());
        };
        while let Some(container) = state.containers.pop() {
            if let Err(error) = cleanup_container_async(executor, &container).await {
                state.containers.push(container);
                self.state = Some(state);
                return Err(error);
            }
        }
        while let Some(volume) = state.volumes.pop() {
            let output = match executor
                .podman_output(
                    hive_backend::container_cli::volume_rm_args(false, &volume)
                        .into_iter()
                        .map(OsString::from),
                    CLEANUP_TIMEOUT,
                )
                .await
            {
                Ok(output) => output,
                Err(error) => {
                    state.volumes.push(volume);
                    self.state = Some(state);
                    return Err(error);
                }
            };
            if !output.status.success() {
                state.volumes.push(volume);
                self.state = Some(state);
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::CleanupFailed,
                    "remove build volume",
                    format!(
                        "could not remove opaque volume: {}",
                        bounded_text(&output.stderr, 1024)
                    ),
                ));
            }
        }
        Ok(())
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        if state.containers.is_empty() && state.volumes.is_empty() {
            return;
        }
        let podman = self.podman.clone();
        let env = self.env.clone();
        let _ = std::thread::Builder::new()
            .name("hive-build-cleanup".to_string())
            .spawn(move || cleanup_sync(&podman, &env, state));
    }
}

async fn cleanup_container_async(executor: &BuildExecutor, name: &str) -> Result<()> {
    let _ = executor
        .podman_output([os("kill"), os("--signal=KILL"), os(name)], CLEANUP_TIMEOUT)
        .await;
    let output = executor
        .podman_output(
            hive_backend::container_cli::rm_args(false, name)
                .into_iter()
                .map(OsString::from),
            CLEANUP_TIMEOUT,
        )
        .await?;
    if !output.status.success() {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CleanupFailed,
            "remove build container",
            format!(
                "could not remove sandbox: {}",
                bounded_text(&output.stderr, 1024)
            ),
        ));
    }
    Ok(())
}

fn cleanup_sync(podman: &Path, env: &BTreeMap<String, String>, mut state: CleanupState) {
    for container in state.containers.drain(..).rev() {
        run_cleanup_command(
            podman,
            env,
            [os("kill"), os("--signal=KILL"), os(&container)],
        );
        run_cleanup_command(
            podman,
            env,
            hive_backend::container_cli::rm_args(false, &container)
                .into_iter()
                .map(OsString::from),
        );
    }
    for volume in state.volumes.drain(..).rev() {
        run_cleanup_command(
            podman,
            env,
            hive_backend::container_cli::volume_rm_args(false, &volume)
                .into_iter()
                .map(OsString::from),
        );
    }
}

fn run_cleanup_command<I>(podman: &Path, env: &BTreeMap<String, String>, args: I)
where
    I: IntoIterator<Item = OsString>,
{
    let child = std::process::Command::new(podman)
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return;
    };
    let deadline = std::time::Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

struct StepEnvFile {
    path: PathBuf,
}

impl StepEnvFile {
    fn create(env: &BTreeMap<String, String>) -> Result<Self> {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!(".hive-build-env-{}", Uuid::new_v4().simple()));
        let guard = Self { path };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&guard.path).map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "create build environment handoff",
                error.to_string(),
            )
        })?;
        for (key, value) in env {
            if value.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0')) {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "create build environment handoff",
                    format!("environment value for {key:?} contains a forbidden delimiter"),
                ));
            }
            writeln!(file, "{key}={value}").map_err(|error| {
                BuildExecutorError::new(
                    BuildExecutorErrorCode::CapabilityUnavailable,
                    "write build environment handoff",
                    error.to_string(),
                )
            })?;
        }
        file.sync_all().map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "sync build environment handoff",
                error.to_string(),
            )
        })?;
        Ok(guard)
    }
}

impl Drop for StepEnvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn clean_env_wrapper<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let mut wrapper = String::from(
        "set -eu; script=$1; shift; cwd=$(/bin/pwd -P); case \"$cwd\" in /workspace/source|/workspace/source/*) ;; *) exit 125;; esac; path=\"$cwd/node_modules/.bin:",
    );
    wrapper.push_str(BASE_PATH);
    wrapper.push_str("\"; exec /usr/bin/env -i PATH=\"$path\" HOME=/workspace/home CI=1 NEXT_TELEMETRY_DISABLED=1 npm_config_engine_strict=false npm_config_fund=false npm_config_audit=false");
    for key in keys {
        wrapper.push_str(" '");
        wrapper.push_str(key);
        wrapper.push_str("='\"$");
        wrapper.push_str(key);
        wrapper.push_str("\"");
    }
    wrapper.push_str(" /bin/sh -c \"$script\" hive-build-step \"$@\"");
    wrapper
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn read_admin_stream<R>(mut reader: R) -> Result<(Vec<u8>, u64)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(64 * 1024);
    let mut total = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::PodmanFailed,
                "read podman output",
                error.to_string(),
            )
        })?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        let room = ADMIN_OUTPUT_CAP.saturating_sub(retained.len());
        let keep = room.min(read);
        retained.extend_from_slice(&buffer[..keep]);
    }
    Ok((retained, total))
}

enum LogEvent {
    Consumed(u64),
    Line {
        stream: BuildLogStream,
        bytes: Vec<u8>,
        dropped: u64,
    },
    Done,
    ReadFailed(String),
}

async fn read_log_stream<R>(
    reader: R,
    stream: BuildLogStream,
    cap: usize,
    sender: mpsc::Sender<LogEvent>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    loop {
        match read_capped_line(&mut reader, cap, &sender).await {
            Ok(Some((bytes, dropped))) => {
                if sender
                    .send(LogEvent::Line {
                        stream,
                        bytes,
                        dropped,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(LogEvent::Done).await;
                return;
            }
            Err(error) => {
                let _ = sender.send(LogEvent::ReadFailed(error.to_string())).await;
                return;
            }
        }
    }
}

async fn read_capped_line<R>(
    reader: &mut R,
    cap: usize,
    sender: &mpsc::Sender<LogEvent>,
) -> std::io::Result<Option<(Vec<u8>, u64)>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut retained = Vec::with_capacity(cap.min(16 * 1024));
    let mut consumed = 0u64;
    let mut dropped = 0u64;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if consumed == 0 {
                Ok(None)
            } else {
                Ok(Some((retained, dropped)))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        let chunk = &available[..take];
        let without_newline = if chunk.last() == Some(&b'\n') {
            &chunk[..chunk.len() - 1]
        } else {
            chunk
        };
        let room = cap.saturating_sub(retained.len());
        let keep = room.min(without_newline.len());
        retained.extend_from_slice(&without_newline[..keep]);
        dropped = dropped.saturating_add((without_newline.len() - keep) as u64);
        consumed = consumed.saturating_add(take as u64);
        let ended = chunk.last() == Some(&b'\n');
        reader.consume(take);
        if sender.send(LogEvent::Consumed(take as u64)).await.is_err() {
            return Ok(None);
        }
        if ended {
            return Ok(Some((retained, dropped)));
        }
    }
}

fn validate_config(config: &BuildExecutorConfig) -> Result<()> {
    if !config.podman_path.is_absolute() || !config.runsc.path.is_absolute() {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            "podman_path and runsc.path must be absolute",
        ));
    }
    image_digest(&config.builder_image)?;
    if config.user.uid == 0 || config.user.gid == 0 {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            "builder uid and gid must both be non-zero",
        ));
    }
    if config.volume.driver != "local" {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            "volume driver must be local for the byte-capped tmpfs contract",
        ));
    }
    if config.volume.capacity_slack_bytes > 16 * 1024 * 1024 {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            "volume capacity rounding slack may not exceed 16 MiB",
        ));
    }
    let limits = &config.limits;
    if limits.cpu_millis == 0
        || limits.memory_bytes == 0
        || limits.pids == 0
        || limits.tmpfs_bytes < 12 * 1024
        || limits.workspace_bytes == 0
        || limits.output_bytes == 0
        || limits.output_entries == 0
        || limits.log_bytes == 0
        || limits.max_log_line_bytes == 0
        || limits.max_step_time.is_zero()
        || limits.max_total_time.is_zero()
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            "all resource, byte, entry, log, and time ceilings must be non-zero",
        ));
    }
    if limits.output_bytes > limits.workspace_bytes
        || limits.max_log_line_bytes as u64 > limits.log_bytes
        || limits.max_step_time > limits.max_total_time
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            "output must fit the workspace, a log line must fit the log cap, and a step must fit the build deadline",
        ));
    }
    for key in &config.allowed_env {
        validate_env_key(key)?;
        if is_reserved_env(key) {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidConfig,
                "validate build executor config",
                format!("{key:?} is platform-owned and cannot be allowlisted"),
            ));
        }
    }
    for key in config.podman_env.keys() {
        validate_env_key(key)?;
    }
    if let Some(policy) = &config.network_policy {
        validate_identifier(&policy.network, 128, "network name")?;
        validate_identifier(&policy.bridge, 15, "network bridge")?;
        validate_identifier(&policy.policy_id, 128, "network policy id")?;
        validate_identifier(&policy.nft_table, 32, "nft table")?;
        validate_identifier(&policy.nft_bridge_table, 32, "nft bridge table")?;
        let dns_addresses = policy
            .dns_upstream_ipv4
            .iter()
            .map(|address| {
                address
                    .parse::<std::net::Ipv4Addr>()
                    .ok()
                    .filter(|parsed| parsed.to_string() == *address)
            })
            .collect::<Option<Vec<_>>>();
        let dns_unique = dns_addresses.as_ref().is_some_and(|addresses| {
            !addresses.is_empty()
                && addresses.iter().copied().collect::<BTreeSet<_>>().len() == addresses.len()
        });
        let fleet_addresses = policy
            .fleet_public_ipv4
            .iter()
            .map(|address| {
                address
                    .parse::<std::net::Ipv4Addr>()
                    .ok()
                    .filter(|parsed| parsed.to_string() == *address)
            })
            .collect::<Option<Vec<_>>>();
        let fleet_sorted = fleet_addresses.as_ref().is_some_and(|addresses| {
            !addresses.is_empty() && addresses.windows(2).all(|pair| pair[0] < pair[1])
        });
        let subnet_valid = policy
            .subnet
            .split_once('/')
            .and_then(|(address, prefix)| {
                Some((
                    address.parse::<std::net::Ipv4Addr>().ok()?,
                    prefix.parse::<u8>().ok()?,
                ))
            })
            .is_some_and(|(_, prefix)| prefix <= 32);
        if !policy.verify_path.is_absolute()
            || !policy.provision_lock_path.is_absolute()
            || !policy.nft_binary.is_absolute()
            || !policy.nft_policy_path.is_absolute()
            || !policy.policy_digest.starts_with("sha256:")
            || parse_sha256(&policy.policy_digest, "network policy digest").is_err()
            || !dns_unique
            || !fleet_sorted
            || !policy.fleet_public_ipv4.contains(&policy.fleet_probe_ipv4)
            || policy.gateway.parse::<std::net::Ipv4Addr>().is_err()
            || !subnet_valid
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidConfig,
                "validate build executor config",
                "egress policy paths, identity, digest, DNS/fleet IPv4 sets, gateway, or subnet are malformed",
            ));
        }
    }
    Ok(())
}

fn validate_step(config: &BuildExecutorConfig, step: &BuildStep) -> Result<()> {
    if step.label.is_empty()
        || step.label.len() > 128
        || step.label.chars().any(char::is_control)
        || step.script.is_empty()
        || step.script.len() > 256 * 1024
        || step.script.contains('\0')
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidRequest,
            "validate build step",
            "step needs a non-empty bounded label and a non-empty script of at most 256 KiB without NUL",
        ));
    }
    validate_relative_path(step.cwd.as_str(), "step cwd")?;
    let argument_bytes = step.args.iter().try_fold(0usize, |total, value| {
        if value.contains('\0') {
            return None;
        }
        total.checked_add(value.len())
    });
    if step.args.len() > 256 || argument_bytes.is_none_or(|bytes| bytes > 256 * 1024) {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidRequest,
            "validate build step",
            "step arguments exceed 256 entries/256 KiB or contain NUL",
        ));
    }
    for (key, value) in &step.env {
        if validate_env_key(key).is_err()
            || is_reserved_env(key)
            || (!config.allowed_env.is_empty() && !config.allowed_env.contains(key))
        {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "validate build step",
                format!("environment key {key:?} is not in the build allowlist"),
            ));
        }
        if value.len() > 64 * 1024 || value.contains('\0') {
            return Err(BuildExecutorError::new(
                BuildExecutorErrorCode::InvalidRequest,
                "validate build step",
                format!("environment value for {key:?} exceeds 64 KiB or contains NUL"),
            ));
        }
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<()> {
    if value.len() > 4096 || value.contains('\0') || value.chars().any(char::is_control) {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidRequest,
            "validate workspace path",
            format!("{label} is too long or contains a control character"),
        ));
    }
    for component in Path::new(value).components() {
        match component {
            Component::CurDir | Component::Normal(_) => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BuildExecutorError::new(
                    BuildExecutorErrorCode::InvalidRequest,
                    "validate workspace path",
                    format!("{label} must stay relative to the copied checkout"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_env_key(key: &str) -> Result<()> {
    let mut bytes = key.bytes();
    let valid = key.len() <= 128
        && bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    if !valid {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate environment key",
            format!("invalid environment key {key:?}"),
        ));
    }
    Ok(())
}

fn is_reserved_env(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "HOME"
            | "CI"
            | "NEXT_TELEMETRY_DISABLED"
            | "npm_config_engine_strict"
            | "npm_config_fund"
            | "npm_config_audit"
    )
}

fn validate_identifier(value: &str, max: usize, label: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate build executor config",
            format!("{label} must use only ASCII letters, digits, dot, underscore, and dash"),
        ));
    }
    Ok(())
}

fn validate_trusted_regular_file(path: &Path, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "validate trusted file",
            format!("cannot inspect {label}: {error}"),
        )
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "validate trusted file",
            format!("{label} must be a root-owned regular file not writable by group/other"),
        ));
    }
    Ok(())
}

fn validate_trusted_executable(path: &Path, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    validate_trusted_regular_file(path, label)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "validate trusted executable",
            format!("cannot reinspect {label}: {error}"),
        )
    })?;
    if metadata.mode() & 0o111 == 0 {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "validate trusted executable",
            format!("{label} is not executable"),
        ));
    }
    Ok(())
}

fn validate_trusted_empty_dir(path: &Path, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "validate trusted directory",
            format!("cannot inspect {label}: {error}"),
        )
    })?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "validate trusted directory",
            format!("{label} must be a root-owned directory not writable by group/other"),
        ));
    }
    let mut entries = std::fs::read_dir(path).map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "validate trusted directory",
            format!("cannot read {label}: {error}"),
        )
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "validate trusted directory",
                format!("cannot enumerate {label}: {error}"),
            )
        })?
        .is_some()
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "validate trusted directory",
            format!("{label} must be empty"),
        ));
    }
    Ok(())
}

async fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityUnavailable,
            "hash runsc binary",
            error.to_string(),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityUnavailable,
                "hash runsc binary",
                error.to_string(),
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn image_digest(reference: &str) -> Result<&str> {
    let (name, digest) = if let Some(digest) = reference.strip_prefix("sha256:") {
        (None, digest)
    } else if let Some((name, digest)) = reference.rsplit_once("@sha256:") {
        (Some(name), digest)
    } else {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate builder image",
            "builder image must be sha256:<64 lowercase hex> or end in @sha256:<64 lowercase hex>",
        ));
    };
    let valid = name.is_none_or(|name| !name.is_empty() && !name.chars().any(char::is_whitespace))
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "validate builder image",
            "builder image must carry an exact lowercase sha256 identity",
        ));
    }
    Ok(digest)
}

fn verify_image_inspect(bytes: &[u8], reference: &str) -> Result<()> {
    let expected = image_digest(reference)?;
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "probe builder image",
            format!("invalid image inspect JSON: {error}"),
        )
    })?;
    let object = first_object(&value).ok_or_else(|| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "probe builder image",
            "image inspect returned no object",
        )
    })?;
    let id_matches = json_str(object, &["Id", "ID", "id"])
        .is_some_and(|value| value.trim_start_matches("sha256:") == expected);
    let digest_matches = id_matches
        || json_str(object, &["Digest", "digest"])
            .map(|value| value.trim_start_matches("sha256:") == expected)
            .unwrap_or(false)
        || object
            .get("RepoDigests")
            .or_else(|| object.get("repoDigests"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|digests| {
                digests.iter().any(|value| {
                    value
                        .as_str()
                        .is_some_and(|value| value.ends_with(&format!("@sha256:{expected}")))
                })
            });
    if !digest_matches {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "probe builder image",
            format!("local image does not report configured sha256:{expected}"),
        ));
    }
    // The loaded image must still carry the reviewed runtime contract: the
    // init as Entrypoint (this executor invokes it explicitly, but a config
    // drift here means the archive is not the reviewed one), the exact
    // non-root user, and the protocol label.
    let image_config = object
        .get("Config")
        .or_else(|| object.get("config"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            BuildExecutorError::new(
                BuildExecutorErrorCode::CapabilityMismatch,
                "probe builder image",
                "image inspect carries no Config object",
            )
        })?;
    let user_ok = json_str(image_config, &["User"]) == Some("65532:65532");
    let entrypoint_ok = image_config
        .get("Entrypoint")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|values| values.len() == 1 && values[0].as_str() == Some(BUILDER_INIT_PATH));
    let workdir_ok = json_str(image_config, &["WorkingDir"]) == Some(WORKSPACE_MOUNT);
    let label_ok = image_config
        .get("Labels")
        .and_then(serde_json::Value::as_object)
        .and_then(|labels| labels.get("org.hive.build-executor.protocol"))
        .and_then(serde_json::Value::as_str)
        == Some("hive-build-executor/v1");
    if !user_ok || !entrypoint_ok || !workdir_ok || !label_ok {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::CapabilityMismatch,
            "probe builder image",
            "image config user/entrypoint/workdir/protocol-label differ from the reviewed contract",
        ));
    }
    Ok(())
}

fn require_success(operation: &'static str, output: &CommandOutput) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(BuildExecutorError::new(
        BuildExecutorErrorCode::PodmanFailed,
        operation,
        format!(
            "podman exited {}: {}",
            output.status,
            bounded_text(&output.stderr, 2048)
        ),
    ))
}

fn parse_two_u64(bytes: &[u8], operation: &'static str) -> Result<(u64, u64)> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        BuildExecutorError::new(
            BuildExecutorErrorCode::UnsafeOutput,
            operation,
            format!("non-UTF-8 audit output: {error}"),
        )
    })?;
    let mut fields = text.split_whitespace();
    let first = fields.next().and_then(|value| value.parse::<u64>().ok());
    let second = fields.next().and_then(|value| value.parse::<u64>().ok());
    if fields.next().is_some() || first.is_none() || second.is_none() {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::UnsafeOutput,
            operation,
            "audit did not return exactly two unsigned integers",
        ));
    }
    Ok((first.unwrap_or(0), second.unwrap_or(0)))
}

fn first_object(value: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value
        .as_array()
        .and_then(|array| array.first())
        .or(Some(value))
        .and_then(serde_json::Value::as_object)
}

fn json_str<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_str))
}

fn digest_field(hasher: &mut Sha256, label: &str, value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_sha256(value: &str, label: &str) -> Result<[u8; 32]> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BuildExecutorError::new(
            BuildExecutorErrorCode::InvalidConfig,
            "parse sha256",
            format!("{label} must be exactly 64 lowercase hexadecimal characters"),
        ));
    }
    let mut output = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(chunk).expect("validated ASCII hex");
        output[index] = u8::from_str_radix(text, 16).expect("validated hexadecimal pair");
    }
    Ok(output)
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn one_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn bounded_text(bytes: &[u8], cap: usize) -> String {
    let keep = bytes.len().min(cap);
    let mut text = String::from_utf8_lossy(&bytes[..keep]).trim().to_string();
    if bytes.len() > keep {
        text.push_str("…[truncated]");
    }
    text
}

fn redact_values(line: &str, values: &[String]) -> String {
    let mut redacted = line.to_string();
    for value in values {
        if !value.is_empty() {
            redacted = redacted.replace(value, "***");
        }
    }
    redacted
}

fn os(value: impl AsRef<OsStr>) -> OsString {
    value.as_ref().to_os_string()
}
