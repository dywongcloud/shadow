use anyhow::{bail, Context};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::{Mutex, OnceLock};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Duration, SystemTime};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::{CStr, CString, OsStr, OsString};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::{File, OpenOptions};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{Read, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Stdio;

pub use hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION;

const MAX_ENTRIES: u64 = 100_000;
const MAX_LOGICAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_MATERIALIZED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 128;
const DEFAULT_STAGE_GC_GRACE_SECS: u64 = 60 * 60;
const DEFAULT_STAGE_GC_MAX_REAP_FRACTION: f64 = 0.5;
const STAGE_NONCE_BYTES: usize = 16;

/// One workspace link that the dependency planner proved is development-only.
/// Staging may omit it only when a canonical `node_modules/<link_name>` symlink
/// resolves to the exact descriptor-bound workspace package and that package's
/// own manifest still declares `target_package_name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeArtifactWorkspaceLink {
    link_name: String,
    target_package_name: String,
    target_rel: PathBuf,
}

impl RuntimeArtifactWorkspaceLink {
    pub fn new(
        link_name: impl Into<String>,
        target_package_name: impl Into<String>,
        target_rel: impl Into<PathBuf>,
    ) -> Self {
        Self {
            link_name: link_name.into(),
            target_package_name: target_package_name.into(),
            target_rel: target_rel.into(),
        }
    }
}

/// Planner output for one immutable runtime artifact. The included paths form
/// its positive closure; omitted workspace links are exact, named exceptions
/// for package-manager materialization and never a broad `node_modules` ignore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeArtifactClosure {
    pub include_rel: Vec<PathBuf>,
    pub omitted_workspace_links: Vec<RuntimeArtifactWorkspaceLink>,
}

impl From<Vec<PathBuf>> for RuntimeArtifactClosure {
    fn from(include_rel: Vec<PathBuf>) -> Self {
        Self {
            include_rel,
            omitted_workspace_links: Vec::new(),
        }
    }
}

impl RuntimeArtifactClosure {
    pub fn new(
        include_rel: Vec<PathBuf>,
        omitted_workspace_links: Vec<RuntimeArtifactWorkspaceLink>,
    ) -> Self {
        Self {
            include_rel,
            omitted_workspace_links,
        }
    }
}

/// The immutable source descriptor shared by every isolated runtime backend.
/// `app_rel` is the deployed application's cwd relative to `checkout_root`.
#[derive(Clone, Debug)]
pub struct RuntimeArtifactSpec {
    pub checkout_root: PathBuf,
    pub app_rel: PathBuf,
    /// Explicit descriptor-relative transitive runtime closure. The app itself,
    /// root pnpm store/link roots and approved sibling workspace packages are
    /// named here; unrelated checkout members are never traversed.
    pub include_rel: Vec<PathBuf>,
    pub omitted_workspace_links: Vec<RuntimeArtifactWorkspaceLink>,
}

/// The two serving locations derived from one validated build descriptor.
/// `host_static_root` is only a locator used to open fluid-gateway's
/// descriptor-bound `StaticRoot`; request-time serving must never treat the
/// `PathBuf` itself as filesystem authority. Function processes use the
/// backend's cwd. `delivery_required` is explicit so callers never infer
/// isolation from a backend name or from whether these paths happen to be equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeArtifactPaths {
    pub host_static_root: PathBuf,
    pub guest_workdir: PathBuf,
    pub delivery_required: bool,
}

impl RuntimeArtifactSpec {
    pub fn new(checkout_root: impl Into<PathBuf>, app_rel: impl Into<PathBuf>) -> Self {
        let app_rel = app_rel.into();
        Self {
            checkout_root: checkout_root.into(),
            include_rel: vec![app_rel.clone()],
            omitted_workspace_links: Vec::new(),
            app_rel,
        }
    }

    pub fn with_includes<C>(
        checkout_root: impl Into<PathBuf>,
        app_rel: impl Into<PathBuf>,
        closure: C,
    ) -> Self
    where
        C: Into<RuntimeArtifactClosure>,
    {
        let app_rel = app_rel.into();
        let RuntimeArtifactClosure {
            mut include_rel,
            omitted_workspace_links,
        } = closure.into();
        include_rel.retain(|include| include != &app_rel);
        include_rel.push(app_rel.clone());
        Self {
            checkout_root: checkout_root.into(),
            app_rel,
            include_rel,
            omitted_workspace_links,
        }
    }

    /// Workspace artifacts require the advertised protocol during mixed rollouts.
    pub fn requires_protocol_capability(&self) -> bool {
        self.app_rel != Path::new("") && self.app_rel != Path::new(".")
    }

    pub fn guest_workdir(&self, guest_root: &str) -> anyhow::Result<String> {
        validate_relative(&self.app_rel, "application path")?;
        let rel = self.app_rel.to_string_lossy();
        if rel.is_empty() || rel == "." {
            Ok(guest_root.to_string())
        } else {
            Ok(format!("{}/{}", guest_root.trim_end_matches('/'), rel))
        }
    }

    /// Return only the validated host locator used by
    /// `fluid_compute::runtime_artifact_paths` to open fluid-gateway's
    /// descriptor-bound `StaticRoot`. This `PathBuf` is not durable serving
    /// authority: no caller may retain it for request-time descendant lookup.
    pub fn host_static_root(&self) -> anyhow::Result<PathBuf> {
        validate_relative(&self.app_rel, "application path")?;
        if self.app_rel == Path::new("") || self.app_rel == Path::new(".") {
            Ok(self.checkout_root.clone())
        } else {
            Ok(self.checkout_root.join(&self.app_rel))
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActiveStageKey {
    parent_device: u64,
    parent_inode: u64,
    name: OsString,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn active_stages() -> &'static Mutex<HashSet<ActiveStageKey>> {
    static ACTIVE: OnceLock<Mutex<HashSet<ActiveStageKey>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ActiveStage(ActiveStageKey);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ActiveStage {
    fn register(parent: SourceIdentity, name: OsString) -> Self {
        let key = ActiveStageKey {
            parent_device: parent.device,
            parent_inode: parent.inode,
            name,
        };
        active_stages()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone());
        Self(key)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for ActiveStage {
    fn drop(&mut self) {
        active_stages()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.0);
    }
}

/// A fully materialized, symlink-free checkout snapshot. Removal is automatic,
/// including cancellation and every error after staging.
pub(crate) struct StagedRuntimeArtifact {
    root_name: OsString,
    content_sha256: String,
    materialized_bytes: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_parent: File,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_descriptor: File,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    _active: ActiveStage,
}

impl StagedRuntimeArtifact {
    pub(crate) fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    /// Validated staged regular-file bytes, rounded up to MiB. Callers that
    /// allocate a filesystem image add their own inode/metadata headroom; they
    /// never reopen the stage with `du` just to rediscover tenant-controlled
    /// paths.
    pub(crate) fn materialized_size_mib(&self) -> u64 {
        self.materialized_bytes.div_ceil(1024 * 1024)
    }

    /// Give one child process a descriptor-backed name for the exact held stage.
    /// The returned path works only on `command`, whose pre-exec hook clears
    /// close-on-exec for this descriptor; it is not a mutable host pathname.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn inherited_path(
        &self,
        command: &mut tokio::process::Command,
    ) -> anyhow::Result<PathBuf> {
        inherit_file_path(command, &self.root_descriptor)
    }

    /// Read one regular staged file through the held root descriptor. A path
    /// rename or symlink swap cannot redirect metadata extraction after the
    /// checkout snapshot was validated.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn read_regular(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let relative = normalized_relative(relative, "staged read path")?;
        let mut file = match open_staged_regular(&self.root_descriptor, &relative) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("open staged runtime artifact file {}", relative.display())
                })
            }
        };
        let state = source_state(&file)?;
        anyhow::ensure!(
            state.identity.is_regular(),
            "staged runtime artifact entry is not a regular file: {}",
            relative.display()
        );
        anyhow::ensure!(
            state.identity.length <= max_bytes,
            "staged runtime artifact file exceeds {max_bytes} bytes: {}",
            relative.display()
        );
        let length = usize::try_from(state.identity.length)
            .context("staged runtime artifact file length does not fit memory")?;
        let mut bytes = vec![0_u8; length];
        file.read_exact(&mut bytes)?;
        let mut extra = [0_u8; 1];
        anyhow::ensure!(
            file.read(&mut extra)? == 0,
            "staged runtime artifact file grew while reading: {}",
            relative.display()
        );
        ensure_same_state(state, source_state(&file)?, &relative)?;
        Ok(Some(bytes))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn is_regular_file(&self, relative: &Path) -> anyhow::Result<bool> {
        let relative = normalized_relative(relative, "staged probe path")?;
        match open_staged_regular(&self.root_descriptor, &relative) {
            Ok(file) => Ok(source_state(&file)?.identity.is_regular()),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(false),
            Err(error) => Err(error).with_context(|| {
                format!("probe staged runtime artifact file {}", relative.display())
            }),
        }
    }

    /// Pack the held stage descriptor, never its mutable pathname. The child
    /// inherits only this descriptor for the duration of tar's exec; the
    /// destination is an already-created platform file wired to stdout.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) async fn package_workspace(&self, destination: &File) -> anyhow::Result<()> {
        let mut command = tokio::process::Command::new("tar");
        let source = inherit_file_path(&mut command, &self.root_descriptor)?;
        command
            .arg("-C")
            .arg(source)
            .arg("--sort=name")
            .arg("--mtime=@0")
            .arg("--owner=0")
            .arg("--group=0")
            .arg("--numeric-owner")
            .arg("--transform=s,^\\./,workspace/,")
            .arg("-cf")
            .arg("-")
            .arg(".")
            .stdout(Stdio::from(destination.try_clone()?))
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let result = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("failed to spawn tar: {error}"))?
            .wait_with_output()
            .await
            .map_err(|error| anyhow::anyhow!("failed to wait for tar: {error}"))?;
        anyhow::ensure!(
            result.status.success(),
            "tar failed packing descriptor-bound runtime artifact: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
        Ok(())
    }

    /// Add the platform-owned identity marker after all repository bytes have
    /// been materialized and hashed. `create_new` makes a repository collision a
    /// loud refusal rather than allowing source to forge the guest handshake.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn write_identity(
        &self,
        identity: &hive_core::RuntimeArtifactIdentity,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            identity.protocol == RUNTIME_ARTIFACT_PROTOCOL_VERSION,
            "runtime artifact identity uses unsupported protocol {}",
            identity.protocol
        );
        anyhow::ensure!(
            identity.content_sha256 == self.content_sha256,
            "runtime artifact identity digest does not match staged content"
        );
        anyhow::ensure!(
            !identity.id.is_empty()
                && identity.id.len() <= 256
                && identity
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
            "runtime artifact identity contains an invalid id"
        );
        let bytes = serde_json::to_vec(identity).context("encode runtime artifact identity")?;
        let mut marker = create_destination_file(
            &self.root_descriptor,
            std::ffi::OsStr::new(hive_core::RUNTIME_ARTIFACT_MARKER_FILE),
            0o444,
        )
        .context("create runtime artifact identity marker")?;
        marker
            .write_all(&bytes)
            .context("write runtime artifact identity marker")?;
        marker
            .sync_all()
            .context("sync runtime artifact identity marker")?;
        self.root_descriptor
            .sync_all()
            .context("sync runtime artifact stage after identity marker")?;
        Ok(())
    }
}

impl Drop for StagedRuntimeArtifact {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _ = remove_exact_stage(&self.root_parent, &self.root_name, &self.root_descriptor);
        }
    }
}

/// Materialize a descriptor-bound, symlink-free checkout snapshot. Linux uses
/// openat2 and refuses kernels without it. macOS keeps every traversal relative
/// to held descriptors and rejects symlinks rather than weakening containment.
pub(crate) async fn stage_runtime_artifact(
    spec: &RuntimeArtifactSpec,
    scratch_parent: &Path,
) -> anyhow::Result<StagedRuntimeArtifact> {
    let spec = spec.clone();
    let scratch_parent = scratch_parent.to_path_buf();
    tokio::task::spawn_blocking(move || stage_blocking(&spec, &scratch_parent))
        .await
        .context("runtime artifact staging task failed")?
}

/// Stage while transferring an external serialization permit into the blocking
/// task. If the async caller is cancelled, the task retains the permit until it
/// has stopped mutating the stage and its unobserved result has been dropped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) async fn stage_runtime_artifact_serialized<G>(
    spec: &RuntimeArtifactSpec,
    scratch_parent: File,
    serialization: G,
) -> anyhow::Result<(StagedRuntimeArtifact, G)>
where
    G: Send + 'static,
{
    let spec = spec.clone();
    tokio::task::spawn_blocking(move || {
        let staged = stage_blocking_in(&spec, scratch_parent)?;
        Ok::<_, anyhow::Error>((staged, serialization))
    })
    .await
    .context("runtime artifact staging task failed")?
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stage_blocking(
    _spec: &RuntimeArtifactSpec,
    _scratch_parent: &Path,
) -> anyhow::Result<StagedRuntimeArtifact> {
    bail!("runtime artifact staging requires descriptor-relative filesystem support")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceIdentity {
    device: u64,
    inode: u64,
    kind: u32,
    length: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SourceIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            kind: metadata.mode() & libc::S_IFMT as u32,
            length: metadata.len(),
        }
    }

    fn from_stat(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            kind: (stat.st_mode as u32) & libc::S_IFMT as u32,
            length: stat.st_size.max(0) as u64,
        }
    }

    fn inode_key(self) -> (u64, u64) {
        (self.device, self.inode)
    }

    fn is_directory(self) -> bool {
        self.kind == libc::S_IFDIR as u32
    }

    fn is_regular(self) -> bool {
        self.kind == libc::S_IFREG as u32
    }

    fn is_symlink(self) -> bool {
        self.kind == libc::S_IFLNK as u32
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceState {
    identity: SourceIdentity,
    mode: u32,
    links: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SourceState {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            identity: SourceIdentity::from_metadata(metadata),
            mode: metadata.mode(),
            links: metadata.nlink(),
        }
    }

    fn from_stat(stat: &libc::stat) -> Self {
        Self {
            identity: SourceIdentity::from_stat(stat),
            mode: stat.st_mode as u32,
            links: stat.st_nlink as u64,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PreparedRoot {
    logical: PathBuf,
    physical: PathBuf,
    descriptor: File,
    state: SourceState,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PreparedOmittedWorkspaceLink {
    logical_suffix: PathBuf,
    target_physical: PathBuf,
    target_state: SourceState,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ChildPreflight {
    name: OsString,
    state: SourceState,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct HardlinkProof {
    links: u64,
    physical_paths: HashSet<PathBuf>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
struct Totals {
    enumerated_entries: u64,
    materialized_entries: u64,
    logical_bytes: u64,
    materialized_bytes: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Totals {
    fn account_enumerated(&mut self, relative: &Path, depth: usize) -> anyhow::Result<()> {
        validate_output_path(relative, depth)?;
        self.enumerated_entries = self.enumerated_entries.saturating_add(1);
        anyhow::ensure!(
            self.enumerated_entries <= MAX_ENTRIES,
            "runtime artifact exceeds {MAX_ENTRIES} enumerated entries"
        );
        Ok(())
    }

    fn reserve_materialized_entry(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.materialized_entries < MAX_ENTRIES,
            "runtime artifact exceeds {MAX_ENTRIES} materialized entries"
        );
        self.materialized_entries += 1;
        Ok(())
    }

    fn account_file(&mut self, bytes: u64, unique: bool) -> anyhow::Result<()> {
        self.reserve_materialized_entry()?;
        self.materialized_bytes = self.materialized_bytes.saturating_add(bytes);
        if unique {
            self.logical_bytes = self.logical_bytes.saturating_add(bytes);
        }
        anyhow::ensure!(
            self.logical_bytes <= MAX_LOGICAL_BYTES,
            "runtime artifact exceeds {MAX_LOGICAL_BYTES} logical bytes"
        );
        anyhow::ensure!(
            self.materialized_bytes <= MAX_MATERIALIZED_BYTES,
            "runtime artifact exceeds {MAX_MATERIALIZED_BYTES} materialized bytes"
        );
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct MaterializeContext<'a> {
    checkout: &'a File,
    approved: &'a [PreparedRoot],
    omitted_workspace_links: &'a [PreparedOmittedWorkspaceLink],
    stage: &'a File,
    app_rel: &'a Path,
    app_seen: bool,
    totals: Totals,
    active_directories: HashSet<(u64, u64)>,
    unique_files: HashSet<(u64, u64)>,
    hardlinks: HashMap<(u64, u64), HardlinkProof>,
    content: Sha256,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_directory_component(parent: &File, name: &OsStr) -> std::io::Result<File> {
    let name = cstring_component(name).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_or_create_scratch_root(path: &Path) -> anyhow::Result<File> {
    anyhow::ensure!(
        path.is_absolute(),
        "runtime artifact scratch root must be absolute: {}",
        path.display()
    );
    let mut current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .context("open filesystem root for runtime artifact scratch")?;
    let mut components = 0_usize;
    for component in path.components() {
        let Component::Normal(name) = component else {
            anyhow::ensure!(
                matches!(component, Component::RootDir),
                "runtime artifact scratch root is not normalized: {}",
                path.display()
            );
            continue;
        };
        components += 1;
        let next = match open_directory_component(&current, name) {
            Ok(directory) => directory,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                let name_c = cstring_component(name)?;
                let rc = unsafe { libc::mkdirat(current.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if rc < 0 {
                    let create_error = std::io::Error::last_os_error();
                    if create_error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(create_error).with_context(|| {
                            format!(
                                "create no-follow runtime artifact scratch component {}",
                                path.display()
                            )
                        });
                    }
                } else {
                    current.sync_all().with_context(|| {
                        format!(
                            "sync parent after creating runtime artifact scratch component {}",
                            path.display()
                        )
                    })?;
                }
                let preflight = lstat_at(&current, name)?;
                let directory = open_directory_component(&current, name).with_context(|| {
                    format!(
                        "open no-follow runtime artifact scratch component {}",
                        path.display()
                    )
                })?;
                ensure_same_state(preflight, source_state(&directory)?, Path::new(name))?;
                directory
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "open no-follow runtime artifact scratch component {}",
                        path.display()
                    )
                })
            }
        };
        anyhow::ensure!(
            source_state(&next)?.identity.is_directory(),
            "runtime artifact scratch path component is not a directory: {}",
            path.display()
        );
        current = next;
    }
    anyhow::ensure!(
        components > 0,
        "runtime artifact scratch root cannot be the filesystem root"
    );
    let metadata = current.metadata()?;
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "runtime artifact scratch root is not owned by the current process user: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.mode() & 0o022 == 0,
        "runtime artifact scratch root is group/world writable: {}",
        path.display()
    );
    Ok(current)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stage_blocking(
    spec: &RuntimeArtifactSpec,
    scratch_parent: &Path,
) -> anyhow::Result<StagedRuntimeArtifact> {
    let parent = open_or_create_scratch_root(scratch_parent)?;
    stage_blocking_in(spec, parent)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stage_blocking_in(
    spec: &RuntimeArtifactSpec,
    scratch_parent: File,
) -> anyhow::Result<StagedRuntimeArtifact> {
    anyhow::ensure!(
        source_state(&scratch_parent)?.identity.is_directory(),
        "runtime artifact scratch descriptor is not a directory"
    );
    let app_rel = normalized_relative(&spec.app_rel, "application path")?;
    let roots = selected_roots(spec, &app_rel)?;
    let checkout = open_checkout_root(&spec.checkout_root)?;

    let mut totals = Totals::default();
    let mut approved = Vec::with_capacity(roots.len());
    for logical in roots {
        totals.account_enumerated(&logical, logical.components().count())?;
        let descriptor = open_source_beneath(&checkout, &logical).with_context(|| {
            format!(
                "open runtime artifact include beneath checkout: {}",
                logical.display()
            )
        })?;
        let state = source_state(&descriptor)?;
        anyhow::ensure!(
            state.identity.is_directory() || state.identity.is_regular(),
            "runtime artifact rejects special include path {}",
            logical.display()
        );
        let physical = resolved_physical_relative(&checkout, &descriptor, &logical)?;
        validate_output_path(&physical, physical.components().count())?;
        anyhow::ensure!(
            !excluded(&physical),
            "runtime artifact include resolves through excluded path: {} -> {}",
            logical.display(),
            physical.display()
        );
        approved.push(PreparedRoot {
            logical,
            physical,
            descriptor,
            state,
        });
    }
    let omitted_workspace_links = prepare_omitted_workspace_links(spec, &checkout, &approved)?;

    let mut staged = create_stage_in(&scratch_parent)?;
    if let Err(error) = sweep_stale_stages(&scratch_parent) {
        tracing::warn!(error = %error, "runtime artifact stale-stage recovery refused or failed");
    }
    let mut content = Sha256::new();
    content.update(b"hive-runtime-artifact-content-v1\0");
    let mut context = MaterializeContext {
        checkout: &checkout,
        approved: &approved,
        omitted_workspace_links: &omitted_workspace_links,
        stage: &staged.root_descriptor,
        app_rel: &app_rel,
        app_seen: false,
        totals,
        active_directories: HashSet::new(),
        unique_files: HashSet::new(),
        hardlinks: HashMap::new(),
        content,
    };

    for prepared in &approved {
        materialize_entry(
            &mut context,
            &prepared.descriptor,
            prepared.state,
            &prepared.physical,
            &prepared.logical,
            prepared.logical.components().count(),
        )?;
    }

    anyhow::ensure!(
        context.app_seen,
        "runtime artifact application path is absent or not a directory: {}",
        app_rel.display()
    );
    for proof in context.hardlinks.values() {
        anyhow::ensure!(
            proof.links == proof.physical_paths.len() as u64,
            "runtime artifact rejects checkout-external hardlink provenance: expected {} links, approved {}",
            proof.links,
            proof.physical_paths.len()
        );
    }
    let digest = context.content.clone().finalize();
    let materialized_bytes = context.totals.materialized_bytes;
    drop(context);
    staged.content_sha256 = hex_digest(&digest);
    staged.materialized_bytes = materialized_bytes;
    Ok(staged)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn selected_roots(spec: &RuntimeArtifactSpec, app_rel: &Path) -> anyhow::Result<Vec<PathBuf>> {
    anyhow::ensure!(
        spec.include_rel.len() < MAX_ENTRIES as usize,
        "runtime artifact descriptor exceeds {MAX_ENTRIES} include paths"
    );
    let mut includes = Vec::with_capacity(spec.include_rel.len() + 1);
    for include in &spec.include_rel {
        includes.push(normalized_relative(include, "include path")?);
    }
    includes.retain(|include| include != app_rel);
    includes.push(app_rel.to_path_buf());
    includes.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    includes.dedup();

    let mut roots: Vec<PathBuf> = Vec::with_capacity(includes.len());
    for include in includes {
        validate_output_path(&include, include.components().count())?;
        anyhow::ensure!(
            !excluded(&include),
            "runtime artifact descriptor explicitly includes excluded path {}",
            include.display()
        );
        if roots.iter().any(|parent| include.starts_with(parent)) {
            continue;
        }
        roots.push(include);
    }
    Ok(roots)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn package_name_path(name: &str, label: &str) -> anyhow::Result<PathBuf> {
    fn valid_segment(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 214
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }

    anyhow::ensure!(
        !name.is_empty() && name.len() <= 214 && name.is_ascii(),
        "runtime artifact {label} is not a bounded ASCII package name: {name:?}"
    );
    if let Some(scoped) = name.strip_prefix('@') {
        let (scope, package) = scoped.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("runtime artifact {label} has a malformed scope: {name:?}")
        })?;
        anyhow::ensure!(
            !package.contains('/') && valid_segment(scope) && valid_segment(package),
            "runtime artifact {label} is not a canonical scoped package name: {name:?}"
        );
        Ok(PathBuf::from(format!("@{scope}")).join(package))
    } else {
        anyhow::ensure!(
            !name.contains(['/', '@']) && valid_segment(name),
            "runtime artifact {label} is not a canonical package name: {name:?}"
        );
        Ok(PathBuf::from(name))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn workspace_manifest_name(directory: &File, logical: &Path) -> anyhow::Result<String> {
    let manifest_name = OsStr::new("package.json");
    let before = lstat_at(directory, manifest_name).with_context(|| {
        format!(
            "read omitted workspace package manifest metadata {}",
            logical.display()
        )
    })?;
    anyhow::ensure!(
        before.identity.is_regular(),
        "runtime artifact omitted workspace package has no regular package.json: {}",
        logical.display()
    );
    anyhow::ensure!(
        before.identity.length <= 1024 * 1024,
        "runtime artifact omitted workspace package manifest exceeds 1048576 bytes: {}",
        logical.display()
    );
    let held = open_source_beneath(directory, Path::new("package.json")).with_context(|| {
        format!(
            "open omitted workspace package manifest {}",
            logical.display()
        )
    })?;
    ensure_same_state(before, source_state(&held)?, &logical.join("package.json"))?;
    ensure_same_state(
        before,
        lstat_at(directory, manifest_name)?,
        &logical.join("package.json"),
    )?;
    let mut readable = readable_descriptor(&held, before, false)?;
    let length = usize::try_from(before.identity.length)
        .context("omitted workspace package manifest length does not fit memory")?;
    let mut bytes = vec![0_u8; length];
    readable.read_exact(&mut bytes)?;
    let mut extra = [0_u8; 1];
    anyhow::ensure!(
        readable.read(&mut extra)? == 0,
        "runtime artifact omitted workspace package manifest grew while reading: {}",
        logical.display()
    );
    ensure_same_state(
        before,
        source_state(&readable)?,
        &logical.join("package.json"),
    )?;
    let package: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parse omitted workspace package manifest {}",
            logical.display()
        )
    })?;
    package
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime artifact omitted workspace package must declare a string name: {}",
                logical.display()
            )
        })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prepare_omitted_workspace_links(
    spec: &RuntimeArtifactSpec,
    checkout: &File,
    approved: &[PreparedRoot],
) -> anyhow::Result<Vec<PreparedOmittedWorkspaceLink>> {
    anyhow::ensure!(
        spec.omitted_workspace_links.len() < MAX_ENTRIES as usize,
        "runtime artifact descriptor exceeds {MAX_ENTRIES} omitted workspace links"
    );
    let mut prepared = Vec::with_capacity(spec.omitted_workspace_links.len());
    for omitted in &spec.omitted_workspace_links {
        let link_name = package_name_path(&omitted.link_name, "workspace link name")?;
        let _target_name = package_name_path(
            &omitted.target_package_name,
            "workspace target package name",
        )?;
        let target = normalized_relative(&omitted.target_rel, "omitted workspace target")?;
        validate_output_path(&target, target.components().count())?;
        anyhow::ensure!(
            !target.as_os_str().is_empty() && !excluded(&target),
            "runtime artifact omitted workspace target is empty or excluded: {}",
            target.display()
        );
        anyhow::ensure!(
            approved.iter().all(|root| {
                !target.starts_with(&root.physical) && !root.physical.starts_with(&target)
            }),
            "runtime artifact omitted workspace target overlaps the approved closure: {}",
            target.display()
        );

        let descriptor = open_source_beneath(checkout, &target).with_context(|| {
            format!(
                "open omitted workspace package beneath checkout: {}",
                target.display()
            )
        })?;
        let target_state = source_state(&descriptor)?;
        anyhow::ensure!(
            target_state.identity.is_directory(),
            "runtime artifact omitted workspace target is not a directory: {}",
            target.display()
        );
        let target_physical = resolved_physical_relative(checkout, &descriptor, &target)?;
        anyhow::ensure!(
            target_physical == target && !excluded(&target_physical),
            "runtime artifact omitted workspace target is not an exact real checkout directory: {} -> {}",
            target.display(),
            target_physical.display()
        );
        let actual_name = workspace_manifest_name(&descriptor, &target)?;
        anyhow::ensure!(
            actual_name == omitted.target_package_name,
            "runtime artifact omitted workspace target name changed: expected {:?}, found {:?} at {}",
            omitted.target_package_name,
            actual_name,
            target.display()
        );
        prepared.push(PreparedOmittedWorkspaceLink {
            logical_suffix: PathBuf::from("node_modules").join(link_name),
            target_physical,
            target_state,
        });
    }
    prepared.sort_by(|left, right| {
        left.logical_suffix
            .cmp(&right.logical_suffix)
            .then_with(|| left.target_physical.cmp(&right.target_physical))
    });
    prepared.dedup_by(|left, right| {
        left.logical_suffix == right.logical_suffix
            && left.target_physical == right.target_physical
            && left.target_state == right.target_state
    });
    Ok(prepared)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn planned_workspace_link_omission(
    context: &MaterializeContext<'_>,
    link_state: SourceState,
    target_state: SourceState,
    target_physical: &Path,
    logical: &Path,
) -> anyhow::Result<bool> {
    if !link_state.identity.is_symlink() {
        return Ok(false);
    }
    for omitted in context.omitted_workspace_links {
        if !logical.ends_with(&omitted.logical_suffix) || target_physical != omitted.target_physical
        {
            continue;
        }
        ensure_same_state(omitted.target_state, target_state, logical)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn materialize_entry(
    context: &mut MaterializeContext<'_>,
    source: &File,
    expected: SourceState,
    physical: &Path,
    logical: &Path,
    depth: usize,
) -> anyhow::Result<()> {
    validate_output_path(logical, depth)?;
    let current = source_state(source)?;
    ensure_same_state(expected, current, logical)?;
    ensure_approved_source(context, source, current, physical, logical)?;

    if current.identity.is_directory() {
        hash_entry_header(&mut context.content, b'd', logical, current.mode, 0);
        if logical == context.app_rel {
            context.app_seen = true;
        }
        let key = current.identity.inode_key();
        if !context.active_directories.insert(key) {
            bail!("runtime artifact symlink loop at {}", logical.display());
        }

        let destination =
            ensure_destination_directory(context.stage, logical, &mut context.totals)?;
        let readable = readable_descriptor(source, current, true)
            .with_context(|| format!("open runtime artifact directory {}", logical.display()))?;
        let children = read_directory_children(&readable, logical, depth, &mut context.totals)
            .with_context(|| {
                format!("enumerate runtime artifact directory {}", logical.display())
            })?;
        for child in children {
            let child_logical = logical.join(&child.name);
            if excluded(&child_logical) {
                anyhow::ensure!(
                    !child.state.identity.is_symlink(),
                    "runtime artifact rejects symlink at excluded path {}",
                    child_logical.display()
                );
                continue;
            }
            anyhow::ensure!(
                child.state.identity.is_symlink()
                    || child.state.identity.is_directory()
                    || child.state.identity.is_regular(),
                "runtime artifact rejects special file at {}",
                child_logical.display()
            );

            let child_physical = physical.join(&child.name);
            let descriptor =
                open_child_source(context.checkout, &readable, &child_physical, &child.name)
                    .with_context(|| {
                        format!(
                            "open runtime artifact entry beneath checkout: {}",
                            child_logical.display()
                        )
                    })?;
            let post_open_link = lstat_at(&readable, &child.name)?;
            ensure_same_state(child.state, post_open_link, &child_logical)?;
            let target_state = source_state(&descriptor)?;
            if !child.state.identity.is_symlink() {
                ensure_same_state(child.state, target_state, &child_logical)?;
            }
            anyhow::ensure!(
                target_state.identity.is_directory() || target_state.identity.is_regular(),
                "runtime artifact rejects symlink target type at {}",
                child_logical.display()
            );
            let target_physical =
                resolved_physical_relative(context.checkout, &descriptor, &child_physical)?;
            validate_output_path(&target_physical, target_physical.components().count())?;
            anyhow::ensure!(
                !excluded(&target_physical),
                "runtime artifact symlink targets excluded path: {} -> {}",
                child_logical.display(),
                target_physical.display()
            );
            if planned_workspace_link_omission(
                context,
                child.state,
                target_state,
                &target_physical,
                &child_logical,
            )? {
                continue;
            }
            materialize_entry(
                context,
                &descriptor,
                target_state,
                &target_physical,
                &child_logical,
                depth + 1,
            )?;
        }
        let after = source_state(source)?;
        ensure_same_state(current, after, logical)?;
        destination.set_permissions(std::fs::Permissions::from_mode(current.mode & 0o777))?;
        context.active_directories.remove(&key);
        return Ok(());
    }

    if current.identity.is_regular() {
        materialize_regular(context, source, current, physical, logical)?;
        return Ok(());
    }

    bail!(
        "runtime artifact rejects special file at {}",
        logical.display()
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn materialize_regular(
    context: &mut MaterializeContext<'_>,
    source: &File,
    state: SourceState,
    physical: &Path,
    logical: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !logical.as_os_str().is_empty(),
        "runtime artifact cannot materialize checkout root as a file"
    );
    anyhow::ensure!(
        state.links > 0,
        "runtime artifact source was unlinked during staging: {}",
        logical.display()
    );
    let key = state.identity.inode_key();
    let proof = context
        .hardlinks
        .entry(key)
        .or_insert_with(|| HardlinkProof {
            links: state.links,
            physical_paths: HashSet::new(),
        });
    anyhow::ensure!(
        proof.links == state.links,
        "runtime artifact hardlink count changed during staging: {}",
        logical.display()
    );
    proof.physical_paths.insert(physical.to_path_buf());

    let unique = context.unique_files.insert(key);
    context.totals.account_file(state.identity.length, unique)?;
    hash_entry_header(
        &mut context.content,
        b'f',
        logical,
        state.mode,
        state.identity.length,
    );
    let parent_rel = logical.parent().unwrap_or_else(|| Path::new(""));
    let destination_parent =
        ensure_destination_directory(context.stage, parent_rel, &mut context.totals)?;
    let name = logical.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "runtime artifact file has no destination name: {}",
            logical.display()
        )
    })?;
    let mut output = create_destination_file(&destination_parent, name, state.mode & 0o777)
        .with_context(|| format!("create runtime artifact destination {}", logical.display()))?;
    let mut input = readable_descriptor(source, state, false)
        .with_context(|| format!("open runtime artifact source {}", logical.display()))?;

    let mut remaining = state.identity.length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..take]).with_context(|| {
            format!(
                "exact-length read of runtime artifact {}",
                logical.display()
            )
        })?;
        context.content.update(&buffer[..take]);
        output.write_all(&buffer[..take])?;
        remaining -= take as u64;
    }
    let mut extra = [0_u8; 1];
    anyhow::ensure!(
        input.read(&mut extra)? == 0,
        "runtime artifact source grew during exact-length read: {}",
        logical.display()
    );
    let after = source_state(&input)?;
    ensure_same_state(state, after, logical)?;
    output.flush()?;
    output.set_permissions(std::fs::Permissions::from_mode(state.mode & 0o777))?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_same_state(
    expected: SourceState,
    actual: SourceState,
    logical: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        expected == actual,
        "runtime artifact source identity changed after preflight (device/inode/type/length/mode/link-count): {}",
        logical.display()
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn source_state(file: &File) -> anyhow::Result<SourceState> {
    Ok(SourceState::from_metadata(&file.metadata()?))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lstat_at(parent: &File, name: &std::ffi::OsStr) -> anyhow::Result<SourceState> {
    let name = cstring_component(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("descriptor-relative lstat failed");
    }
    Ok(SourceState::from_stat(unsafe { &stat.assume_init() }))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_checkout_root(path: &Path) -> anyhow::Result<File> {
    let canonical = std::fs::canonicalize(path).with_context(|| {
        format!(
            "runtime artifact checkout root is unavailable: {}",
            path.display()
        )
    })?;
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical)
        .with_context(|| {
            format!(
                "open runtime artifact checkout root {}",
                canonical.display()
            )
        })?;
    anyhow::ensure!(
        source_state(&root)?.identity.is_directory(),
        "runtime artifact checkout root is not a directory"
    );
    Ok(root)
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
pub(crate) const RESOLVE_NO_XDEV: u64 = 0x01;
#[cfg(target_os = "linux")]
pub(crate) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
#[cfg(target_os = "linux")]
pub(crate) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
pub(crate) const RESOLVE_BENEATH: u64 = 0x08;

#[cfg(target_os = "linux")]
pub(crate) fn openat2_raw(
    parent: &File,
    path: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
    resolve: u64,
) -> std::io::Result<File> {
    let path = CString::new(path.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let how = OpenHow {
        flags: flags as u64,
        mode: mode as u64,
        resolve,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    } as i32;
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
pub(crate) fn openat2_required(
    parent: &File,
    path: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
    resolve: u64,
) -> anyhow::Result<File> {
    match openat2_raw(parent, path, flags, mode, resolve) {
        Ok(file) => Ok(file),
        Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => bail!(
            "Linux openat2 is unavailable; runtime artifact staging refuses an insecure fallback"
        ),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_source_from(parent: &File, relative: &Path) -> anyhow::Result<File> {
    if relative.as_os_str().is_empty() {
        return parent.try_clone().map_err(Into::into);
    }
    openat2_required(
        parent,
        relative.as_os_str(),
        libc::O_PATH | libc::O_CLOEXEC,
        0,
        RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV,
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn open_source_beneath(root: &File, relative: &Path) -> anyhow::Result<File> {
    open_source_from(root, relative)
}

#[cfg(target_os = "macos")]
fn open_source_beneath(root: &File, relative: &Path) -> anyhow::Result<File> {
    let mut current = root.try_clone()?;
    let components = normal_components(relative)?;
    for (index, component) in components.iter().enumerate() {
        let preflight = lstat_at(&current, component.as_os_str())?;
        anyhow::ensure!(
            !preflight.identity.is_symlink(),
            "runtime artifact symlinks require Linux openat2: {}",
            relative.display()
        );
        anyhow::ensure!(
            preflight.identity.is_directory() || preflight.identity.is_regular(),
            "runtime artifact rejects special source: {}",
            relative.display()
        );
        let descriptor = openat_nofollow(&current, component.as_os_str())?;
        let state = source_state(&descriptor)?;
        ensure_same_state(preflight, state, relative)?;
        if index + 1 != components.len() {
            anyhow::ensure!(
                state.identity.is_directory(),
                "runtime artifact source component is not a directory: {}",
                relative.display()
            );
        }
        current = descriptor;
    }
    Ok(current)
}

#[cfg(target_os = "macos")]
fn openat_nofollow(parent: &File, name: &std::ffi::OsStr) -> anyhow::Result<File> {
    let name = cstring_component(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("descriptor-relative open failed");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_child_source(
    checkout: &File,
    _parent: &File,
    physical: &Path,
    _name: &std::ffi::OsStr,
) -> anyhow::Result<File> {
    open_source_beneath(checkout, physical)
}

#[cfg(target_os = "macos")]
fn open_child_source(
    _checkout: &File,
    parent: &File,
    _physical: &Path,
    name: &std::ffi::OsStr,
) -> anyhow::Result<File> {
    let preflight = lstat_at(parent, name)?;
    anyhow::ensure!(
        !preflight.identity.is_symlink(),
        "runtime artifact symlinks require Linux openat2"
    );
    openat_nofollow(parent, name)
}

#[cfg(target_os = "linux")]
fn readable_descriptor(
    source: &File,
    expected: SourceState,
    directory: bool,
) -> anyhow::Result<File> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        libc::O_CLOEXEC | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
    );
    let file = options
        .open(&path)
        .with_context(|| format!("reopen held descriptor through {}", path.display()))?;
    ensure_same_state(expected, source_state(&file)?, Path::new("held descriptor"))?;
    Ok(file)
}

#[cfg(target_os = "macos")]
fn readable_descriptor(
    source: &File,
    expected: SourceState,
    directory: bool,
) -> anyhow::Result<File> {
    let file = source.try_clone()?;
    let state = source_state(&file)?;
    ensure_same_state(expected, state, Path::new("held descriptor"))?;
    anyhow::ensure!(
        !directory || state.identity.is_directory(),
        "held descriptor is not a directory"
    );
    Ok(file)
}

#[cfg(target_os = "linux")]
fn resolved_physical_relative(
    checkout: &File,
    source: &File,
    _fallback: &Path,
) -> anyhow::Result<PathBuf> {
    let checkout_path = std::fs::read_link(format!("/proc/self/fd/{}", checkout.as_raw_fd()))
        .context("resolve held checkout descriptor")?;
    let source_path = std::fs::read_link(format!("/proc/self/fd/{}", source.as_raw_fd()))
        .context("resolve held source descriptor")?;
    let relative = source_path.strip_prefix(&checkout_path).map_err(|_| {
        anyhow::anyhow!(
            "runtime artifact source descriptor escaped checkout root: {}",
            source_path.display()
        )
    })?;
    normalized_relative(relative, "resolved source path")
}

#[cfg(target_os = "macos")]
fn resolved_physical_relative(
    _checkout: &File,
    _source: &File,
    fallback: &Path,
) -> anyhow::Result<PathBuf> {
    normalized_relative(fallback, "resolved source path")
}

#[cfg(target_os = "linux")]
fn ensure_approved_source(
    context: &MaterializeContext<'_>,
    _source: &File,
    state: SourceState,
    physical: &Path,
    logical: &Path,
) -> anyhow::Result<()> {
    for approved in context.approved {
        let Ok(suffix) = physical.strip_prefix(&approved.physical) else {
            continue;
        };
        let Ok(candidate) = open_source_from(&approved.descriptor, suffix) else {
            continue;
        };
        if source_state(&candidate)?.identity == state.identity {
            return Ok(());
        }
    }
    bail!(
        "runtime artifact symlink targets path outside the approved descriptor closure: {} -> {}",
        logical.display(),
        physical.display()
    )
}

#[cfg(target_os = "macos")]
fn ensure_approved_source(
    context: &MaterializeContext<'_>,
    _source: &File,
    _state: SourceState,
    physical: &Path,
    logical: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        context
            .approved
            .iter()
            .any(|approved| physical.starts_with(&approved.physical)),
        "runtime artifact source is outside approved closure: {} -> {}",
        logical.display(),
        physical.display()
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct DirectoryStream(*mut libc::DIR);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn set_errno(value: i32) {
    *libc::__errno_location() = value;
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> i32 {
    *libc::__errno_location()
}

#[cfg(target_os = "macos")]
unsafe fn set_errno(value: i32) {
    *libc::__error() = value;
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> i32 {
    *libc::__error()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_directory_children(
    directory: &File,
    logical: &Path,
    depth: usize,
    totals: &mut Totals,
) -> anyhow::Result<Vec<ChildPreflight>> {
    // `dup`/`File::try_clone` shares the directory stream offset with the held
    // capability. Open `.` relative to it instead so every enumeration starts
    // from a fresh open file description; stale-stage recovery intentionally
    // scans the same parent descriptor across successive bounded passes.
    let duplicate = open_directory_component(directory, OsStr::new("."))?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(std::io::Error::last_os_error()).context("fdopendir failed");
    }
    let stream = DirectoryStream(stream);
    let mut children = Vec::new();
    loop {
        unsafe {
            set_errno(0);
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let code = unsafe { errno() };
            if code != 0 {
                return Err(std::io::Error::from_raw_os_error(code))
                    .context("descriptor-relative readdir failed");
            }
            break;
        }
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = OsString::from_vec(bytes.to_vec());
        let child_logical = logical.join(&name);
        totals.account_enumerated(&child_logical, depth + 1)?;
        let state = lstat_at(directory, &name)?;
        children.push(ChildPreflight { name, state });
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(children)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PendingStageAllocation {
    parent: File,
    name: OsString,
    descriptor: Option<File>,
    armed: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for PendingStageAllocation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(descriptor) = &self.descriptor {
            let _ = remove_exact_stage(&self.parent, &self.name, descriptor);
            return;
        }
        if let Ok(name) = cstring_component(&self.name) {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
            }
            let _ = self.parent.sync_all();
        }
    }
}

#[cfg(target_os = "linux")]
fn fill_stage_random(bytes: &mut [u8]) -> anyhow::Result<()> {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let read = unsafe {
            libc::getrandom(bytes[offset..].as_mut_ptr().cast(), bytes.len() - offset, 0)
        };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("obtain runtime artifact stage randomness");
        }
        anyhow::ensure!(read > 0, "runtime artifact stage randomness returned EOF");
        offset += read as usize;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn fill_stage_random(bytes: &mut [u8]) -> anyhow::Result<()> {
    unsafe {
        libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unique_stage_name() -> anyhow::Result<OsString> {
    let mut nonce = [0_u8; STAGE_NONCE_BYTES];
    fill_stage_random(&mut nonce)?;
    Ok(OsString::from(format!(
        ".runtime-artifact-v2-{}-{}",
        hive_core::now_ms(),
        hex_digest(&nonce)
    )))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create_stage_in(parent: &File) -> anyhow::Result<StagedRuntimeArtifact> {
    let parent_state = source_state(parent)?;
    anyhow::ensure!(
        parent_state.identity.is_directory(),
        "runtime artifact stage parent is not a directory"
    );
    for _ in 0..256 {
        let root_parent = parent.try_clone()?;
        let cleanup_parent = parent.try_clone()?;
        let name = unique_stage_name()?;
        let name_c = cstring_component(&name)?;
        let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(error).context("create descriptor-relative runtime artifact stage");
        }
        let mut allocation = PendingStageAllocation {
            parent: cleanup_parent,
            name: name.clone(),
            descriptor: None,
            armed: true,
        };
        parent
            .sync_all()
            .context("sync runtime artifact stage parent after creation")?;
        let preflight = lstat_at(parent, &name)?;
        anyhow::ensure!(
            preflight.identity.is_directory(),
            "runtime artifact stage was replaced before opening"
        );
        let descriptor = open_destination_directory(parent, &name)
            .context("open newly-created runtime artifact stage")?;
        let opened = source_state(&descriptor)?;
        ensure_same_state(preflight, opened, Path::new(&name))?;
        ensure_same_state(lstat_at(parent, &name)?, opened, Path::new(&name))?;
        allocation.descriptor = Some(descriptor);
        let root_descriptor = allocation
            .descriptor
            .take()
            .expect("runtime artifact stage descriptor was just installed");
        let active = ActiveStage::register(parent_state.identity, name.clone());
        allocation.armed = false;
        return Ok(StagedRuntimeArtifact {
            root_name: name,
            content_sha256: String::new(),
            materialized_bytes: 0,
            root_parent,
            root_descriptor,
            _active: active,
        });
    }
    bail!("could not allocate a unique runtime artifact stage directory")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_destination_directory(
    stage: &File,
    relative: &Path,
    totals: &mut Totals,
) -> anyhow::Result<File> {
    let mut current = stage.try_clone()?;
    for component in normal_components(relative)? {
        match open_destination_directory(&current, component.as_os_str()) {
            Ok(next) => {
                current = next;
                continue;
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "runtime artifact destination rejects non-directory or symlink component {}",
                        relative.display()
                    )
                });
            }
        }
        totals.reserve_materialized_entry()?;
        let name = cstring_component(component.as_os_str())?;
        let rc = unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "create descriptor-relative runtime artifact directory {}",
                    relative.display()
                )
            });
        }
        current =
            open_destination_directory(&current, component.as_os_str()).with_context(|| {
                format!(
                    "open descriptor-relative runtime artifact directory {}",
                    relative.display()
                )
            })?;
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn open_destination_directory(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
    openat2_raw(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
        RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV,
    )
}

#[cfg(target_os = "macos")]
fn open_destination_directory(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn create_destination_file(
    parent: &File,
    name: &std::ffi::OsStr,
    mode: u32,
) -> std::io::Result<File> {
    openat2_raw(
        parent,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        mode,
        RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV,
    )
}

#[cfg(target_os = "macos")]
fn create_destination_file(
    parent: &File,
    name: &std::ffi::OsStr,
    mode: u32,
) -> std::io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_staged_regular(parent: &File, relative: &Path) -> std::io::Result<File> {
    if relative.as_os_str().is_empty() {
        return Err(std::io::Error::from_raw_os_error(libc::EISDIR));
    }
    openat2_raw(
        parent,
        relative.as_os_str(),
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
        RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV,
    )
}

#[cfg(target_os = "macos")]
fn open_staged_regular(parent: &File, relative: &Path) -> std::io::Result<File> {
    let components = normal_components(relative).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    if components.is_empty() {
        return Err(std::io::Error::from_raw_os_error(libc::EISDIR));
    }
    let mut current = parent.try_clone()?;
    for (index, component) in components.iter().enumerate() {
        let name = CString::new(component.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
        })?;
        let flags = libc::O_RDONLY
            | libc::O_NONBLOCK
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if index + 1 == components.len() {
                0
            } else {
                libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        current = unsafe { File::from_raw_fd(fd) };
    }
    Ok(current)
}

/// Make one already-open file visible to a single exec child without changing
/// the parent's close-on-exec discipline. The returned procfs/devfs path names
/// the exact open file description, not a mutable filesystem pathname.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn inherit_file_path(
    command: &mut tokio::process::Command,
    file: &File,
) -> anyhow::Result<PathBuf> {
    let inherited = file
        .try_clone()
        .context("duplicate descriptor for child inheritance")?;
    let descriptor = inherited.as_raw_fd();
    anyhow::ensure!(descriptor >= 0, "cannot inherit a closed file descriptor");
    unsafe {
        command.pre_exec(move || {
            // The command owns `inherited` until spawn/output completes, so the
            // descriptor cannot be closed and reused between path construction
            // and fork. After fork this exact open file description is owned by
            // the child; clearing CLOEXEC keeps it alive until the exec'd tool
            // has consumed the descriptor-backed argument.
            let _keep_alive = &inherited;
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(target_os = "linux")]
    let path = PathBuf::from(format!("/proc/self/fd/{descriptor}"));
    #[cfg(target_os = "macos")]
    let path = PathBuf::from(format!("/dev/fd/{descriptor}"));
    Ok(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_cleanup_children(directory: &File) -> anyhow::Result<Vec<ChildPreflight>> {
    // `dup`/`File::try_clone` shares the directory stream offset with the held
    // capability. Open `.` relative to it instead so every enumeration starts
    // from a fresh open file description; stale-stage recovery intentionally
    // scans the same parent descriptor across successive bounded passes.
    let duplicate = open_directory_component(directory, OsStr::new("."))?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(std::io::Error::last_os_error()).context("cleanup fdopendir failed");
    }
    let stream = DirectoryStream(stream);
    let mut children = Vec::new();
    loop {
        unsafe {
            set_errno(0);
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let code = unsafe { errno() };
            if code != 0 {
                return Err(std::io::Error::from_raw_os_error(code))
                    .context("descriptor-relative cleanup readdir failed");
            }
            break;
        }
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        anyhow::ensure!(
            children.len() < MAX_ENTRIES as usize + 1,
            "runtime artifact cleanup refuses more than {} entries in one stage",
            MAX_ENTRIES + 1
        );
        let name = OsString::from_vec(bytes.to_vec());
        let state = lstat_at(directory, &name)?;
        children.push(ChildPreflight { name, state });
    }
    children.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(children)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_directory_inode(left: SourceIdentity, right: SourceIdentity) -> bool {
    left.is_directory()
        && right.is_directory()
        && left.device == right.device
        && left.inode == right.inode
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stage_created_ms(name: &OsStr) -> Option<u64> {
    let name = name.to_str()?;
    if let Some(rest) = name.strip_prefix(".runtime-artifact-v2-") {
        let (millis, nonce) = rest.split_once('-')?;
        if nonce.len() != STAGE_NONCE_BYTES * 2
            || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        return millis.parse().ok();
    }
    let rest = name.strip_prefix(".runtime-artifact-")?;
    let mut fields = rest.split('-');
    let pid = fields.next()?;
    let millis = fields.next()?;
    let counter = fields.next()?;
    if fields.next().is_some() || pid.parse::<u32>().is_err() || counter.parse::<u64>().is_err() {
        return None;
    }
    millis.parse().ok()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stage_gc_grace() -> anyhow::Result<Duration> {
    let seconds = match std::env::var("HIVE_RUNTIME_ARTIFACT_STAGE_GC_GRACE_SECS") {
        Ok(value) => value
            .parse::<u64>()
            .context("HIVE_RUNTIME_ARTIFACT_STAGE_GC_GRACE_SECS must be an integer")?,
        Err(_) => DEFAULT_STAGE_GC_GRACE_SECS,
    };
    anyhow::ensure!(
        seconds > 0,
        "HIVE_RUNTIME_ARTIFACT_STAGE_GC_GRACE_SECS must be positive"
    );
    Ok(Duration::from_secs(seconds))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stage_gc_max_reap_fraction() -> anyhow::Result<f64> {
    let fraction = match std::env::var("HIVE_RUNTIME_ARTIFACT_STAGE_GC_MAX_REAP_FRACTION") {
        Ok(value) => value
            .parse::<f64>()
            .context("HIVE_RUNTIME_ARTIFACT_STAGE_GC_MAX_REAP_FRACTION must be a finite number")?,
        Err(_) => DEFAULT_STAGE_GC_MAX_REAP_FRACTION,
    };
    anyhow::ensure!(
        fraction.is_finite() && fraction > 0.0 && fraction <= 0.5,
        "HIVE_RUNTIME_ARTIFACT_STAGE_GC_MAX_REAP_FRACTION must be in (0, 0.5]"
    );
    Ok(fraction)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sweep_stale_stages(parent: &File) -> anyhow::Result<()> {
    let parent_identity = source_state(parent)?.identity;
    let active = active_stages()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter(|key| {
            key.parent_device == parent_identity.device && key.parent_inode == parent_identity.inode
        })
        .map(|key| key.name.clone())
        .collect::<HashSet<_>>();
    anyhow::ensure!(
        !active.is_empty(),
        "runtime artifact stale-stage sweep refuses an empty active keep set"
    );

    let now = SystemTime::now();
    let now_ms = hive_core::now_ms();
    let grace = stage_gc_grace()?;
    let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
    let mut recognized = 0_usize;
    let mut active_seen = 0_usize;
    let mut stale = Vec::new();
    for child in read_cleanup_children(parent)? {
        let Some(created_ms) = stage_created_ms(&child.name) else {
            continue;
        };
        anyhow::ensure!(
            child.state.identity.is_directory(),
            "runtime artifact stage-shaped cache entry is not a directory: {}",
            child.name.to_string_lossy()
        );
        recognized += 1;
        if active.contains(&child.name) {
            active_seen += 1;
            continue;
        }
        if now_ms.saturating_sub(created_ms) < grace_ms {
            continue;
        }
        let directory = open_destination_directory(parent, &child.name)
            .context("open stale runtime artifact stage candidate")?;
        ensure_same_state(
            child.state,
            source_state(&directory)?,
            Path::new(&child.name),
        )?;
        let modified = directory.metadata()?.modified()?;
        if now.duration_since(modified).unwrap_or(Duration::ZERO) < grace {
            continue;
        }
        stale.push((created_ms, child.name, child.state));
    }
    anyhow::ensure!(
        active_seen == active.len(),
        "runtime artifact stale-stage sweep could not prove its complete active keep set"
    );
    anyhow::ensure!(
        recognized > 0,
        "runtime artifact stale-stage sweep refuses an empty recognized set"
    );

    stale.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let max_reap = ((recognized as f64) * stage_gc_max_reap_fraction()?).floor() as usize;
    if stale.len() > max_reap {
        tracing::warn!(
            stale = stale.len(),
            recognized,
            max_reap,
            "runtime artifact stale-stage recovery bounded this pass"
        );
    }
    for (_, name, expected) in stale.into_iter().take(max_reap) {
        let descriptor = open_destination_directory(parent, &name)
            .context("reopen stale runtime artifact stage candidate")?;
        ensure_same_state(expected, source_state(&descriptor)?, Path::new(&name))?;
        remove_exact_stage(parent, &name, &descriptor)?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove_directory_contents(
    directory: &File,
    remaining: &mut u64,
    depth: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        depth <= MAX_DEPTH,
        "runtime artifact cleanup refuses a stage deeper than {MAX_DEPTH} components"
    );
    let children = read_cleanup_children(directory)?;
    for child in children {
        anyhow::ensure!(
            *remaining > 0,
            "runtime artifact cleanup refuses more than {} entries in one stage",
            MAX_ENTRIES + 1
        );
        *remaining -= 1;
        let name = cstring_component(&child.name)?;
        if child.state.identity.is_directory() {
            let nested = open_destination_directory(directory, &child.name)?;
            ensure_same_state(child.state, source_state(&nested)?, Path::new(&child.name))?;
            remove_directory_contents(&nested, remaining, depth + 1)?;
            let rc =
                unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("remove staged runtime artifact directory");
            }
        } else {
            let rc = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("remove staged runtime artifact entry");
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove_exact_stage(parent: &File, name: &OsStr, descriptor: &File) -> anyhow::Result<()> {
    let held = source_state(descriptor)?.identity;
    let named = lstat_at(parent, name)?.identity;
    anyhow::ensure!(
        same_directory_inode(held, named),
        "runtime artifact stage identity changed before cleanup: {}",
        name.to_string_lossy()
    );
    let mut remaining = MAX_ENTRIES + 1;
    remove_directory_contents(descriptor, &mut remaining, 0)?;
    let held_after = source_state(descriptor)?.identity;
    let named_after = lstat_at(parent, name)?.identity;
    anyhow::ensure!(
        same_directory_inode(held, held_after) && same_directory_inode(held, named_after),
        "runtime artifact stage identity changed during cleanup: {}",
        name.to_string_lossy()
    );
    unlink_directory_at(parent, name)?;
    parent
        .sync_all()
        .context("sync runtime artifact stage parent after cleanup")?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unlink_directory_at(parent: &File, name: &OsStr) -> anyhow::Result<()> {
    let name = cstring_component(name)?;
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("remove runtime artifact stage root");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cstring_component(name: &std::ffi::OsStr) -> anyhow::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| anyhow::anyhow!("runtime artifact path contains NUL"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn normal_components(path: &Path) -> anyhow::Result<Vec<OsString>> {
    path.components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_os_string()),
            Component::CurDir => Ok(OsString::new()),
            _ => Err(anyhow::anyhow!(
                "runtime artifact path is not normalized: {}",
                path.display()
            )),
        })
        .filter(|component| component.as_ref().map_or(true, |name| !name.is_empty()))
        .collect()
}

fn normalized_relative(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    validate_relative(path, label)?;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(name) = component {
            normalized.push(name);
        }
    }
    Ok(normalized)
}

fn validate_relative(path: &Path, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path.is_absolute(),
        "runtime artifact {label} must be relative"
    );
    for component in path.components() {
        anyhow::ensure!(
            matches!(component, Component::Normal(_) | Component::CurDir),
            "runtime artifact {label} contains traversal: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_output_path(path: &Path, depth: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        depth <= MAX_DEPTH,
        "runtime artifact path exceeds depth {MAX_DEPTH}: {}",
        path.display()
    );
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    };
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().len();
    anyhow::ensure!(
        bytes <= MAX_PATH_BYTES,
        "runtime artifact path exceeds {MAX_PATH_BYTES} bytes"
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hash_entry_header(hasher: &mut Sha256, kind: u8, path: &Path, mode: u32, length: u64) {
    let bytes = path.as_os_str().as_bytes();
    hasher.update([kind]);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    hasher.update((mode & 0o777).to_le_bytes());
    hasher.update(length.to_le_bytes());
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn excluded(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        matches!(
            name.to_str(),
            Some(
                ".git"
                    | ".gitignore"
                    | ".gitattributes"
                    | ".gitmodules"
                    | ".hg"
                    | ".svn"
                    | ".npmrc"
                    | ".pnpmfile.cjs"
                    | ".yarnrc"
                    | ".yarnrc.yml"
                    | ".netrc"
                    | ".gitconfig"
                    | ".git-credentials"
                    | ".npm"
                    | ".pnpm-store"
                    | ".yarn"
                    | ".cache"
                    | ".turbo"
                    | ".aws"
                    | ".ssh"
                    | ".docker"
                    | ".pypirc"
                    | "pip.conf"
                    | "auth.json"
                    | "NuGet.Config"
                    | ".bunfig.toml"
            )
        ) || name
            .to_str()
            .is_some_and(|name| name == ".env" || name.starts_with(".env."))
    })
}
