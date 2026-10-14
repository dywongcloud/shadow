//! Litebox cell backend: a cell is one host process run under Microsoft's LiteBox, an unprivileged
//! Linux syscall-interception sandbox. It serves nodes where neither Firecracker nor the PVM
//! fallback can run; an operator opts a node in (`HIVE_LITEBOX_VERIFIED`, licensed by a passing
//! [`LiteboxBackend::smoke_test`]), it is never detected.
//!
//! ## Scope
//!
//! `start_function` and the sandbox `exec_command`/`exec_pty` paths run inside litebox. `run_build`
//! is a plain host process (`crate::mock::run_build_process`), and `__container__` cells bypass
//! litebox for host podman (`crate::podman_run_container`).
//!
//! ## Mechanism
//!
//! The runner `litebox_runner_linux_userland` is a separate binary (`-Z --rewrite-syscalls
//! --forward-env`), never a Cargo dependency; `ansible/roles/litebox` builds it with the patches
//! described in `files/PATCHES.md`. Guest and runner are one process: every `syscall` opcode in the
//! target ELF is rewritten ahead of exec to trap into litebox's dispatcher, with a seccomp-bpf
//! allowlist at the kernel boundary.
//!
//! ## Guest filesystem
//!
//! The guest sees nothing of the host except what `--initial-files=<tar>` stages: the program's
//! `ldd` closure and the delivered application tree under `/workspace`. The runner's cwd is `/` and
//! cannot be set, so `start_function` passes each main entry as an absolute guest path validated
//! against the immutable app archive, and the preload changes into the validated workdir before
//! tenant code runs.
//!
//! ## Networking
//!
//! A TUN link cannot carry loopback and litebox's guest address was compiled in, so every runner
//! owns a TUN device with its own `/30` ([`LiteboxNet`]): the runner is told the pair through
//! `LITEBOX_GUEST_IP`/`LITEBOX_GATEWAY_IP` (`networking.patch`) and the function tunnel dials the
//! guest address directly, with no NAT or namespace. A wildcard bind is reachable as is;
//! `litebox-bind-shim.js` (preloaded through `NODE_OPTIONS=--require`, Node's `net.Server` only)
//! rewrites an explicit loopback bind to the guest address.
//!
//! ## Security posture
//!
//! Guest and enforcement share one address space, so this is not Firecracker or gVisor isolation:
//! upstream does not treat the rewriter as a security boundary, and syscalls generated at runtime
//! (V8's JIT) escape it, leaving the seccomp-bpf allowlist as the only backstop. Never substitute
//! it silently for Firecracker capability: selecting it stays a deliberate per-node operator
//! decision.

use crate::{
    CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink, SealedRuntimeArtifact,
};
use anyhow::Context as _;
use async_trait::async_trait;
use hive_core::{now_ms, BuildJob, BuildResult, CellId, RuntimeArtifactIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read as StdRead, Seek as StdSeek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
#[cfg(unix)]
use std::{
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Debug)]
pub struct LiteboxConfig {
    /// Prebuilt `litebox_runner_linux_userland` binary.
    pub runner_bin: PathBuf,
    /// Ephemeral per-cell scratch and probe files; durable archives live under `cache_root`, never
    /// here.
    pub root: PathBuf,
    /// Durable artifact store (`litebox-images-v1`: app and runtime archives, image references) and
    /// build output: a reboot or tmp sweep must not delete what the replicated deployment record
    /// still names.
    pub cache_root: PathBuf,
    /// Simulated cold-provision latency, as in `MockConfig`; the real provision cost is a directory
    /// create (default 0).
    pub provision_latency: Duration,
}

impl Default for LiteboxConfig {
    fn default() -> Self {
        let runner_bin = std::env::var("HIVE_LITEBOX_RUNNER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/litebox-runner"));
        LiteboxConfig {
            runner_bin,
            root: std::env::temp_dir().join("hive-litebox-cells"),
            cache_root: std::env::var("HIVE_LITEBOX_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(
                        std::env::var("HIVE_DATA").unwrap_or_else(|_| "/var/lib/hive".to_string()),
                    )
                    .join("litebox-cache")
                }),
            provision_latency: Duration::from_millis(0),
        }
    }
}

/// One runner's TUN link: a device plus its own `/30` out of `10.88.0.0/16`. The host routes to the
/// guest end directly (no NAT or namespace); the runner learns the pair from
/// `LITEBOX_GUEST_IP`/`LITEBOX_GATEWAY_IP`, which `networking.patch` adds.
#[derive(Clone)]
struct LiteboxNet {
    tun_dev: String,
    /// Host end of the link: the guest's gateway (`LITEBOX_GATEWAY_IP`).
    host_ip: String,
    /// Guest end of the link: what the function tunnel dials, routable from the host as is
    /// (`LITEBOX_GUEST_IP`).
    guest_ip: String,
}

struct LiteboxLinkRollback {
    tun_dev: String,
    armed: bool,
}

impl LiteboxLinkRollback {
    fn new(tun_dev: String) -> Self {
        Self {
            tun_dev,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiteboxLinkRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let tun_dev = self.tun_dev.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                delete_litebox_link(&tun_dev).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                delete_litebox_link(&tun_dev).await;
            });
        }
    }
}

struct LiteboxProvisionGuard {
    id: CellId,
    root: PathBuf,
    cell_nets: Arc<AsyncMutex<HashMap<CellId, LiteboxNet>>>,
    armed: bool,
}

impl LiteboxProvisionGuard {
    fn new(
        id: CellId,
        root: PathBuf,
        cell_nets: Arc<AsyncMutex<HashMap<CellId, LiteboxNet>>>,
    ) -> Self {
        Self {
            id,
            root,
            cell_nets,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiteboxProvisionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let id = self.id.clone();
        let root = self.root.clone();
        let cell_nets = self.cell_nets.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let net = cell_nets.lock().await.remove(&id);
                if let Some(net) = net {
                    delete_litebox_link(&net.tun_dev).await;
                }
                let _ = tokio::fs::remove_dir_all(root).await;
            });
        }
    }
}

async fn delete_litebox_link(tun_dev: &str) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {tun_dev} 2>/dev/null"
        ))
        .kill_on_drop(true)
        .status()
        .await;
}

/// Recreates `tun_dev` from scratch. The leading `ip link del` of a device that does not exist yet
/// exits non-zero, so the script must not run under `set -e`: only the `&&` chain after it decides
/// the exit status.
fn create_link_script(tun_dev: &str, host_ip: &str) -> String {
    format!(
        "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; \
             ip link del {tun_dev} 2>/dev/null; \
             ip tuntap add dev {tun_dev} mode tun && \
             ip addr add {host_ip}/30 dev {tun_dev} && \
             ip link set {tun_dev} up"
    )
}

/// A function cell's runner: guest and runner are one host process, killed on `terminate`.
struct LiteboxFunctionProcess {
    child: tokio::process::Child,
    /// Held open until the runner is reaped: keeps the archive descriptor the child inherited from
    /// being reused parent-side.
    _initial_files: File,
}

/// One in-flight sandbox exec or interactive shell. The runner leads its own process group, so one
/// `killpg(pgid)` (from `kill_exec` or `terminate`) ends the whole guest tree; the waiter task
/// removes the entry when the runner is reaped, so a late kill is a no-op.
#[derive(Clone)]
struct LiteboxExec {
    cell: CellId,
    pgid: i32,
}

pub struct LiteboxBackend {
    cfg: LiteboxConfig,
    funcs: Arc<AsyncMutex<HashMap<CellId, LiteboxFunctionProcess>>>,
    /// Live sandbox execs keyed by `ExecRequest.id` — see [`LiteboxExec`].
    execs: Arc<AsyncMutex<HashMap<String, LiteboxExec>>>,
    /// Per-cell tunnel-server accept loops, aborted on terminate.
    tunnels: Arc<AsyncMutex<HashMap<CellId, tokio::task::JoinHandle<()>>>>,
    /// Container cells bypass litebox and run through the shared owned launch path.
    containers: Arc<AsyncMutex<HashMap<CellId, crate::ContainerLaunch>>>,
    /// Set up in `provision`, torn down in `terminate`; absent for container cells.
    cell_nets: Arc<AsyncMutex<HashMap<CellId, LiteboxNet>>>,
    /// Monotonic allocator of `/30` link slots (`LINK_SLOTS`, same derivation as
    /// `FirecrackerBackend::net_idx`); it wraps, since a slot only collides with a link that is
    /// somehow still alive.
    net_idx: Arc<std::sync::atomic::AtomicUsize>,
    /// Serializes artifact and reference publication and reaping, so one immutable generation is
    /// never written and reaped at once; request-driven acquisition stays outside it.
    artifact_lock: Arc<AsyncMutex<()>>,
    /// Memo of app archives already hash-verified against their committed identity, keyed by image
    /// (one entry per launched image); [`VerifiedArchive`] says what makes a hit safe.
    verified_archives: Arc<std::sync::Mutex<HashMap<String, VerifiedArchive>>>,
    sampler: Arc<crate::CpuSampler>,
}

/// Proof that an app archive's bytes matched its committed SHA-256, taken against one file
/// identity: a hit needs the same reference bytes, the same dev/ino/len/mtime, the same expected
/// SHA and an age under [`VERIFIED_ARCHIVE_TTL_MS`]. The content comparison is remembered, never
/// skipped.
struct VerifiedArchive {
    reference_sha256: String,
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
    archive_sha256: String,
    verified_at_ms: u64,
}

const VERIFIED_ARCHIVE_TTL_MS: u64 = 300_000;

/// Node bind-rewrite shim, embedded so the runtime binary needs no separate deploy step;
/// `append_litebox_runtime_augmentation` writes it into the guest tar.
const NODE_BIND_SHIM_JS: &str = include_str!("litebox-bind-shim.js");
/// Where `NODE_OPTIONS=--require` finds the shim inside the guest (the tar root); the host scratch
/// path is invisible to the guest.
const GUEST_BIND_SHIM_PATH: &str = "/hive-litebox-bind-shim.js";
/// Tar-layer directory the probe's guest must be able to write into, under the delivered workdir so
/// it exercises the path shape of an application's own `mkdir` (`network_smoke_test_inner`, check
/// (c)).
const PROBE_WRITE_DIR: &str = "/workspace/hive-write-probe";
const GUEST_RUNTIME_GUARD_PATH: &str = "/hive-litebox-runtime-guard.js";
const INITIAL_FILES_ALIAS_NAME: &str = "initial-files.tar";
const DELIVERED_WORKDIR: &str = "/workspace";
const LITEBOX_ARTIFACT_SCHEMA: u16 = 1;
const RUNTIME_GUARD_EXIT_CODE: i32 = 78;
const MAX_REFERENCE_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_JSON_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_SCRIPTS: usize = 256;
const MAX_PACKAGE_SCRIPT_BYTES: usize = 16 * 1024;
const MAX_GUEST_ENV_BYTES: usize = 1024 * 1024;
const DEFAULT_ARTIFACT_GC_GRACE_SECS: u64 = 600;
const DEFAULT_ARTIFACT_GC_MAX_REAP_FRACTION: f64 = 0.5;
/// `/30` links in `10.88.0.0/16` (65536 addresses, four per link): the slot space `allocate_link`
/// wraps within.
const LINK_SLOTS: usize = 16384;
/// Concurrent local connects per guest address, process-wide (`fluid_tunnel`): the guest's network
/// stack re-arms one listener serially, so connects queue at the tunnel instead of being
/// accept-closed by the guest.
const GUEST_LOCAL_CONNECT_PERMITS: usize = 1;
/// Archive hashing slower than this is logged: a cold start pays it once per deployment, node and
/// TTL instead of once per launch.
const SLOW_ARCHIVE_VERIFY_MS: u128 = 50;
/// `tar` flag that stores what a symlink points at: a SONAME is very often a symlink.
const TAR_DEREFERENCE_FLAG: &str = "-h";

/// Guest-side half of the artifact handshake, run before tenant code in Node and Bun: it exits with
/// `RUNTIME_GUARD_EXIT_CODE` unless the mounted marker matches the host's protocol, id and content
/// hash.
const RUNTIME_GUARD_JS: &str = r#"'use strict';
const fs = require('fs');
function refuse(message) {
  try { process.stderr.write(`litebox runtime artifact refusal: ${message}\n`); } catch (_) {}
  process.exit(78);
}
try {
  const marker = JSON.parse(fs.readFileSync('/workspace/.hive-runtime-artifact-v1.json', 'utf8'));
  const protocol = Number(process.env.HIVE_RUNTIME_ARTIFACT_PROTOCOL);
  if (marker.protocol !== protocol ||
      marker.id !== process.env.HIVE_RUNTIME_ARTIFACT_ID ||
      marker.content_sha256 !== process.env.HIVE_RUNTIME_ARTIFACT_SHA256) {
    refuse('mounted identity does not match the host expectation');
  }
} catch (error) {
  refuse(error && error.message ? error.message : String(error));
}
require('/hive-litebox-bind-shim.js');
"#;

static ARTIFACT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeArchiveReference {
    source_sha256: String,
    archive_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiteboxImageReference {
    schema: u16,
    image: String,
    identity: RuntimeArtifactIdentity,
    guest_workdir: String,
    app_archive_sha256: String,
    #[serde(default)]
    package_scripts: BTreeMap<String, String>,
    #[serde(default)]
    next_entry: Option<String>,
    #[serde(default)]
    runtimes: BTreeMap<String, RuntimeArchiveReference>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectRuntime {
    Node,
    Bun,
}

struct DirectLaunch {
    runtime: DirectRuntime,
    bin: PathBuf,
    args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ArtifactArea {
    Apps,
    Runtimes,
    Temporary,
    Staging,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CacheEntryState {
    device: u64,
    inode: u64,
    mode: u32,
    length: u64,
    modified: SystemTime,
    modified_nanos: i64,
}

impl CacheEntryState {
    fn is_regular(self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
    }

    fn is_directory(self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

struct ArtifactDirectory {
    path: PathBuf,
    descriptor: File,
    identity: DirectoryIdentity,
}

impl ArtifactDirectory {
    fn verify_binding(&self) -> anyhow::Result<()> {
        let current = open_directory_tree(&self.path, false)?;
        anyhow::ensure!(
            current.identity == self.identity,
            "litebox artifact directory identity changed at {}",
            self.path.display()
        );
        Ok(())
    }

    #[cfg(unix)]
    fn open_regular(&self, name: &OsStr) -> std::io::Result<File> {
        let name = cache_component(name)?;
        let fd = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache entry is not a regular file",
            ));
        }
        Ok(file)
    }

    #[cfg(unix)]
    fn create_file(&self, name: &OsStr, mode: u32) -> std::io::Result<File> {
        let name = cache_component(name)?;
        let fd = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(unix)]
    fn child_names(&self) -> anyhow::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(descriptor_directory_path(&self.descriptor))? {
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }
}

struct ArtifactDirectories {
    images: ArtifactDirectory,
    apps: ArtifactDirectory,
    runtimes: ArtifactDirectory,
    references: ArtifactDirectory,
    staging: ArtifactDirectory,
    temporary: ArtifactDirectory,
}

impl ArtifactDirectories {
    fn prepare(cache_root: &Path) -> anyhow::Result<Self> {
        let cache = open_directory_tree(cache_root, true)?;
        let images = open_or_create_directory(&cache, OsStr::new("litebox-images-v1"))?;
        let apps = open_or_create_directory(&images, OsStr::new("apps"))?;
        let runtimes = open_or_create_directory(&images, OsStr::new("runtimes"))?;
        let references = open_or_create_directory(&images, OsStr::new("refs"))?;
        let staging = open_or_create_directory(&images, OsStr::new(".artifact-staging"))?;
        let temporary = open_or_create_directory(&images, OsStr::new(".tmp"))?;
        let directories = Self {
            images,
            apps,
            runtimes,
            references,
            staging,
            temporary,
        };
        directories.verify_bindings()?;
        Ok(directories)
    }

    fn verify_bindings(&self) -> anyhow::Result<()> {
        self.images.verify_binding()?;
        self.apps.verify_binding()?;
        self.runtimes.verify_binding()?;
        self.references.verify_binding()?;
        self.staging.verify_binding()?;
        self.temporary.verify_binding()?;
        Ok(())
    }
}

struct ArtifactTemp {
    parent: File,
    name: OsString,
    file: File,
    armed: bool,
}

impl ArtifactTemp {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ArtifactTemp {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Ok(name) = cache_component(&self.name) {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }
}

struct ArtifactScratchAllocation {
    parent: File,
    name: OsString,
    armed: bool,
}

impl Drop for ArtifactScratchAllocation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Ok(name) = cache_component(&self.name) {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
            }
        }
    }
}

struct ArtifactScratch {
    parent: File,
    name: OsString,
    directory: File,
    children: Vec<OsString>,
}

impl Drop for ArtifactScratch {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            for child in &self.children {
                if let Ok(name) = cache_component(child) {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
                    }
                }
            }
            if let Ok(name) = cache_component(&self.name) {
                unsafe {
                    libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
            }
        }
    }
}

#[cfg(unix)]
fn cache_component(name: &OsStr) -> std::io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid litebox cache path component",
        ));
    }
    CString::new(bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(unix)]
fn directory_identity(file: &File) -> anyhow::Result<DirectoryIdentity> {
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_dir(),
        "litebox cache descriptor is not a directory"
    );
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &OsStr) -> std::io::Result<File> {
    let name = cache_component(name)?;
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

#[cfg(unix)]
fn open_directory_tree(path: &Path, create: bool) -> anyhow::Result<ArtifactDirectory> {
    anyhow::ensure!(
        path.is_absolute(),
        "litebox artifact cache root must be an absolute path: {}",
        path.display()
    );
    let mut descriptor = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut walked = PathBuf::from("/");
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => anyhow::bail!(
                "litebox artifact cache path is not normalized: {}",
                path.display()
            ),
        };
        walked.push(name);
        let next = match open_directory_at(&descriptor, name) {
            Ok(next) => next,
            Err(error) if create && error.raw_os_error() == Some(libc::ENOENT) => {
                let name_c = cache_component(name)?;
                let rc = unsafe { libc::mkdirat(descriptor.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if rc < 0 {
                    let mkdir_error = std::io::Error::last_os_error();
                    if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(mkdir_error).with_context(|| {
                            format!("create litebox artifact directory {}", walked.display())
                        });
                    }
                } else {
                    descriptor.sync_all().with_context(|| {
                        format!(
                            "sync parent after creating litebox artifact directory {}",
                            walked.display()
                        )
                    })?;
                }
                open_directory_at(&descriptor, name).with_context(|| {
                    format!(
                        "open newly-created litebox artifact directory {}",
                        walked.display()
                    )
                })?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "open no-follow litebox artifact directory {}",
                        walked.display()
                    )
                })
            }
        };
        descriptor = next;
    }
    let identity = directory_identity(&descriptor)?;
    Ok(ArtifactDirectory {
        path: path.to_path_buf(),
        descriptor,
        identity,
    })
}

#[cfg(not(unix))]
fn open_directory_tree(_path: &Path, _create: bool) -> anyhow::Result<ArtifactDirectory> {
    anyhow::bail!("litebox artifact cache requires no-follow Unix directory descriptors")
}

#[cfg(unix)]
fn open_or_create_directory(
    parent: &ArtifactDirectory,
    name: &OsStr,
) -> anyhow::Result<ArtifactDirectory> {
    let path = parent.path.join(name);
    let descriptor = match open_directory_at(&parent.descriptor, name) {
        Ok(descriptor) => descriptor,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            let name_c = cache_component(name)?;
            let rc =
                unsafe { libc::mkdirat(parent.descriptor.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if rc < 0 {
                let mkdir_error = std::io::Error::last_os_error();
                if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(mkdir_error).with_context(|| {
                        format!("create litebox artifact directory {}", path.display())
                    });
                }
            } else {
                parent.descriptor.sync_all().with_context(|| {
                    format!(
                        "sync parent after creating litebox artifact directory {}",
                        path.display()
                    )
                })?;
            }
            open_directory_at(&parent.descriptor, name).with_context(|| {
                format!(
                    "open newly-created litebox artifact directory {}",
                    path.display()
                )
            })?
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "open no-follow litebox artifact directory {}",
                    path.display()
                )
            })
        }
    };
    let identity = directory_identity(&descriptor)?;
    anyhow::ensure!(
        identity.device == parent.identity.device,
        "litebox artifact child directory crosses a filesystem boundary at {}",
        path.display()
    );
    Ok(ArtifactDirectory {
        path,
        descriptor,
        identity,
    })
}

#[cfg(not(unix))]
fn open_or_create_directory(
    _parent: &ArtifactDirectory,
    _name: &OsStr,
) -> anyhow::Result<ArtifactDirectory> {
    anyhow::bail!("litebox artifact cache requires no-follow Unix directory descriptors")
}

#[cfg(target_os = "linux")]
fn descriptor_directory_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "macos")]
fn descriptor_directory_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

fn read_bounded_file(mut file: File, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file(),
        "litebox cache entry is not a regular file"
    );
    anyhow::ensure!(
        before.len() <= max_bytes,
        "litebox cache entry exceeds {max_bytes} bytes"
    );
    let length = usize::try_from(before.len()).context("litebox cache entry is too large")?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    let mut extra = [0_u8; 1];
    anyhow::ensure!(
        file.read(&mut extra)? == 0,
        "litebox cache entry grew while reading"
    );
    let after = file.metadata()?;
    #[cfg(unix)]
    anyhow::ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec(),
        "litebox cache entry identity changed while reading"
    );
    Ok(bytes)
}

async fn sha256_open_file(file: &File) -> anyhow::Result<String> {
    let file = file.try_clone()?;
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let before = file.metadata()?;
        anyhow::ensure!(before.is_file(), "litebox artifact is not a regular file");
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 128 * 1024];
        let mut offset = 0_u64;
        while offset < before.len() {
            let take = (before.len() - offset).min(buffer.len() as u64) as usize;
            let mut filled = 0usize;
            while filled < take {
                let read = file.read_at(&mut buffer[filled..take], offset + filled as u64)?;
                anyhow::ensure!(read > 0, "litebox artifact shrank while hashing");
                filled += read;
            }
            hasher.update(&buffer[..take]);
            offset += take as u64;
        }
        let mut extra = [0_u8; 1];
        anyhow::ensure!(
            file.read_at(&mut extra, before.len())? == 0,
            "litebox artifact grew while hashing"
        );
        let after = file.metadata()?;
        #[cfg(unix)]
        anyhow::ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "litebox artifact identity changed while hashing"
        );
        Ok(hex_sha256(&hasher.finalize()))
    })
    .await
    .context("litebox artifact hashing task failed")?
}

/// File identity as `sha256_open_file` validates it: dev/ino/len/mtime. `None`
/// when the file cannot be stat'd (caller must then hash).
fn archive_stamp(path: &std::path::Path) -> Option<(u64, u64, u64, i128)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((
        m.dev(),
        m.ino(),
        m.len(),
        (m.mtime() as i128) * 1_000_000_000i128 + (m.mtime_nsec() as i128),
    ))
}

fn verified_archive_hit(
    memo: &std::sync::Mutex<HashMap<String, VerifiedArchive>>,
    image: &str,
    reference_sha256: &str,
    stamp: Option<(u64, u64, u64, i128)>,
    expected: &str,
    now_ms: u64,
) -> bool {
    let Some((dev, ino, len, mtime_ns)) = stamp else {
        return false;
    };
    let Ok(m) = memo.lock() else { return false };
    match m.get(image) {
        Some(v) => {
            v.reference_sha256 == reference_sha256
                && v.archive_sha256 == expected
                && v.dev == dev
                && v.ino == ino
                && v.len == len
                && v.mtime_ns == mtime_ns
                && now_ms.saturating_sub(v.verified_at_ms) < VERIFIED_ARCHIVE_TTL_MS
        }
        None => false,
    }
}

fn forget_verified_archive(memo: &std::sync::Mutex<HashMap<String, VerifiedArchive>>, image: &str) {
    if let Ok(mut m) = memo.lock() {
        m.remove(image);
    }
}

fn remember_verified_archive(
    memo: &std::sync::Mutex<HashMap<String, VerifiedArchive>>,
    image: &str,
    reference_sha256: &str,
    stamp: Option<(u64, u64, u64, i128)>,
    expected: &str,
    now_ms: u64,
) {
    let Some((dev, ino, len, mtime_ns)) = stamp else {
        return;
    };
    if let Ok(mut m) = memo.lock() {
        m.insert(
            image.to_string(),
            VerifiedArchive {
                reference_sha256: reference_sha256.to_string(),
                dev,
                ino,
                len,
                mtime_ns,
                archive_sha256: expected.to_string(),
                verified_at_ms: now_ms,
            },
        );
    }
}

/// Open an app archive whose bytes match the committed identity, re-hashing only when the memo
/// cannot vouch for this exact file; the comparison is never skipped, only its recomputation.
async fn verified_app_archive(
    memo: &std::sync::Mutex<HashMap<String, VerifiedArchive>>,
    image: &str,
    directory: &ArtifactDirectory,
    reference: &LiteboxImageReference,
) -> anyhow::Result<File> {
    let expected = reference.app_archive_sha256.as_str();
    let reference_sha256 = {
        use sha2::Digest as _;
        let canonical = serde_json::to_vec(reference)
            .context("serialize litebox image reference for the verify memo")?;
        format!("{:x}", sha2::Sha256::digest(&canonical))
    };
    let name = LiteboxBackend::app_archive_name(expected);
    let stamp = archive_stamp(&directory.path.join(&name));
    let now_ms = hive_core::now_ms();
    if verified_archive_hit(memo, image, &reference_sha256, stamp, expected, now_ms) {
        return directory.open_regular(&name).map_err(|error| {
            anyhow::anyhow!(
                "open immutable litebox artifact {}/{}: {error}",
                directory.path.display(),
                name.to_string_lossy()
            )
        });
    }
    forget_verified_archive(memo, image);
    let started = std::time::Instant::now();
    let file = verify_immutable_open(directory, &name, expected).await?;
    let verify_ms = started.elapsed().as_millis();
    remember_verified_archive(memo, image, &reference_sha256, stamp, expected, now_ms);
    if verify_ms >= SLOW_ARCHIVE_VERIFY_MS {
        tracing::info!(
            image = %image,
            bytes = stamp.map(|(_, _, len, _)| len).unwrap_or(0),
            verify_ms,
            "litebox: hash-verified the app archive"
        );
    }
    Ok(file)
}

async fn verify_immutable_open(
    directory: &ArtifactDirectory,
    name: &OsStr,
    expected_sha256: &str,
) -> anyhow::Result<File> {
    anyhow::ensure!(valid_sha256(expected_sha256), "invalid expected SHA-256");
    let file = directory.open_regular(name).with_context(|| {
        format!(
            "open immutable litebox artifact {}/{}",
            directory.path.display(),
            name.to_string_lossy()
        )
    })?;
    let actual = sha256_open_file(&file).await?;
    anyhow::ensure!(
        actual == expected_sha256,
        "artifact SHA-256 mismatch at {}/{} (got {actual}, expected {expected_sha256})",
        directory.path.display(),
        name.to_string_lossy()
    );
    Ok(file)
}

#[cfg(unix)]
fn rename_cache_entry(
    source_parent: &File,
    source_name: &OsStr,
    destination_parent: &File,
    destination_name: &OsStr,
) -> anyhow::Result<()> {
    let source_name = cache_component(source_name)?;
    let destination_name = cache_component(destination_name)?;
    let rc = unsafe {
        libc::renameat(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("rename litebox cache entry");
    }
    Ok(())
}

#[cfg(unix)]
fn link_cache_entry(
    source_parent: &File,
    source_name: &OsStr,
    destination_parent: &File,
    destination_name: &OsStr,
) -> std::io::Result<()> {
    let source_name = cache_component(source_name)?;
    let destination_name = cache_component(destination_name)?;
    let rc = unsafe {
        libc::linkat(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            0,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Hard-link the exact inode `source` names, never its mutable cache pathname: `/proc/self/fd/<fd>`
/// plus `AT_SYMLINK_FOLLOW` is the unprivileged `linkat` form.
#[cfg(target_os = "linux")]
fn link_open_file_alias(
    source: &File,
    destination_parent: &File,
    destination_name: &OsStr,
) -> std::io::Result<()> {
    let source_path = CString::new(format!("/proc/self/fd/{}", source.as_raw_fd()))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination_name = cache_component(destination_name)?;
    let rc = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            source_path.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn link_open_file_alias(
    _source: &File,
    _destination_parent: &File,
    _destination_name: &OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "descriptor-bound Litebox archive aliases require Linux procfs",
    ))
}

#[cfg(unix)]
fn unlink_cache_entry(parent: &File, name: &OsStr, directory: bool) -> anyhow::Result<()> {
    let name = cache_component(name)?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("unlink litebox cache entry");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn stat_mtime_nanos(stat: &libc::stat) -> i64 {
    stat.st_mtime_nsec
}

#[cfg(target_os = "macos")]
fn stat_mtime_nanos(stat: &libc::stat) -> i64 {
    stat.st_mtime_nsec
}

#[cfg(unix)]
fn cache_entry_state(
    directory: &ArtifactDirectory,
    name: &OsStr,
) -> anyhow::Result<CacheEntryState> {
    let name = cache_component(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            directory.descriptor.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("lstat litebox cache entry");
    }
    let stat = unsafe { stat.assume_init() };
    Ok(CacheEntryState {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        mode: stat.st_mode as u32,
        length: u64::try_from(stat.st_size).unwrap_or(0),
        modified: SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(stat.st_mtime).unwrap_or(0)),
        modified_nanos: stat_mtime_nanos(&stat),
    })
}

fn artifact_area_directory(
    directories: &ArtifactDirectories,
    area: ArtifactArea,
) -> &ArtifactDirectory {
    match area {
        ArtifactArea::Apps => &directories.apps,
        ArtifactArea::Runtimes => &directories.runtimes,
        ArtifactArea::Temporary => &directories.temporary,
        ArtifactArea::Staging => &directories.staging,
    }
}

#[cfg(unix)]
fn remove_cache_entry(
    directory: &ArtifactDirectory,
    name: &OsStr,
    expected: CacheEntryState,
) -> anyhow::Result<()> {
    let current = cache_entry_state(directory, name)?;
    anyhow::ensure!(
        current == expected,
        "litebox artifact GC entry changed between scan and removal: {}/{}",
        directory.path.display(),
        name.to_string_lossy()
    );
    let kind = current.mode & libc::S_IFMT as u32;
    if current.is_directory() {
        let nested_file = open_directory_at(&directory.descriptor, name)?;
        let nested = ArtifactDirectory {
            path: directory.path.join(name),
            identity: directory_identity(&nested_file)?,
            descriptor: nested_file,
        };
        anyhow::ensure!(
            nested.identity.device == current.device && nested.identity.inode == current.inode,
            "litebox artifact GC directory changed while opening {}/{}",
            directory.path.display(),
            name.to_string_lossy()
        );
        remove_cache_directory_contents(&nested)?;
        unlink_cache_entry(&directory.descriptor, name, true)?;
    } else if kind == libc::S_IFREG as u32 || kind == libc::S_IFLNK as u32 {
        unlink_cache_entry(&directory.descriptor, name, false)?;
    } else {
        anyhow::bail!(
            "litebox artifact GC refuses special cache entry {}/{}",
            directory.path.display(),
            name.to_string_lossy()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn remove_cache_directory_contents(directory: &ArtifactDirectory) -> anyhow::Result<()> {
    for name in directory.child_names()? {
        let state = cache_entry_state(directory, &name)?;
        remove_cache_entry(directory, &name, state)?;
    }
    Ok(())
}

/// Entries planted in the probe's read-only tar layer, shaped like a deployment's own `/workspace`
/// tree and 0o444 like every platform entry: `.keep` is overwritten in place (the copy-up path),
/// `seed.json` is replaced by write-tmp-then-rename (the rename-over-lower path).
fn probe_write_entries() -> Vec<(PathBuf, Vec<u8>)> {
    vec![
        (
            PathBuf::from(PROBE_WRITE_DIR.trim_start_matches('/')).join(".keep"),
            b"hive litebox write probe\n".to_vec(),
        ),
        (
            PathBuf::from(PROBE_WRITE_DIR.trim_start_matches('/')).join("seed.json"),
            b"{\"seed\":true}\n".to_vec(),
        ),
    ]
}

impl LiteboxBackend {
    pub fn new(cfg: LiteboxConfig) -> Self {
        Self::reap_orphaned_runners(&cfg.runner_bin);
        fluid_tunnel::set_local_connect_permits(GUEST_LOCAL_CONNECT_PERMITS);
        LiteboxBackend {
            cfg,
            funcs: Arc::new(AsyncMutex::new(HashMap::new())),
            execs: Arc::new(AsyncMutex::new(HashMap::new())),
            tunnels: Arc::new(AsyncMutex::new(HashMap::new())),
            containers: Arc::new(AsyncMutex::new(HashMap::new())),
            cell_nets: Arc::new(AsyncMutex::new(HashMap::new())),
            net_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            artifact_lock: Arc::new(AsyncMutex::new(())),
            verified_archives: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sampler: Arc::new(crate::CpuSampler::new()),
        }
    }

    /// SIGKILL every process whose executable is this backend's runner binary. Boot-time only:
    /// afterwards live runners belong to this process. Matched by `/proc/<pid>/exe` against the
    /// configured path (never by name), after `strip_deleted_exe_suffix`, so a runner binary swap
    /// does not hide an orphan.
    fn reap_orphaned_runners(runner_bin: &Path) {
        #[cfg(target_os = "linux")]
        {
            let canonical = runner_bin.canonicalize().ok();
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return;
            };
            let mut reaped = 0u32;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == std::process::id() {
                    continue;
                }
                let exe = std::fs::read_link(entry.path().join("exe"))
                    .ok()
                    .map(|p| strip_deleted_exe_suffix(&p));
                let matches = match (&exe, &canonical) {
                    (Some(exe), Some(canonical)) => exe == canonical || exe.as_path() == runner_bin,
                    (Some(exe), None) => exe.as_path() == runner_bin,
                    _ => false,
                };
                if matches {
                    // SAFETY: kill(2) with a specific pid and no pointer args.
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                    reaped += 1;
                }
            }
            if reaped > 0 {
                tracing::warn!(
                    reaped,
                    runner = %runner_bin.display(),
                    "reaped litebox guest(s) orphaned by a previous hive-cloud incarnation"
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = runner_bin;
    }

    fn unique_temp_name(&self, label: &str, suffix: &str) -> OsString {
        let id = ARTIFACT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        OsString::from(format!(
            ".{label}-{}-{}-{id}{suffix}",
            std::process::id(),
            now_ms()
        ))
    }

    fn allocate_temp_file(
        &self,
        temporary: &ArtifactDirectory,
        label: &str,
        suffix: &str,
    ) -> anyhow::Result<ArtifactTemp> {
        #[cfg(not(unix))]
        anyhow::bail!("litebox artifact publication requires Unix descriptors");
        #[cfg(unix)]
        for _ in 0..256 {
            let parent = temporary.descriptor.try_clone()?;
            let name = self.unique_temp_name(label, suffix);
            match temporary.create_file(&name, 0o600) {
                Ok(file) => {
                    return Ok(ArtifactTemp {
                        parent,
                        name,
                        file,
                        armed: true,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("could not allocate a unique litebox artifact temp file")
    }

    fn allocate_scratch_directory(
        &self,
        temporary: &ArtifactDirectory,
        label: &str,
    ) -> anyhow::Result<ArtifactScratch> {
        #[cfg(not(unix))]
        anyhow::bail!("litebox artifact publication requires Unix descriptors");
        #[cfg(unix)]
        for _ in 0..256 {
            let allocation_parent = temporary.descriptor.try_clone()?;
            let name = self.unique_temp_name(label, "");
            let name_c = cache_component(&name)?;
            let rc =
                unsafe { libc::mkdirat(temporary.descriptor.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(error).context("create litebox artifact scratch directory");
            }
            let mut allocation = ArtifactScratchAllocation {
                parent: allocation_parent,
                name: name.clone(),
                armed: true,
            };
            let preflight = cache_entry_state(temporary, &name)?;
            let directory = open_directory_at(&temporary.descriptor, &name)
                .context("open litebox artifact scratch directory")?;
            let identity = directory_identity(&directory)?;
            anyhow::ensure!(
                preflight.is_directory()
                    && identity.device == preflight.device
                    && identity.inode == preflight.inode,
                "litebox artifact scratch directory changed while opening"
            );
            let scratch = ArtifactScratch {
                parent: temporary.descriptor.try_clone()?,
                name,
                directory,
                children: Vec::new(),
            };
            allocation.armed = false;
            return Ok(scratch);
        }
        anyhow::bail!("could not allocate a unique litebox artifact scratch directory")
    }

    /// Give Litebox the lexical `.tar` pathname its CLI requires without reopening the
    /// content-addressed cache name: a hard link of the verified open archive inside a private held
    /// directory, whose Drop guard removes both names on every path.
    fn allocate_initial_files_alias(
        &self,
        temporary: &ArtifactDirectory,
        archive: &File,
    ) -> anyhow::Result<ArtifactScratch> {
        let source_before = archive.metadata()?;
        anyhow::ensure!(
            source_before.is_file(),
            "litebox initial-files descriptor is not a regular file"
        );
        let mut alias = self.allocate_scratch_directory(temporary, "initial-files")?;
        let name = OsString::from(INITIAL_FILES_ALIAS_NAME);
        link_open_file_alias(archive, &alias.directory, &name)
            .context("create descriptor-bound Litebox initial-files alias")?;
        alias.children.push(name.clone());

        #[cfg(unix)]
        {
            let name = cache_component(&name)?;
            let fd = unsafe {
                libc::openat(
                    alias.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("open descriptor-bound Litebox initial-files alias");
            }
            let linked = unsafe { File::from_raw_fd(fd) };
            let source_after = archive.metadata()?;
            let linked_metadata = linked.metadata()?;
            anyhow::ensure!(
                source_before.dev() == source_after.dev()
                    && source_before.ino() == source_after.ino()
                    && source_before.len() == source_after.len()
                    && source_before.modified().ok() == source_after.modified().ok(),
                "litebox initial-files descriptor changed while binding its private alias"
            );
            anyhow::ensure!(
                linked_metadata.is_file()
                    && linked_metadata.dev() == source_before.dev()
                    && linked_metadata.ino() == source_before.ino()
                    && linked_metadata.len() == source_before.len(),
                "litebox initial-files alias is not bound to the verified archive inode"
            );
        }
        Ok(alias)
    }

    /// Probe only: append the platform preloads and the probe's write-test entries to a canonical
    /// tar; `append_litebox_runtime_augmentation` leaves exactly one end marker after them.
    async fn stage_bind_shim(
        &self,
        _directories: &ArtifactDirectories,
        archive: &ArtifactTemp,
    ) -> anyhow::Result<()> {
        append_litebox_runtime_augmentation(
            archive.file.try_clone()?,
            Vec::new(),
            None,
            None,
            probe_write_entries(),
        )
        .await
    }

    /// Allocate this cell's TUN link for its function process and record it in `cell_nets`; `None`
    /// when link setup fails (no `ip`, `CAP_NET_ADMIN` or `/dev/net/tun`), which `provision` turns
    /// into a hard error for non-container cells.
    async fn setup_cell_net(&self, id: &CellId) -> Option<LiteboxNet> {
        let (net, mut rollback) = self.allocate_link().await?;
        self.cell_nets.lock().await.insert(id.clone(), net.clone());
        rollback.commit();
        Some(net)
    }

    /// Idempotently enable forwarding and NAT for litebox guests (`10.88.0.0/16`): internet egress
    /// only (guest-to-guest traffic, private ranges, the metadata endpoint and the node's admin
    /// port are dropped ahead of the ACCEPTs, inserted at the head of FORWARD), plus a hairpin
    /// redirect of 80/443 for a guest calling its own public hostname. The hairpin address reaches
    /// the script only as `$HAIRPIN_IP`, never interpolated. The nft branch keeps its own
    /// `hive_nat_litebox` table, which a lockdown regeneration leaves alone. Failures are
    /// non-fatal: cells simply get no egress.
    #[cfg(target_os = "linux")]
    async fn ensure_guest_nat() {
        use std::sync::atomic::Ordering;
        static NAT_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if NAT_READY.swap(true, Ordering::SeqCst) {
            return;
        }
        let hairpin_ip = match std::env::var("HIVE_PUBLIC_IP")
            .ok()
            .map(|s| s.trim().to_string())
        {
            Some(ip) if ip.parse::<std::net::Ipv4Addr>().is_ok() => ip,
            _ => String::new(),
        };
        let script = r#"
            export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH
            sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1
            if command -v iptables >/dev/null 2>&1; then
              if [ -n "$HAIRPIN_IP" ]; then
                iptables -t nat -C PREROUTING -s 10.88.0.0/16 -d "$HAIRPIN_IP" -p tcp -m multiport --dports 80,443 -j REDIRECT 2>/dev/null \
                  || iptables -t nat -I PREROUTING 1 -s 10.88.0.0/16 -d "$HAIRPIN_IP" -p tcp -m multiport --dports 80,443 -j REDIRECT
              fi
              iptables -C FORWARD -s 10.88.0.0/16 -d 10.88.0.0/16 -j DROP 2>/dev/null || iptables -I FORWARD 1 -s 10.88.0.0/16 -d 10.88.0.0/16 -j DROP
              # INTERNET-ONLY EGRESS. A bare `-s 10.88/16 ACCEPT` would forward
              # guest packets to everything the host can route: this node's own
              # admin listener (:8786), the rest of the VPC, and the cloud
              # metadata endpoint 169.254.169.254 — whose response hands
              # untrusted tenant code the node's own credentials. Guests get
              # the internet and nothing else. These are appended BEFORE the
              # ACCEPTs below, so they win.
              # `-I FORWARD 1`, never `-A`: these must sit AHEAD of the ACCEPTs
              # below. The ACCEPT rules may already exist from an earlier run,
              # and iptables matches in order — appending the denies put them
              # after the accepts and left every one of them dead (measured
              # 2026-10-08: a correctly-installed-looking ruleset where a guest
              # could still reach 169.254.169.254). Inserting at the head is
              # order-independent.
              for DST in 169.254.0.0/16 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 100.64.0.0/10; do
                iptables -C FORWARD -s 10.88.0.0/16 -d "$DST" -j DROP 2>/dev/null || iptables -I FORWARD 1 -s 10.88.0.0/16 -d "$DST" -j DROP
              done
              # The platform's own admin API, by port and regardless of
              # destination: it is not only on a private address, so the
              # ranges above do not cover it. A guest that reaches it can
              # mint operator JWTs and drive the whole control plane.
              for PORT in 8786; do
                iptables -C FORWARD -s 10.88.0.0/16 -p tcp --dport "$PORT" -j DROP 2>/dev/null || iptables -I FORWARD 1 -s 10.88.0.0/16 -p tcp --dport "$PORT" -j DROP
              done
              iptables -t nat -C POSTROUTING -s 10.88.0.0/16 -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s 10.88.0.0/16 -j MASQUERADE
              iptables -C FORWARD -s 10.88.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -s 10.88.0.0/16 -j ACCEPT
              iptables -C FORWARD -d 10.88.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -d 10.88.0.0/16 -j ACCEPT
            elif command -v nft >/dev/null 2>&1; then
              nft add table ip hive_nat_litebox 2>/dev/null
              if [ -n "$HAIRPIN_IP" ]; then
                nft 'add chain ip hive_nat_litebox pre { type nat hook prerouting priority -100 ; }' 2>/dev/null
                nft flush chain ip hive_nat_litebox pre 2>/dev/null
                nft add rule ip hive_nat_litebox pre ip saddr 10.88.0.0/16 ip daddr "$HAIRPIN_IP" tcp dport '{80, 443}' redirect 2>/dev/null
              fi
              nft 'add chain ip hive_nat_litebox post { type nat hook postrouting priority 100 ; }' 2>/dev/null
              nft flush chain ip hive_nat_litebox post 2>/dev/null
              nft add rule ip hive_nat_litebox post ip saddr 10.88.0.0/16 masquerade 2>/dev/null
              nft 'add chain ip hive_nat_litebox fwd { type filter hook forward priority 0 ; }' 2>/dev/null
              nft flush chain ip hive_nat_litebox fwd 2>/dev/null
              nft add rule ip hive_nat_litebox fwd ip saddr 10.88.0.0/16 ip daddr 10.88.0.0/16 drop 2>/dev/null
              # INTERNET-ONLY EGRESS (same reasoning as the iptables branch):
              # without these, the accept below forwards guest traffic to this
              # node's admin listener, the VPC, and 169.254.169.254, whose reply
              # gives untrusted tenant code the node's cloud credentials.
              nft add rule ip hive_nat_litebox fwd ip saddr 10.88.0.0/16 ip daddr '{ 169.254.0.0/16, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10 }' drop 2>/dev/null
              # Admin API by port, regardless of destination — see the
              # iptables branch.
              nft add rule ip hive_nat_litebox fwd ip saddr 10.88.0.0/16 tcp dport 8786 drop 2>/dev/null
              nft add rule ip hive_nat_litebox fwd ip saddr 10.88.0.0/16 accept 2>/dev/null
              nft add rule ip hive_nat_litebox fwd ip daddr 10.88.0.0/16 accept 2>/dev/null
            fi"#;
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HAIRPIN_IP", &hairpin_ip)
            .kill_on_drop(true)
            .output()
            .await;
    }

    #[cfg(not(target_os = "linux"))]
    async fn ensure_guest_nat() {}

    /// One runner's TUN link (see [`LiteboxNet`]). A TUN device has one owner: a second runner
    /// attaching gets `EBUSY` and panics, so the cell's provision-time link serves only the
    /// function process and every `exec_command`/`exec_pty` runner allocates its own, whose armed
    /// rollback its waiter task drops to delete the link. `None` on any setup failure.
    async fn allocate_link(&self) -> Option<(LiteboxNet, LiteboxLinkRollback)> {
        use std::sync::atomic::Ordering;
        Self::ensure_guest_nat().await;
        let i = self.net_idx.fetch_add(1, Ordering::SeqCst) % LINK_SLOTS;
        let third = ((i >> 6) & 0xff) as u8;
        let base = ((i & 0x3f) as u8) * 4;
        let host_ip = format!("10.88.{third}.{}", base + 1);
        let guest_ip = format!("10.88.{third}.{}", base + 2);
        let tun_dev = format!("lbt{i}");
        let rollback = LiteboxLinkRollback::new(tun_dev.clone());

        let script = create_link_script(&tun_dev, &host_ip);
        let ok = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .kill_on_drop(true)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {tun_dev} 2>/dev/null"
                ))
                .status()
                .await;
            return None;
        }
        let net = LiteboxNet {
            tun_dev,
            host_ip,
            guest_ip,
        };
        Some((net, rollback))
    }

    /// Best-effort removal of the cell's TUN device.
    async fn teardown_cell_net(&self, id: &CellId) {
        if let Some(net) = self.cell_nets.lock().await.remove(id) {
            let script = format!(
                "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {} 2>/dev/null",
                net.tun_dev
            );
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .status()
                .await;
        }
    }

    /// Tier 1: the runner binary exists on Linux. Not proof the sandbox works; [`Self::smoke_test`]
    /// is the functional check.
    pub fn is_supported(&self) -> bool {
        cfg!(target_os = "linux") && self.cfg.runner_bin.exists()
    }

    fn image_reference_name(image: &str) -> OsString {
        let key = sha256_parts(&[b"hive-litebox-image-reference-v1\0", image.as_bytes()]);
        OsString::from(format!("{key}.json"))
    }

    fn app_archive_name(sha256: &str) -> OsString {
        OsString::from(format!("{sha256}.tar"))
    }

    fn runtime_archive_name(sha256: &str) -> OsString {
        OsString::from(format!("{sha256}.tar"))
    }

    fn prepare_artifact_dirs(&self) -> anyhow::Result<ArtifactDirectories> {
        ArtifactDirectories::prepare(&self.cfg.cache_root)
    }

    async fn load_image_reference_locked(
        &self,
        directories: &ArtifactDirectories,
        image: &str,
    ) -> anyhow::Result<LiteboxImageReference> {
        directories.verify_bindings()?;
        let name = Self::image_reference_name(image);
        let file = directories.references.open_regular(&name).map_err(|error| {
            anyhow::anyhow!(
                "{}: litebox runtime artifact reference is missing for image {image} at {}/{}: {error}",
                hive_core::fault::NODE_IMAGE_MISSING,
                directories.references.path.display(),
                name.to_string_lossy()
            )
        })?;
        let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES).map_err(|error| {
            anyhow::anyhow!(
                "{}: read litebox runtime artifact reference for image {image}: {error:#}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        let reference: LiteboxImageReference = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "{}: decode litebox runtime artifact reference for image {image}: {error}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        validate_image_reference(&reference, image)?;
        verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: litebox app archive validation failed for image {image}: {error:#}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        directories.verify_bindings()?;
        Ok(reference)
    }

    /// Publish the image's application binding exactly once; a retry may see runtime-closure
    /// metadata a prior cold start added, but never a different immutable binding.
    async fn write_image_reference_locked(
        &self,
        directories: &ArtifactDirectories,
        reference: &LiteboxImageReference,
    ) -> anyhow::Result<()> {
        validate_image_reference(reference, &reference.image)?;
        let bytes = encoded_image_reference(reference)?;
        verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await?;
        directories.verify_bindings()?;
        let mut temp = self.allocate_temp_file(&directories.temporary, "reference", ".json")?;
        {
            let mut file = tokio::fs::File::from_std(temp.file.try_clone()?);
            file.write_all(&bytes).await?;
            file.sync_all().await?;
        }
        let destination_name = Self::image_reference_name(&reference.image);
        match link_cache_entry(
            &temp.parent,
            &temp.name,
            &directories.references.descriptor,
            &destination_name,
        ) {
            Ok(()) => {
                directories.references.descriptor.sync_all()?;
                let published = directories.references.open_regular(&destination_name)?;
                anyhow::ensure!(
                    read_bounded_file(published, MAX_REFERENCE_BYTES)? == bytes,
                    "litebox image reference changed during first publication"
                );
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                let existing = self
                    .load_image_reference_locked(directories, &reference.image)
                    .await?;
                ensure_same_application_binding(&existing, reference)?;
            }
            Err(error) => return Err(error).context("publish no-replace litebox image reference"),
        }
        unlink_cache_entry(&temp.parent, &temp.name, false)?;
        temp.disarm();
        directories.temporary.descriptor.sync_all()?;
        directories.verify_bindings()?;
        Ok(())
    }

    /// Replace only the lazily derived runtime-closure metadata, after re-proving that the durable
    /// application binding is the one the cold start loaded; cancellation before the rename leaves
    /// only a Drop-cleaned private temp.
    async fn write_runtime_reference_locked(
        &self,
        directories: &ArtifactDirectories,
        expected_binding: &mut LiteboxImageReference,
        runtime_key: &str,
        runtime: RuntimeArchiveReference,
    ) -> anyhow::Result<()> {
        let mut existing = self
            .load_image_reference_locked(directories, &expected_binding.image)
            .await?;
        ensure_same_application_binding(&existing, expected_binding)?;
        verify_immutable_open(
            &directories.runtimes,
            &Self::runtime_archive_name(&runtime.archive_sha256),
            &runtime.archive_sha256,
        )
        .await?;
        if existing.runtimes.get(runtime_key) == Some(&runtime) {
            *expected_binding = existing;
            return Ok(());
        }
        let prior_runtimes = existing.runtimes.clone();
        existing.runtimes.insert(runtime_key.to_string(), runtime);
        validate_image_reference(&existing, &existing.image)?;
        let bytes = encoded_image_reference(&existing)?;

        directories.verify_bindings()?;
        let observed = self
            .load_image_reference_locked(directories, &existing.image)
            .await?;
        ensure_same_application_binding(&observed, &existing)?;
        anyhow::ensure!(
            observed.runtimes == prior_runtimes,
            "litebox runtime reference metadata changed before its serialized update"
        );

        let mut temp = self.allocate_temp_file(&directories.temporary, "reference", ".json")?;
        {
            let mut file = tokio::fs::File::from_std(temp.file.try_clone()?);
            file.write_all(&bytes).await?;
            file.sync_all().await?;
        }
        rename_cache_entry(
            &temp.parent,
            &temp.name,
            &directories.references.descriptor,
            &Self::image_reference_name(&existing.image),
        )?;
        temp.disarm();
        directories.references.descriptor.sync_all()?;
        directories.temporary.descriptor.sync_all()?;
        let published = directories
            .references
            .open_regular(&Self::image_reference_name(&existing.image))?;
        anyhow::ensure!(
            read_bounded_file(published, MAX_REFERENCE_BYTES)? == bytes,
            "litebox runtime reference metadata changed during publication"
        );
        directories.verify_bindings()?;
        *expected_binding = existing;
        Ok(())
    }

    async fn publish_immutable_locked(
        &self,
        directories: &ArtifactDirectories,
        temp: &mut ArtifactTemp,
        destination: &ArtifactDirectory,
        destination_name: &OsStr,
        sha256: &str,
    ) -> anyhow::Result<File> {
        directories.verify_bindings()?;
        #[cfg(unix)]
        temp.file
            .set_permissions(std::fs::Permissions::from_mode(0o400))?;
        temp.file.sync_all()?;
        let actual = sha256_open_file(&temp.file).await?;
        anyhow::ensure!(
            actual == sha256,
            "litebox publication temp hash changed (got {actual}, expected {sha256})"
        );
        match link_cache_entry(
            &temp.parent,
            &temp.name,
            &destination.descriptor,
            destination_name,
        ) {
            Ok(()) => {
                destination.descriptor.sync_all()?;
                unlink_cache_entry(&temp.parent, &temp.name, false)?;
                temp.disarm();
                directories.temporary.descriptor.sync_all()?;
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
            Err(error) => return Err(error).context("publish immutable litebox artifact"),
        }
        let file = verify_immutable_open(destination, destination_name, sha256).await?;
        directories.verify_bindings()?;
        Ok(file)
    }

    async fn package_metadata(
        &self,
        staged: &crate::runtime_artifact::StagedRuntimeArtifact,
        app_rel: &Path,
    ) -> anyhow::Result<(BTreeMap<String, String>, Option<String>)> {
        let package_relative = app_rel.join("package.json");
        let scripts = if let Some(bytes) =
            staged.read_regular(&package_relative, MAX_PACKAGE_JSON_BYTES)?
        {
            let package: serde_json::Value = serde_json::from_slice(&bytes)?;
            let mut scripts = BTreeMap::new();
            if let Some(values) = package.get("scripts").and_then(|value| value.as_object()) {
                anyhow::ensure!(
                    values.len() <= MAX_PACKAGE_SCRIPTS,
                    "selected application declares more than {MAX_PACKAGE_SCRIPTS} package scripts"
                );
                for (name, value) in values {
                    let Some(script) = value.as_str() else {
                        anyhow::bail!(
                            "selected application package script {name:?} is not a string"
                        );
                    };
                    anyhow::ensure!(
                        name.len() <= 128 && script.len() <= MAX_PACKAGE_SCRIPT_BYTES,
                        "selected application package script {name:?} exceeds the litebox metadata limit"
                    );
                    scripts.insert(name.clone(), script.to_string());
                }
            }
            scripts
        } else {
            BTreeMap::new()
        };

        let mut base = app_rel.to_path_buf();
        let next_entry = loop {
            let relative = base.join("node_modules/next/dist/bin/next");
            if staged.is_regular_file(&relative)? {
                break Some(guest_path(&relative)?);
            }
            if !base.pop() {
                break None;
            }
        };
        Ok((scripts, next_entry))
    }

    async fn ensure_combined_tar_locked(
        &self,
        directories: &ArtifactDirectories,
        image: &str,
        runtime_key: &str,
        bin: &Path,
        reference: &mut LiteboxImageReference,
    ) -> anyhow::Result<File> {
        let mut deps = ldd_closure(bin).await?;
        add_glibc_compat_stubs(&mut deps);
        deps.sort();
        deps.dedup();
        let source_sha256 = runtime_source_sha256(
            bin,
            &deps,
            &reference.app_archive_sha256,
            &reference.identity,
            NODE_BIND_SHIM_JS,
            RUNTIME_GUARD_JS,
        )
        .await?;
        if let Some(cached) = reference.runtimes.get(runtime_key) {
            if cached.source_sha256 == source_sha256 {
                match verify_immutable_open(
                    &directories.runtimes,
                    &Self::runtime_archive_name(&cached.archive_sha256),
                    &cached.archive_sha256,
                )
                .await
                {
                    Ok(file) => return Ok(file),
                    Err(error) => {
                        tracing::warn!(
                            runtime_key = %runtime_key,
                            archive_sha256 = %cached.archive_sha256,
                            error = %error,
                            "cached runtime archive failed verification; rebuilding it"
                        );
                    }
                }
            }
        }

        let app = verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await?;
        let mut temp = self.allocate_temp_file(&directories.temporary, "runtime", ".tar")?;
        {
            let mut input = tokio::fs::File::from_std(app.try_clone()?);
            let mut output = tokio::fs::File::from_std(temp.file.try_clone()?);
            tokio::io::copy(&mut input, &mut output).await?;
            output.sync_all().await?;
        }
        append_litebox_runtime_augmentation(
            temp.file.try_clone()?,
            deps.clone(),
            Some(reference.identity.clone()),
            Some(bin.to_path_buf()),
            Vec::new(),
        )
        .await
        .with_context(|| format!("failed to augment runtime tar for {}", bin.display()))?;
        temp.file.sync_all()?;
        let after = runtime_source_sha256(
            bin,
            &deps,
            &reference.app_archive_sha256,
            &reference.identity,
            NODE_BIND_SHIM_JS,
            RUNTIME_GUARD_JS,
        )
        .await?;
        anyhow::ensure!(
            after == source_sha256,
            "{}: litebox runtime executable or linked-library closure changed during publication",
            hive_core::fault::NODE_RUNTIME_MISSING
        );
        let archive_sha256 = sha256_open_file(&temp.file).await?;
        let destination_name = Self::runtime_archive_name(&archive_sha256);
        let archive = self
            .publish_immutable_locked(
                directories,
                &mut temp,
                &directories.runtimes,
                &destination_name,
                &archive_sha256,
            )
            .await?;
        self.write_runtime_reference_locked(
            directories,
            reference,
            runtime_key,
            RuntimeArchiveReference {
                source_sha256,
                archive_sha256,
            },
        )
        .await?;
        if let Err(error) = self.gc_artifacts_locked(directories).await {
            tracing::warn!(error = %error, "litebox artifact GC refused or failed");
        }
        anyhow::ensure!(
            reference.image == image,
            "litebox runtime artifact reference changed image during publication"
        );
        Ok(archive)
    }

    /// Reap archives no reference names, plus stale temp and staging entries, under blast-radius
    /// guards: an empty keep set, an entry younger than the grace age
    /// (`HIVE_LITEBOX_ARTIFACT_GC_GRACE_SECS`) and a reaped fraction over the cap
    /// (`HIVE_LITEBOX_ARTIFACT_GC_MAX_REAP_FRACTION`) are all refused. Referenced archives are
    /// checked for presence only, never hashed (hashing them under `artifact_lock` queued every
    /// cold start behind it); content is verified where it is used.
    async fn gc_artifacts_locked(&self, directories: &ArtifactDirectories) -> anyhow::Result<()> {
        directories.verify_bindings()?;
        let mut keep_apps = BTreeSet::new();
        let mut keep_runtimes = BTreeSet::new();
        for name in directories.references.child_names()? {
            let file = directories
                .references
                .open_regular(&name)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "litebox artifact GC refuses non-file reference {}/{}: {error}",
                        directories.references.path.display(),
                        name.to_string_lossy()
                    )
                })?;
            let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES)?;
            let reference: LiteboxImageReference = serde_json::from_slice(&bytes)?;
            validate_image_reference(&reference, &reference.image)?;
            anyhow::ensure!(
                name == Self::image_reference_name(&reference.image),
                "litebox artifact GC refuses a reference with a mismatched filename: {}",
                name.to_string_lossy()
            );
            let app_name = Self::app_archive_name(&reference.app_archive_sha256);
            directories.apps.open_regular(&app_name).with_context(|| {
                format!(
                    "litebox artifact GC: referenced app archive missing for {}",
                    reference.image
                )
            })?;
            keep_apps.insert(app_name);
            for runtime in reference.runtimes.values() {
                let runtime_name = Self::runtime_archive_name(&runtime.archive_sha256);
                directories
                    .runtimes
                    .open_regular(&runtime_name)
                    .with_context(|| {
                        format!(
                            "litebox artifact GC: referenced runtime archive missing for {}",
                            reference.image
                        )
                    })?;
                keep_runtimes.insert(runtime_name);
            }
        }
        anyhow::ensure!(
            !keep_apps.is_empty(),
            "litebox artifact GC refuses an empty keep set"
        );

        let grace = Duration::from_secs(
            std::env::var("HIVE_LITEBOX_ARTIFACT_GC_GRACE_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_ARTIFACT_GC_GRACE_SECS),
        );
        let max_fraction = std::env::var("HIVE_LITEBOX_ARTIFACT_GC_MAX_REAP_FRACTION")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
            .unwrap_or(DEFAULT_ARTIFACT_GC_MAX_REAP_FRACTION);
        let now = SystemTime::now();
        let mut total = 0usize;
        let mut reap = Vec::new();
        for (area, directory) in [
            (ArtifactArea::Apps, &directories.apps),
            (ArtifactArea::Runtimes, &directories.runtimes),
            (ArtifactArea::Temporary, &directories.temporary),
            (ArtifactArea::Staging, &directories.staging),
        ] {
            for name in directory.child_names()? {
                total += 1;
                if (area == ArtifactArea::Apps && keep_apps.contains(&name))
                    || (area == ArtifactArea::Runtimes && keep_runtimes.contains(&name))
                {
                    continue;
                }
                let state = cache_entry_state(directory, &name)?;
                anyhow::ensure!(
                    !matches!(area, ArtifactArea::Apps | ArtifactArea::Runtimes)
                        || state.is_regular(),
                    "litebox artifact GC refuses a non-file immutable archive {}/{}",
                    directory.path.display(),
                    name.to_string_lossy()
                );
                let old_enough = now
                    .duration_since(state.modified)
                    .ok()
                    .is_some_and(|age| age >= grace);
                if old_enough {
                    reap.push((area, name, state));
                }
            }
        }
        let fraction = reap.len() as f64 / total.max(1) as f64;
        anyhow::ensure!(
            fraction <= max_fraction,
            "litebox artifact GC refuses to reap {}/{} entries ({fraction:.3} > {max_fraction:.3})",
            reap.len(),
            total
        );
        directories.verify_bindings()?;
        let mut touched = BTreeSet::new();
        for (area, name, state) in reap {
            let directory = artifact_area_directory(directories, area);
            remove_cache_entry(directory, &name, state)?;
            touched.insert(area);
        }
        for area in touched {
            artifact_area_directory(directories, area)
                .descriptor
                .sync_all()?;
        }
        directories.verify_bindings()?;
        Ok(())
    }

    /// Retire one exact platform-issued image reference. The backend trait has no
    /// deployment-deletion hook yet: its eventual caller must invoke this only after the image is
    /// absent from live platform state.
    pub async fn retire_image_reference(&self, image: &str) -> anyhow::Result<bool> {
        anyhow::ensure!(
            valid_identity_id(image),
            "runtime artifact image contains an invalid platform identity"
        );
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let name = Self::image_reference_name(image);
        let file = match directories.references.open_regular(&name) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES)?;
        let reference: LiteboxImageReference = serde_json::from_slice(&bytes)?;
        validate_image_reference(&reference, image)?;
        directories.verify_bindings()?;
        unlink_cache_entry(&directories.references.descriptor, &name, false)?;
        directories.references.descriptor.sync_all()?;
        directories.verify_bindings()?;
        if let Err(error) = self.gc_artifacts_locked(&directories).await {
            tracing::warn!(error = %error, "litebox artifact GC refused or failed after reference retirement");
        }
        Ok(true)
    }

    /// Tier 2: functional proof in three checks. (a) Rewriter: `/bin/echo`, staged with its `ldd`
    /// closure, must print a marker through the live rewriter. (b) Network: a throwaway TUN link
    /// carries a real HTTP round trip to a Node server (wildcard bind, the case `networking.patch`
    /// fixes) through the patched litebox and the bind shim. (c) Writable workdir: that guest
    /// creates, overwrites and renames files inside a read-only tar-layer directory before it
    /// listens. A PASS licenses `HIVE_LITEBOX_VERIFIED=1` but is never applied automatically, and
    /// it does not cover Bun, Python or load. Bring-up only: it creates a real TUN device and NAT
    /// rules, so never run it (`--litebox-probe`) against a node carrying live traffic.
    pub async fn smoke_test(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_supported(),
            "litebox runner binary not present at {} (or not on Linux) — nothing to smoke-test",
            self.cfg.runner_bin.display()
        );
        self.rewriter_smoke_test().await?;
        self.network_smoke_test().await?;
        Ok(())
    }

    async fn rewriter_smoke_test(&self) -> anyhow::Result<()> {
        let probe_bin = resolve_bin("echo").await;
        let deps = ldd_closure(&probe_bin).await.unwrap_or_default();
        let tar_path = self.cfg.root.join("litebox-smoke-deps.tar");
        if let Some(parent) = tar_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        if !deps.is_empty() {
            let mut cmd = Command::new("tar");
            cmd.arg(TAR_DEREFERENCE_FLAG)
                .arg("--absolute-names")
                .arg("-cf")
                .arg(&tar_path);
            for d in &deps {
                cmd.arg(d);
            }
            let out = cmd
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
            anyhow::ensure!(
                out.status.success(),
                "tar failed staging smoke-test deps: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let marker = format!("litebox-smoke-{}", now_ms());
        let mut cmd = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut cmd);
        cmd.arg("-Z").arg("--rewrite-syscalls");
        if !deps.is_empty() {
            cmd.arg(format!("--initial-files={}", tar_path.display()));
        }
        cmd.arg("--").arg(&probe_bin).arg(&marker);
        let out = cmd
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn litebox runner: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "litebox rewriter smoke test exited non-zero ({}); stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        anyhow::ensure!(
            stdout.trim() == marker,
            "litebox rewriter smoke test ran but stdout did not match — the sandboxed process's \
             output was not reliably delivered to the host. got: {stdout:?}, want: {marker:?}"
        );
        Ok(())
    }

    async fn network_smoke_test(&self) -> anyhow::Result<()> {
        let probe_id = CellId::new();
        let net = self.setup_cell_net(&probe_id).await.ok_or_else(|| {
            anyhow::anyhow!(
                "litebox network smoke test: failed to set up a TUN device — needs `ip`, \
                 CAP_NET_ADMIN, and /dev/net/tun"
            )
        })?;
        let result = self.network_smoke_test_inner(&net).await;
        self.teardown_cell_net(&probe_id).await;
        result
    }

    async fn network_smoke_test_inner(&self, net: &LiteboxNet) -> anyhow::Result<()> {
        /// Wildcard-bound by the probe's server (`.listen(PROBE_PORT)`, no host): the case
        /// `networking.patch` fixes natively, so a PASS proves the patch and not only the TUN
        /// plumbing. The bind shim is preloaded too, a no-op here that proves the shim did not break.
        const PROBE_PORT: u16 = 18080;
        /// Ceiling on the HTTP round trip: litebox's userspace TCP stack has no kernel socket behind
        /// it, so a guest that dies mid-response never sends a FIN and `read_to_end` would wait
        /// forever.
        const ROUND_TRIP_DEADLINE: Duration = Duration::from_secs(10);

        let node_bin = resolve_bin("node").await;
        anyhow::ensure!(
            node_bin.exists(),
            "litebox network smoke test: no `node` binary found on PATH — cannot probe \
             networking without a runtime to serve through it"
        );
        let deps = ldd_closure(&node_bin).await?;
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let archive = self.allocate_temp_file(&directories.temporary, "network-smoke", ".tar")?;
        let mut tar = Command::new("tar");
        let archive_path = crate::runtime_artifact::inherit_file_path(&mut tar, &archive.file)?;
        let out = tar
            .arg(TAR_DEREFERENCE_FLAG)
            .arg("--absolute-names")
            .arg("-cf")
            .arg(archive_path)
            .args(&deps)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "tar failed staging node's deps for the network smoke test: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        self.stage_bind_shim(&directories, &archive).await?;
        archive.file.sync_all()?;

        let marker = format!("litebox-net-smoke-{}", now_ms());
        let write_dir = format!("{PROBE_WRITE_DIR}/data");
        let write_token = format!("write-ok-{}", now_ms());
        let script = format!(
            "const fs=require('fs');\
             fs.mkdirSync('{write_dir}',{{recursive:true}});\
             fs.writeFileSync('{write_dir}/probe.json','{write_token}');\
             fs.writeFileSync('{PROBE_WRITE_DIR}/.keep','over-'+'{write_token}');\
             fs.writeFileSync('{PROBE_WRITE_DIR}/seed.json.tmp','ren-'+'{write_token}');\
             fs.renameSync('{PROBE_WRITE_DIR}/seed.json.tmp','{PROBE_WRITE_DIR}/seed.json');\
             const back=[fs.readFileSync('{write_dir}/probe.json','utf8'),\
               fs.readFileSync('{PROBE_WRITE_DIR}/.keep','utf8'),\
               fs.readFileSync('{PROBE_WRITE_DIR}/seed.json','utf8'),\
               fs.statSync('{PROBE_WRITE_DIR}/seed.json').size].join('|');\
             require('http').createServer((q,r)=>{{r.end('{marker}:'+back)}}).listen({PROBE_PORT});"
        );
        let want_body = format!(
            "{marker}:{write_token}|over-{write_token}|ren-{write_token}|{}",
            format!("ren-{write_token}").len()
        );

        let mut cmd = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut cmd);
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &archive.file)?;
        let initial_files =
            crate::runtime_artifact::inherit_file_path(&mut cmd, &initial_files_alias.directory)?
                .join(INITIAL_FILES_ALIAS_NAME);
        cmd.arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files.display()))
            .arg("--")
            .arg(&node_bin)
            .arg("-e")
            .arg(&script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            .envs(guest_ca_env())
            .env("NODE_OPTIONS", format!("--require {GUEST_BIND_SHIM_PATH}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn litebox runner for the network smoke test: {e}")
        })?;
        use tokio::io::AsyncReadExt;
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        let addr = format!("{}:{PROBE_PORT}", net.guest_ip);
        if let Err(e) = crate::mock::wait_tcp_ready(&addr, Duration::from_secs(10)).await {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let out = stdout_task.await.unwrap_or_default();
            let err = stderr_task.await.unwrap_or_default();
            return Err(e.context(format!(
                "litebox network smoke test: server never became reachable (guest stdout: {:?}, \
                 stderr: {:?})",
                String::from_utf8_lossy(&out).trim(),
                String::from_utf8_lossy(&err).trim(),
            )));
        }
        drop(initial_files_alias);

        use tokio::io::AsyncWriteExt;
        let round_trip = tokio::time::timeout(ROUND_TRIP_DEADLINE, async {
            let mut stream = tokio::net::TcpStream::connect(&addr).await?;
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await?;
            let mut resp = Vec::new();
            stream.read_to_end(&mut resp).await?;
            anyhow::Ok(resp)
        })
        .await
        .map_err(|_| anyhow::anyhow!("litebox network smoke test: HTTP round trip timed out"))
        .and_then(|r| r);
        let _ = child.start_kill();
        let _ = child.wait().await;
        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let guest_output = format!(
            "guest stdout: {:?}, stderr: {:?}",
            String::from_utf8_lossy(&out).trim(),
            String::from_utf8_lossy(&err).trim(),
        );

        let resp = round_trip.map_err(|e| e.context(guest_output.clone()))?;
        let resp_text = String::from_utf8_lossy(&resp);
        anyhow::ensure!(
            resp_text.contains(&marker),
            "litebox network smoke test: real HTTP round trip completed but the response was \
             wrong — got: {resp_text:?}, want a body containing {marker:?} ({guest_output})"
        );
        anyhow::ensure!(
            resp_text.contains(&want_body),
            "litebox write probe FAILED: the guest served HTTP but one of the four writes inside \
             {PROBE_WRITE_DIR} (a read-only tar-layer directory, the shape of every deployment's \
             /workspace) did not round-trip — got: {resp_text:?}, want a body containing \
             {want_body:?}. Segments: create-in-tar-dir | overwrite-0444-file | rename-over-file | \
             renamed-size. This is the EACCES-at-launch / copy-up / rename regression class: \
             networking.patch hunks 4-8 are missing or ineffective in the runner on this host \
             ({guest_output})"
        );
        Ok(())
    }
}

fn sha256_parts(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hex_sha256(&hasher.finalize())
}

/// Undoes the kernel's `" (deleted)"` annotation on a `/proc/<pid>/exe` target whose inode was
/// unlinked while still mapped, which every runner-binary deploy causes, not only a crash.
#[cfg(target_os = "linux")]
fn strip_deleted_exe_suffix(path: &Path) -> PathBuf {
    const SUFFIX: &str = " (deleted)";
    match path.to_str() {
        Some(s) if s.ends_with(SUFFIX) => PathBuf::from(&s[..s.len() - SUFFIX.len()]),
        _ => path.to_path_buf(),
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_identity_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_guest_workdir(workdir: &str) -> anyhow::Result<()> {
    let path = Path::new(workdir);
    anyhow::ensure!(path.is_absolute(), "litebox guest workdir is not absolute");
    anyhow::ensure!(
        path == Path::new(DELIVERED_WORKDIR) || path.starts_with(Path::new(DELIVERED_WORKDIR)),
        "litebox guest workdir escapes {DELIVERED_WORKDIR}: {workdir}"
    );
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => normalized.push(value),
            _ => anyhow::bail!("litebox guest workdir is not normalized: {workdir}"),
        }
    }
    anyhow::ensure!(
        normalized.to_string_lossy() == workdir,
        "litebox guest workdir is not canonical: {workdir}"
    );
    Ok(())
}

fn encoded_image_reference(reference: &LiteboxImageReference) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(reference)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_REFERENCE_BYTES,
        "litebox runtime artifact reference exceeds {MAX_REFERENCE_BYTES} bytes"
    );
    Ok(bytes)
}

fn ensure_same_application_binding(
    existing: &LiteboxImageReference,
    candidate: &LiteboxImageReference,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        existing.schema == candidate.schema
            && existing.image == candidate.image
            && existing.identity == candidate.identity
            && existing.guest_workdir == candidate.guest_workdir
            && existing.app_archive_sha256 == candidate.app_archive_sha256
            && existing.package_scripts == candidate.package_scripts
            && existing.next_entry == candidate.next_entry,
        "litebox image {} is already published with a different immutable application/identity/workdir/package binding",
        candidate.image
    );
    Ok(())
}

fn validate_image_reference(
    reference: &LiteboxImageReference,
    expected_image: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        reference.schema == LITEBOX_ARTIFACT_SCHEMA,
        "{}: unsupported litebox artifact-reference schema {}",
        hive_core::fault::NODE_IMAGE_MISSING,
        reference.schema
    );
    anyhow::ensure!(
        reference.image == expected_image && valid_identity_id(&reference.image),
        "{}: litebox artifact reference does not name the exact image",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    anyhow::ensure!(
        reference.identity.protocol == hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION
            && reference.identity.id == reference.image
            && valid_sha256(&reference.identity.content_sha256),
        "{}: litebox artifact reference carries an invalid runtime identity",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    validate_guest_workdir(&reference.guest_workdir).map_err(|error| {
        anyhow::anyhow!(
            "{}: invalid litebox artifact workdir: {error:#}",
            hive_core::fault::NODE_IMAGE_MISSING
        )
    })?;
    anyhow::ensure!(
        valid_sha256(&reference.app_archive_sha256),
        "{}: litebox artifact reference carries an invalid app archive hash",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    anyhow::ensure!(
        reference.package_scripts.len() <= MAX_PACKAGE_SCRIPTS
            && reference.package_scripts.iter().all(|(name, script)| {
                name.len() <= 128 && script.len() <= MAX_PACKAGE_SCRIPT_BYTES
            }),
        "{}: litebox artifact reference carries invalid package-script metadata",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    if let Some(next) = &reference.next_entry {
        validate_guest_workdir(
            Path::new(next)
                .parent()
                .and_then(Path::to_str)
                .unwrap_or_default(),
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: invalid litebox Next entry: {error:#}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            next.ends_with("/node_modules/next/dist/bin/next"),
            "{}: litebox artifact reference carries a non-canonical Next entry",
            hive_core::fault::NODE_IMAGE_MISSING
        );
    }
    for runtime in reference.runtimes.values() {
        anyhow::ensure!(
            valid_sha256(&runtime.source_sha256) && valid_sha256(&runtime.archive_sha256),
            "{}: litebox artifact reference carries invalid runtime archive hashes",
            hive_core::fault::NODE_IMAGE_MISSING
        );
    }
    Ok(())
}

fn guest_path(relative: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(!relative.is_absolute(), "guest artifact path is absolute");
    let mut path = PathBuf::from(DELIVERED_WORKDIR);
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => path.push(value),
            _ => anyhow::bail!("guest artifact path contains traversal"),
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Cache key of a runtime archive: every input that shapes the guest's content is hashed in
/// (closure files, artifact identity, bind shim, runtime guard, guest `/etc` files and the CA
/// bundle), so changing any of them invalidates every cached archive.
async fn runtime_source_sha256(
    bin: &Path,
    deps: &[PathBuf],
    app_archive_sha256: &str,
    identity: &RuntimeArtifactIdentity,
    bind_shim: &str,
    runtime_guard: &str,
) -> anyhow::Result<String> {
    const MAX_RUNTIME_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    let mut paths = deps.to_vec();
    paths.push(bin.to_path_buf());
    paths.sort();
    paths.dedup();
    let mut hasher = Sha256::new();
    hasher.update(b"hive-litebox-runtime-source-v1\0");
    hasher.update(app_archive_sha256.as_bytes());
    let identity_bytes = serde_json::to_vec(identity)?;
    hasher.update((identity_bytes.len() as u64).to_le_bytes());
    hasher.update(&identity_bytes);
    hasher.update((bind_shim.len() as u64).to_le_bytes());
    hasher.update(bind_shim.as_bytes());
    hasher.update((runtime_guard.len() as u64).to_le_bytes());
    hasher.update(runtime_guard.as_bytes());
    hasher.update((GUEST_RESOLV_CONF.len() as u64).to_le_bytes());
    hasher.update(GUEST_RESOLV_CONF);
    hasher.update((GUEST_NSSWITCH_CONF.len() as u64).to_le_bytes());
    hasher.update(GUEST_NSSWITCH_CONF);
    if let Some(ca) = host_ca_bundle() {
        for path in GUEST_CA_PATHS {
            hasher.update((path.len() as u64).to_le_bytes());
            hasher.update(path.as_bytes());
        }
        hasher.update((ca.len() as u64).to_le_bytes());
        hasher.update(&ca);
    }
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 128 * 1024];
    for path in paths {
        anyhow::ensure!(
            path.is_absolute(),
            "litebox runtime closure contains a non-absolute path: {}",
            path.display()
        );
        let before = tokio::fs::metadata(&path).await?;
        anyhow::ensure!(
            before.is_file(),
            "litebox runtime closure contains a non-file: {}",
            path.display()
        );
        total = total.saturating_add(before.len());
        anyhow::ensure!(
            total <= MAX_RUNTIME_SOURCE_BYTES,
            "litebox runtime closure exceeds {MAX_RUNTIME_SOURCE_BYTES} bytes"
        );
        let path_bytes = path.to_string_lossy();
        hasher.update((path_bytes.len() as u64).to_le_bytes());
        hasher.update(path_bytes.as_bytes());
        hasher.update(before.len().to_le_bytes());
        let mut file = tokio::fs::File::open(&path).await?;
        let mut remaining = before.len();
        while remaining > 0 {
            let take = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..take]).await?;
            hasher.update(&buffer[..take]);
            remaining -= take as u64;
        }
        let mut extra = [0_u8; 1];
        anyhow::ensure!(
            file.read(&mut extra).await? == 0,
            "litebox runtime source grew while hashing: {}",
            path.display()
        );
        let after = file.metadata().await?;
        anyhow::ensure!(
            before.len() == after.len() && before.modified().ok() == after.modified().ok(),
            "litebox runtime source changed while hashing: {}",
            path.display()
        );
    }
    Ok(hex_sha256(&hasher.finalize()))
}

fn launch_refusal(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: litebox requires one direct Node/Bun process and refused this launch shape: {reason}; no process-manager or backend fallback was attempted",
        hive_core::fault::NODE_BACKEND_UNAVAILABLE
    )
}

/// The one refusal for "this backend cannot run Bun", shared by `start_function`'s `func.runtime`
/// check and the check after `resolve_direct_launch` (which catches `func.runtime == Node` with
/// `start_cmd[0] == "bun"`). A NODE fault (`NODE_RUNTIME_MISSING`), not a launch-shape refusal: no
/// node of this backend can run Bun, whatever is staged.
fn bun_unsupported_refusal() -> anyhow::Error {
    anyhow::anyhow!(
        "{}: litebox has no supported Bun runtime on this node — its syscall shim panics on \
         Bun's own readlink(\"/proc/self/fd/3\") boot probe (upstream unimplemented!()), so \
         only Node is supported on this backend (operator remedy: place Bun deployments on a \
         Firecracker or Mock-backed node instead; not an application fault)",
        hive_core::fault::NODE_RUNTIME_MISSING
    )
}

fn command_basename(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

fn host_interpreted_environment_key(key: &str) -> bool {
    key.starts_with("LD_")
        || key.starts_with("DYLD_")
        || key.starts_with("MALLOC_")
        || key.starts_with("LITEBOX_")
        || matches!(
            key,
            "GLIBC_TUNABLES"
                | "GCONV_PATH"
                | "GETCONF_DIR"
                | "LOCPATH"
                | "NLSPATH"
                | "LIBC_FATAL_STDERR_"
                | "LIBC_MALLOC_DEBUG"
                | "RUST_LOG"
                | "RUST_BACKTRACE"
                | "RUST_LIB_BACKTRACE"
                | "RUST_MIN_STACK"
                | "TMPDIR"
                | "TMP"
                | "TEMP"
        )
}

fn platform_environment_key(key: &str) -> bool {
    key.starts_with("HIVE_RUNTIME_")
        || matches!(
            key,
            "PORT" | "PATH" | "HOME" | "NODE_OPTIONS" | "BUN_OPTIONS"
        )
}

fn sanitized_guest_environment(func: &FunctionLaunch) -> anyhow::Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    let mut total = 0usize;
    for (key, value) in &func.env {
        anyhow::ensure!(
            !key.is_empty() && !key.contains(['\0', '=']) && !value.contains('\0'),
            "litebox tenant environment contains an invalid process-environment entry"
        );
        if host_interpreted_environment_key(key) || platform_environment_key(key) {
            continue;
        }
        total = total
            .saturating_add(key.len())
            .saturating_add(value.len() + 2);
        anyhow::ensure!(
            total <= MAX_GUEST_ENV_BYTES,
            "litebox tenant environment exceeds {MAX_GUEST_ENV_BYTES} bytes"
        );
        environment.insert(key.clone(), value.clone());
    }
    Ok(environment)
}

fn forwarded_package_args<'a>(args: &'a [String]) -> anyhow::Result<&'a [String]> {
    if args.is_empty() {
        return Ok(args);
    }
    if args.first().map(String::as_str) == Some("--") {
        return Ok(&args[1..]);
    }
    Err(launch_refusal(
        "package-manager arguments are ambiguous without a `--` separator",
    ))
}

fn strict_script_tokens(script: &str, port: u16) -> anyhow::Result<Vec<String>> {
    if script.len() > MAX_PACKAGE_SCRIPT_BYTES || !script.is_ascii() {
        return Err(launch_refusal(
            "the selected package script exceeds the bounded launch-metadata grammar",
        ));
    }
    if script.chars().any(|value| {
        matches!(
            value,
            '\0' | '\n' | '\r' | '\'' | '"' | '\\' | ';' | '&' | '|' | '<' | '>' | '`' | '(' | ')'
        )
    }) {
        return Err(launch_refusal(
            "the selected package script requires shell parsing or command composition",
        ));
    }
    let mut tokens = Vec::new();
    for token in script.split_ascii_whitespace() {
        if token == "$PORT" || token == "${PORT}" {
            tokens.push(port.to_string());
            continue;
        }
        if token
            .chars()
            .any(|value| matches!(value, '$' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '#'))
        {
            return Err(launch_refusal(
                "the selected package script requires shell expansion",
            ));
        }
        tokens.push(token.to_string());
    }
    if tokens.first().map(String::as_str) == Some("exec") {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        return Err(launch_refusal("the selected package script is empty"));
    }
    Ok(tokens)
}

fn expand_package_manager(
    argv: &[String],
    reference: &LiteboxImageReference,
    port: u16,
) -> anyhow::Result<Vec<String>> {
    let manager = command_basename(&argv[0]);
    if matches!(manager, "npx" | "bunx" | "corepack") {
        return Err(launch_refusal(format!(
            "{manager} is a process-manager wrapper"
        )));
    }
    let (script_name, remaining) = match manager {
        "npm" | "pnpm" | "yarn" => match argv.get(1).map(String::as_str) {
            Some("start") => ("start", forwarded_package_args(&argv[2..])?),
            Some("run") | Some("run-script") => {
                let name = argv.get(2).ok_or_else(|| {
                    launch_refusal(format!("{manager} run is missing a script name"))
                })?;
                (name.as_str(), forwarded_package_args(&argv[3..])?)
            }
            _ => {
                return Err(launch_refusal(format!(
                    "unsupported {manager} invocation; only a named package script can be reduced"
                )))
            }
        },
        "bun" if argv.get(1).map(String::as_str) == Some("run") => {
            let name = argv
                .get(2)
                .ok_or_else(|| launch_refusal("bun run is missing a script name"))?;
            (name.as_str(), forwarded_package_args(&argv[3..])?)
        }
        _ => return Ok(argv.to_vec()),
    };
    let script = reference.package_scripts.get(script_name).ok_or_else(|| {
        launch_refusal(format!(
            "package script {script_name:?} is absent from the validated selected application"
        ))
    })?;
    let mut direct = strict_script_tokens(script, port)?;
    direct.extend(remaining.iter().cloned());
    Ok(direct)
}

/// Validates one direct runtime entry and returns it as an absolute guest path. A relative entry is
/// joined onto `guest_workdir` and `..` is normalized lexically on the JOINED path: the entry is
/// already the resolved physical path of a package manager's symlinked `.bin/<tool>` launcher,
/// which legitimately climbs out of the app directory into a shared store inside the artifact
/// (pnpm: `../../node_modules/next/dist/bin/next`), and seeding with the workdir makes `pop()` step
/// out of a real directory and fail only above the artifact root. `validate_guest_workdir` then
/// re-checks the result.
fn validated_direct_entry(
    runtime: DirectRuntime,
    entry: &str,
    guest_workdir: &str,
) -> anyhow::Result<String> {
    if entry.is_empty() || entry.contains('\0') || entry.starts_with('-') || entry == "." {
        return Err(launch_refusal(
            "the direct runtime entry must be one explicit module path, not stdin, eval, an option, or a directory",
        ));
    }
    if runtime == DirectRuntime::Node && entry == "inspect" {
        return Err(launch_refusal(
            "Node's inspect client is not a direct application module",
        ));
    }
    if runtime == DirectRuntime::Bun
        && matches!(
            entry,
            "add"
                | "build"
                | "create"
                | "init"
                | "install"
                | "link"
                | "pm"
                | "publish"
                | "remove"
                | "repl"
                | "run"
                | "test"
                | "update"
                | "upgrade"
                | "x"
        )
    {
        return Err(launch_refusal(
            "Bun subcommands and package-script shorthand are not direct entry modules",
        ));
    }

    let path = Path::new(entry);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let mut absolute = PathBuf::from(guest_workdir);
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(value) => absolute.push(value),
                Component::ParentDir => {
                    anyhow::ensure!(
                        absolute.pop(),
                        "{}",
                        launch_refusal("the direct runtime entry escapes the validated artifact")
                    );
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(launch_refusal(
                        "the direct runtime entry is not artifact-relative",
                    ))
                }
            }
        }
        absolute
    };
    let absolute = absolute.to_str().ok_or_else(|| {
        launch_refusal("the direct runtime entry is not valid UTF-8 after normalization")
    })?;
    validate_guest_workdir(absolute).map_err(|error| {
        launch_refusal(format!(
            "the direct runtime entry escapes the validated artifact: {error:#}"
        ))
    })?;

    if runtime == DirectRuntime::Bun {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase);
        let path_shaped = entry.contains('/') || entry.starts_with("./");
        anyhow::ensure!(
            path_shaped
                || extension.as_deref().is_some_and(|extension| {
                    matches!(
                        extension,
                        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx"
                    )
                }),
            "{}",
            launch_refusal(
                "Bun's bare script-name shorthand is ambiguous; use an explicit module path"
            )
        );
    }
    Ok(absolute.to_string())
}

/// GNU tar's member-miss stderr: the deployment's own fault (its start command names a file the
/// sealed tree never had), never a node fault.
const TAR_MEMBER_MISS_STDERR: &str = "Not found in archive";

async fn validate_archive_main_entry(archive: &File, guest_entry: &str) -> anyhow::Result<()> {
    let archive_entry = Path::new(guest_entry)
        .strip_prefix("/")
        .map_err(|_| launch_refusal("the direct runtime entry is not an absolute guest path"))?;
    let mut command = Command::new("tar");
    let archive_path = crate::runtime_artifact::inherit_file_path(&mut command, archive)?;
    let output = command
        .arg("-tvf")
        .arg(archive_path)
        .arg("--")
        .arg(archive_entry)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            launch_refusal(format!(
                "could not inspect the immutable application archive: {error}"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains(TAR_MEMBER_MISS_STDERR) {
            anyhow::bail!(
                "{}: main entry {guest_entry:?} does not exist in this deployment's sealed \
                 application tree (the start_cmd names a file that was never committed or \
                 built — fix the entry in fluid.json functions[].start_cmd / package.json \
                 scripts.start, or make the build produce it); not a node fault",
                hive_core::fault::DEPLOYMENT_START_FAILED
            );
        }
        return Err(launch_refusal(format!(
            "could not inspect the immutable application archive for main entry {guest_entry:?}: {}",
            stderr.trim()
        )));
    }
    let entry_kind = output
        .stdout
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    anyhow::ensure!(
        entry_kind == Some(b'-'),
        "{}",
        launch_refusal(format!(
            "main entry {guest_entry:?} is not one regular, symlink-free application file"
        ))
    );
    Ok(())
}

fn validate_bun_arguments(args: &[String]) -> anyhow::Result<()> {
    if args.len() <= 1 {
        return Ok(());
    }
    anyhow::ensure!(
        args.get(1).map(String::as_str) == Some("--"),
        "{}",
        launch_refusal("Bun application arguments require an explicit `--` boundary so runtime flags cannot be smuggled after the entry")
    );
    Ok(())
}

fn validated_next_arguments(
    args: &[String],
    entry: String,
    port: u16,
) -> anyhow::Result<Vec<String>> {
    if args.first().map(String::as_str) != Some("start") {
        return Err(launch_refusal(
            "only the production `next start` server can be reduced",
        ));
    }
    let mut result = vec![entry, "start".to_string()];
    let mut index = 1usize;
    let mut directory_seen = false;
    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "-p" | "--port" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| launch_refusal("next start port option is missing its value"))?;
                anyhow::ensure!(
                    value == &port.to_string(),
                    "{}",
                    launch_refusal("next start may bind only the platform-assigned port")
                );
                result.push(argument.clone());
                result.push(value.clone());
                index += 2;
            }
            "--keepAliveTimeout" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    launch_refusal("next start keepAliveTimeout is missing its value")
                })?;
                let timeout = value.parse::<u32>().map_err(|_| {
                    launch_refusal("next start keepAliveTimeout is not a bounded integer")
                })?;
                anyhow::ensure!(
                    timeout > 0,
                    "{}",
                    launch_refusal("next start keepAliveTimeout must be positive")
                );
                result.push(argument.clone());
                result.push(value.clone());
                index += 2;
            }
            _ if argument
                .strip_prefix("--port=")
                .is_some_and(|value| value == port.to_string()) =>
            {
                result.push(argument.clone());
                index += 1;
            }
            _ if argument.starts_with("--keepAliveTimeout=") => {
                let value = argument.trim_start_matches("--keepAliveTimeout=");
                let timeout = value.parse::<u32>().map_err(|_| {
                    launch_refusal("next start keepAliveTimeout is not a bounded integer")
                })?;
                anyhow::ensure!(
                    timeout > 0,
                    "{}",
                    launch_refusal("next start keepAliveTimeout must be positive")
                );
                result.push(argument.clone());
                index += 1;
            }
            _ if argument.starts_with('-') => {
                return Err(launch_refusal(format!(
                    "next start option {argument:?} is outside the direct-server allowlist"
                )))
            }
            _ => {
                anyhow::ensure!(
                    !directory_seen,
                    "{}",
                    launch_refusal("next start accepts at most one explicit application directory")
                );
                let path = Path::new(argument);
                anyhow::ensure!(
                    !path.is_absolute()
                        && path.components().all(|component| {
                            matches!(component, Component::Normal(_) | Component::CurDir)
                        }),
                    "{}",
                    launch_refusal("next start application directory escapes the artifact workdir")
                );
                directory_seen = true;
                result.push(argument.clone());
                index += 1;
            }
        }
    }
    Ok(result)
}

/// The one Node runtime flag the platform's own exported-server launcher emits (TypeScript
/// entries): a loader toggle with no eval/exec/stdin semantics, so allowing exactly it ahead of the
/// entry does not widen the launch grammar; every other option still refuses on the leading dash.
const NODE_STRIP_TYPES_FLAG: &str = "--experimental-strip-types";

async fn resolve_direct_launch(
    func: &FunctionLaunch,
    reference: &LiteboxImageReference,
    app_archive: &File,
) -> anyhow::Result<DirectLaunch> {
    let argv = expand_package_manager(&func.start_cmd, reference, func.port)?;
    let command = argv
        .first()
        .ok_or_else(|| launch_refusal("the reduced command is empty"))?;
    let base = command_basename(command);
    if matches!(
        base,
        "npm" | "pnpm" | "yarn" | "npx" | "bunx" | "corepack" | "turbo"
    ) {
        return Err(launch_refusal(format!(
            "nested process-manager command {base:?} cannot be executed by litebox"
        )));
    }
    let mut args = argv[1..].to_vec();
    let (runtime, name) = match base {
        "node" | "nodejs" => {
            anyhow::ensure!(
                !args.is_empty(),
                "{}",
                launch_refusal("direct Node launch is missing an entry module")
            );
            (DirectRuntime::Node, "node")
        }
        "bun" => {
            anyhow::ensure!(
                !args.is_empty(),
                "{}",
                launch_refusal("direct Bun launch is missing an entry module")
            );
            (DirectRuntime::Bun, "bun")
        }
        "next" => {
            let entry = reference.next_entry.clone().ok_or_else(|| {
                launch_refusal("the validated artifact does not contain a direct Next CLI entry")
            })?;
            args = validated_next_arguments(&args, entry, func.port)?;
            (DirectRuntime::Node, "node")
        }
        _ => {
            return Err(launch_refusal(format!(
                "command {base:?} is not a direct Node/Bun runtime"
            )))
        }
    };
    let mut entry_index = 0usize;
    if runtime == DirectRuntime::Node {
        while args
            .get(entry_index)
            .is_some_and(|arg| arg == NODE_STRIP_TYPES_FLAG)
        {
            entry_index += 1;
        }
        anyhow::ensure!(
            entry_index < args.len(),
            "{}",
            launch_refusal("direct Node launch is missing an entry module after runtime flags")
        );
    }
    let entry = validated_direct_entry(runtime, &args[entry_index], &reference.guest_workdir)?;
    validate_archive_main_entry(app_archive, &entry).await?;
    args[entry_index] = entry;
    if runtime == DirectRuntime::Bun {
        validate_bun_arguments(&args)?;
    }
    let bin = resolve_bin(name).await;
    let bin = tokio::fs::canonicalize(&bin).await.unwrap_or(bin);
    Ok(DirectLaunch { runtime, bin, args })
}

/// Kills and reaps a still-running guest child on any exit from the scope it is armed in: an `Err`,
/// an early `?`, or the whole future being dropped because the caller gave up. `kill_on_drop`
/// signals but never reaps, so abandoned cold starts left zombie runners; `disarm()` on the success
/// path hands the child back (`terminate()` reaps it then).
struct ChildReapGuard<'a> {
    child: Option<&'a mut Child>,
}

impl<'a> ChildReapGuard<'a> {
    fn new(child: &'a mut Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_deref_mut().expect("guard already disarmed")
    }

    fn disarm(mut self) {
        self.child = None;
    }
}

/// `Drop` cannot await, so the reap runs in a detached task: it still happens when this drop is
/// itself part of another future's cancellation teardown.
impl Drop for ChildReapGuard<'_> {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        if let Some(pid) = child.id() {
            tokio::spawn(async move {
                // SAFETY: reaping our own just-killed child by raw pid — the
                // `Child` handle itself cannot be moved into this task
                // because it is still borrowed by the guard's caller scope.
                let mut status = 0;
                unsafe {
                    libc::waitpid(pid as libc::pid_t, &mut status, 0);
                }
            });
        }
    }
}

async fn wait_litebox_ready(
    child: &mut Child,
    address: &str,
    budget: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.code() == Some(RUNTIME_GUARD_EXIT_CODE) {
                anyhow::bail!(
                    "{}: litebox guest rejected the exact runtime artifact before tenant execution",
                    hive_core::fault::NODE_IMAGE_MISSING
                );
            }
            anyhow::bail!(
                "{}: litebox direct runtime exited before listening on {address} ({status})",
                hive_core::fault::DEPLOYMENT_START_FAILED
            );
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "{}: litebox direct runtime did not listen on {address} within {}s",
                hive_core::fault::DEPLOYMENT_START_FAILED,
                budget.as_secs()
            );
        }
        let connect_budget = (deadline - now).min(Duration::from_millis(250));
        if tokio::time::timeout(connect_budget, tokio::net::TcpStream::connect(address))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

impl Default for LiteboxBackend {
    fn default() -> Self {
        LiteboxBackend::new(LiteboxConfig::default())
    }
}

/// Resets `SIGINT`/`SIGALRM` to `SIG_DFL` in the forked child before exec: litebox's
/// `register_exception_handlers` asserts the previous disposition was the default and crashes
/// otherwise, and a child of a process run without a tty (SSH without `-t`, systemd) commonly
/// inherits a non-default SIGINT.
fn reset_signal_dispositions_before_exec(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            for sig in [libc::SIGINT, libc::SIGALRM] {
                if libc::signal(sig, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Resolves a runtime binary name to the absolute host path the litebox CLI can open: the CLI does
/// its own `open()` with no `execvp`-style PATH search, so a bare `node` (the usual `start_cmd[0]`)
/// must be resolved here first.
async fn resolve_bin(name: &str) -> PathBuf {
    if name.contains('/') {
        return PathBuf::from(name);
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join(name);
            if tokio::fs::metadata(&candidate)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

/// The absolute-path shared-library closure of `bin`: its interpreter and every `NEEDED` entry
/// `ldd` resolves, without the VDSO. A static binary (`ldd` exits non-zero) is zero deps,
/// `Ok(vec![])`, not an error.
async fn ldd_closure(bin: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let out = Command::new("ldd")
        .arg(bin)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run ldd on {}: {e}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let diagnostic = format!("{stdout}\n{stderr}");
    if !out.status.success() {
        if diagnostic.contains("not a dynamic executable")
            || diagnostic.contains("statically linked")
        {
            return Ok(Vec::new());
        }
        anyhow::bail!(
            "{}: ldd could not resolve the runtime closure for {}: {}",
            hive_core::fault::NODE_RUNTIME_MISSING,
            bin.display(),
            diagnostic.trim()
        );
    }
    anyhow::ensure!(
        !diagnostic.lines().any(|line| line.contains("=> not found")),
        "{}: the runtime closure for {} has an unresolved shared library: {}",
        hive_core::fault::NODE_RUNTIME_MISSING,
        bin.display(),
        diagnostic.trim()
    );
    let mut paths = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(idx) = line.find("=> ") {
            if let Some(p) = line[idx + 3..].split_whitespace().next() {
                if p.starts_with('/') {
                    let validated = validate_ldd_path(Path::new(p))?;
                    paths.push(validated);
                }
            }
        } else if line.starts_with('/') {
            if let Some(p) = line.split_whitespace().next() {
                let validated = validate_ldd_path(Path::new(p))?;
                paths.push(validated);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Legacy sonames modern glibc folded into libc: native addons in the application tree (such as
/// `@next/swc-linux-x64-gnu`) and coreutils builds linked against the split libraries still list
/// them as NEEDED, so the guest's `dlopen` fails unless the compat stubs the host ships are staged
/// with the closure.
const GLIBC_COMPAT_STUBS: [&str; 4] = [
    "/lib64/libpthread.so.0",
    "/lib64/libdl.so.2",
    "/lib64/librt.so.1",
    "/lib64/libutil.so.1",
];

/// Adds the compat stubs this host ships to a closure; an absent stub is skipped, never an error.
fn add_glibc_compat_stubs(deps: &mut Vec<PathBuf>) {
    for stub in GLIBC_COMPAT_STUBS {
        let path = PathBuf::from(stub);
        if !deps.contains(&path)
            && std::path::Path::new(stub)
                .metadata()
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        {
            deps.push(path);
        }
    }
}

/// Directories an `ldd`-reported library must resolve into; anything else is rejected before it
/// reaches a filesystem operation (defense in depth).
const ALLOWED_LIBRARY_DIRS: &[&str] = &[
    "/usr/lib/",
    "/lib/",
    "/lib64/",
    "/usr/lib64/",
    "/usr/local/lib/",
];

fn validate_ldd_path(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve LDD path {}: {e}", path.display()))?;
    let resolved_str = resolved.to_string_lossy();
    let allowed = ALLOWED_LIBRARY_DIRS
        .iter()
        .any(|prefix| resolved_str.starts_with(prefix));
    anyhow::ensure!(
        allowed,
        "LDD path {} resolves to {} which is outside the approved library directory allowlist",
        path.display(),
        resolved.display()
    );
    Ok(path.to_path_buf())
}

async fn append_litebox_runtime_augmentation(
    archive: File,
    deps: Vec<PathBuf>,
    identity: Option<RuntimeArtifactIdentity>,
    runtime_bin: Option<PathBuf>,
    extra_entries: Vec<(PathBuf, Vec<u8>)>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        append_litebox_runtime_augmentation_blocking(
            archive,
            &deps,
            identity.as_ref(),
            runtime_bin.as_deref(),
            &extra_entries,
        )
    })
    .await
    .context("litebox runtime tar augmentation task failed")?
}

/// A ustar header, never GNU: litebox's tar reader understands the ustar `prefix`+`name` split for
/// paths past 100 bytes (up to 100+155) but not GNU's long-name extension entry, which it misparses
/// as a file.
#[cfg(target_os = "linux")]
fn litebox_tar_header(length: u64, mode: u32) -> anyhow::Result<tar::Header> {
    let mut header = tar::Header::new_ustar();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(mode & 0o777);
    header.set_size(length);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_username("")?;
    header.set_groupname("")?;
    header.set_cksum();
    Ok(header)
}

#[cfg(target_os = "linux")]
fn append_platform_tar_entry(
    builder: &mut tar::Builder<File>,
    path: &Path,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let mut header = litebox_tar_header(bytes.len() as u64, 0o444)?;
    builder
        .append_data(&mut header, path, bytes)
        .with_context(|| format!("append litebox platform file {}", path.display()))?;
    Ok(())
}

/// Guest resolver config, planted because the tar otherwise ships none and every `getaddrinfo`
/// fails with EAI_AGAIN. Two public resolvers, reachable once `ensure_guest_nat` has installed the
/// masquerade (the guest has none of its own and the host runs none on the `10.88/16` link).
/// `use-vc` forces glibc onto TCP: a litebox guest has no UDP egress, so without it this config is
/// correct and still unusable.
const GUEST_RESOLV_CONF: &[u8] =
    b"nameserver 1.1.1.1\nnameserver 8.8.8.8\noptions timeout:2 attempts:2 use-vc\n";
/// Explicit lookup order so it is identical on every base image: `files` first,
/// so /etc/hosts still wins, then `dns`.
const GUEST_NSSWITCH_CONF: &[u8] = b"hosts: files dns\n";

/// Where the guest's TLS stacks look for CA roots; distros disagree and OpenSSL uses its
/// compiled-in path, not ours, so the bundle is planted at every spelling. The last is the
/// compiled-in default of RHEL-family OpenSSL, which Python's `ssl` reads.
const GUEST_CA_PATHS: [&str; 3] = [
    "etc/ssl/certs/ca-certificates.crt",
    "etc/pki/tls/certs/ca-bundle.crt",
    "etc/pki/tls/cert.pem",
];

const PEM_CERTIFICATE_MARKER: &[u8; 27] = b"-----BEGIN CERTIFICATE-----";
const MIN_CA_BUNDLE_BYTES: usize = 16 * 1024;

/// A real bundle holds many PEM certificates; the 49-byte stubs some images leave at the well-known
/// paths do not. Non-emptiness shipped such a stub into every guest and the certificate error
/// persisted, so require a BEGIN CERTIFICATE marker and a realistic size.
fn is_real_ca_bundle(bytes: &[u8]) -> bool {
    bytes.len() > MIN_CA_BUNDLE_BYTES && bytes.windows(27).any(|w| w == *PEM_CERTIFICATE_MARKER)
}

/// Host CA bundle candidates, best first: the assembled RHEL/TencentOS trust store, then the compat
/// stubs that can be near-empty.
const HOST_CA_BUNDLE_PATHS: [&str; 5] = [
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/cert.pem",
    "/etc/ssl/cert.pem",
];

/// The host's CA bundle, best-effort: without roots in the guest Node cannot validate any server
/// certificate (`UNABLE_TO_GET_ISSUER_CERT_LOCALLY`), which breaks server-side auth and every HTTPS
/// call a tenant app makes. `None` is not an error: the guest keeps the roots its base image has.
fn host_ca_bundle() -> Option<Vec<u8>> {
    for path in HOST_CA_BUNDLE_PATHS {
        if let Ok(bytes) = std::fs::read(path) {
            if is_real_ca_bundle(&bytes) {
                return Some(bytes);
            }
        }
    }
    None
}

/// Absolute path of the bundle inside the guest, for `NODE_EXTRA_CA_CERTS`.
const GUEST_CA_BUNDLE_PATH: &str = "/etc/ssl/certs/ca-certificates.crt";

/// `Some(("NODE_EXTRA_CA_CERTS", path))` when a real bundle gets planted, else `None`. Node reads
/// the variable whatever OpenSSL's compiled-in default path is, and `.envs()` adds nothing for
/// `None`, so a host without a readable trust store never points Node at a missing file.
fn guest_ca_env() -> Option<(String, String)> {
    host_ca_bundle().map(|_| {
        (
            "NODE_EXTRA_CA_CERTS".to_string(),
            GUEST_CA_BUNDLE_PATH.to_string(),
        )
    })
}

#[cfg(target_os = "linux")]
const TAR_BLOCK_BYTES: u64 = 512;
#[cfg(target_os = "linux")]
const TAR_END_MARKER_BLOCKS: u64 = 2;

/// Start of the trailing zero run: `tar -cf` pads to its blocking factor, so the true terminator is
/// not the last 1024 bytes.
#[cfg(target_os = "linux")]
fn trailing_zero_run_start(archive: &mut File, total_blocks: u64) -> anyhow::Result<u64> {
    let mut zero_block = [0_u8; TAR_BLOCK_BYTES as usize];
    let mut start_block = total_blocks;
    let mut index = total_blocks;
    while index > 0 {
        let candidate = index - 1;
        archive.seek(SeekFrom::Start(candidate * TAR_BLOCK_BYTES))?;
        archive.read_exact(&mut zero_block)?;
        if zero_block.iter().any(|byte| *byte != 0) {
            break;
        }
        start_block = candidate;
        index = candidate;
    }
    Ok(start_block)
}

/// Appends the runtime layer to a canonical tar and re-terminates it: the `ldd` closure and the
/// runtime binary (opened `openat2` beneath `/`, no magic links, re-stat'd after the copy), the
/// artifact marker, `extra_entries` (guest path relative to `/`, bytes, 0o444; the probe plants a
/// tar-layer directory its guest then writes into), the resolver and CA files and the two preloads.
/// The runtime binary goes in at its exact launch path because a guest-side `execve` (Next.js
/// spawning its worker, an app re-execing `process.execPath`) resolves through the guest
/// filesystem; unlike libraries it is exempt from the library allowlist.
#[cfg(target_os = "linux")]
fn append_litebox_runtime_augmentation_blocking(
    mut archive: File,
    deps: &[PathBuf],
    identity: Option<&RuntimeArtifactIdentity>,
    runtime_bin: Option<&Path>,
    extra_entries: &[(PathBuf, Vec<u8>)],
) -> anyhow::Result<()> {
    let metadata = archive.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.len() >= TAR_END_MARKER_BLOCKS * TAR_BLOCK_BYTES
            && metadata.len() % TAR_BLOCK_BYTES == 0,
        "litebox application authority is not a canonical block-aligned tar"
    );
    let total_blocks = metadata.len() / TAR_BLOCK_BYTES;
    let terminator_start_block = trailing_zero_run_start(&mut archive, total_blocks)?;
    anyhow::ensure!(
        total_blocks - terminator_start_block >= TAR_END_MARKER_BLOCKS,
        "litebox application authority has no exact two-block tar terminator"
    );
    let prefix_bytes = terminator_start_block * TAR_BLOCK_BYTES;
    archive.set_len(prefix_bytes)?;
    archive.seek(SeekFrom::Start(prefix_bytes))?;

    let root = File::open("/").context("open root filesystem")?;
    let mut seen_inodes: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut builder = tar::Builder::new(archive);
    builder.mode(tar::HeaderMode::Deterministic);
    builder.follow_symlinks(false);
    for dep in deps {
        let relative = dep
            .strip_prefix("/")
            .map_err(|_| anyhow::anyhow!("LDD path {} is not absolute", dep.display()))?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "LDD path is the root directory"
        );
        let mut file = crate::runtime_artifact::openat2_required(
            &root,
            relative.as_os_str(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            crate::runtime_artifact::RESOLVE_BENEATH
                | crate::runtime_artifact::RESOLVE_NO_MAGICLINKS,
        )
        .with_context(|| format!("openat2 failed for {}", dep.display()))?;
        let resolved = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .canonicalize()
            .with_context(|| format!("cannot resolve real path for {}", dep.display()))?;
        let resolved_str = resolved.to_string_lossy();
        anyhow::ensure!(
            ALLOWED_LIBRARY_DIRS
                .iter()
                .any(|prefix| resolved_str.starts_with(prefix)),
            "LDD path {} resolves to {} which is outside the approved library directory allowlist",
            dep.display(),
            resolved.display()
        );
        let before = file
            .metadata()
            .with_context(|| format!("cannot stat {}", dep.display()))?;
        anyhow::ensure!(
            before.is_file(),
            "LDD path is not a file: {}",
            dep.display()
        );
        anyhow::ensure!(
            seen_inodes.insert((before.dev(), before.ino())),
            "duplicate LDD inode detected while staging {} (dev={}, ino={})",
            dep.display(),
            before.dev(),
            before.ino()
        );
        let mut header = litebox_tar_header(before.len(), before.mode())?;
        builder
            .append_data(&mut header, relative, &mut file)
            .with_context(|| format!("append linked library {}", dep.display()))?;
        let after = file.metadata()?;
        anyhow::ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mode() == after.mode()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "linked library changed while staging: {}",
            dep.display()
        );
    }

    if let Some(bin) = runtime_bin {
        let relative = bin
            .strip_prefix("/")
            .map_err(|_| anyhow::anyhow!("runtime binary {} is not absolute", bin.display()))?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "runtime binary path is the root directory"
        );
        let mut file = crate::runtime_artifact::openat2_required(
            &root,
            relative.as_os_str(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            crate::runtime_artifact::RESOLVE_BENEATH
                | crate::runtime_artifact::RESOLVE_NO_MAGICLINKS,
        )
        .with_context(|| format!("openat2 failed for runtime binary {}", bin.display()))?;
        let before = file
            .metadata()
            .with_context(|| format!("cannot stat runtime binary {}", bin.display()))?;
        anyhow::ensure!(
            before.is_file(),
            "runtime binary is not a file: {}",
            bin.display()
        );
        if seen_inodes.insert((before.dev(), before.ino())) {
            let mut header = litebox_tar_header(before.len(), before.mode())?;
            builder
                .append_data(&mut header, relative, &mut file)
                .with_context(|| format!("append runtime binary {}", bin.display()))?;
            let after = file.metadata()?;
            anyhow::ensure!(
                before.dev() == after.dev()
                    && before.ino() == after.ino()
                    && before.len() == after.len()
                    && before.mode() == after.mode()
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec(),
                "runtime binary changed while staging: {}",
                bin.display()
            );
        }
    }

    if let Some(identity) = identity {
        let identity_bytes = serde_json::to_vec(identity)?;
        append_platform_tar_entry(
            &mut builder,
            &Path::new("workspace").join(hive_core::RUNTIME_ARTIFACT_MARKER_FILE),
            &identity_bytes,
        )?;
    }
    for (path, bytes) in extra_entries {
        append_platform_tar_entry(&mut builder, path, bytes)?;
    }
    append_platform_tar_entry(
        &mut builder,
        Path::new("etc/resolv.conf"),
        GUEST_RESOLV_CONF,
    )?;
    append_platform_tar_entry(
        &mut builder,
        Path::new("etc/nsswitch.conf"),
        GUEST_NSSWITCH_CONF,
    )?;
    if let Some(ca) = host_ca_bundle() {
        for path in GUEST_CA_PATHS {
            append_platform_tar_entry(&mut builder, Path::new(path), &ca)?;
        }
    }
    append_platform_tar_entry(
        &mut builder,
        Path::new(GUEST_BIND_SHIM_PATH.trim_start_matches('/')),
        NODE_BIND_SHIM_JS.as_bytes(),
    )?;
    append_platform_tar_entry(
        &mut builder,
        Path::new(GUEST_RUNTIME_GUARD_PATH.trim_start_matches('/')),
        RUNTIME_GUARD_JS.as_bytes(),
    )?;
    builder
        .finish()
        .context("finish litebox combined runtime tar")?;
    let archive = builder
        .into_inner()
        .context("close litebox combined runtime tar writer")?;
    archive.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn append_litebox_runtime_augmentation_blocking(
    _archive: File,
    _deps: &[PathBuf],
    _identity: Option<&RuntimeArtifactIdentity>,
    _runtime_bin: Option<&Path>,
    _extra_entries: &[(PathBuf, Vec<u8>)],
) -> anyhow::Result<()> {
    anyhow::bail!("descriptor-relative tar augmentation requires Linux openat2")
}

/// The fixed set of programs the interactive shell itself may `exec()` in the guest, each staged
/// with its exact host path and `ldd` closure: only the first exec loads from the host, and later
/// guest-side execs resolve through the staged tar. Deliberately small and closed, never "whatever
/// is on `$PATH`": a missing `grep` fails loudly, while silently running anything the host has
/// installed would be a large unaudited surface.
const SHELL_GUEST_PROGRAMS: &[&str] = &[
    "/bin/sh",
    "/usr/bin/ls",
    "/usr/bin/cat",
    "/usr/bin/pwd",
    "/usr/bin/echo",
    "/usr/bin/mkdir",
    "/usr/bin/rm",
    "/usr/bin/cp",
    "/usr/bin/mv",
    "/usr/bin/grep",
    "/usr/bin/head",
    "/usr/bin/tail",
    "/usr/bin/wc",
    "/usr/bin/find",
    "/usr/bin/env",
];

/// The interactive shell's rc file path, named by `ENV` (bash-as-sh, dash and ksh read it for an
/// interactive shell after job-control setup). Deliberately NOT under `$HOME`: a tar entry
/// implicitly creates its parent, and a `/root` the guest uid cannot write makes the history write
/// at `exit` hit a permission check litebox unwraps (runner exit 101), where a missing directory
/// gives a silent ENOENT; `HISTFILE` is cleared for the same reason.
const SHELL_RC_GUEST_PATH: &str = "/usr/share/hive/shellrc";
/// `litebox-shellrc.sh`: its `exec 2>&1` moves the shell's stderr onto the pty (the runner's own
/// stderr must stay a pipe, since a pty stderr kills it at job-control init, and a separate pipe
/// and pty are read by two tasks, so the next prompt overtook the previous command's output), and a
/// no-fork "command not found" guard: interactive bash forks before it looks a command up, which
/// under the fork emulation kills the child and silences the session, so an `extdebug` DEBUG trap
/// checks the command word with `command -v` in the parent. Limits: a skipped command leaves `$?`
/// at 0, a quoted or expanded first word is not checked, and `sh -c` runs read no rc.
#[cfg(target_os = "linux")]
const SHELL_RC_BYTES: &[u8] = include_bytes!("litebox-shellrc.sh");

/// Staged only where the host has them (an absent one is skipped with a debug line, never an exec
/// failure; the fixed set above stays strict): the rest of the POSIX tool set a shell one-liner
/// reaches for, still a closed list. Completeness is hang-avoidance as much as convenience, because
/// a command the guest cannot find is dangerous under the fork emulation (see `SHELL_RC_BYTES`).
/// Entries may be symlinks the staging `openat2` dereferences (Rocky's alternatives-managed `awk`),
/// staged under their `/usr/bin` path.
const SHELL_GUEST_OPTIONAL_PROGRAMS: &[&str] = &[
    "/usr/bin/uname",
    "/usr/bin/id",
    "/usr/bin/whoami",
    "/usr/bin/hostname",
    "/usr/bin/printf",
    "/usr/bin/true",
    "/usr/bin/false",
    "/usr/bin/test",
    "/usr/bin/[",
    "/usr/bin/sleep",
    "/usr/bin/date",
    "/usr/bin/touch",
    "/usr/bin/ln",
    "/usr/bin/chmod",
    "/usr/bin/sed",
    "/usr/bin/awk",
    "/usr/bin/sort",
    "/usr/bin/uniq",
    "/usr/bin/cut",
    "/usr/bin/tr",
    "/usr/bin/xargs",
    "/usr/bin/tee",
    "/usr/bin/basename",
    "/usr/bin/dirname",
    "/usr/bin/stat",
    "/usr/bin/du",
    "/usr/bin/df",
    "/usr/bin/readlink",
    "/usr/bin/realpath",
    "/usr/bin/seq",
    "/usr/bin/sha256sum",
    "/usr/bin/md5sum",
    "/usr/bin/base64",
    "/usr/bin/which",
    "/usr/bin/tar",
    "/usr/bin/gzip",
];

/// The toolchain a sandbox shell (and a `shell: true` exec) offers: `python3`, `git`, `curl` and
/// `wget`; `npx` rides the node support tree. Closed like the lists above and staged optionally, so
/// a base image missing `wget` still boots a shell. Unlike them these load runtime data that `ldd`
/// never names, which `support_plan_for` stages (python's stdlib and `lib-dynload` closure, git's
/// helpers and templates); `curl` and `wget` are ordinary ELFs. A host symlink is staged as the
/// real file under its canonical path plus a matching symlink entry (`/usr/bin/node` ->
/// `/usr/bin/node-22`), so a shebang naming the real interpreter resolves.
const SHELL_GUEST_TOOL_PROGRAMS: &[&str] = &[
    "/usr/bin/python3",
    "/usr/bin/git",
    "/usr/bin/curl",
    "/usr/bin/wget",
];

/// Shared objects glibc `dlopen`s out of `getaddrinfo`, which no `ldd` output names: without them
/// nothing resolving through libc (`curl`, `wget`, python's `socket`, `git`) finds a resolver in
/// the guest. Staged only where the host has them.
const GUEST_NSS_LIBS: &[&str] = &[
    "/lib64/libnss_files.so.2",
    "/lib64/libnss_dns.so.2",
    "/lib64/libresolv.so.2",
];

/// Host directories a bare `exec_command` name is resolved against: the guest `PATH` plus
/// `/usr/local/bin` for operator-installed runtimes. The resolved binary is staged with its `ldd`
/// closure like the fixed shell set, so a command the host has runs in the guest and one it lacks
/// fails loudly at start, never mid-run with a bare ENOENT.
const EXEC_PROGRAM_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin"];

/// Bound on one `ExecOutput` line's bytes before truncation (the provider bounds again at 16 KiB),
/// so a newline-free firehose cannot grow the pump's buffer without limit.
const EXEC_MAX_LINE_BYTES: usize = 64 * 1024;

/// Resolves the program a sandbox `exec_command` names to the HOST binary the runner loads (only
/// the first exec loads from the host; later guest-side `execve`s resolve through the staged tar,
/// which is why the caller stages this same path). A relative path is refused: the guest's cwd and
/// the host's are different filesystems.
fn resolve_guest_program(cmd: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !cmd.trim().is_empty() && !cmd.contains('\0'),
        "sandbox command must be a non-empty program name"
    );
    if cmd.contains('/') {
        let path = PathBuf::from(cmd);
        anyhow::ensure!(
            path.is_absolute(),
            "sandbox command path must be absolute inside a litebox guest: {cmd}"
        );
        anyhow::ensure!(
            path.is_file(),
            "command {cmd} is not available inside this litebox sandbox"
        );
        return Ok(path);
    }
    for dir in EXEC_PROGRAM_DIRS {
        let candidate = Path::new(dir).join(cmd);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "command '{cmd}' is not available inside this litebox sandbox (searched {})",
        EXEC_PROGRAM_DIRS.join(":")
    )
}

const SHEBANG_SCAN_MAX_BYTES: u64 = 1024 * 1024;

/// The interpreter a file's shebang names, as the program plus its optional single argument
/// (`#!/usr/bin/env node` -> `["/usr/bin/env", "node"]`); `None` unless `path` is a `#!/…` script.
/// A file over `SHEBANG_SCAN_MAX_BYTES` is never read.
fn shebang_interpreter(path: &Path) -> Option<Vec<String>> {
    if std::fs::metadata(path).ok()?.len() > SHEBANG_SCAN_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if !bytes.starts_with(b"#!") {
        return None;
    }
    let end = bytes
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(bytes.len());
    let line = std::str::from_utf8(&bytes[2..end]).ok()?;
    let parts: Vec<String> = line.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() || !parts[0].starts_with('/') {
        return None;
    }
    Some(parts)
}

/// Resolves a NON-shell `exec_command` to the `(program, argv)` the runner is spawned with. The
/// runner's rewriter accepts only an ELF as the first program (a `#!/…` script fails with `Unknown
/// file magic`) while the guest's own `execve` honours shebangs, so a shebang script (`npx`, `npm`)
/// starts as `<interpreter> [<arg>] <script> <args>`, as the kernel would have. The script is
/// passed by its canonical path: the guest's `/usr/bin/npx` is the synthesized exec wrapper
/// (`build_shell_tar_blocking`'s `copies`), which node would parse as JavaScript.
fn resolve_guest_argv(cmd: &str, args: &[String]) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let program = resolve_guest_program(cmd)?;
    let Some(interpreter) = shebang_interpreter(&program) else {
        return Ok((program, args.to_vec()));
    };
    let mut argv = Vec::with_capacity(interpreter.len() + args.len() + 1);
    argv.extend(interpreter);
    argv.push(
        canonical_target(&program)
            .unwrap_or_else(|| program.clone())
            .display()
            .to_string(),
    );
    argv.extend(args.iter().cloned());
    Ok((PathBuf::from(&argv[0]), argv[1..].to_vec()))
}

/// Pumps one exec pipe into `ExecOutput` events, one per line, each bounded to
/// `EXEC_MAX_LINE_BYTES`. Keeps DRAINING (discarding) after the receiver goes away: a guest blocked
/// on a full pipe would otherwise never exit and never be reaped, leaking the runner until
/// `kill_exec`/`terminate`.
async fn pump_exec_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    stream: hive_core::LogStream,
    id: String,
    tx: tokio::sync::mpsc::UnboundedSender<hive_core::AgentEvent>,
) {
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = [0_u8; 8192];
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    let mut discard = false;
    let mut emit = |line: &[u8], discard: &mut bool| {
        if *discard {
            return;
        }
        let mut line = line;
        while line.last().is_some_and(|b| *b == b'\r') {
            line = &line[..line.len() - 1];
        }
        let text = if line.len() > EXEC_MAX_LINE_BYTES {
            let mut end = EXEC_MAX_LINE_BYTES;
            while end > 0 && std::str::from_utf8(&line[..end]).is_err() {
                end -= 1;
            }
            format!("{}… [truncated]", String::from_utf8_lossy(&line[..end]))
        } else {
            String::from_utf8_lossy(line).into_owned()
        };
        if tx
            .send(hive_core::AgentEvent::ExecOutput {
                id: id.clone(),
                stream,
                line: text,
            })
            .is_err()
        {
            *discard = true;
        }
    };
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=nl).collect();
                    emit(&line[..line.len() - 1], &mut discard);
                }
                if pending.len() > EXEC_MAX_LINE_BYTES {
                    let line: Vec<u8> = pending.drain(..).collect();
                    emit(&line, &mut discard);
                }
            }
        }
    }
    if !pending.is_empty() {
        emit(&pending, &mut discard);
    }
}

/// A support tree plus the `$PATH`-visible entry points that make its commands reachable by name:
/// the guest shell resolves a typed `npm` through `PATH=/usr/bin:/bin`, and on the host
/// `/usr/bin/npm` is a symlink into the tree (one the walk skips), so without an entry at
/// `/usr/bin/npm` the search finds nothing. `entry_points` are `(guest_path, real_target)`,
/// `real_target` being a path the walk already stages; the entry is written as a REAL FILE carrying
/// its bytes, because a symlink entry point is invisible to the guest's `PATH` search
/// (`build_shell_tar_blocking`'s `copies`). An entry point is staged only when the host's symlink
/// canonicalizes to exactly `real_target` (`readlink` text is often relative), so a base-image
/// change that moves the real file stages nothing rather than a copy of nothing.
struct SupportPlan {
    trees: Vec<PathBuf>,
    entry_points: Vec<(PathBuf, PathBuf)>,
}

/// Support trees a resolved `extra` program needs beyond its `ldd` closure: `node` and `python3`
/// are interpreters whose standard library is a tree of plain files. Keyed by the resolved
/// program's file NAME (`resolve_guest_program` already turned `node` into `/usr/bin/node`); a tree
/// the host lacks is skipped harmlessly (see `walk_support_tree`). `git` brings its helpers and
/// templates, with `git-remote-https` (a symlink the walk skips) as an explicit entry point.
/// `python3.x` takes the first existing of `/usr/lib64/python3.x` and `/usr/lib/python3.x`: lib64
/// comes first because on RHEL-family hosts `/usr/lib/python3.x` holds site-packages only (no
/// `os.py`, no `encodings/`), which would leave the interpreter unable to boot. `node` takes the
/// bundled npm tree, since `npm` and `npx` are scripts under it (`npm/bin/*-cli.js`), not binaries:
/// staging `node` alone leaves `npm install` not found.
fn support_plan_for(resolved: &Path) -> Option<SupportPlan> {
    let name = resolved.file_name().and_then(|n| n.to_str())?;
    if name == "git" {
        return Some(SupportPlan {
            trees: vec![
                PathBuf::from("/usr/libexec/git-core"),
                PathBuf::from("/usr/share/git-core"),
            ],
            entry_points: vec![(
                PathBuf::from("/usr/libexec/git-core/git-remote-https"),
                PathBuf::from("/usr/libexec/git-core/git-remote-http"),
            )],
        });
    }
    if let Some(version) = name.strip_prefix("python") {
        if version.starts_with('3') {
            return [
                PathBuf::from(format!("/usr/lib64/python{version}")),
                PathBuf::from(format!("/usr/lib/python{version}")),
            ]
            .into_iter()
            .find(|dir| dir.is_dir())
            .map(|dir| SupportPlan {
                trees: vec![dir],
                entry_points: Vec::new(),
            });
        }
    }
    match name {
        "node" | "node-22" | "node-24" | "node-26" => Some(SupportPlan {
            trees: vec![PathBuf::from("/usr/lib/node_modules_22")],
            entry_points: vec![
                (
                    PathBuf::from("/usr/bin/npm"),
                    PathBuf::from("/usr/lib/node_modules_22/npm/bin/npm-cli.js"),
                ),
                (
                    PathBuf::from("/usr/bin/npx"),
                    PathBuf::from("/usr/lib/node_modules_22/npm/bin/npx-cli.js"),
                ),
            ],
        }),
        _ => None,
    }
}

/// Bound on how many of a support tree's own shared objects get their `ldd` closure resolved.
/// Python's `lib-dynload/*.so` extensions (59 of them) carry `NEEDED` entries (libssl, libffi,
/// liblzma, ...) that `ldd` of the interpreter never reports, and without them `import ssl` fails
/// in the guest; the bound keeps a tree that grew thousands of `.so` files from turning one shell
/// start into thousands of `ldd` spawns.
const SUPPORT_TREE_SO_CLOSURE_MAX: usize = 128;

/// Upper bound on one duplicated `$PATH` entry point (`build_shell_tar_blocking`'s `copies`): real
/// ones are interpreters and CLI shims of a few KiB, because the megabytes behind them are staged
/// once and shared by inode.
const GUEST_COPY_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Bound on any one support-tree walk: real trees here hold 1.7-2.8k files (npm, Python's stdlib),
/// so a limit an order of magnitude above catches a misconfigured path pointing at something huge
/// instead of silently staging tens of thousands of files into every shell.
const SUPPORT_TREE_MAX_FILES: usize = 20_000;

/// Recursively lists every REGULAR file under `dir` (absolute paths, since guest paths are absolute
/// throughout). Symlinks are skipped, not followed, matching `follow_symlinks(false)` (the only
/// ones in the known trees are documentation under `npm/man` and `npm/docs`), and
/// `.pyc`/`__pycache__` caches are skipped. Best-effort: an unreadable entry is logged and skipped,
/// since an incomplete tree is a gap in the stdlib, not a missing shell.
fn walk_support_tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(error) => {
                tracing::debug!(dir = %d.display(), %error, "litebox support tree: read_dir failed; skipping this subtree");
                continue;
            }
        };
        for entry in entries.flatten() {
            if out.len() >= SUPPORT_TREE_MAX_FILES {
                tracing::warn!(
                    dir = %dir.display(),
                    limit = SUPPORT_TREE_MAX_FILES,
                    "litebox support tree: hit the file-count bound; the rest of this tree is NOT staged"
                );
                return out;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "__pycache__" || n == "man" || n == "docs")
                {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) != Some("pyc")
            {
                out.push(path);
            }
        }
    }
    out
}

/// Async half of guest-tar construction for interactive shells and one-shot execs: the fixed
/// shell/coreutils set plus `extra` programs (the resolved `exec_command` binary, or the sandbox
/// runtime for a shell; `ExecPtyRequest.programs`), each with its `ldd` closure (a subprocess
/// spawn, which cannot run in the blocking writer) and, for an interpreter, its support tree,
/// merged, deduped and handed to the blocking writer. A closure failure on an EXTRA program (a
/// static binary or a script has none) stages it alone; the fixed set's closure stays strict.
async fn build_guest_tar(archive: File, extra: Vec<PathBuf>) -> anyhow::Result<()> {
    let mut programs: Vec<PathBuf> = SHELL_GUEST_PROGRAMS.iter().map(PathBuf::from).collect();
    let mut deps: Vec<PathBuf> = Vec::new();
    let mut copies: Vec<(PathBuf, PathBuf)> = Vec::new();
    for program in &programs {
        deps.extend(ldd_closure(program).await?);
    }
    for program in SHELL_GUEST_OPTIONAL_PROGRAMS {
        let path = PathBuf::from(program);
        if !path.is_file() {
            tracing::debug!(
                program,
                "litebox guest tar: optional tool absent on this host; skipped"
            );
            continue;
        }
        match ldd_closure(&path).await {
            Ok(closure) => {
                deps.extend(closure);
                programs.push(path);
            }
            Err(error) => tracing::debug!(
                program,
                %error,
                "litebox guest tar: optional tool has no resolvable closure; skipped"
            ),
        }
    }
    for tool in SHELL_GUEST_TOOL_PROGRAMS {
        let path = PathBuf::from(tool);
        if !path.is_file() {
            tracing::debug!(
                tool,
                "litebox guest tar: sandbox tool absent on this host; skipped"
            );
            continue;
        }
        stage_guest_program(&path, &mut programs, &mut deps, &mut copies).await;
    }
    for lib in GUEST_NSS_LIBS {
        if Path::new(lib).is_file() {
            deps.push(PathBuf::from(lib));
        }
    }
    for program in extra {
        stage_guest_program(&program, &mut programs, &mut deps, &mut copies).await;
    }
    add_glibc_compat_stubs(&mut deps);
    programs.append(&mut deps);
    programs.sort();
    programs.dedup();
    copies.sort();
    copies.dedup();
    tokio::task::spawn_blocking(move || build_shell_tar_blocking(archive, &programs, &copies))
        .await
        .context("litebox shell guest tar construction task failed")?
}

/// When `program` is a host SYMLINK: the real file it points at. The guest gets the real file under
/// BOTH spellings, the canonical one as a normal staged file and the symlink's own path as a byte
/// copy (`stage_guest_program`'s `copies`), because a litebox guest's `stat`/`lstat` of a symlink
/// stored in the tar layer answers ENOSYS: anything that LOOKS A PROGRAM UP rather than exec'ing a
/// literal path (`command -v`, `PATH` search) never finds it, and a shebang naming
/// `/usr/bin/node-22` needs the real file. Duplicating the bytes costs a few KiB; the closure and
/// the support tree are shared by inode dedup.
fn canonical_target(program: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(program).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let canonical = std::fs::canonicalize(program).ok()?;
    if canonical == program || !canonical.is_file() {
        return None;
    }
    Some(canonical)
}

/// Stages one program and everything it needs at RUNTIME into a guest tar: its `ldd` closure, its
/// support tree (`support_plan_for`), the closures of that tree's own shared objects, and its
/// `$PATH` entry points. Never fails the tar: every gap is a debug line and a tool missing in the
/// guest, the contract `SHELL_GUEST_OPTIONAL_PROGRAMS` has always had.
async fn stage_guest_program(
    program: &Path,
    programs: &mut Vec<PathBuf>,
    deps: &mut Vec<PathBuf>,
    copies: &mut Vec<(PathBuf, PathBuf)>,
) {
    let canonical = canonical_target(program);
    let staged = canonical.clone().unwrap_or_else(|| program.to_path_buf());
    if let Some(canonical) = canonical {
        copies.push((program.to_path_buf(), canonical));
    }
    match ldd_closure(&staged).await {
        Ok(closure) => deps.extend(closure),
        Err(error) => tracing::debug!(
            program = %staged.display(),
            %error,
            "litebox: no dynamic closure for program; staging it alone"
        ),
    }
    let Some(plan) = support_plan_for(&staged) else {
        programs.push(staged);
        return;
    };
    let mut tree_files: Vec<PathBuf> = Vec::new();
    for tree in &plan.trees {
        if !tree.is_dir() {
            tracing::debug!(
                program = %staged.display(),
                tree = %tree.display(),
                "litebox: support tree absent on this host; skipped"
            );
            continue;
        }
        let walk_target = tree.clone();
        let files = tokio::task::spawn_blocking(move || walk_support_tree(&walk_target))
            .await
            .unwrap_or_default();
        tracing::debug!(
            program = %staged.display(),
            tree = %tree.display(),
            files = files.len(),
            "litebox: staging support tree"
        );
        tree_files.extend(files);
    }
    for object in tree_files
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("so"))
        .take(SUPPORT_TREE_SO_CLOSURE_MAX)
    {
        match ldd_closure(object).await {
            Ok(closure) => deps.extend(closure),
            Err(error) => tracing::debug!(
                object = %object.display(),
                %error,
                "litebox: support-tree shared object has no resolvable closure; skipped"
            ),
        }
    }
    programs.extend(tree_files);
    for (guest_path, target) in &plan.entry_points {
        match std::fs::symlink_metadata(guest_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                match (
                    std::fs::canonicalize(guest_path),
                    std::fs::canonicalize(target),
                ) {
                    (Ok(actual), Ok(expected)) if actual == expected => {
                        copies.push((guest_path.clone(), target.clone()));
                    }
                    (Ok(actual), _) => tracing::debug!(
                        guest_path = %guest_path.display(),
                        expected = %target.display(),
                        actual = %actual.display(),
                        "litebox: $PATH entry point resolves somewhere unexpected; not staged"
                    ),
                    (Err(error), _) => tracing::debug!(
                        guest_path = %guest_path.display(),
                        %error,
                        "litebox: $PATH entry point symlink unresolvable; not staged"
                    ),
                }
            }
            _ => tracing::debug!(
                guest_path = %guest_path.display(),
                "litebox: $PATH entry point absent or not a symlink on this host; not staged"
            ),
        }
    }
    programs.push(staged);
}

/// Writes the shell guest tar: each of `paths` at its host path (opened `openat2` beneath `/`
/// without magic links, deduped by inode), then `copies`, the `$PATH`-visible second spellings of a
/// symlinked program (`/usr/bin/python3` for `python3.12`, `/usr/bin/node` for `node-22`,
/// `/usr/bin/npm` and `npx` for the npm tree). Those are written as REAL FILES, not symlink
/// entries, which a guest cannot see (`canonical_target`), and they bypass the inode dedup on
/// purpose: two names for one file is the point. A script among them is wrapped, not copied, since
/// it resolves its `require`/imports relative to its REAL location (a copy at `/usr/bin/npx` fails
/// with `Cannot find module '../lib/cli.js'`): the wrapper execs the real path through the script's
/// own shebang interpreter, which is what must be on `$PATH`. This tar is a separate archive from
/// the runtime augmentation's, so the resolver, nsswitch and CA files are planted here too (without
/// them a typed `curl` has no resolver and no CA roots), followed by the shell rc.
#[cfg(target_os = "linux")]
fn build_shell_tar_blocking(
    archive: File,
    paths: &[PathBuf],
    copies: &[(PathBuf, PathBuf)],
) -> anyhow::Result<()> {
    let root = File::open("/").context("open root filesystem")?;
    let mut seen_inodes: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut builder = tar::Builder::new(archive);
    builder.mode(tar::HeaderMode::Deterministic);
    builder.follow_symlinks(false);

    for path in paths {
        let relative = path
            .strip_prefix("/")
            .map_err(|_| anyhow::anyhow!("shell guest path {} is not absolute", path.display()))?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "shell guest path is the root directory"
        );
        let mut file = crate::runtime_artifact::openat2_required(
            &root,
            relative.as_os_str(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            crate::runtime_artifact::RESOLVE_BENEATH
                | crate::runtime_artifact::RESOLVE_NO_MAGICLINKS,
        )
        .with_context(|| format!("openat2 failed for shell guest path {}", path.display()))?;
        let before = file
            .metadata()
            .with_context(|| format!("cannot stat {}", path.display()))?;
        anyhow::ensure!(
            before.is_file(),
            "shell guest path is not a file: {}",
            path.display()
        );
        if !seen_inodes.insert((before.dev(), before.ino())) {
            continue;
        }
        let mut header = litebox_tar_header(before.len(), before.mode())?;
        builder
            .append_data(&mut header, relative, &mut file)
            .with_context(|| format!("append shell guest path {}", path.display()))?;
        let after = file.metadata()?;
        anyhow::ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mode() == after.mode()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "shell guest path changed while staging: {}",
            path.display()
        );
    }

    for (guest_path, host_path) in copies {
        let relative = guest_path.strip_prefix("/").map_err(|_| {
            anyhow::anyhow!(
                "shell guest copy path {} is not absolute",
                guest_path.display()
            )
        })?;
        let bytes = match std::fs::read(host_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::debug!(
                    guest_path = %guest_path.display(),
                    host_path = %host_path.display(),
                    %error,
                    "litebox: cannot read the file backing a $PATH entry point; not staged"
                );
                continue;
            }
        };
        if bytes.len() as u64 > GUEST_COPY_MAX_BYTES {
            tracing::debug!(
                guest_path = %guest_path.display(),
                bytes = bytes.len(),
                limit = GUEST_COPY_MAX_BYTES,
                "litebox: $PATH entry point is too large to duplicate; not staged"
            );
            continue;
        }
        if bytes.starts_with(b"#!") {
            let interpreter = bytes[2..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|end| &bytes[2..2 + end])
                .and_then(|line| std::str::from_utf8(line).ok())
                .and_then(|line| line.split_whitespace().next())
                .unwrap_or("/bin/sh");
            let wrapper = format!(
                "#!/bin/sh\nexec {interpreter} {host} \"$@\"\n",
                host = host_path.display()
            );
            let mut header = litebox_tar_header(wrapper.len() as u64, 0o755)?;
            let mut slice: &[u8] = wrapper.as_bytes();
            builder
                .append_data(&mut header, relative, &mut slice)
                .with_context(|| {
                    format!(
                        "append shell guest wrapper {} -> {}",
                        guest_path.display(),
                        host_path.display()
                    )
                })?;
            continue;
        }
        let mode = std::fs::metadata(host_path)
            .map(|meta| meta.mode())
            .unwrap_or(0o755);
        let mut header = litebox_tar_header(bytes.len() as u64, mode)?;
        let mut slice: &[u8] = &bytes;
        builder
            .append_data(&mut header, relative, &mut slice)
            .with_context(|| {
                format!(
                    "append shell guest copy {} (bytes of {})",
                    guest_path.display(),
                    host_path.display()
                )
            })?;
    }

    append_platform_tar_entry(
        &mut builder,
        Path::new("etc/resolv.conf"),
        GUEST_RESOLV_CONF,
    )?;
    append_platform_tar_entry(
        &mut builder,
        Path::new("etc/nsswitch.conf"),
        GUEST_NSSWITCH_CONF,
    )?;
    if let Some(bundle) = host_ca_bundle() {
        for path in GUEST_CA_PATHS {
            append_platform_tar_entry(&mut builder, Path::new(path), &bundle)?;
        }
    }

    append_platform_tar_entry(
        &mut builder,
        Path::new(SHELL_RC_GUEST_PATH.trim_start_matches('/')),
        SHELL_RC_BYTES,
    )?;

    builder.finish().context("finish litebox shell guest tar")?;
    let archive = builder
        .into_inner()
        .context("close litebox shell guest tar writer")?;
    archive.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn build_shell_tar_blocking(
    _archive: File,
    _paths: &[PathBuf],
    _copies: &[(PathBuf, PathBuf)],
) -> anyhow::Result<()> {
    anyhow::bail!("descriptor-relative tar construction requires Linux openat2")
}

#[async_trait]
impl CellBackend for LiteboxBackend {
    fn name(&self) -> &'static str {
        "litebox"
    }

    fn requires_runtime_artifact_authorization(&self) -> bool {
        true
    }

    /// Gives the cell a working root under its tenant's subtree (`root/<tenant>/<id>`, as
    /// `MockBackend::provision` does), so one tenant's cells never see another's files, and a
    /// non-container cell its TUN link before the port is known. A function cell with no network
    /// can never serve, so a link failure is a hard provision error here, unlike Firecracker's
    /// egress-only degrade; container cells run through host podman with its own network.
    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        let tenant = if spec.tenant.trim().is_empty() {
            "personal"
        } else {
            spec.tenant.as_str()
        };
        let root = self
            .cfg
            .root
            .join(crate::sanitize_tenant(tenant))
            .join(spec.id.as_str());
        let mut provision =
            LiteboxProvisionGuard::new(spec.id.clone(), root.clone(), self.cell_nets.clone());
        tokio::fs::create_dir_all(&root).await?;
        if !self.cfg.provision_latency.is_zero() {
            tokio::time::sleep(self.cfg.provision_latency).await;
        }
        if spec.container.is_none() {
            anyhow::ensure!(
                self.setup_cell_net(&spec.id).await.is_some(),
                "litebox: failed to set up this cell's TUN device (needs `ip`, CAP_NET_ADMIN, \
                 and /dev/net/tun) — see hive_backend::litebox's module doc, \"Networking\" \
                 section"
            );
        }
        provision.commit();
        Ok(CellHandle {
            id: spec.id.clone(),
            image: spec.image.clone(),
            resources: spec.resources.clone(),
            root,
            endpoint: None,
        })
    }

    async fn provision_runtime(
        &self,
        spec: &CellSpec,
        expected: Option<&RuntimeArtifactIdentity>,
    ) -> anyhow::Result<CellHandle> {
        if spec.container.is_some() {
            anyhow::ensure!(
                expected.is_none(),
                "container launch unexpectedly carried a litebox runtime artifact identity"
            );
        } else {
            let expected = expected.ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: litebox provisioning is missing its caller-authorized runtime artifact identity",
                    hive_core::fault::NODE_IMAGE_MISSING
                )
            })?;
            let attached = self
                .runtime_artifact_identity(&spec.image)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: litebox runtime artifact is missing for image {}",
                        hive_core::fault::NODE_IMAGE_MISSING,
                        spec.image
                    )
                })?;
            anyhow::ensure!(
                &attached == expected,
                "{}: litebox provisioning observed a different runtime artifact identity",
                hive_core::fault::NODE_IMAGE_MISSING
            );
        }
        self.provision(spec).await
    }

    /// Deliberately NOT sandboxed: a build is fork/exec-heavy, so it runs as a plain host process
    /// through `MockBackend`'s pipeline.
    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult> {
        crate::mock::run_build_process(cell, job, sink, &self.cfg.cache_root).await
    }

    /// Publishes the exact universal package as immutable application authority.
    /// Backend-only runtime closure and identity bytes are added later to a
    /// separately addressed combined archive; no checkout is re-read here.
    async fn deliver_build(
        &self,
        image: &str,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_identity_id(image),
            "runtime artifact image contains an invalid platform identity"
        );
        let publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let package_descriptor = artifact.package_descriptor().clone();
        let app_rel = PathBuf::from(&package_descriptor.app_rel);
        let (staged, _publication) =
            crate::runtime_artifact::stage_sealed_runtime_artifact_serialized(
                artifact,
                directories.staging.descriptor.try_clone()?,
                publication,
            )
            .await?;
        let identity = artifact.identity(image)?;
        anyhow::ensure!(
            staged.content_sha256() == identity.content_sha256,
            "verified runtime package materialized a different semantic identity"
        );
        anyhow::ensure!(
            staged
                .read_regular(
                    Path::new(hive_core::RUNTIME_ARTIFACT_MARKER_FILE),
                    MAX_REFERENCE_BYTES,
                )?
                .is_none(),
            "selected application collides with the platform runtime identity marker"
        );
        let guest_workdir = artifact.guest_workdir(DELIVERED_WORKDIR)?;
        validate_guest_workdir(&guest_workdir)?;
        let (package_scripts, next_entry) = self.package_metadata(&staged, &app_rel).await?;

        let (mut package, verified_descriptor) = artifact.verified_package()?.into_parts();
        anyhow::ensure!(
            verified_descriptor == package_descriptor,
            "sealed universal package descriptor changed during litebox delivery"
        );
        package.seek(SeekFrom::Start(0))?;
        let mut temp = self.allocate_temp_file(&directories.temporary, "app", ".tar")?;
        let copied = {
            let mut input = tokio::fs::File::from_std(package.try_clone()?);
            let mut output = tokio::fs::File::from_std(temp.file.try_clone()?);
            let copied = tokio::io::copy(&mut input, &mut output).await?;
            output.sync_all().await?;
            copied
        };
        anyhow::ensure!(
            copied == package_descriptor.package_bytes,
            "sealed universal package length changed during litebox delivery"
        );
        let app_archive_sha256 = sha256_open_file(&temp.file).await?;
        anyhow::ensure!(
            app_archive_sha256 == package_descriptor.package_sha256,
            "sealed universal package bytes changed during litebox delivery"
        );
        let destination_name = Self::app_archive_name(&app_archive_sha256);
        self.publish_immutable_locked(
            &directories,
            &mut temp,
            &directories.apps,
            &destination_name,
            &app_archive_sha256,
        )
        .await?;
        let reference = LiteboxImageReference {
            schema: LITEBOX_ARTIFACT_SCHEMA,
            image: image.to_string(),
            identity,
            guest_workdir,
            app_archive_sha256,
            package_scripts,
            next_entry,
            runtimes: BTreeMap::new(),
        };
        self.write_image_reference_locked(&directories, &reference)
            .await?;
        if let Err(error) = self.gc_artifacts_locked(&directories).await {
            tracing::warn!(error = %error, "litebox artifact GC refused or failed");
        }
        Ok(())
    }

    async fn runtime_artifact_identity(
        &self,
        image: &str,
    ) -> anyhow::Result<Option<RuntimeArtifactIdentity>> {
        anyhow::ensure!(
            valid_identity_id(image),
            "runtime artifact image contains an invalid platform identity"
        );
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let name = Self::image_reference_name(image);
        let file = match directories.references.open_regular(&name) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES)?;
        let reference: LiteboxImageReference = serde_json::from_slice(&bytes)?;
        validate_image_reference(&reference, image)?;
        // Hash-verified once per deployment per TTL; every later launch of the
        // same deployment (and `provision_runtime`'s re-derive of this very
        // identity) reuses that proof instead of re-reading the whole archive.
        let _archive = verified_app_archive(
            &self.verified_archives,
            image,
            &directories.apps,
            &reference,
        )
        .await?;
        directories.verify_bindings()?;
        Ok(Some(reference.identity))
    }

    fn delivered_workdir(
        &self,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<Option<String>> {
        artifact.guest_workdir(DELIVERED_WORKDIR).map(Some)
    }

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        if func.start_cmd.is_empty() {
            return Err(launch_refusal("function start command is empty"));
        }

        // CONTAINER cell: bypass litebox entirely, run via host podman — the
        // same helper FirecrackerBackend calls. See module doc for why.
        if func.start_cmd[0] == "__container__" {
            let image = func.start_cmd.get(1).cloned().unwrap_or_default();
            let internal: u16 = func
                .start_cmd
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080);
            let net_json = func
                .start_cmd
                .get(3)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty());
            let runtime = crate::container_runtime();
            let mut ports = vec![crate::ContainerPort::tcp(internal, func.port)];
            ports.extend(func.udp_ports.iter().map(|u| crate::ContainerPort {
                container_port: u.container_port,
                host_port: u.host_port,
                protocol: crate::ContainerProtocol::Udp,
            }));
            // Extra raw/published TCP publishes (`FunctionLaunch::tcp_ports` —
            // includes the primary, whose pairing is already ports[0]; a
            // duplicate `-p` for the same pair fails the run). Without these
            // the raw proxy's per-port loopback leg (`Lease::tcp_host_port`)
            // dials a port nothing publishes — connection refused on every
            // node running THIS backend while the mock path worked.
            ports.extend(func.tcp_ports.iter().filter_map(|t| {
                if t.host_port == func.port {
                    return None;
                }
                Some(crate::ContainerPort {
                    container_port: t.container_port,
                    host_port: t.host_port,
                    protocol: crate::ContainerProtocol::Tcp,
                })
            }));
            let launch = crate::podman_run_container(
                &cell.id,
                &image,
                &ports,
                &func.env,
                func.max_concurrency,
                crate::PODMAN_PATH,
                runtime.as_deref(),
                net_json,
                &crate::ContainerLimits::for_container(func.memory_mib, func.cpus, func.pids),
                func.raw_proxy,
                func.gpu,
            )
            .await?;
            let endpoint = launch.endpoint();
            self.containers.lock().await.insert(cell.id.clone(), launch);
            return Ok(endpoint);
        }

        // Bun is refused here, before any reservation, network setup, tar
        // alias, or process spawn — never after. `func.runtime` (the
        // authoritative platform discriminator FunctionLaunch itself carries,
        // never a parse of `start_cmd`'s tenant-controlled argv text) is the
        // only signal trusted for this decision, exactly like
        // `hive-cell-agent`'s own `platform_runtime_program` gate.
        //
        // This backend's syscall shim panics (`unimplemented!()`,
        // `litebox_shim_linux/src/syscalls/file.rs:1210`) on Bun's own
        // boot-time `readlink("/proc/self/fd/3")` probe — a hard Rust panic
        // inside the guest's interception layer, not a recoverable error.
        // Letting a Bun launch reach `resolve_direct_launch`/spawn below would
        // turn every Bun cold start into an uncontrolled crash instead of an
        // honest, typed node fault. This mirrors `resources::detect`'s
        // `bun_runtime: Some(false)` verdict for this backend — placement
        // should already have excluded this node for a Bun deployment
        // (`schedule::bun_capable`), so reaching here means the gossiped
        // capability and this launch disagree (a stale registry entry, a
        // launch forced past placement). Never falls back to Mock or the
        // host, never classifies as capacity or a tenant-app fault — this is
        // a NODE fault, the same class `NODE_IMAGE_MISSING` and
        // `NODE_BACKEND_UNAVAILABLE` are.
        if func.runtime == hive_core::Runtime::Bun {
            return Err(bun_unsupported_refusal());
        }

        // Plain function: bind this cell to the exact durable reference before
        // resolving any repository-controlled launch metadata. Package managers
        // are never run here: a narrow, validated package-script reducer emits
        // one direct Node process or a typed backend-capability refusal (Bun is
        // refused above, before this lock, before any of the machinery below).
        //
        // The guard is RELEASED as soon as the runner holds the archive (see the
        // drop after `spawn`), not at the end of this function: it used to be
        // held across `wait_litebox_ready` (up to 15 s) as well, so every other
        // deployment's cold start on the node queued behind one guest's boot --
        // a boot that starts a hundred deployments then spent minutes, and the
        // last ones answered tens of seconds after the node was already up.
        let publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let mut reference = self
            .load_image_reference_locked(&directories, &cell.image)
            .await?;
        let runtime_workdir = func.workdir.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: litebox function is missing its exact runtime artifact workdir",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            runtime_workdir == reference.guest_workdir,
            "{}: litebox runtime workdir mismatch (launch {:?}, artifact {:?})",
            hive_core::fault::NODE_IMAGE_MISSING,
            runtime_workdir,
            reference.guest_workdir
        );
        let expected = func.runtime_artifact.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: litebox launch is missing its authoritative runtime artifact identity",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            expected == &reference.identity,
            "{}: litebox launch requested a different runtime artifact identity",
            hive_core::fault::NODE_IMAGE_MISSING
        );
        // `Runtime::Bun` is refused unconditionally above, before this branch
        // is ever reached, so only `Node` remains a legitimate plain-function
        // runtime on this backend.
        if func.runtime != hive_core::Runtime::Node {
            return Err(launch_refusal(format!(
                "runtime {:?} is not a Node runtime",
                func.runtime
            )));
        }
        let t_stage = std::time::Instant::now();
        let app_archive = verified_app_archive(
            &self.verified_archives,
            &cell.image,
            &directories.apps,
            &reference,
        )
        .await?;
        let app_ms = t_stage.elapsed().as_millis();
        let DirectLaunch {
            runtime,
            bin,
            mut args,
        } = resolve_direct_launch(func, &reference, &app_archive).await?;
        // Belt-and-braces for the same panic this function's entry already
        // refuses on `func.runtime`: `resolve_direct_launch` derives ITS
        // runtime from `start_cmd`'s argv text (needed to tell `node` from
        // `bun` from a shared package-script reducer), so a launch whose
        // `start_cmd` names `bun` while `func.runtime` claims something else
        // — a malformed or mismatched launch that reached this point despite
        // the entry check — would otherwise still be spawned against the
        // unsupported syscall shim. The entry check above already refused
        // every launch where `func.runtime` itself says Bun, so reaching
        // `DirectRuntime::Bun` here can only mean that disagreement.
        if runtime == DirectRuntime::Bun {
            return Err(bun_unsupported_refusal());
        }
        anyhow::ensure!(
            bin.is_file(),
            "{}: `{}` is not installed on this node, so the direct runtime cannot start here (operator remedy; not an application fault)",
            hive_core::fault::NODE_RUNTIME_MISSING,
            bin.display()
        );
        let runtime_key = format!(
            "{}:{}",
            match runtime {
                DirectRuntime::Node => "node",
                DirectRuntime::Bun => "bun",
            },
            bin.display()
        );
        let t_stage = std::time::Instant::now();
        let initial_files = self
            .ensure_combined_tar_locked(
                &directories,
                &cell.image,
                &runtime_key,
                &bin,
                &mut reference,
            )
            .await?;
        let tar_ms = t_stage.elapsed().as_millis();

        let net = self
            .cell_nets
            .lock()
            .await
            .get(&cell.id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "litebox: cell {} has no TUN device (provision should have set one up) — this is a bug, not a runtime condition",
                    cell.id
                )
            })?;
        if runtime == DirectRuntime::Bun {
            args.insert(0, GUEST_RUNTIME_GUARD_PATH.to_string());
            args.insert(0, "--preload".to_string());
        }

        let guest_environment = sanitized_guest_environment(func)?;
        let mut command = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut command);
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &initial_files)?;
        let initial_files_path = crate::runtime_artifact::inherit_file_path(
            &mut command,
            &initial_files_alias.directory,
        )?
        .join(INITIAL_FILES_ALIAS_NAME);
        directories.verify_bindings()?;
        command
            .arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files_path.display()))
            .arg("--")
            .arg(&bin)
            .args(&args)
            // Nothing from hive-node is inherited. Host-loader, runner-control
            // and platform-owned keys were removed before the remaining tenant
            // application environment was inserted.
            .env_clear()
            .envs(&guest_environment)
            .env("PORT", func.port.to_string())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            // Node honours this regardless of OpenSSL's compiled-in default
            // path, which is what the planted bundle at
            // /etc/ssl/certs/ca-certificates.crt was NOT being read from
            // (UNABLE_TO_GET_ISSUER_CERT_LOCALLY persisted with a valid
            // 226 KiB bundle present, measured 2026-10-08).
            .envs(guest_ca_env())
            .env("HIVE_RUNTIME_WORKDIR", &reference.guest_workdir)
            .env(
                "HIVE_RUNTIME_ARTIFACT_PROTOCOL",
                reference.identity.protocol.to_string(),
            )
            .env("HIVE_RUNTIME_ARTIFACT_ID", &reference.identity.id)
            .env(
                "HIVE_RUNTIME_ARTIFACT_SHA256",
                &reference.identity.content_sha256,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Piped, not nulled: a runtime that exits before listening is
            // reported to the tenant/operator only through the launch error, so
            // a discarded stderr turned every guest-side failure (shim panic,
            // module resolution, bind refusal) into an unexplained "exit
            // status: 1" — witnessed live on nodes-wtf. A bounded tail of it is
            // folded into that error; a HEALTHY guest's stderr keeps draining
            // into the same bounded buffer so the pipe can never fill and
            // backpressure the app.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if runtime == DirectRuntime::Node {
            command.env(
                "NODE_OPTIONS",
                format!("--require {GUEST_RUNTIME_GUARD_PATH}"),
            );
        } else {
            // Bun's CLI preload above is the ordered guard. Do not let a
            // tenant-supplied compatibility/options variable inject an earlier
            // preload ahead of that platform-owned handshake.
            command.env_remove("NODE_OPTIONS").env_remove("BUN_OPTIONS");
        }

        let t_spawn = std::time::Instant::now();
        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn litebox runner at {}: {error}",
                self.cfg.runner_bin.display()
            )
        })?;
        // The runner inherited the archive's file descriptor (`--initial-files`
        // is a /proc/self/fd path), so the bytes it will read are pinned for
        // that process regardless of what publication/GC does to the directory
        // entry afterwards. Nothing below needs the publication lock.
        drop(publication);
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        if let Some(mut pipe) = child.stderr.take() {
            const STDERR_TAIL_CAP: usize = 8 * 1024;
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0_u8; 4096];
                while let Ok(read) = pipe.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.extend_from_slice(&buffer[..read]);
                    if tail.len() > STDERR_TAIL_CAP {
                        let excess = tail.len() - STDERR_TAIL_CAP;
                        tail.drain(..excess);
                    }
                }
            });
        }
        let func_addr = format!("{}:{}", net.guest_ip, func.port);
        // Armed for the whole readiness wait: covers a normal timeout/exit
        // `Err` AND the future being dropped outright (the caller gave up —
        // see `ChildReapGuard`'s doc). Disarmed only once the guest has
        // proven it is listening, at which point `child` moves into
        // long-lived tracking and `terminate()` becomes the reaper.
        let mut reap_guard = ChildReapGuard::new(&mut child);
        if let Err(error) =
            wait_litebox_ready(reap_guard.child_mut(), &func_addr, Duration::from_secs(15)).await
        {
            // Give the reader a beat to drain what the dying guest wrote.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let tail = {
                let tail = stderr_tail
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                String::from_utf8_lossy(&tail).trim().to_string()
            };
            if tail.is_empty() {
                return Err(error);
            }
            return Err(anyhow::anyhow!("{error}; guest stderr tail: {tail}"));
        }
        {
            let spawn_ready_ms = t_spawn.elapsed().as_millis();
            tracing::info!(
                image = %cell.image,
                app_ms,
                tar_ms,
                spawn_ready_ms,
                total_ms = app_ms + tar_ms + spawn_ready_ms,
                "litebox cold-start stages"
            );
        }
        reap_guard.disarm();
        // Guest readiness proves Litebox has materialized the initial tar. Drop
        // the descriptor-relative pathname now; cancellation before this point
        // takes the same Drop cleanup path automatically.
        drop(initial_files_alias);
        // Publication/GC serialization was already released above (right after
        // `spawn`, when the runner inherited the archive's descriptor).

        let raw_proxy = func.raw_proxy;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let tunnel_addr = listener.local_addr()?.to_string();
        let max_conc = func.max_concurrency.max(1);
        let task = crate::AbortTask::new(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((conn, _)) => {
                        let local = func_addr.clone();
                        tokio::spawn(async move {
                            if raw_proxy {
                                fluid_tunnel::TunnelServer::serve_raw(conn, local).await;
                            } else {
                                fluid_tunnel::TunnelServer::serve_maybe_raw(conn, local, max_conc)
                                    .await;
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        }));
        let mut funcs = self.funcs.lock().await;
        let mut tunnels = self.tunnels.lock().await;
        funcs.insert(
            cell.id.clone(),
            LiteboxFunctionProcess {
                child,
                _initial_files: initial_files,
            },
        );
        if let Some(task) = task.publish() {
            tunnels.insert(cell.id.clone(), task);
        }
        Ok(CellEndpoint::Tcp(tunnel_addr))
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        let id = cell.id.clone();
        let root = cell.root.clone();
        let tunnels = self.tunnels.clone();
        let containers = self.containers.clone();
        let funcs = self.funcs.clone();
        let cell_nets = self.cell_nets.clone();
        let execs = self.execs.clone();
        let cleanup = tokio::spawn(async move {
            let tunnel = tunnels.lock().await.remove(&id);
            if let Some(task) = tunnel {
                task.abort();
            }
            let container = containers.lock().await.remove(&id);
            if let Some(container) = container {
                container.terminate().await;
            }
            // Every live sandbox exec in this cell — one-shot commands AND
            // interactive shells, each its own process group — dies with the
            // cell, before its TUN does. A guest that ignores its stdio (a
            // spinning fork child, `sleep infinity`) otherwise outlives the
            // sandbox that owned it: witnessed, a DELETE answered 200 while
            // the runner held a core at 100% until an operator killed it.
            let doomed: Vec<(String, i32)> = {
                let mut execs = execs.lock().await;
                let ids: Vec<String> = execs
                    .iter()
                    .filter(|(_, e)| e.cell == id)
                    .map(|(k, _)| k.clone())
                    .collect();
                ids.into_iter()
                    .filter_map(|k| execs.remove(&k).map(|e| (k, e.pgid)))
                    .collect()
            };
            for (exec_id, pgid) in doomed {
                #[cfg(unix)]
                {
                    // SAFETY: killpg(2) with a pgid this process spawned and a
                    // signal number; no pointer arguments.
                    let rc = unsafe { libc::killpg(pgid, libc::SIGKILL) };
                    if rc != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            tracing::warn!(
                                cell = %id,
                                exec = %exec_id,
                                pgid,
                                %error,
                                "litebox terminate: killpg on a live exec failed"
                            );
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = (exec_id, pgid);
                }
            }
            let process = funcs.lock().await.remove(&id);
            if let Some(mut process) = process {
                let _ = process.child.start_kill();
                let _ = process.child.wait().await;
            }
            let net = cell_nets.lock().await.remove(&id);
            if let Some(net) = net {
                delete_litebox_link(&net.tun_dev).await;
            }
            let _ = tokio::fs::remove_dir_all(root).await;
        });
        cleanup
            .await
            .map_err(|e| anyhow::anyhow!("litebox cleanup task failed: {e}"))
    }

    async fn cpu_percent(&self, cell: &CellHandle) -> Option<f32> {
        // Guest and runner are one process (litebox has no separate VMM), so
        // the runner's own PID directly IS the guest's CPU usage — sampling
        // is more direct here than the Firecracker VMM-proxy case.
        let pid = {
            let funcs = self.funcs.lock().await;
            funcs.get(&cell.id).and_then(|c| c.child.id())?
        };
        self.sampler.cpu_percent(pid, cell.resources.vcpus)
    }

    /// One argv command inside `cell` (Sandboxes `run_command`) — the SAME
    /// guest mechanism `exec_pty` uses: a fresh `litebox-runner` spawned
    /// against the cell's TUN device with a guest tar holding the fixed
    /// shell/coreutils set plus the resolved program and its `ldd` closure.
    /// stdout/stderr are separate pipes streamed as distinct `ExecOutput`
    /// lines, then exactly one `ExecDone { exit_code }` — `None` when the
    /// runner died by signal (a `kill_exec`), never a fake `Some(0)`. The
    /// runner is its own process-group leader so `kill_exec` can terminate
    /// the whole guest tree by exec id. Returns as soon as the runner is
    /// spawned; the caller drains the channel.
    async fn exec_command(
        &self,
        cell: &CellHandle,
        req: hive_core::ExecRequest,
    ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>> {
        anyhow::ensure!(
            !req.id.is_empty(),
            "litebox exec requires a non-empty exec id"
        );
        // The runner is an unprivileged host process; there is no `sudo` in
        // the guest and no privilege to elevate to. Refuse loudly at start.
        anyhow::ensure!(
            !req.sudo,
            "litebox sandboxes cannot elevate privileges: sudo is not available inside a litebox guest"
        );
        if self.execs.lock().await.contains_key(&req.id) {
            anyhow::bail!("exec id {} is already running on this node", req.id);
        }
        // The cell must exist (its provision-time link proves it), but this
        // runner gets its OWN link: a TUN device has one owner, and sharing
        // the cell's with a live shell or function was `EBUSY` at startup
        // (see `allocate_link`).
        anyhow::ensure!(
            self.cell_nets.lock().await.contains_key(&cell.id),
            "litebox: cell {} has no TUN device (provision should have set one up) — this is a bug, not a runtime condition",
            cell.id
        );
        let (net, link) = self.allocate_link().await.ok_or_else(|| {
            anyhow::anyhow!(
                "litebox: could not allocate a TUN device for sandbox exec {} (needs `ip`, CAP_NET_ADMIN, /dev/net/tun)",
                req.id
            )
        })?;

        let (program, args) = if req.shell {
            let mut line = req.cmd.clone();
            for arg in &req.args {
                line.push(' ');
                line.push_str(arg);
            }
            (PathBuf::from("/bin/sh"), vec!["-c".to_string(), line])
        } else {
            resolve_guest_argv(&req.cmd, &req.args)?
        };

        let directories = self.prepare_artifact_dirs()?;
        directories.verify_bindings()?;
        let exec_tar = self.allocate_temp_file(&directories.temporary, "exec", ".tar")?;
        // The program the runner first execs, plus (for a shebang script) the
        // script itself: `resolve_guest_argv` turns `npx` into
        // `/usr/bin/node-22 /usr/bin/npx`, and the script has to be in the
        // guest for that argv to mean anything.
        let mut staged = vec![program.clone()];
        if !req.shell {
            if let Ok(named) = resolve_guest_program(&req.cmd) {
                staged.push(named);
            }
        }
        build_guest_tar(exec_tar.file.try_clone()?, staged).await?;
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &exec_tar.file)?;

        let cwd = if req.cwd.is_empty() {
            "/".to_string()
        } else {
            req.cwd.clone()
        };
        let mut command = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut command);
        let initial_files_path = crate::runtime_artifact::inherit_file_path(
            &mut command,
            &initial_files_alias.directory,
        )?
        .join(INITIAL_FILES_ALIAS_NAME);
        command
            .arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files_path.display()))
            .arg("--")
            .arg(&program)
            .args(&args)
            .env_clear()
            .envs(&req.env)
            .env("HOME", "/root")
            .env("PATH", "/usr/bin:/bin")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            // Node honours this regardless of OpenSSL's compiled-in default
            // path, which is what the planted bundle at
            // /etc/ssl/certs/ca-certificates.crt was NOT being read from
            // (UNABLE_TO_GET_ISSUER_CERT_LOCALLY persisted with a valid
            // 226 KiB bundle present, measured 2026-10-08).
            .envs(guest_ca_env())
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group: `kill_exec` sends SIGKILL to the GROUP, which
        // reaches every process the guest forked, not only the runner.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn litebox runner for sandbox exec at {}: {error}",
                self.cfg.runner_bin.display()
            )
        })?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("litebox runner exited before its pid could be read"))?;
        let pgid = i32::try_from(pid).context("litebox runner pid does not fit a pgid")?;
        self.execs.lock().await.insert(
            req.id.clone(),
            LiteboxExec {
                cell: cell.id.clone(),
                pgid,
            },
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stdout = tokio::spawn(pump_exec_lines(
            child.stdout.take(),
            hive_core::LogStream::Stdout,
            req.id.clone(),
            tx.clone(),
        ));
        let stderr = tokio::spawn(pump_exec_lines(
            child.stderr.take(),
            hive_core::LogStream::Stderr,
            req.id.clone(),
            tx.clone(),
        ));
        let execs = self.execs.clone();
        let exec_id = req.id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Every output line precedes the terminal event.
            let _ = stdout.await;
            let _ = stderr.await;
            execs.lock().await.remove(&exec_id);
            // `code()` is `None` for a signal death — exactly the "killed"
            // meaning the protocol reserves for `exit_code: None`.
            let exit_code = status.ok().and_then(|s| s.code());
            let _ = tx.send(hive_core::AgentEvent::ExecDone {
                id: exec_id,
                exit_code,
            });
            // The runner has exited, so the guest tar and its private alias
            // are provably no longer being read; their Drop guards unlink.
            // The runner's own TUN link goes the same way: the armed rollback
            // deletes the device (see `allocate_link`).
            drop(initial_files_alias);
            drop(exec_tar);
            drop(link);
        });
        Ok(rx)
    }

    /// Terminate a still-running `exec_command` by id: SIGKILL to the whole
    /// process group the runner leads (the guest's forked descendants
    /// included). Idempotent — an id that already finished is a no-op. The
    /// waiter task then observes the signal death and emits
    /// `ExecDone { exit_code: None }`.
    async fn kill_exec(&self, cell: &CellHandle, exec_id: &str) -> anyhow::Result<()> {
        let entry = self.execs.lock().await.get(exec_id).cloned();
        let Some(entry) = entry else {
            return Ok(());
        };
        anyhow::ensure!(
            entry.cell == cell.id,
            "exec {exec_id} belongs to cell {}, not {}",
            entry.cell,
            cell.id
        );
        #[cfg(unix)]
        {
            // SAFETY: killpg(2) with a pgid this process spawned and a signal
            // number; no pointer arguments.
            let rc = unsafe { libc::killpg(entry.pgid, libc::SIGKILL) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                // ESRCH: the group is already gone (raced its own exit) —
                // the waiter reports the real exit either way.
                anyhow::ensure!(
                    error.raw_os_error() == Some(libc::ESRCH),
                    "killpg({}) for exec {exec_id} failed: {error}",
                    entry.pgid
                );
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("litebox exec kill requires a Unix process group")
        }
    }

    async fn exec_pty(
        &self,
        cell: &CellHandle,
        req: hive_core::ExecPtyRequest,
    ) -> anyhow::Result<(
        tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
        crate::PtyIo,
    )> {
        // litebox has no separate guest kernel/agent — the guest IS this
        // host's own `litebox-runner` child process (syscall interception,
        // not virtualization), so unlike Firecracker's vsock-agent-protocol
        // exec_pty, no wire protocol is needed at all: a REAL host pty
        // allocated here and handed to the runner as its stdin/stdout/stderr
        // gives the guest shell genuine raw-mode terminal behavior for free
        // (the `AnEntrypoint/litebox` base this platform tracks also
        // implements its own guest-internal /dev/ptmx pty subsystem — see
        // ansible/roles/litebox/files/PATCHES.md's "litebox fork tracking"
        // section — but wiring THIS host-side pty is simpler and needs none
        // of that: the shell's own line discipline runs against a real
        // kernel pty exactly as it would over SSH, the same technique
        // `hive-cell-agent`'s Firecracker-guest PTY support uses on the
        // OTHER side of the vsock boundary).
        // The cell must exist, but the shell runner gets its OWN TUN link
        // (one owner per device; sharing the cell's with a function or a
        // one-shot exec was `EBUSY` — see `allocate_link`).
        anyhow::ensure!(
            self.cell_nets.lock().await.contains_key(&cell.id),
            "litebox: cell {} has no TUN device (provision should have set one up) — this is a bug, not a runtime condition",
            cell.id
        );
        let (net, link) = self.allocate_link().await.ok_or_else(|| {
            anyhow::anyhow!(
                "litebox: could not allocate a TUN device for the sandbox shell {} (needs `ip`, CAP_NET_ADMIN, /dev/net/tun)",
                req.id
            )
        })?;

        let directories = self.prepare_artifact_dirs()?;
        directories.verify_bindings()?;
        let shell_tar = self.allocate_temp_file(&directories.temporary, "shell", ".tar")?;
        // The fixed tool set plus whatever the caller says this shell must be
        // able to run (the sandbox's runtime — `node` for node22). A program
        // the host cannot resolve is skipped with a warning rather than
        // failing the shell: a shell without `node` is still a shell, and the
        // warning names what the tenant will find missing.
        let mut extra: Vec<PathBuf> = Vec::new();
        for program in &req.programs {
            match resolve_guest_program(program) {
                Ok(path) => extra.push(path),
                Err(error) => tracing::warn!(
                    program,
                    %error,
                    "litebox shell: requested runtime program is not resolvable on this host; the shell opens without it"
                ),
            }
        }
        build_guest_tar(shell_tar.file.try_clone()?, extra).await?;
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &shell_tar.file)?;

        let (master_fd, slave_fd) = open_pty_pair(req.cols, req.rows)
            .context("litebox: failed to allocate a host pty for the sandbox shell")?;

        let shell = if req.shell.is_empty() {
            "/bin/sh".to_string()
        } else {
            req.shell.clone()
        };
        // Litebox does not implement the job-control tty ioctls (tcsetpgrp
        // answers ENOSYS, -38). A shell whose stderr IS the pty goes
        // interactive on its own, runs job-control setup, and the runner dies
        // with status 21 before a single prompt byte — measured 2026-09-01 on
        // fc-sanjose for `/bin/sh`, `+m`, `--norc +m` and `-i +m` alike. With
        // stderr OFF the pty the very same shell (`-i`) prints its prompt,
        // executes input and exits cleanly, warning once "cannot set terminal
        // process group (-38)" / "no job control in this shell". So: stdin
        // and stdout stay on the pty (echo, line discipline, readline all
        // real), stderr is a PIPE pumped into the same terminal stream below,
        // and `-i` asks for the interactive shell that stderr-not-a-tty would
        // otherwise suppress. Every shell this platform offers (sh, bash,
        // dash, zsh, fish) accepts `-i`. The pipe is only the STARTUP
        // arrangement: `ENV` names the staged rc file whose `exec 2>&1`
        // moves stderr onto the pty once job-control init is behind the
        // shell, so prompts and output stop racing each other across two
        // channels (`SHELL_RC_GUEST_PATH`).
        let shell_args: Vec<&str> = vec!["-i"];
        let cwd = if req.cwd.is_empty() {
            "/".to_string()
        } else {
            req.cwd.clone()
        };

        let mut command = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut command);
        let initial_files_path = crate::runtime_artifact::inherit_file_path(
            &mut command,
            &initial_files_alias.directory,
        )?
        .join(INITIAL_FILES_ALIAS_NAME);
        command
            .arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files_path.display()))
            .arg("--")
            .arg(&shell)
            .args(&shell_args)
            .env_clear()
            .envs(&req.env)
            .env("HOME", "/root")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("ENV", SHELL_RC_GUEST_PATH)
            // No history file: the guest tree has no writable `$HOME`, and a
            // failed create there is a litebox panic, not an EACCES (see
            // `SHELL_RC_GUEST_PATH`).
            .env("HISTFILE", "")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            // Node honours this regardless of OpenSSL's compiled-in default
            // path, which is what the planted bundle at
            // /etc/ssl/certs/ca-certificates.crt was NOT being read from
            // (UNABLE_TO_GET_ISSUER_CERT_LOCALLY persisted with a valid
            // 226 KiB bundle present, measured 2026-10-08).
            .envs(guest_ca_env())
            .current_dir(&cwd)
            // SAFETY: dup'd fds are valid, open, and owned until this Stdio
            // takes them — the runner inherits them across exec as its own
            // fd 0/1; stderr is deliberately a pipe (see `shell_args`).
            .stdin(Stdio::from(slave_fd.try_clone()?))
            .stdout(Stdio::from(slave_fd))
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group, tracked in `execs` under the session id exactly
        // like a one-shot exec, so `terminate` can kill a shell (and whatever
        // it forked) together with its cell. The runner already has no
        // controlling terminal (no setsid/TIOCSCTTY), so job control is
        // unchanged by the setpgid.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn litebox runner for interactive shell at {}: {error}",
                self.cfg.runner_bin.display()
            )
        })?;
        let session_id = req.id.clone();
        let cell_id = cell.id.clone();
        let funcs = self.funcs.clone();
        let execs = self.execs.clone();
        if let Some(pid) = child.id() {
            if let Ok(pgid) = i32::try_from(pid) {
                execs.lock().await.insert(
                    session_id.clone(),
                    LiteboxExec {
                        cell: cell_id.clone(),
                        pgid,
                    },
                );
            }
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // The runner's stderr rides the same terminal stream, line by line,
        // minus the two one-line litebox job-control warnings every session
        // would otherwise open with (they describe the runner, not the
        // tenant's shell, and are documented on `shell_args` above). Anything
        // else the runner or the shell writes to stderr — a real error — is
        // shown verbatim.
        if let Some(mut stderr) = child.stderr.take() {
            let tx_err = tx.clone();
            let id_err = session_id.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                // Raw chunks, never lines: an interactive shell writes its
                // PROMPT to stderr without a trailing newline, so a line
                // reader would hold it forever and the terminal would open
                // blank (witnessed on the first cut of this pump: 101
                // upgrade, runner alive, zero frames). The pipe bypasses the
                // pty's output translation, so `\n` becomes `\r\n` here or
                // xterm renders a staircase. The two litebox job-control
                // warnings are whole lines and are dropped where they occur.
                let mut buffer = [0_u8; 4096];
                loop {
                    let n = match stderr.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let chunk = &buffer[..n];
                    let mut out = Vec::with_capacity(n + 16);
                    // A dropped warning owns the newline that FOLLOWS it:
                    // `split` hands that newline to the next segment's
                    // boundary, so it is swallowed there. Truncating the
                    // CRLF BEFORE the dropped line instead left the final
                    // warning's newline as a bare CRLF and every terminal
                    // opened with a blank line above its first prompt
                    // (witnessed on the roll that introduced this pump).
                    let mut swallow_newline = false;
                    for (i, segment) in chunk.split(|b| *b == b'\n').enumerate() {
                        if i > 0 {
                            if swallow_newline {
                                swallow_newline = false;
                            } else {
                                out.extend_from_slice(b"\r\n");
                            }
                        }
                        let text = String::from_utf8_lossy(segment);
                        if text.contains("cannot set terminal process group")
                            || text.contains("no job control in this shell")
                        {
                            swallow_newline = true;
                            continue;
                        }
                        out.extend_from_slice(segment);
                    }
                    if out.is_empty() {
                        continue;
                    }
                    if tx_err
                        .send(hive_core::AgentEvent::PtyOutput {
                            id: id_err.clone(),
                            bytes: out,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        let mut master_read = tokio::fs::File::from_std(master_fd.try_clone()?.into());
        let tx_reader = tx.clone();
        let id_reader = session_id.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buffer = [0_u8; 8192];
            loop {
                match master_read.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx_reader
                            .send(hive_core::AgentEvent::PtyOutput {
                                id: id_reader.clone(),
                                bytes: buffer[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    // EIO on a pty master is the normal "slave closed" signal
                    // once the shell (and everything else holding the slave
                    // open) has exited — not a real error.
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(_) => break,
                }
            }
        });

        let id_waiter = session_id.clone();
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            execs.lock().await.remove(&id_waiter);
            let exit_code = status.and_then(|s| s.code());
            let _ = tx.send(hive_core::AgentEvent::PtyExited {
                id: id_waiter,
                exit_code,
            });
            // Drop this cell's placeholder from `funcs` if a real function
            // launch never claimed the slot — best-effort, the interactive
            // shell was never registered there to begin with, this just
            // guards a hypothetical future caller that keys off cell_id.
            let _ = funcs.lock().await.remove(&cell_id);
            // The shell runner's own TUN link goes with it: the armed rollback
            // deletes the device now that no process holds it (`allocate_link`).
            drop(link);
            // The guest tar and its descriptor-bound alias MUST outlive the
            // runner: their Drop guards unlink the alias name and its scratch
            // directory. Dropping them at the end of `exec_pty` (as this
            // function once did) raced the runner's own open of
            // `--initial-files=/proc/self/fd/N/initial-files.tar` — the entry
            // was gone a few milliseconds after spawn, litebox died with a bare
            // `No such file or directory (os error 2)` on the pty, and every
            // dashboard terminal "instantly disconnected" (witnessed
            // 2026-09-01 on fc-sanjose: 101 upgrade, then that error frame and
            // `{"type":"exited","exit_code":1}`). `exec_command` has always
            // held both in its waiter; this is the same discipline.
            drop(initial_files_alias);
            drop(shell_tar);
        });

        let master_for_io = std::sync::Arc::new(master_fd);
        let master_input = master_for_io.clone();
        let master_resize = master_for_io;
        let pty = crate::PtyIo::new(
            move |bytes: Vec<u8>| {
                use std::io::Write;
                let mut f = &*master_input;
                let _ = f.write_all(&bytes);
            },
            move |cols: u16, rows: u16| {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(
                        std::os::fd::AsRawFd::as_raw_fd(&*master_resize),
                        libc::TIOCSWINSZ as _,
                        &ws,
                    );
                }
            },
        );
        Ok((rx, pty))
    }
}

/// Allocate a real host pty (`openpty(3)`) sized to `cols`x`rows`, returned as
/// a `(master, slave)` pair of owned fds. Used by `LiteboxBackend::exec_pty`
/// to give an interactive sandbox shell genuine raw-mode terminal behavior —
/// see that method's own doc comment for why a host-side pty is sufficient
/// here (litebox's guest and this host process share one address space, so
/// handing the slave straight to the spawned runner as its stdin/stdout/
/// stderr works exactly like a real terminal session, no in-guest pty
/// subsystem required).
#[cfg(target_os = "linux")]
fn open_pty_pair(cols: u16, rows: u16) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty is given valid out-params and a stack winsize; on
    // success both fds are open, valid, and owned by this process.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    anyhow::ensure!(
        rc == 0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: both fds were just returned by a successful openpty() call
    // above and are not owned anywhere else yet.
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    Ok((master, slave))
}

#[cfg(not(target_os = "linux"))]
fn open_pty_pair(_cols: u16, _rows: u16) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    anyhow::bail!("litebox interactive shell requires a real Linux pty (openpty)")
}
