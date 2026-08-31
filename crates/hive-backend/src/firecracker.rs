//! Firecracker cell backend — a cell is a real aarch64 microVM on KVM.
//!
//! Runs inside a Lima VM started with `nestedVirtualization=true` on an Apple
//! Silicon M3/M4 host, where `/dev/kvm` is exposed to the guest. This is the
//! faithful analogue of Hive's design: one cell == one Firecracker process,
//! and the box daemon (this code) talks to the cell daemon (`hive-cell-agent`,
//! running inside the guest) over **vsock**.
//!
//! Lifecycle here mirrors the control plane's expectations:
//! * [`provision`](FirecrackerBackend::provision) boots a microVM and leaves the
//!   in-guest agent idle — this is what the warm pool pre-pays.
//! * [`run_build`](FirecrackerBackend::run_build) connects to the agent over
//!   vsock, ships the job, and streams logs/result back.
//! * [`terminate`](FirecrackerBackend::terminate) kills the Firecracker process
//!   and reclaims the cell's runtime dir (cells are single-use).
//!
//! It compiles on all platforms; it only *works* where `/dev/kvm` and the
//! `firecracker` binary exist.

use crate::{
    CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink, SealedRuntimeArtifact,
};
use anyhow::Context;
use async_trait::async_trait;
use hive_core::{
    agent_handshake_transcript, now_ms, validate_agent_handshake_response_frame,
    validate_agent_versioned_launch_event_frame, AgentBootProof, AgentEvent,
    AgentFunctionFaultCode, AgentHandshake, AgentHandshakeReady, AgentRequest, AgentWireProtocol,
    BuildJob, BuildResult, CellId, LogLine, LogStream, RuntimeArtifactIdentity,
    RuntimeArtifactRootfsMetadata, AGENT_HANDSHAKE_NONCE_BYTES, AGENT_WIRE_CAPABILITIES,
    AGENT_WIRE_PROTOCOL_VERSION, CELL_AGENT_PORT, CELL_FUNCTION_PORT, CELL_GUEST_CID,
    RUNTIME_ARTIFACT_PROTOCOL_VERSION, RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION,
    RUNTIME_ARTIFACT_ROOTFS_SIDECAR_SUFFIX,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const RUNTIME_ARTIFACT_PUBLICATION_SCHEMA: u16 = 1;
const RUNTIME_ARTIFACT_LOCK_STRIPES: usize = 64;
const RUNTIME_ARTIFACT_STALE_GRACE_SECS: u64 = 60 * 60;
const RUNTIME_ARTIFACT_STALE_SCAN_MAX_ENTRIES: usize = 8192;
const RUNTIME_ARTIFACT_STALE_MAX_REAP_FRACTION: f64 = 0.6;
static RUNTIME_ARTIFACT_LOCKS: std::sync::OnceLock<Vec<Mutex<()>>> = std::sync::OnceLock::new();
static ROOTFS_BOOT_LOCKS: std::sync::OnceLock<Vec<Arc<Mutex<()>>>> = std::sync::OnceLock::new();
static RUNTIME_ARTIFACT_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RuntimeArtifactPublication {
    schema: u16,
    identity: RuntimeArtifactIdentity,
    image_sha256: String,
    image_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootfsFileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

#[derive(Clone, Debug)]
struct VerifiedRootfsProtocol {
    image_stamp: RootfsFileStamp,
    proof_stamp: RootfsFileStamp,
    metadata: RuntimeArtifactRootfsMetadata,
}

struct RuntimeArtifactAuthorizationGuard {
    cell: CellId,
    authorizations: Arc<std::sync::Mutex<HashMap<CellId, RuntimeArtifactIdentity>>>,
}

impl RuntimeArtifactAuthorizationGuard {
    fn install(
        cell: CellId,
        identity: RuntimeArtifactIdentity,
        authorizations: Arc<std::sync::Mutex<HashMap<CellId, RuntimeArtifactIdentity>>>,
    ) -> anyhow::Result<Self> {
        let mut entries = authorizations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            !entries.contains_key(&cell),
            "cell {cell} already has an in-flight runtime artifact authorization"
        );
        entries.insert(cell.clone(), identity);
        drop(entries);
        Ok(Self {
            cell,
            authorizations,
        })
    }
}

impl Drop for RuntimeArtifactAuthorizationGuard {
    fn drop(&mut self) {
        self.authorizations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cell);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PublicationFileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationLinkState {
    Missing,
    Owned,
    Foreign,
}

struct PublicationTempGuard {
    temporary_image: PathBuf,
    temporary_identity: PathBuf,
    image: PathBuf,
    identity: PathBuf,
    backup: PathBuf,
    temporary_image_owner: PublicationFileIdentity,
    temporary_identity_owner: PublicationFileIdentity,
    backup_owner: Option<PublicationFileIdentity>,
    publication_started: bool,
    publication_committed: bool,
    restore_legacy: bool,
    armed: bool,
}

impl PublicationTempGuard {
    fn new(
        temporary_image: PathBuf,
        temporary_identity: PathBuf,
        image: PathBuf,
        identity: PathBuf,
        backup: PathBuf,
    ) -> anyhow::Result<Self> {
        let temporary_image_owner = publication_file_identity(&temporary_image)?;
        let temporary_identity_owner = publication_file_identity(&temporary_identity)?;
        Ok(Self {
            temporary_image,
            temporary_identity,
            image,
            identity,
            backup,
            temporary_image_owner,
            temporary_identity_owner,
            backup_owner: None,
            publication_started: false,
            publication_committed: false,
            restore_legacy: false,
            armed: true,
        })
    }

    fn begin_publication(&mut self, restore_legacy: bool) -> anyhow::Result<()> {
        self.temporary_image_owner = publication_file_identity(&self.temporary_image)?;
        self.temporary_identity_owner = publication_file_identity(&self.temporary_identity)?;
        anyhow::ensure!(
            publication_link_state(&self.temporary_image, &self.temporary_image_owner)?
                == PublicationLinkState::Owned
                && publication_link_state(
                    &self.temporary_identity,
                    &self.temporary_identity_owner
                )? == PublicationLinkState::Owned,
            "runtime artifact publication temporaries changed before publication"
        );
        if restore_legacy {
            let backup_owner = publication_file_identity(&self.backup)?;
            self.backup_owner = Some(backup_owner.clone());
            anyhow::ensure!(
                publication_file_identity(&self.image)? == backup_owner,
                "runtime artifact legacy backup does not own the prior canonical image"
            );
        }
        self.publication_started = true;
        self.restore_legacy = restore_legacy;
        Ok(())
    }

    fn mark_publication_committed(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            publication_link_state(&self.image, &self.temporary_image_owner)?
                == PublicationLinkState::Owned
                && publication_link_state(&self.identity, &self.temporary_identity_owner)?
                    == PublicationLinkState::Owned,
            "runtime artifact canonical pair changed before publication commit"
        );
        self.publication_committed = true;
        Ok(())
    }

    fn remove_transients(&self) -> anyhow::Result<()> {
        if self.publication_committed {
            if let Some(backup_owner) = self.backup_owner.as_ref() {
                remove_owned_publication_file(&self.backup, backup_owner)?;
            }
        }
        remove_owned_publication_file(&self.temporary_image, &self.temporary_image_owner)?;
        remove_owned_publication_file(&self.temporary_identity, &self.temporary_identity_owner)?;
        if !self.publication_started {
            if let Some(backup_owner) = self.backup_owner.as_ref() {
                remove_owned_publication_file(&self.backup, backup_owner)?;
            }
        }
        Ok(())
    }

    fn rollback_publication(&self) -> anyhow::Result<()> {
        if !self.publication_started || self.publication_committed {
            return Ok(());
        }

        let image_state = publication_link_state(&self.image, &self.temporary_image_owner)?;
        let identity_state =
            publication_link_state(&self.identity, &self.temporary_identity_owner)?;
        anyhow::ensure!(
            image_state != PublicationLinkState::Foreign
                && identity_state != PublicationLinkState::Foreign,
            "refusing to roll back runtime artifact canonical links owned by another publisher"
        );
        if self.restore_legacy {
            let backup_owner = self.backup_owner.as_ref().ok_or_else(|| {
                anyhow::anyhow!("runtime artifact legacy rollback has no owned backup")
            })?;
            anyhow::ensure!(
                publication_link_state(&self.backup, backup_owner)? == PublicationLinkState::Owned,
                "refusing runtime artifact rollback without its exact legacy backup"
            );
        }

        // The image is the commit marker, so unlink and sync it first. Every
        // durable prefix after this point is recoverable: sidecar+backup,
        // backup alone, then the restored legacy image.
        if image_state == PublicationLinkState::Owned {
            remove_owned_publication_file(&self.image, &self.temporary_image_owner)?;
        }
        if identity_state == PublicationLinkState::Owned {
            remove_owned_publication_file(&self.identity, &self.temporary_identity_owner)?;
        }
        if self.restore_legacy {
            let backup_owner = self.backup_owner.as_ref().expect("checked above");
            anyhow::ensure!(
                publication_link_state(&self.image, &self.temporary_image_owner)?
                    == PublicationLinkState::Missing
                    && publication_link_state(&self.identity, &self.temporary_identity_owner)?
                        == PublicationLinkState::Missing
                    && publication_link_state(&self.backup, backup_owner)?
                        == PublicationLinkState::Owned,
                "runtime artifact rollback namespace changed before legacy restore"
            );
            std::fs::rename(&self.backup, &self.image).with_context(|| {
                format!(
                    "restore cancelled runtime artifact publication {}",
                    self.image.display()
                )
            })?;
            sync_parent_blocking(&self.image)?;
        }
        Ok(())
    }

    fn commit(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.publication_committed,
            "runtime artifact publication guard committed before its canonical pair"
        );
        self.remove_transients()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PublicationTempGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self.rollback_publication() {
            tracing::error!(
                image = %self.image.display(),
                step = "rollback",
                error = %error,
                "runtime artifact cancellation cleanup failed; preserved paths require recovery"
            );
            return;
        }
        if let Err(error) = self.remove_transients() {
            tracing::error!(
                image = %self.image.display(),
                step = "temporary cleanup",
                error = %error,
                "runtime artifact cancellation cleanup failed; preserved paths require recovery"
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct FirecrackerConfig {
    pub firecracker_bin: PathBuf,
    /// Uncompressed guest kernel (vmlinux) image.
    pub kernel_image: PathBuf,
    /// Directory of per-image base rootfs files, named `<image>.ext4`.
    /// Image names with `/` or `:` are sanitized to `_`.
    pub rootfs_dir: PathBuf,
    /// Per-cell runtime state (api sockets, overlay disks, vsock) lives here.
    pub run_dir: PathBuf,
    /// Logical name of the shared base rootfs to boot when an image has no
    /// dedicated `<image>.ext4` (the common case: every deployment boots the same
    /// runtime base and gets its build output from the attached data drive).
    pub base_image: String,
    /// Host-side build cache directory (tarballs keyed by cache key). Survives
    /// the ephemeral microVMs — the cross-build cache the agent restores from.
    pub cache_dir: PathBuf,
    pub boot_args: String,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        FirecrackerConfig {
            firecracker_bin: PathBuf::from("/usr/local/bin/firecracker"),
            kernel_image: PathBuf::from("/var/lib/hive/vmlinux"),
            rootfs_dir: PathBuf::from("/var/lib/hive/rootfs"),
            run_dir: PathBuf::from("/var/lib/hive/run"),
            base_image: "default".to_string(),
            cache_dir: PathBuf::from("/var/lib/hive/cache"),
            // The agent runs as PID1 (init=). The rootfs drive is the first
            // virtio-blk device, so the kernel finds it at /dev/vda.
            boot_args:
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/sbin/hive-cell-agent"
                    .to_string(),
        }
    }
}

pub struct FirecrackerBackend {
    cfg: FirecrackerConfig,
    /// Live Firecracker processes, keyed by cell. Kept here because `CellHandle`
    /// is `Clone` and stored in the control plane, but a `Child` is not.
    procs: Arc<Mutex<HashMap<CellId, Child>>>,
    /// Per-cell host TAP device name, for outbound networking — torn down with the
    /// cell. See `setup_cell_net`.
    taps: Arc<Mutex<HashMap<CellId, String>>>,
    /// Monotonic index for allocating each cell a distinct /30 egress subnet.
    net_idx: Arc<std::sync::atomic::AtomicU32>,
    /// Set once host NAT/forwarding has been configured.
    nat_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Container cells run on the host and own their exact container identity and
    /// tunnel task here; they have no `Child` in `procs`.
    containers: Arc<Mutex<HashMap<CellId, crate::ContainerLaunch>>>,
    /// Caller-authorized H1 identities scoped to one in-flight provision. The
    /// synchronous mutex exists so the Drop guard can erase an authorization on
    /// request cancellation; entries never survive the provision future.
    artifact_authorizations: Arc<std::sync::Mutex<HashMap<CellId, RuntimeArtifactIdentity>>>,
    /// Exact rootfs proofs, keyed by image path and invalidated by image or proof
    /// inode/size/mtime changes. The multi-GiB image is hashed once at boot (or
    /// first use), never on every cold start; an atomic rootfs/proof replacement
    /// necessarily changes a stamp and forces re-verification before another
    /// deployment VM can boot.
    rootfs_protocols: Arc<Mutex<HashMap<PathBuf, VerifiedRootfsProtocol>>>,
    /// Proof selected while the immutable base bytes for one deployment cell were
    /// stable and copied. Launch consumes this per-cell fact rather than whatever
    /// rootfs publication happens to be canonical later.
    cell_rootfs_proofs: Arc<std::sync::Mutex<HashMap<CellId, RuntimeArtifactRootfsMetadata>>>,
    /// Throttled batch CPU sampler for `cpu_percent` (#2): samples the per-cell
    /// Firecracker VMM host process, whose CPU tracks the guest's vCPU work
    /// (Firecracker is a thin VMM).
    sampler: Arc<crate::CpuSampler>,
}

impl FirecrackerBackend {
    pub fn new(cfg: FirecrackerConfig) -> Self {
        let procs = Arc::new(Mutex::new(HashMap::new()));
        let containers: Arc<Mutex<HashMap<CellId, crate::ContainerLaunch>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // Self-management GC: periodically reap orphaned per-cell run dirs — the
        // multi-GB microVM overlays left behind when a cell's firecracker process
        // is gone (e.g. after a node restart). Without this, dead overlays
        // accumulate and exhaust host disk (the SJ outage). Only dirs with no live
        // process/container AND untouched for a grace period are removed, so an
        // in-flight provision (dir created before the process registers) is never reaped.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let procs_gc = procs.clone();
            let ctrs_gc = containers.clone();
            let run_dir = cfg.run_dir.clone();
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                    Self::gc_orphans(&run_dir, &procs_gc, &ctrs_gc).await;
                }
            });
        }
        FirecrackerBackend {
            cfg,
            procs,
            taps: Arc::new(Mutex::new(HashMap::new())),
            net_idx: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            nat_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            containers,
            artifact_authorizations: Arc::new(std::sync::Mutex::new(HashMap::new())),
            rootfs_protocols: Arc::new(Mutex::new(HashMap::new())),
            cell_rootfs_proofs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sampler: Arc::new(crate::CpuSampler::new()),
        }
    }

    /// PATH for invoking podman on a Linux Firecracker host (systemd units run with
    /// a minimal env). Covers the standard distro locations.
    const PODMAN_PATH: &'static str = crate::PODMAN_PATH;

    /// Remove run dirs for cells that are NOT live (no entry in `procs` for microVM
    /// cells, nor in `containers` for host-podman cells) and whose dir hasn't been
    /// modified in the last 5 minutes (so an in-flight provision is safe). Frees
    /// orphaned overlay disk. Best-effort.
    /// Reclaim per-deployment data images in `rootfs_dir` that no live
    /// deployment references.
    ///
    /// `gc_orphans` below only ever walked `run_dir` (ephemeral per-cell run
    /// dirs). The PERSISTENT `<image>.data.ext4` files this backend writes in
    /// `deliver_build` were structurally invisible to it — no age, no reference
    /// check, nothing — and no other code path deletes them either
    /// (`terminate` removes only `cell.root`; there is no delete method on the
    /// backend trait at all). So every redeploy left its predecessor's whole
    /// multi-GiB image behind, forever.
    ///
    /// Measured consequence on fc-sanjose, 2026-07-31: 370 images, 627 GiB
    /// apparent on a 493 GiB disk, dating back five weeks. The node reached 0
    /// bytes free and took 9 customer deployments down — and because the
    /// headroom check ran `gc_orphans` and freed nothing, it reported "after
    /// GC", which read as "nothing left to reclaim" when in fact the GC could
    /// not see the 325 GiB sitting next to it.
    ///
    /// `keep` is the set of image stems still referenced. Anything else is
    /// removed once older than `grace` — the age guard matters because an image
    /// is written by `deliver_build` BEFORE the deployment that references it
    /// finishes registering, and reaping that window would delete a build in
    /// flight.
    pub async fn gc_rootfs_images(
        rootfs_dir: &std::path::Path,
        keep: &std::collections::HashSet<String>,
        grace: Duration,
    ) -> (usize, u64) {
        let now = std::time::SystemTime::now();
        let mut removed = 0usize;
        let mut bytes = 0u64;

        // Collect first, decide second — the blast-radius check below has to see
        // the whole picture before a single file is unlinked.
        let mut candidates: Vec<(std::path::PathBuf, String, u64)> = Vec::new();
        let mut total_images = 0usize;
        let mut entries = match tokio::fs::read_dir(rootfs_dir).await {
            Ok(d) => d,
            Err(_) => return (0, 0),
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            // Only this backend's own per-deployment data images. Never touch
            // the shared base rootfs/kernel artifacts sitting in the same dir.
            let Some(stem) = name.strip_suffix(".data.ext4") else {
                continue;
            };
            total_images += 1;
            // Match BOTH the literal stem and the prefix-stripped form.
            //
            // `deliver_build` names images `dpl-{bid}` where `bid` is ALREADY
            // `dpl-<hash>`, so every file on disk is `dpl-dpl-<hash>.data.ext4`
            // — a real double-prefix bug (git.rs's `format!("dpl-{}", bid)`).
            // A caller that builds `keep` from deployment ids the obvious way
            // produces `dpl-<hash>`, which matches NOTHING on disk, and this GC
            // would then delete every live deployment's data disk. Measured on
            // fc-sanjose while writing this: 0 of 369 stems matched raw
            // deployment ids, 328 of 369 matched once the extra prefix was
            // stripped. Accepting both forms makes that class of caller bug
            // unable to cause data loss, instead of relying on every caller
            // knowing about the naming quirk.
            let alt = stem.strip_prefix("dpl-").unwrap_or(stem);
            if keep.contains(stem) || keep.contains(alt) {
                continue;
            }
            let Ok(md) = e.metadata().await else { continue };
            let recent = md
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|age| age < grace)
                .unwrap_or(true); // unknown mtime => treat as recent, never reap
            if recent {
                continue;
            }
            candidates.push((e.path(), stem.to_string(), md.len()));
        }

        // BLAST-RADIUS GUARD. An empty or wrongly-namespaced `keep` set makes
        // every image on disk look orphaned, and this function would then delete
        // every live deployment's data disk — unrecoverable. No legitimate
        // reclaim pass ever needs to remove most of the fleet's images at once,
        // so refuse rather than proceed, and say why. `HIVE_GC_MAX_REAP_FRACTION`
        // relaxes it for a deliberate bulk cleanup.
        let max_fraction = std::env::var("HIVE_GC_MAX_REAP_FRACTION")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|f| *f > 0.0 && *f <= 1.0)
            .unwrap_or(0.5);
        if keep.is_empty() && total_images > 0 {
            tracing::error!(
                total_images,
                "gc: REFUSING to reclaim — the keep-set is empty, which would delete every \
                 deployment image on this node. This is a caller bug, not an empty disk."
            );
            return (0, 0);
        }
        if total_images > 0 {
            let frac = candidates.len() as f64 / total_images as f64;
            if frac > max_fraction {
                tracing::error!(
                    candidates = candidates.len(),
                    total_images,
                    fraction = frac,
                    max_fraction,
                    "gc: REFUSING to reclaim — too large a share of images look orphaned, which \
                     usually means the keep-set was built in the wrong namespace (see the \
                     dpl-dpl- double-prefix note). Raise HIVE_GC_MAX_REAP_FRACTION to override."
                );
                return (0, 0);
            }
        }

        for (path, stem, sz) in candidates {
            if tokio::fs::remove_file(&path).await.is_ok() {
                removed += 1;
                bytes += sz;
                tracing::info!(image = %stem, mib = sz / (1024 * 1024), "gc: reclaimed orphaned deployment image");
            }
        }
        if removed > 0 {
            tracing::warn!(
                images = removed,
                mib = bytes / (1024 * 1024),
                "gc: reclaimed orphaned deployment images from rootfs dir"
            );
        }
        (removed, bytes)
    }

    async fn gc_orphans(
        run_dir: &std::path::Path,
        procs: &Arc<Mutex<HashMap<CellId, Child>>>,
        containers: &Arc<Mutex<HashMap<CellId, crate::ContainerLaunch>>>,
    ) {
        let mut live: std::collections::HashSet<String> =
            procs.lock().await.keys().map(|c| c.to_string()).collect();
        live.extend(containers.lock().await.keys().map(|c| c.to_string()));
        let grace = Duration::from_secs(300);
        let now = std::time::SystemTime::now();
        let mut tenants = match tokio::fs::read_dir(run_dir).await {
            Ok(d) => d,
            Err(_) => return,
        };
        while let Ok(Some(tenant)) = tenants.next_entry().await {
            let tp = tenant.path();
            if !tp.is_dir() {
                continue;
            }
            let mut cells = match tokio::fs::read_dir(&tp).await {
                Ok(d) => d,
                Err(_) => continue,
            };
            while let Ok(Some(cell)) = cells.next_entry().await {
                let cid = cell.file_name().to_string_lossy().to_string();
                if live.contains(&cid) {
                    continue;
                }
                // Age guard: skip dirs modified recently (possible in-flight provision).
                let recent = cell
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| now.duration_since(t).ok())
                    .map(|age| age < grace)
                    .unwrap_or(false);
                if recent {
                    continue;
                }
                let p = cell.path();
                if tokio::fs::remove_dir_all(&p).await.is_ok() {
                    tracing::info!(cell = %cid, "gc: reaped orphaned cell run dir");
                }
            }
        }
    }

    /// Free bytes available on the filesystem backing `path` (statvfs). 0 on error.
    fn disk_free_bytes(path: &std::path::Path) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let cpath = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => return 0,
            };
            // SAFETY: zeroed statvfs is a valid initial value; we only read it on success.
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } == 0 {
                return (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
            }
            0
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            u64::MAX
        }
    }

    /// Disk-pressure guard for cold starts: if the run filesystem is critically low,
    /// reap orphaned overlays first, then refuse the provision (clear error, no
    /// silent half-boot) if still below the floor. Each microVM overlay is multi-GB,
    /// so booting into a near-full disk is what corrupted SJ's state (the outage).
    async fn ensure_disk_headroom(&self) -> anyhow::Result<()> {
        // Floor: a microVM overlay pair (rootfs + data) plus slack. ~3 GiB by
        // default; `HIVE_DISK_FLOOR_MIB` tunes it. It was a hardcoded const,
        // which meant a fleet with larger images had no way to raise it without
        // a rebuild.
        let floor_bytes: u64 = std::env::var("HIVE_DISK_FLOOR_MIB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mib| mib * 1024 * 1024)
            .unwrap_or(3 * 1024 * 1024 * 1024);
        let base = if self.cfg.run_dir.exists() {
            self.cfg.run_dir.as_path()
        } else {
            std::path::Path::new("/")
        };
        let before = Self::disk_free_bytes(base);
        if before >= floor_bytes {
            return Ok(());
        }
        tracing::warn!(
            free_mib = before / (1024 * 1024),
            floor_mib = floor_bytes / (1024 * 1024),
            "disk below floor before provision — running orphan GC"
        );
        Self::gc_orphans(&self.cfg.run_dir, &self.procs, &self.containers).await;
        let free = Self::disk_free_bytes(base);
        // Report what the GC actually accomplished. The old message ended at
        // "after GC", which read as "nothing left to reclaim" even when the GC
        // had freed literally zero bytes because it could not see the images
        // filling the disk (see `gc_rootfs_images`). Naming the delta makes an
        // ineffective GC visible instead of implying an exhausted one.
        anyhow::ensure!(
            free >= floor_bytes,
            "no capacity: host disk critically low ({} MiB free, need {} MiB); GC reclaimed {} MiB",
            free / (1024 * 1024),
            floor_bytes / (1024 * 1024),
            free.saturating_sub(before) / (1024 * 1024)
        );
        Ok(())
    }

    /// Probe so the box daemon can choose mock vs. firecracker.
    pub fn is_supported(&self) -> bool {
        cfg!(target_os = "linux")
            && std::path::Path::new("/dev/kvm").exists()
            && self.cfg.firecracker_bin.exists()
    }

    fn rootfs_for(&self, image: &str) -> PathBuf {
        self.cfg
            .rootfs_dir
            .join(format!("{}.ext4", crate::sanitize_image(image)))
    }

    fn rootfs_protocol_sidecar_for(rootfs: &std::path::Path) -> PathBuf {
        let mut path = rootfs.as_os_str().to_os_string();
        path.push(RUNTIME_ARTIFACT_ROOTFS_SIDECAR_SUFFIX);
        PathBuf::from(path)
    }

    fn rootfs_boot_lock_for(path: &std::path::Path) -> anyhow::Result<Arc<Mutex<()>>> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cell rootfs path has no parent: {}", path.display()))?;
        let file_name = path.file_name().ok_or_else(|| {
            anyhow::anyhow!("cell rootfs path has no file name: {}", path.display())
        })?;
        let canonical = std::fs::canonicalize(parent)
            .with_context(|| format!("canonicalize cell rootfs directory {}", parent.display()))?
            .join(file_name);
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in canonical.as_os_str().to_string_lossy().bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let locks = ROOTFS_BOOT_LOCKS.get_or_init(|| {
            (0..RUNTIME_ARTIFACT_LOCK_STRIPES)
                .map(|_| Arc::new(Mutex::new(())))
                .collect()
        });
        Ok(locks[hash as usize % locks.len()].clone())
    }

    fn rootfs_open_stamp(file: &std::fs::File) -> anyhow::Result<RootfsFileStamp> {
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file(),
            "opened cell rootfs is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                metadata.uid() == 0 && metadata.nlink() == 1 && metadata.mode() & 0o022 == 0,
                "opened cell rootfs must be root-owned, single-link, and not group/world-writable"
            );
            Ok(RootfsFileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(RootfsFileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    fn write_cell_rootfs_protocol_metadata(
        rootfs: &std::path::Path,
        metadata: &RuntimeArtifactRootfsMetadata,
    ) -> anyhow::Result<RootfsFileStamp> {
        use std::io::Write as _;
        let sidecar = Self::rootfs_protocol_sidecar_for(rootfs);
        let bytes = serde_json::to_vec(metadata)?;
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= 4096,
            "cell rootfs protocol proof must be between 1 and 4096 bytes"
        );
        remove_file_if_exists(&sidecar)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o444);
        }
        let mut file = options.open(&sidecar)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        let mut permissions = file.metadata()?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o444);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(true);
        std::fs::set_permissions(&sidecar, permissions)?;
        sync_parent_blocking(&sidecar)?;
        Self::rootfs_protocol_stamp(rootfs)
    }

    fn rootfs_stamp(path: &std::path::Path) -> anyhow::Result<RootfsFileStamp> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("stat rootfs image {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "rootfs image is not a regular file: {}",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                metadata.uid() == 0 && metadata.nlink() == 1 && metadata.mode() & 0o022 == 0,
                "rootfs image must be root-owned, single-link, and not group/world-writable: {}",
                path.display()
            );
            Ok(RootfsFileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(RootfsFileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    fn rootfs_protocol_stamp(rootfs: &std::path::Path) -> anyhow::Result<RootfsFileStamp> {
        let sidecar = Self::rootfs_protocol_sidecar_for(rootfs);
        let parent = sidecar
            .parent()
            .ok_or_else(|| anyhow::anyhow!("rootfs protocol sidecar has no parent directory"))?;
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        anyhow::ensure!(
            parent_metadata.is_dir(),
            "rootfs directory is not a directory"
        );
        let metadata = std::fs::symlink_metadata(&sidecar)
            .with_context(|| format!("stat rootfs runtime-artifact proof {}", sidecar.display()))?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "rootfs runtime-artifact proof is not a regular file: {}",
            sidecar.display()
        );
        anyhow::ensure!(
            metadata.len() > 0 && metadata.len() <= 4096,
            "rootfs runtime-artifact proof must be between 1 and 4096 bytes: {}",
            sidecar.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                parent_metadata.uid() == 0 && parent_metadata.mode() & 0o022 == 0,
                "rootfs directory must be root-owned and not group/world-writable: {}",
                parent.display()
            );
            anyhow::ensure!(
                metadata.uid() == 0 && metadata.nlink() == 1 && metadata.mode() & 0o222 == 0,
                "rootfs runtime-artifact proof must be root-owned, single-link, and read-only: {}",
                sidecar.display()
            );
            Ok(RootfsFileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(RootfsFileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    fn lowercase_sha256(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn read_rootfs_protocol_metadata(
        rootfs: &std::path::Path,
    ) -> anyhow::Result<RuntimeArtifactRootfsMetadata> {
        let sidecar = Self::rootfs_protocol_sidecar_for(rootfs);
        let parent = sidecar
            .parent()
            .ok_or_else(|| anyhow::anyhow!("rootfs protocol sidecar has no parent directory"))?;
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        anyhow::ensure!(
            parent_metadata.is_dir(),
            "rootfs directory is not a directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            anyhow::ensure!(
                parent_metadata.uid() == 0 && parent_metadata.mode() & 0o022 == 0,
                "rootfs directory must be root-owned and not group/world-writable: {}",
                parent.display()
            );
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&sidecar)
                .with_context(|| {
                    format!("open rootfs runtime-artifact proof {}", sidecar.display())
                })?;
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file()
                    && metadata.uid() == 0
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o222 == 0
                    && metadata.len() > 0
                    && metadata.len() <= 4096,
                "rootfs runtime-artifact proof must be a root-owned, read-only, single-link regular file no larger than 4096 bytes: {}",
                sidecar.display()
            );
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            let mut limited =
                std::io::Read::take(std::io::Read::by_ref(&mut file), metadata.len() + 1);
            std::io::Read::read_to_end(&mut limited, &mut bytes)?;
            anyhow::ensure!(
                bytes.len() as u64 == metadata.len(),
                "rootfs runtime-artifact proof changed during exact-length read"
            );
            return serde_json::from_slice(&bytes).with_context(|| {
                format!("parse rootfs runtime-artifact proof {}", sidecar.display())
            });
        }
        #[cfg(not(unix))]
        {
            let bytes = std::fs::read(&sidecar)?;
            anyhow::ensure!(!bytes.is_empty() && bytes.len() <= 4096);
            serde_json::from_slice(&bytes).with_context(|| {
                format!("parse rootfs runtime-artifact proof {}", sidecar.display())
            })
        }
    }

    async fn verified_rootfs_protocol(
        &self,
        rootfs: &std::path::Path,
    ) -> anyhow::Result<RuntimeArtifactRootfsMetadata> {
        let image_stamp = Self::rootfs_stamp(rootfs)?;
        let proof_stamp = Self::rootfs_protocol_stamp(rootfs)?;
        let mut proofs = self.rootfs_protocols.lock().await;
        if let Some(proof) = proofs
            .get(rootfs)
            .filter(|proof| proof.image_stamp == image_stamp && proof.proof_stamp == proof_stamp)
        {
            return Ok(proof.metadata.clone());
        }

        let metadata = Self::read_rootfs_protocol_metadata(rootfs)?;
        anyhow::ensure!(
            metadata.schema == RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION,
            "unsupported rootfs protocol proof schema {}",
            metadata.schema
        );
        anyhow::ensure!(
            metadata.protocol == RUNTIME_ARTIFACT_PROTOCOL_VERSION,
            "rootfs implements runtime-artifact protocol {}, host requires {}",
            metadata.protocol,
            RUNTIME_ARTIFACT_PROTOCOL_VERSION
        );
        anyhow::ensure!(
            metadata.agent_wire_protocol == AGENT_WIRE_PROTOCOL_VERSION,
            "rootfs implements agent wire protocol {}, host requires {}",
            metadata.agent_wire_protocol,
            AGENT_WIRE_PROTOCOL_VERSION
        );
        anyhow::ensure!(
            metadata.agent_wire_capabilities == AGENT_WIRE_CAPABILITIES,
            "rootfs implements agent wire capabilities {:#x}, host requires {:#x}",
            metadata.agent_wire_capabilities,
            AGENT_WIRE_CAPABILITIES
        );
        anyhow::ensure!(
            Self::lowercase_sha256(&metadata.agent_sha256)
                && Self::lowercase_sha256(&metadata.image_sha256)
                && metadata.image_bytes > 0,
            "rootfs protocol proof has an invalid agent/image digest or byte count"
        );
        let (digest, bytes) = hash_file_sha256(rootfs).await?;
        anyhow::ensure!(
            digest == metadata.image_sha256 && bytes == metadata.image_bytes,
            "rootfs image bytes do not match their protocol proof"
        );
        let final_image_stamp = Self::rootfs_stamp(rootfs)?;
        let final_proof_stamp = Self::rootfs_protocol_stamp(rootfs)?;
        anyhow::ensure!(
            final_image_stamp == image_stamp && final_proof_stamp == proof_stamp,
            "rootfs image or its proof changed while the agent protocols were verified"
        );
        proofs.insert(
            rootfs.to_path_buf(),
            VerifiedRootfsProtocol {
                image_stamp,
                proof_stamp,
                metadata: metadata.clone(),
            },
        );
        Ok(metadata)
    }

    async fn verified_rootfs_runtime_artifact_protocol(
        &self,
        rootfs: &std::path::Path,
    ) -> anyhow::Result<u16> {
        Ok(self.verified_rootfs_protocol(rootfs).await?.protocol)
    }

    /// Protocol proved by the exact shared base rootfs this backend will boot.
    /// `hive-cloud` advertises only this result; a sidecar's existence alone is
    /// intentionally insufficient.
    pub async fn base_runtime_artifact_protocol(&self) -> anyhow::Result<u16> {
        let rootfs = self.rootfs_for(&self.cfg.base_image);
        self.verified_rootfs_runtime_artifact_protocol(&rootfs)
            .await
    }

    /// Complete wire fact proved from the exact base-rootfs bytes. This is the
    /// additive scheduler input for capability-gating later NodeInfo work.
    pub async fn base_agent_wire_protocol(&self) -> anyhow::Result<AgentWireProtocol> {
        let rootfs = self.rootfs_for(&self.cfg.base_image);
        let metadata = self.verified_rootfs_protocol(&rootfs).await?;
        Ok(AgentWireProtocol {
            protocol: metadata.agent_wire_protocol,
            capabilities: metadata.agent_wire_capabilities,
        })
    }

    /// Per-deployment build-output ext4 (the artifact `deliver_build` packs and
    /// `provision` attaches as the cell's second drive). Lives alongside the
    /// base rootfs images, keyed by the same logical image name.
    fn data_image_for(&self, image: &str) -> PathBuf {
        self.cfg
            .rootfs_dir
            .join(format!("{}.data.ext4", crate::sanitize_image(image)))
    }

    fn data_identity_for(&self, image: &str) -> PathBuf {
        let mut path = self.data_image_for(image).into_os_string();
        path.push(".identity.json");
        PathBuf::from(path)
    }

    fn publication_temp_token() -> String {
        use std::sync::atomic::Ordering;
        let sequence = RUNTIME_ARTIFACT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        format!("{}-{nanos:032x}-{sequence:016x}", std::process::id())
    }

    fn publication_temp_for(path: &std::path::Path, token: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(format!(".publishing.{token}"));
        PathBuf::from(value)
    }

    fn publication_backup_for(path: &std::path::Path) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(".legacy-unverified");
        PathBuf::from(value)
    }

    fn cleanup_stale_publication_files(
        rootfs_dir: &std::path::Path,
        out: &std::path::Path,
        sidecar: &std::path::Path,
        keep: &std::collections::HashSet<PathBuf>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            !keep.is_empty(),
            "refusing runtime artifact stale cleanup with an empty keep-set"
        );
        for path in keep {
            let metadata = std::fs::symlink_metadata(path).with_context(|| {
                format!(
                    "refusing runtime artifact stale cleanup without live keep path {}",
                    path.display()
                )
            })?;
            anyhow::ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "refusing runtime artifact stale cleanup with non-regular keep path {}",
                path.display()
            );
        }

        let out_name = out
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("runtime artifact image has no file name"))?
            .to_string_lossy();
        let sidecar_name = sidecar
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("runtime artifact sidecar has no file name"))?
            .to_string_lossy();
        let out_temp_prefix = format!("{out_name}.publishing");
        let sidecar_temp_prefix = format!("{sidecar_name}.publishing");
        let backup_name = format!("{out_name}.legacy-unverified");
        let grace = Duration::from_secs(
            std::env::var("HIVE_RUNTIME_ARTIFACT_STALE_GRACE_SECS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(RUNTIME_ARTIFACT_STALE_GRACE_SECS),
        );
        let max_fraction = std::env::var("HIVE_RUNTIME_ARTIFACT_STALE_MAX_REAP_FRACTION")
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|fraction| *fraction > 0.0 && *fraction <= 1.0)
            .unwrap_or(RUNTIME_ARTIFACT_STALE_MAX_REAP_FRACTION);
        let now = std::time::SystemTime::now();
        let mut matched = 0usize;
        let mut candidates = Vec::new();
        let entries = std::fs::read_dir(rootfs_dir)?;
        for (scanned, entry) in entries.enumerate() {
            anyhow::ensure!(
                scanned < RUNTIME_ARTIFACT_STALE_SCAN_MAX_ENTRIES,
                "refusing runtime artifact stale cleanup after scanning more than {} entries",
                RUNTIME_ARTIFACT_STALE_SCAN_MAX_ENTRIES
            );
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale_kind = if name == backup_name {
                Some(2_u8)
            } else if name == out_temp_prefix || name.starts_with(&format!("{out_temp_prefix}.")) {
                Some(0_u8)
            } else if name == sidecar_temp_prefix
                || name.starts_with(&format!("{sidecar_temp_prefix}."))
            {
                Some(1_u8)
            } else {
                None
            };
            let Some(stale_kind) = stale_kind else {
                continue;
            };
            matched += 1;
            let path = entry.path();
            if keep.contains(&path) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            let Some(modified) = metadata.modified().ok() else {
                continue;
            };
            let old_enough = now
                .duration_since(modified)
                .map(|age| age >= grace)
                .unwrap_or(false);
            if old_enough {
                candidates.push((path, metadata.len(), modified, stale_kind));
            }
        }

        // A hard link inherits the legacy image's old inode mtime, so its mtime
        // alone cannot prove the backup name is stale. Reap that link only when
        // both unique files from the same maximum crash residue are themselves
        // old enough. A lone backup is retained rather than guessed stale.
        let old_image_temporary = candidates.iter().any(|candidate| candidate.3 == 0);
        let old_identity_temporary = candidates.iter().any(|candidate| candidate.3 == 1);
        if !old_image_temporary || !old_identity_temporary {
            candidates.retain(|candidate| candidate.3 != 2);
        }

        let population = matched.saturating_add(keep.len());
        if population > 0 {
            let fraction = candidates.len() as f64 / population as f64;
            anyhow::ensure!(
                fraction <= max_fraction,
                "refusing runtime artifact stale cleanup: {} of {} publication files ({fraction:.3}) exceed max reap fraction {max_fraction:.3}",
                candidates.len(),
                population
            );
        }

        let mut removed = 0usize;
        for (path, len, modified, _) in candidates {
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != len
                || metadata.modified().ok() != Some(modified)
            {
                continue;
            }
            std::fs::remove_file(&path)?;
            removed += 1;
        }
        if removed > 0 {
            sync_parent_blocking(out)?;
            tracing::warn!(
                removed,
                image = %out.display(),
                "removed guarded stale runtime artifact publication files"
            );
        }
        Ok(removed)
    }

    fn canonical_links_publication_temporary(
        rootfs_dir: &std::path::Path,
        canonical: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let canonical_owner = publication_file_identity(canonical)?;
        let canonical_name = canonical
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("runtime artifact path has no file name"))?
            .to_string_lossy();
        let temporary_prefix = format!("{canonical_name}.publishing");
        for (scanned, entry) in std::fs::read_dir(rootfs_dir)?.enumerate() {
            anyhow::ensure!(
                scanned < RUNTIME_ARTIFACT_STALE_SCAN_MAX_ENTRIES,
                "refusing runtime artifact recovery after scanning more than {} entries",
                RUNTIME_ARTIFACT_STALE_SCAN_MAX_ENTRIES
            );
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != temporary_prefix && !name.starts_with(&format!("{temporary_prefix}.")) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            if publication_file_identity_from_metadata(&entry.path(), &metadata)? == canonical_owner
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn artifact_lock_for(path: &std::path::Path) -> anyhow::Result<&'static Mutex<()>> {
        let parent = path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "runtime artifact publication path has no parent: {}",
                path.display()
            )
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            anyhow::anyhow!(
                "runtime artifact publication path has no file name: {}",
                path.display()
            )
        })?;
        let canonical_path = std::fs::canonicalize(parent)
            .with_context(|| {
                format!(
                    "canonicalize runtime artifact publication directory {}",
                    parent.display()
                )
            })?
            .join(file_name);
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in canonical_path.as_os_str().to_string_lossy().bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let locks = RUNTIME_ARTIFACT_LOCKS.get_or_init(|| {
            (0..RUNTIME_ARTIFACT_LOCK_STRIPES)
                .map(|_| Mutex::new(()))
                .collect()
        });
        Ok(&locks[hash as usize % locks.len()])
    }

    fn validate_runtime_identity(
        bytes: &[u8],
        image: &str,
    ) -> anyhow::Result<RuntimeArtifactIdentity> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= 4096,
            "runtime artifact identity exceeds 4096 bytes for image {image:?}"
        );
        let identity: RuntimeArtifactIdentity = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid runtime artifact identity for image {image:?}"))?;
        anyhow::ensure!(
            identity.protocol == RUNTIME_ARTIFACT_PROTOCOL_VERSION,
            "runtime artifact protocol {} is unsupported for image {image:?}",
            identity.protocol
        );
        anyhow::ensure!(
            identity.id == image,
            "runtime artifact identity {:?} does not match requested image {image:?}",
            identity.id
        );
        anyhow::ensure!(
            identity.content_sha256.len() == 64
                && identity
                    .content_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "runtime artifact identity has an invalid content digest for image {image:?}"
        );
        Ok(identity)
    }

    fn validate_data_publication(
        bytes: &[u8],
        image: &str,
    ) -> anyhow::Result<RuntimeArtifactPublication> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= 4096,
            "runtime artifact publication exceeds 4096 bytes for image {image:?}"
        );
        let publication: RuntimeArtifactPublication = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid runtime artifact publication for image {image:?}"))?;
        anyhow::ensure!(
            publication.schema == RUNTIME_ARTIFACT_PUBLICATION_SCHEMA,
            "runtime artifact publication schema {} is unsupported for image {image:?}",
            publication.schema
        );
        let identity = &publication.identity;
        anyhow::ensure!(
            identity.protocol == RUNTIME_ARTIFACT_PROTOCOL_VERSION,
            "runtime artifact protocol {} is unsupported for image {image:?}",
            identity.protocol
        );
        anyhow::ensure!(
            identity.id == image,
            "runtime artifact identity {:?} does not match requested image {image:?}",
            identity.id
        );
        for (label, digest) in [
            ("content", identity.content_sha256.as_str()),
            ("image", publication.image_sha256.as_str()),
        ] {
            anyhow::ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "runtime artifact publication has an invalid {label} digest for image {image:?}"
            );
        }
        anyhow::ensure!(
            publication.image_bytes > 0,
            "runtime artifact publication has an empty image for {image:?}"
        );
        Ok(publication)
    }

    async fn read_publication_at(
        path: &std::path::Path,
        image: &str,
    ) -> anyhow::Result<RuntimeArtifactPublication> {
        let bytes = tokio::fs::read(path).await.with_context(|| {
            format!(
                "runtime artifact publication is unavailable for image {image:?}: {}",
                path.display()
            )
        })?;
        Self::validate_data_publication(&bytes, image)
    }

    async fn verify_publication_image(
        publication: &RuntimeArtifactPublication,
        path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let (digest, bytes) = hash_file_sha256(path).await?;
        anyhow::ensure!(
            bytes == publication.image_bytes && digest == publication.image_sha256,
            "runtime artifact image bytes do not match its committed publication ({})",
            hive_core::fault::NODE_IMAGE_MISSING
        );
        Ok(())
    }

    async fn read_data_publication(
        &self,
        image: &str,
    ) -> anyhow::Result<RuntimeArtifactPublication> {
        Self::read_publication_at(&self.data_identity_for(image), image).await
    }

    async fn verified_data_identity(
        &self,
        image: &str,
        exact_copy: &std::path::Path,
    ) -> anyhow::Result<RuntimeArtifactIdentity> {
        let publication = self.read_data_publication(image).await?;
        Self::verify_publication_image(&publication, exact_copy)
            .await
            .with_context(|| format!("verify copied runtime artifact for image {image:?}"))?;
        Ok(publication.identity)
    }

    /// Locate a deployment's data image on disk for the storage broker
    /// (`storage_broker`/`hive_backend::snapshot` callers), accounting for
    /// the historical `dpl-dpl-` double-prefix documented on
    /// [`Self::gc_rootfs_images`]: `deliver_build` writes images keyed by
    /// `dpl-{bid}` where `bid` is already `dpl-<hash>`, so what's actually on
    /// disk today is double-prefixed. Tries that as-written form first (what
    /// exists today), then the bare id as a forward-compatible fallback if
    /// that naming bug is ever fixed at the write site — the exact same
    /// both-forms tolerance `gc_rootfs_images` already needed, kept in one
    /// place rather than re-derived per caller. `None` when neither file
    /// exists: a caller must never guess a path that might not be real.
    pub fn locate_data_image(&self, deployment_id: &str) -> Option<PathBuf> {
        let doubled = self.data_image_for(&format!("dpl-{deployment_id}"));
        if doubled.is_file() {
            return Some(doubled);
        }
        let bare = self.data_image_for(deployment_id);
        if bare.is_file() {
            return Some(bare);
        }
        None
    }

    /// Directory a deployment's snapshots are stored under — sibling to the
    /// data images themselves, namespaced by id so `snapshot::snapshot_image`
    /// / `count_snapshots` / `list_snapshots` never need to see any OTHER
    /// deployment's snapshots to do their job.
    ///
    /// The id is embedded inside a longer `snaps-<id>` component rather than
    /// used bare, on purpose: `sanitize_image` passes `.` through unchanged
    /// (it only rewrites `/` and other non-`[A-Za-z0-9.-]` characters), so a
    /// bare sanitized `deployment_id` of exactly `".."` would still BE `".."`
    /// — a real directory-traversal component escaping this dir entirely.
    /// `rootfs_for`/`data_image_for` are already safe from this because they
    /// always suffix `.ext4`/`.data.ext4` (so `".."` becomes the harmless
    /// filename `"...ext4"`, never a standalone `..` component); this method
    /// didn't have a suffix and needs the same embedding to get the same
    /// guarantee. In practice every caller of this method (`storage_api`)
    /// already requires `deployment_id` to match a real, internally-generated
    /// id before ever reaching here — this is defense in depth for whatever
    /// calls it next, not the only gate.
    pub fn snapshot_dir(&self, deployment_id: &str) -> PathBuf {
        self.cfg
            .rootfs_dir
            .join("snapshots")
            .join(format!("snaps-{}", crate::sanitize_image(deployment_id)))
    }

    /// Idempotently enable IP forwarding + NAT so guest microVMs (172.16/16) can
    /// reach the internet. Run once per process; failures are non-fatal (cells
    /// just won't get egress).
    async fn ensure_host_nat(&self) {
        use std::sync::atomic::Ordering;
        if self.nat_ready.swap(true, Ordering::SeqCst) {
            return;
        }
        // Use iptables where present, else nftables (modern distros ship only
        // `nft`). Both set a MASQUERADE on the guest subnet + allow forwarding.
        // TENANT ISOLATION: the cell↔cell DROP must precede the ACCEPTs. Without
        // it, `-s 172.16/16 ACCEPT` also forwards guest→guest traffic (the host
        // routes every per-cell /30), letting one tenant's microVM reach another's.
        // Internet egress return-traffic is unaffected (src is external, so the
        // 172.16→172.16 pair never matches), and host↔guest control-plane traffic
        // uses OUTPUT/INPUT, not FORWARD.
        // HAIRPIN: a guest that POSTs to its OWN deployment's public hostname
        // resolves the node's PUBLIC ip, but that address lives on the cloud
        // provider's 1:1 NAT and not on any host interface — so the packet is
        // MASQUERADEd out the default route and dropped by the provider, which
        // surfaced as `RUNTIME_TUNNEL_FAILED` on a callback the app made to
        // itself. hive-cloud already listens on 0.0.0.0:{80,443}, so redirecting
        // those flows back into the host is all that's needed. Scoped to THIS
        // node's own public ip: traffic to any other node still egresses
        // normally, and because REDIRECT lands the packet on the host's INPUT
        // path the cell↔cell FORWARD DROP above is untouched (a guest still
        // cannot reach another guest this way — it reaches the same public
        // host-routing hive-cloud serves everyone).
        let hairpin = match std::env::var("HIVE_PUBLIC_IP")
            .ok()
            .map(|s| s.trim().to_string())
        {
            // `auto` is a real configured value meaning "detect at runtime", not
            // an address — and an unset/unparseable value means this host has no
            // inbound-reachable address to hairpin to at all.
            Some(ip) if ip.parse::<std::net::Ipv4Addr>().is_ok() => ip,
            _ => String::new(),
        };
        // `HAIRPIN_IP` arrives as an ENV VAR rather than interpolated into the
        // script: the nft branch is full of `{ ... }` chain bodies, so a
        // `format!` here would need every brace escaped, and an env var also
        // leaves no shell-injection surface for a configured value.
        let script = r#"
            export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH
            sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1
            if command -v iptables >/dev/null 2>&1; then
              if [ -n "$HAIRPIN_IP" ]; then
                iptables -t nat -C PREROUTING -s 172.16.0.0/16 -d "$HAIRPIN_IP" -p tcp -m multiport --dports 80,443 -j REDIRECT 2>/dev/null \
                  || iptables -t nat -I PREROUTING 1 -s 172.16.0.0/16 -d "$HAIRPIN_IP" -p tcp -m multiport --dports 80,443 -j REDIRECT
              fi
              iptables -C FORWARD -s 172.16.0.0/16 -d 172.16.0.0/16 -j DROP 2>/dev/null || iptables -I FORWARD 1 -s 172.16.0.0/16 -d 172.16.0.0/16 -j DROP
              iptables -t nat -C POSTROUTING -s 172.16.0.0/16 -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s 172.16.0.0/16 -j MASQUERADE
              iptables -C FORWARD -s 172.16.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -s 172.16.0.0/16 -j ACCEPT
              iptables -C FORWARD -d 172.16.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -d 172.16.0.0/16 -j ACCEPT
            elif command -v nft >/dev/null 2>&1; then
              nft add table ip hive_nat 2>/dev/null
              if [ -n "$HAIRPIN_IP" ]; then
                nft 'add chain ip hive_nat pre { type nat hook prerouting priority -100 ; }' 2>/dev/null
                nft flush chain ip hive_nat pre 2>/dev/null
                nft add rule ip hive_nat pre ip saddr 172.16.0.0/16 ip daddr "$HAIRPIN_IP" tcp dport '{80, 443}' redirect 2>/dev/null
              fi
              nft 'add chain ip hive_nat post { type nat hook postrouting priority 100 ; }' 2>/dev/null
              nft flush chain ip hive_nat post 2>/dev/null
              nft add rule ip hive_nat post ip saddr 172.16.0.0/16 masquerade 2>/dev/null
              nft 'add chain ip hive_nat fwd { type filter hook forward priority 0 ; }' 2>/dev/null
              nft flush chain ip hive_nat fwd 2>/dev/null
              nft add rule ip hive_nat fwd ip saddr 172.16.0.0/16 ip daddr 172.16.0.0/16 drop 2>/dev/null
              nft add rule ip hive_nat fwd ip saddr 172.16.0.0/16 accept 2>/dev/null
              nft add rule ip hive_nat fwd ip daddr 172.16.0.0/16 accept 2>/dev/null
            fi"#;
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HAIRPIN_IP", &hairpin)
            .output()
            .await;
    }

    /// Allocate a /30 egress subnet + host TAP for a cell. Returns the kernel
    /// `ip=` boot-arg fragment + guest MAC to attach as eth0, or None if host
    /// networking couldn't be set up (cell then boots without egress — never a
    /// regression vs. the old vsock-only behavior).
    async fn setup_cell_net(&self, id: &CellId) -> Option<CellNet> {
        use std::sync::atomic::Ordering;
        self.ensure_host_nat().await;
        let i = self.net_idx.fetch_add(1, Ordering::SeqCst) % 16384;
        let third = ((i >> 6) & 0xff) as u8;
        let base = ((i & 0x3f) as u8) * 4;
        let host_ip = format!("172.16.{third}.{}", base + 1);
        let guest_ip = format!("172.16.{third}.{}", base + 2);
        let tap = format!("fc{i}");
        let mut rollback = LinkRollback::new(tap.clone());
        let mac = format!("02:fc:00:00:{:02x}:{:02x}", (i >> 8) as u8, i as u8);
        // Recreate the tap fresh (delete any stale one from a prior cell at this index).
        let script = format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; \
             ip link del {tap} 2>/dev/null; ip tuntap add dev {tap} mode tap 2>/dev/null && \
             ip addr add {host_ip}/30 dev {tap} 2>/dev/null && ip link set {tap} up 2>/dev/null"
        );
        let ok = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .kill_on_drop(true)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        self.taps.lock().await.insert(id.clone(), tap.clone());
        rollback.commit();
        Some(CellNet {
            tap,
            mac,
            // kernel ip= autoconfig: client::gw:netmask::device:off
            ip_cmdline: format!("ip={guest_ip}::{host_ip}:255.255.255.252::eth0:off"),
        })
    }

    /// Write `/etc/resolv.conf` into the PER-CELL overlay (never the shared base
    /// image) via debugfs — no mount needed. Deployment cells require this final
    /// mutation to complete before their exact boot bytes are hashed; build and
    /// sandbox cells retain the historical best-effort caller posture.
    async fn write_guest_resolv(&self, overlay: &std::path::Path) -> anyhow::Result<()> {
        const RESOLVER: &[u8] = b"nameserver 8.8.8.8\nnameserver 1.1.1.1\n";
        let tmp = overlay.with_extension("resolv.tmp");
        tokio::fs::write(&tmp, RESOLVER).await?;
        let mutation = async {
            // Removing a nonexistent path is harmless; the subsequent write and
            // exact read-back are the authoritative mutation checks. debugfs can
            // report command-level errors while still exiting zero, so status
            // alone must never bless the boot bytes.
            let _ = Command::new("debugfs")
                .args(["-w", "-R", "rm /etc/resolv.conf"])
                .arg(overlay)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            let write = Command::new("debugfs")
                .args(["-w", "-R"])
                .arg(format!("write {} /etc/resolv.conf", tmp.display()))
                .arg(overlay)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            anyhow::ensure!(
                write.success(),
                "debugfs could not write the per-cell guest resolver into {}",
                overlay.display()
            );
            let observed = Command::new("debugfs")
                .args(["-R", "cat /etc/resolv.conf"])
                .arg(overlay)
                .stderr(Stdio::null())
                .output()
                .await?;
            anyhow::ensure!(
                observed.status.success() && observed.stdout == RESOLVER,
                "debugfs resolver read-back did not match the required final bytes in {}",
                overlay.display()
            );
            Ok(())
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        mutation
    }
}

/// Per-cell egress networking config (host TAP + guest boot args).
struct CellNet {
    tap: String,
    mac: String,
    ip_cmdline: String,
}

struct FirecrackerProvisionGuard {
    id: CellId,
    root: PathBuf,
    procs: Arc<Mutex<HashMap<CellId, Child>>>,
    taps: Arc<Mutex<HashMap<CellId, String>>>,
    cell_rootfs_proofs: Arc<std::sync::Mutex<HashMap<CellId, RuntimeArtifactRootfsMetadata>>>,
    rootfs_boot_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    armed: bool,
}

impl FirecrackerProvisionGuard {
    fn new(
        id: CellId,
        root: PathBuf,
        procs: Arc<Mutex<HashMap<CellId, Child>>>,
        taps: Arc<Mutex<HashMap<CellId, String>>>,
        cell_rootfs_proofs: Arc<std::sync::Mutex<HashMap<CellId, RuntimeArtifactRootfsMetadata>>>,
    ) -> Self {
        Self {
            id,
            root,
            procs,
            taps,
            cell_rootfs_proofs,
            rootfs_boot_guard: None,
            armed: true,
        }
    }

    fn hold_rootfs_boot_guard(&mut self, guard: tokio::sync::OwnedMutexGuard<()>) {
        self.rootfs_boot_guard = Some(guard);
    }

    fn refuse_without_cleanup(&mut self) {
        self.rootfs_boot_guard.take();
        self.armed = false;
    }

    fn commit(&mut self) {
        self.rootfs_boot_guard.take();
        self.armed = false;
    }
}

impl Drop for FirecrackerProvisionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cell_rootfs_proofs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        let id = self.id.clone();
        let root = self.root.clone();
        let procs = self.procs.clone();
        let taps = self.taps.clone();
        let rootfs_boot_guard = self.rootfs_boot_guard.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Failed-provision cleanup owns the same per-cell serialization
                // guard, so a retry cannot recreate files while this task removes
                // the prior attempt's private rootfs and sidecar.
                let _rootfs_boot_guard = rootfs_boot_guard;
                cleanup_firecracker_process_and_tap(&id, &procs, &taps).await;
                let _ = tokio::fs::remove_dir_all(root).await;
            });
        }
    }
}

struct LinkRollback {
    name: String,
    armed: bool,
}

impl LinkRollback {
    fn new(name: String) -> Self {
        Self { name, armed: true }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for LinkRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let name = self.name.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                delete_link(&name).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                delete_link(&name).await;
            });
        }
    }
}

async fn delete_link(name: &str) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {name} 2>/dev/null"
        ))
        .kill_on_drop(true)
        .status()
        .await;
}

async fn cleanup_firecracker_process_and_tap(
    id: &CellId,
    procs: &Arc<Mutex<HashMap<CellId, Child>>>,
    taps: &Arc<Mutex<HashMap<CellId, String>>>,
) {
    let process = procs.lock().await.remove(id);
    if let Some(mut child) = process {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let tap = taps.lock().await.remove(id);
    if let Some(tap) = tap {
        delete_link(&tap).await;
    }
}

/// Guest root where the validated checkout artifact is mounted.
pub const DELIVERED_WORKDIR: &str = "/workspace";

/// Copy `src` to `dst`, preferring a copy-on-write REFLINK clone over a full
/// block-level copy when the host filesystem supports it (XFS formatted with
/// `reflink=1` — the `mkfs.xfs` default since RHEL/Rocky 8, or Btrfs). A
/// reflink clone shares the underlying extents and completes in roughly
/// constant time regardless of file size, which matters here: `base`/
/// `data_src` are multi-hundred-MB-to-multi-GB rootfs images copied on EVERY
/// cold start (a real, measured contributor to provision latency — this is
/// NOT true CoW at the Firecracker level, just a faster way to produce the
/// per-cell writable copy it needs). Falls back to a plain byte-for-byte copy
/// — IDENTICAL to the prior behavior — whenever reflink isn't available
/// (ext4, a cross-filesystem copy, or any `cp` failure), so this can only ever
/// be as fast as before, never slower or less correct. Linux-only mechanism
/// (`cp --reflink` is a GNU coreutils extension); on any other OS this is a
/// pure passthrough to `tokio::fs::copy`.
async fn reflink_or_copy(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(out) = tokio::process::Command::new("cp")
            .arg("--reflink=auto")
            .arg(src)
            .arg(dst)
            .output()
            .await
        {
            if out.status.success() {
                return Ok(());
            }
        }
    }
    tokio::fs::copy(src, dst).await.map(|_| ())
}

async fn hash_open_rootfs_sha256(
    file: &std::fs::File,
    path: &std::path::Path,
) -> anyhow::Result<(String, u64, RootfsFileStamp)> {
    let file = file.try_clone()?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let before = FirecrackerBackend::rootfs_open_stamp(&file)?;
        let mut file = file;
        file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            bytes = bytes.saturating_add(read as u64);
        }
        let after = FirecrackerBackend::rootfs_open_stamp(&file)?;
        anyhow::ensure!(
            before == after && bytes == before.len,
            "cell rootfs changed during exact-byte hashing: {}",
            path.display()
        );
        let mut digest = String::with_capacity(64);
        use std::fmt::Write as _;
        for byte in hasher.finalize() {
            let _ = write!(digest, "{byte:02x}");
        }
        Ok((digest, bytes, after))
    })
    .await
    .map_err(|error| anyhow::anyhow!("cell rootfs hashing task failed: {error}"))?
}

async fn hash_file_sha256(path: &std::path::Path) -> anyhow::Result<(String, u64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open runtime artifact image {}", path.display()))?;
    let before = file.metadata().await?;
    anyhow::ensure!(before.is_file(), "runtime artifact image is not a file");
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    let after = file.metadata().await?;
    #[cfg(unix)]
    let stable = {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mode() == after.mode()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
    };
    #[cfg(not(unix))]
    let stable = before.len() == after.len() && before.modified().ok() == after.modified().ok();
    anyhow::ensure!(
        stable && bytes == before.len(),
        "runtime artifact image changed during exact-byte hashing: {}",
        path.display()
    );
    let digest = hasher.finalize();
    let mut value = String::with_capacity(64);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(value, "{byte:02x}");
    }
    Ok((value, bytes))
}

fn remove_file_if_exists(path: &std::path::Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn publication_file_identity_from_metadata(
    path: &std::path::Path,
    metadata: &std::fs::Metadata,
) -> anyhow::Result<PublicationFileIdentity> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "runtime artifact publication path is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(PublicationFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(PublicationFileIdentity {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

fn publication_file_identity(path: &std::path::Path) -> anyhow::Result<PublicationFileIdentity> {
    let metadata = std::fs::symlink_metadata(path)?;
    publication_file_identity_from_metadata(path, &metadata)
}

fn publication_link_state(
    path: &std::path::Path,
    owner: &PublicationFileIdentity,
) -> anyhow::Result<PublicationLinkState> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PublicationLinkState::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    let current = publication_file_identity_from_metadata(path, &metadata)?;
    Ok(if &current == owner {
        PublicationLinkState::Owned
    } else {
        PublicationLinkState::Foreign
    })
}

fn remove_owned_publication_file(
    path: &std::path::Path,
    owner: &PublicationFileIdentity,
) -> anyhow::Result<bool> {
    match publication_link_state(path, owner)? {
        PublicationLinkState::Missing => Ok(false),
        PublicationLinkState::Owned => {
            std::fs::remove_file(path)?;
            sync_parent_blocking(path)?;
            Ok(true)
        }
        PublicationLinkState::Foreign => anyhow::bail!(
            "refusing to remove runtime artifact publication path owned by another writer: {}",
            path.display()
        ),
    }
}

fn same_file_identity(left: &std::path::Path, right: &std::path::Path) -> anyhow::Result<bool> {
    Ok(publication_file_identity(left)? == publication_file_identity(right)?)
}

fn sync_parent_blocking(path: &std::path::Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("runtime artifact publication has no parent directory"))?;
    let directory = std::fs::File::open(parent)?;
    directory.sync_all()?;
    Ok(())
}

#[async_trait]
impl CellBackend for FirecrackerBackend {
    fn name(&self) -> &'static str {
        "firecracker"
    }

    fn requires_runtime_artifact_authorization(&self) -> bool {
        true
    }

    async fn provision_runtime(
        &self,
        spec: &CellSpec,
        expected: Option<&RuntimeArtifactIdentity>,
    ) -> anyhow::Result<CellHandle> {
        let authorization = match expected {
            Some(identity) => {
                anyhow::ensure!(
                    spec.container.is_none() && spec.image.starts_with("dpl-"),
                    "runtime artifact authorization was presented for a non-artifact cell {} ({})",
                    spec.id,
                    hive_core::fault::NODE_IMAGE_MISSING
                );
                Some(RuntimeArtifactAuthorizationGuard::install(
                    spec.id.clone(),
                    identity.clone(),
                    self.artifact_authorizations.clone(),
                )?)
            }
            None => None,
        };
        let result = self.provision(spec).await;
        drop(authorization);
        result
    }

    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        // CONTAINER cell: run via podman on the HOST (outside the microVM). No KVM /
        // microVM boot needed — just a per-cell run dir; `start_function` does the
        // `podman run`. This is what lets a Firecracker node also run containers.
        if let Some(ctr) = &spec.container {
            let tenant = if spec.tenant.trim().is_empty() {
                "personal"
            } else {
                spec.tenant.as_str()
            };
            let run_dir = self
                .cfg
                .run_dir
                .join(crate::sanitize_tenant(tenant))
                .join(spec.id.as_str());
            let mut provision = FirecrackerProvisionGuard::new(
                spec.id.clone(),
                run_dir.clone(),
                self.procs.clone(),
                self.taps.clone(),
                self.cell_rootfs_proofs.clone(),
            );
            tokio::fs::create_dir_all(&run_dir).await?;
            tracing::debug!(cell = %spec.id, image = %ctr.image, "provisioning container cell (host podman)");
            provision.commit();
            return Ok(CellHandle {
                id: spec.id.clone(),
                image: spec.image.clone(),
                resources: spec.resources.clone(),
                root: run_dir,
                endpoint: None,
            });
        }

        // No hypervisor on this node — a NODE fault, not capacity. On this fleet
        // the usual cause is the documented PVM one: `kvm_pvm` refuses to load
        // while host PTI is active, so `/dev/kvm` silently disappears and every
        // cold start here fails identically until the node is fixed.
        anyhow::ensure!(
            self.is_supported(),
            "node cannot run microVMs: the firecracker backend is unavailable (needs Linux + \
             /dev/kvm + {}) — this node needs its hypervisor reprovisioned (on a PVM host check \
             that `pti=off` is in effect and `kvm_pvm` is loaded). Not an application fault and \
             not host capacity ({})",
            self.cfg.firecracker_bin.display(),
            hive_core::fault::NODE_BACKEND_UNAVAILABLE
        );
        // Self-manage disk before allocating multi-GB overlays: GC orphans + refuse
        // if still critically low, so we never boot into a near-full filesystem.
        self.ensure_disk_headroom().await?;

        // Per-tenant run dir (`<run_dir>/<tenant>/<cell-id>`) so each team's VM
        // sockets / overlays / console logs are isolated on the host. (The VM
        // itself is already isolated by its own kernel + per-cell vsock; this
        // partitions the host-side artifacts too.) Empty tenant => "personal".
        let tenant = if spec.tenant.trim().is_empty() {
            "personal"
        } else {
            spec.tenant.as_str()
        };
        let run_dir = self
            .cfg
            .run_dir
            .join(crate::sanitize_tenant(tenant))
            .join(spec.id.as_str());
        let mut provision = FirecrackerProvisionGuard::new(
            spec.id.clone(),
            run_dir.clone(),
            self.procs.clone(),
            self.taps.clone(),
            self.cell_rootfs_proofs.clone(),
        );
        tokio::fs::create_dir_all(&run_dir).await?;

        let api_sock = run_dir.join("api.sock");
        let vsock_uds = run_dir.join("vsock.sock");
        let log_file = run_dir.join("console.log");
        let overlay = run_dir.join("rootfs.ext4");
        let rootfs_boot_guard = Self::rootfs_boot_lock_for(&overlay)?.lock_owned().await;
        provision.hold_rootfs_boot_guard(rootfs_boot_guard);
        let duplicate_live = self.procs.lock().await.contains_key(&spec.id);
        if duplicate_live {
            // This attempt owns no process or rootfs bytes. Disarm its cleanup
            // before refusing: the ordinary failure guard would otherwise remove
            // the already-live cell that won this per-cell serialization lock.
            provision.refuse_without_cleanup();
            anyhow::bail!(
                "cell {} already has a live Firecracker process; refusing to mutate its rootfs",
                spec.id
            );
        }

        // Per-cell writable rootfs. Platform deployment artifacts (`dpl-*`) always
        // boot the one shared base whose exact bytes back NodeInfo capability; their
        // tenant code is the separately-attached data drive. Build/sandbox cells may
        // still select a dedicated rootfs and otherwise fall back to that base.
        let base = if spec.image.starts_with("dpl-") {
            self.rootfs_for(&self.cfg.base_image)
        } else {
            let per_image = self.rootfs_for(&spec.image);
            if per_image.exists() {
                per_image
            } else {
                self.rootfs_for(&self.cfg.base_image)
            }
        };
        // A missing base rootfs is a NODE PROVISIONING fault: this node cannot
        // boot ANY microVM until an operator puts the image back, and no amount
        // of retrying, scaling, or fixing the app changes that. Name the absent
        // path and the remedy, and carry `fault::NODE_IMAGE_MISSING` so the
        // gateway reports NODE_IMAGE_MISSING instead of blaming host capacity —
        // the fc-sanjose-cvm-2 outage was exactly this error read as
        // CAPACITY_EXHAUSTED on a node with 923 GB free.
        anyhow::ensure!(
            base.exists(),
            "node is missing its base rootfs image: {} does not exist (image '{}', base '{}') — \
             this node needs its base rootfs reprovisioned; no microVM can boot here until it is \
             restored. Not an application fault and not host capacity ({})",
            base.display(),
            spec.image,
            self.cfg.base_image,
            hive_core::fault::NODE_IMAGE_MISSING
        );
        // Same class, different artifact: the kernel is loaded by firecracker
        // itself, so without this check the boot fails later as an opaque
        // "firecracker API PUT /boot-source failed" and lands right back in the
        // capacity catch-all.
        anyhow::ensure!(
            self.cfg.kernel_image.exists(),
            "node is missing its guest kernel image: {} does not exist — this node needs its \
             boot artifacts reprovisioned; no microVM can boot here until it is restored. Not an \
             application fault and not host capacity ({})",
            self.cfg.kernel_image.display(),
            hive_core::fault::NODE_IMAGE_MISSING
        );

        // Platform deployment images are issued as `dpl-*`. Unlike build and
        // sandbox cells, they MUST carry the immutable data image + publication
        // sidecar and MUST boot a rootfs that proved the matching guest protocol.
        // Perform every gate before Firecracker starts, so an old guest can never
        // run tenant code and only then be rejected by the new host handshake.
        let data_src = self.data_image_for(&spec.image);
        let runtime_artifact_required = spec.image.starts_with("dpl-");
        let mut selected_rootfs_proof = None;
        let mut selected_rootfs_copy_stamp = None;
        let expected_runtime_artifact = if runtime_artifact_required {
            let expected = self
                .artifact_authorizations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&spec.id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "deployment {:?} reached provision without a caller-authorized runtime artifact identity ({})",
                        spec.image,
                        hive_core::fault::NODE_IMAGE_MISSING
                    )
                })?;
            anyhow::ensure!(
                expected.id == spec.image && expected.protocol == RUNTIME_ARTIFACT_PROTOCOL_VERSION,
                "deployment {:?} has a mismatched caller-authorized runtime artifact identity ({})",
                spec.image,
                hive_core::fault::NODE_IMAGE_MISSING
            );
            anyhow::ensure!(
                data_src.is_file(),
                "node is missing the required runtime artifact image for deployment {:?}: {} ({})",
                spec.image,
                data_src.display(),
                hive_core::fault::NODE_IMAGE_MISSING
            );
            let publication = self
                .read_data_publication(&spec.image)
                .await
                .with_context(|| {
                    format!(
                        "node has no valid runtime artifact publication for deployment {:?} ({})",
                        spec.image,
                        hive_core::fault::NODE_IMAGE_MISSING
                    )
                })?;
            anyhow::ensure!(
                publication.identity == expected,
                "deployment {:?} publication changed after caller authorization; refusing to boot it ({})",
                spec.image,
                hive_core::fault::NODE_IMAGE_MISSING
            );
            let copy_stamp = Self::rootfs_stamp(&base)?;
            let rootfs_proof = self.verified_rootfs_protocol(&base).await.with_context(|| {
                format!(
                    "node rootfs {} does not prove the complete agent protocol; rebuild this exact rootfs before scheduling deployments here ({})",
                    base.display(),
                    hive_core::fault::NODE_RUNTIME_MISSING
                )
            })?;
            anyhow::ensure!(
                Self::rootfs_stamp(&base)? == copy_stamp,
                "base rootfs changed while its complete agent proof was selected ({})",
                hive_core::fault::NODE_RUNTIME_MISSING
            );
            selected_rootfs_copy_stamp = Some(copy_stamp);
            selected_rootfs_proof = Some(rootfs_proof);
            Some(expected)
        } else {
            None
        };
        reflink_or_copy(&base, &overlay).await?;
        if let Some(copy_stamp) = selected_rootfs_copy_stamp.as_ref() {
            anyhow::ensure!(
                Self::rootfs_stamp(&base)? == *copy_stamp,
                "base rootfs changed while its proved bytes were copied into cell {} ({})",
                spec.id,
                hive_core::fault::NODE_RUNTIME_MISSING
            );
        }
        // Give the guest working DNS in its private overlay, then authenticate
        // the resulting FINAL bytes rather than the immutable base provenance.
        // A deployment H1 cannot proceed if this expected mutation did not land.
        let resolv_result = self.write_guest_resolv(&overlay).await;
        if runtime_artifact_required {
            resolv_result.with_context(|| {
                format!(
                    "cell {} could not complete its final rootfs mutation ({})",
                    spec.id,
                    hive_core::fault::NODE_RUNTIME_MISSING
                )
            })?;
        } else if let Err(error) = resolv_result {
            tracing::warn!(cell = %spec.id, error = %error, "could not write guest resolver; continuing for non-deployment cell");
        }

        let mut boot_rootfs_file = None;
        let mut boot_rootfs_stamp = None;
        let mut boot_rootfs_proof_stamp = None;
        let rootfs_drive_path = if let Some(base_proof) = selected_rootfs_proof.take() {
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let file = options
                .open(&overlay)
                .with_context(|| format!("open final private rootfs for cell {}", spec.id))?;
            let (image_sha256, image_bytes, hashed_stamp) =
                hash_open_rootfs_sha256(&file, &overlay).await?;
            anyhow::ensure!(
                Self::rootfs_stamp(&overlay)? == hashed_stamp,
                "cell {} rootfs path changed while its held inode was hashed ({})",
                spec.id,
                hive_core::fault::NODE_RUNTIME_MISSING
            );
            anyhow::ensure!(
                image_sha256 != base_proof.image_sha256,
                "cell {} final rootfs still equals its immutable base after the required resolver mutation ({})",
                spec.id,
                hive_core::fault::NODE_RUNTIME_MISSING
            );
            let final_proof = RuntimeArtifactRootfsMetadata {
                schema: base_proof.schema,
                protocol: base_proof.protocol,
                agent_wire_protocol: base_proof.agent_wire_protocol,
                agent_wire_capabilities: base_proof.agent_wire_capabilities,
                agent_sha256: base_proof.agent_sha256,
                image_sha256,
                image_bytes,
            };
            let proof_stamp = Self::write_cell_rootfs_protocol_metadata(&overlay, &final_proof)?;
            anyhow::ensure!(
                Self::read_rootfs_protocol_metadata(&overlay)? == final_proof,
                "cell {} final rootfs sidecar does not describe its exact hashed bytes",
                spec.id
            );
            let held_path = PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                file.as_raw_fd()
            ));
            boot_rootfs_stamp = Some(hashed_stamp);
            boot_rootfs_proof_stamp = Some(proof_stamp);
            boot_rootfs_file = Some(file);
            selected_rootfs_proof = Some(final_proof);
            held_path
        } else {
            overlay.clone()
        };

        // A published build artifact carries a separately-committed exact
        // identity. Copy both into this cell's private run directory so launch
        // validates the image actually attached to this VM, not whatever may
        // later appear at the shared publication path.
        // The in-guest agent mounts /dev/vdb at DELIVERED_WORKDIR (/workspace).
        let data_overlay = run_dir.join("data.ext4");
        let cell_identity_path = run_dir.join("runtime-artifact.identity.json");
        let has_data;
        {
            let _artifact_guard = Self::artifact_lock_for(&data_src)?.lock().await;
            has_data = data_src.is_file();
            if has_data {
                reflink_or_copy(&data_src, &data_overlay).await?;
                let identity = self
                    .verified_data_identity(&spec.image, &data_overlay)
                    .await?;
                if let Some(expected) = expected_runtime_artifact.as_ref() {
                    anyhow::ensure!(
                        &identity == expected,
                        "deployment {:?} attached artifact differs from the caller-authorized identity ({})",
                        spec.image,
                        hive_core::fault::NODE_IMAGE_MISSING
                    );
                }
                let identity_bytes = serde_json::to_vec(&identity)?;
                let mut identity_file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&cell_identity_path)
                    .await?;
                identity_file.write_all(&identity_bytes).await?;
                identity_file.sync_all().await?;
            }
        }

        // Spawn the Firecracker process bound to a fresh API socket, retrying
        // the spawn (not the whole provision — the rootfs copy above stays)
        // up to twice as defense-in-depth against genuine transient failures
        // (disk I/O stalls, real resource contention under heavy concurrent
        // provisioning). NOT a fix for what was actually crashing every
        // sandbox: coredump analysis (`coredumpctl info`, console.log capture)
        // showed a 100%-deterministic firecracker-side panic —
        // "Invalid instance ID: InvalidChar('_', 7)" — because firecracker's
        // own `--id` validation rejects underscores and every sandbox cell id
        // (`sbx-sbx_<hex>`) contains one (deployment cell ids use only
        // hyphens, so they never hit this). Retrying the identical invalid id
        // three times failed identically three times; the real fix is
        // `firecracker_safe_id` below, sanitizing only the CLI arg.
        const SPAWN_ATTEMPTS: u32 = 3;
        let mut last_err = None;
        let mut process = None;
        for attempt in 1..=SPAWN_ATTEMPTS {
            let _ = tokio::fs::remove_file(&api_sock).await;
            let console = std::fs::File::create(&log_file)?;
            let mut child = Command::new(&self.cfg.firecracker_bin)
                .arg("--api-sock")
                .arg(&api_sock)
                .arg("--id")
                .arg(firecracker_safe_id(spec.id.as_str()))
                .stdin(Stdio::null())
                .stdout(Stdio::from(console.try_clone()?))
                .stderr(Stdio::from(console))
                .kill_on_drop(true)
                .spawn()?;

            match wait_for_path(&api_sock, Duration::from_secs(5)).await {
                Ok(()) => {
                    last_err = None;
                    process = Some(child);
                    break;
                }
                Err(e) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    tracing::warn!(cell = %spec.id, attempt, max = SPAWN_ATTEMPTS, error = %e, "firecracker spawn did not bind its API socket — retrying");
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
        let process =
            process.ok_or_else(|| anyhow::anyhow!("firecracker spawn produced no process"))?;

        // Outbound networking: give the cell a host TAP + NAT egress so the app
        // can reach databases/APIs (Upstash, OpenAI, …). Best-effort — if it
        // fails, the VM still boots (vsock-only), so this never regresses serving.
        let net = self.setup_cell_net(&spec.id).await;
        let boot_args = match &net {
            Some(n) => format!("{} {}", self.cfg.boot_args, n.ip_cmdline),
            None => self.cfg.boot_args.clone(),
        };

        // Configure the microVM via the REST API, then boot it.
        let mem_mib = spec.resources.mem_mib;
        let vcpus = spec.resources.vcpus;
        fc_put(
            &api_sock,
            "/machine-config",
            &serde_json::json!({ "vcpu_count": vcpus, "mem_size_mib": mem_mib, "smt": false }),
        )
        .await?;
        fc_put(
            &api_sock,
            "/boot-source",
            &serde_json::json!({
                "kernel_image_path": self.cfg.kernel_image,
                "boot_args": boot_args,
            }),
        )
        .await?;
        // Attach the egress NIC (eth0 in the guest, backed by the host TAP).
        if let Some(n) = &net {
            fc_put(
                &api_sock,
                "/network-interfaces/eth0",
                &serde_json::json!({
                    "iface_id": "eth0",
                    "host_dev_name": n.tap,
                    "guest_mac": n.mac,
                }),
            )
            .await?;
        }
        fc_put(
            &api_sock,
            "/drives/rootfs",
            &serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": rootfs_drive_path,
                "is_root_device": true,
                "is_read_only": false,
            }),
        )
        .await?;
        // Second drive: the delivered build output (/dev/vdb in the guest).
        if has_data {
            fc_put(
                &api_sock,
                "/drives/data",
                &serde_json::json!({
                    "drive_id": "data",
                    "path_on_host": data_overlay,
                    "is_root_device": false,
                    "is_read_only": false,
                }),
            )
            .await?;
        }
        // vsock device: the host endpoint is `vsock_uds`; the guest agent
        // listens on CELL_AGENT_PORT and we host-initiate a CONNECT later.
        fc_put(
            &api_sock,
            "/vsock",
            &serde_json::json!({
                "guest_cid": CELL_GUEST_CID,
                "uds_path": vsock_uds,
            }),
        )
        .await?;
        // virtio-rng entropy device. Without a hardware RNG the guest's entropy
        // pool starts empty, so anything that calls getrandom() at startup (e.g.
        // Node's crypto init) BLOCKS for many seconds until the pool fills — long
        // enough that the function misses its readiness window. Feeding the guest
        // entropy makes cold starts fast and deterministic. Best-effort: older
        // Firecracker without the device simply 400s and we proceed.
        let _ = fc_put(&api_sock, "/entropy", &serde_json::json!({})).await;
        if let (Some(file), Some(image_stamp), Some(proof_stamp), Some(final_proof)) = (
            boot_rootfs_file.as_ref(),
            boot_rootfs_stamp.as_ref(),
            boot_rootfs_proof_stamp.as_ref(),
            selected_rootfs_proof.as_ref(),
        ) {
            anyhow::ensure!(
                Self::rootfs_open_stamp(file)? == *image_stamp
                    && Self::rootfs_stamp(&overlay)? == *image_stamp
                    && Self::rootfs_protocol_stamp(&overlay)? == *proof_stamp
                    && Self::read_rootfs_protocol_metadata(&overlay)? == *final_proof,
                "cell {} final rootfs inode/length or exact sidecar changed before VMM boot ({})",
                spec.id,
                hive_core::fault::NODE_RUNTIME_MISSING
            );
        }
        fc_put(
            &api_sock,
            "/actions",
            &serde_json::json!({ "action_type": "InstanceStart" }),
        )
        .await?;
        // Firecracker has opened the descriptor-pinned drive and started the
        // guest. Only now may the host release the held source inode; later guest
        // writes are ordinary writes through the VMM's already-open drive.
        drop(boot_rootfs_file);

        if let Some(rootfs_proof) = selected_rootfs_proof {
            let mut proofs = self
                .cell_rootfs_proofs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            anyhow::ensure!(
                !proofs.contains_key(&spec.id),
                "cell {} already has a selected rootfs proof",
                spec.id
            );
            proofs.insert(spec.id.clone(), rootfs_proof);
        }
        self.procs.lock().await.insert(spec.id.clone(), process);
        provision.commit();
        Ok(CellHandle {
            id: spec.id.clone(),
            image: spec.image.clone(),
            resources: spec.resources.clone(),
            root: run_dir,
            endpoint: Some(vsock_uds.to_string_lossy().into_owned()),
        })
    }

    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult> {
        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;

        // The agent may still be booting; retry the host-initiated CONNECT.
        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;

        let _ = sink.send(LogLine {
            ts_ms: now_ms(),
            stream: LogStream::System,
            line: format!("[{}] connected to cell agent; dispatching build", cell.id),
        });

        // Send the job.
        let req = serde_json::to_vec(&AgentRequest::Run(job.clone()))?;
        write_frame(&mut stream, &req).await?;

        let started_at_ms = now_ms();
        let timeout = Duration::from_secs(job.resources.timeout_secs.max(1));

        let cache_dir = self.cfg.cache_dir.clone();
        let pump = async {
            loop {
                let frame = read_frame(&mut stream).await?;
                let ev: AgentEvent = serde_json::from_slice(&frame)?;
                match ev {
                    AgentEvent::Log(line) => {
                        let _ = sink.send(line);
                    }
                    AgentEvent::Done(res) => return anyhow::Ok(res),
                    // Build cache: serve the restore tarball from host disk.
                    AgentEvent::CacheGet { key, .. } => {
                        let tar = tokio::fs::read(cache_path(&cache_dir, &key))
                            .await
                            .unwrap_or_default();
                        let _ = sink.send(LogLine {
                            ts_ms: now_ms(),
                            stream: LogStream::System,
                            line: format!(
                                "[{}] cache {} [{key}] ({} bytes)",
                                cell.id,
                                if tar.is_empty() { "miss" } else { "hit" },
                                tar.len()
                            ),
                        });
                        let reply = serde_json::to_vec(&AgentRequest::CacheData { tar })?;
                        write_frame(&mut stream, &reply).await?;
                    }
                    // Build cache: persist the tarball produced by the build.
                    AgentEvent::CachePut { key, tar } => {
                        let _ = tokio::fs::create_dir_all(&cache_dir).await;
                        let _ = tokio::fs::write(cache_path(&cache_dir, &key), &tar).await;
                    }
                    AgentEvent::ProtocolFault(fault) => anyhow::bail!(
                        "{}: guest refused the build connection with protocol fault {:?}: {}",
                        hive_core::fault::NODE_RUNTIME_MISSING,
                        fault.code,
                        fault.message
                    ),
                    AgentEvent::HandshakeReady(_) => anyhow::bail!(
                        "{}: guest sent an out-of-order handshake reply during a build",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                    // Not applicable during a build (Sandboxes-only events).
                    AgentEvent::Pong
                    | AgentEvent::RuntimeArtifactReady(_)
                    | AgentEvent::FunctionReady
                    | AgentEvent::FunctionError(_)
                    | AgentEvent::FunctionFault(_)
                    | AgentEvent::ExecOutput { .. }
                    | AgentEvent::ExecDone { .. }
                    | AgentEvent::PtyOutput { .. }
                    | AgentEvent::PtyExited { .. } => {}
                }
            }
        };

        match tokio::time::timeout(timeout, pump).await {
            Ok(res) => res,
            Err(_) => {
                let _ = sink.send(LogLine {
                    ts_ms: now_ms(),
                    stream: LogStream::System,
                    line: format!("[{}] build exceeded {timeout:?}; killing cell", cell.id),
                });
                Ok(BuildResult {
                    job_id: job.id.clone(),
                    exit_code: -1,
                    timed_out: true,
                    started_at_ms,
                    finished_at_ms: now_ms(),
                })
            }
        }
    }

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        // CONTAINER cell: run the OCI image via podman on the HOST and front it with
        // the tunnel server (same as the mock backend). The cell has no microVM /
        // vsock — it was provisioned as a lightweight host-container cell.
        if func.start_cmd.first().map(String::as_str) == Some("__container__") {
            let image = func.start_cmd.get(1).cloned().unwrap_or_default();
            let internal: u16 = func
                .start_cmd
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080);
            // Multi-service (compose) deploys carry a JSON network config in start_cmd[3].
            let net_json = func
                .start_cmd
                .get(3)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty());
            let runtime = crate::container_runtime();
            if let Some(rt) = &runtime {
                tracing::info!(cell = %cell.id, runtime = %rt, "running container under sandbox runtime");
            }
            // Primary port from `start_cmd` (TCP, drives readiness + the tunnel),
            // plus one `/udp` publish per `FunctionLaunch::udp_ports` entry —
            // the loopback datagram legs the UDP relay forwards to (host ports
            // chosen upstream by fluid-compute's `cold_start`, which records
            // them on the instance registry for the relay's resolution).
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
                Self::PODMAN_PATH,
                runtime.as_deref(),
                net_json,
                &crate::ContainerLimits::for_container(func.memory_mib, func.cpus, func.pids),
                // Non-HTTP protocol (gRPC/TCP/UDP): raw byte-splice tunnel mode.
                func.raw_proxy,
                // Serverless GPU: CDI passthrough of the host's GPUs.
                func.gpu,
            )
            .await?;
            let endpoint = launch.endpoint();
            self.containers.lock().await.insert(cell.id.clone(), launch);
            return Ok(endpoint);
        }

        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;

        // Bind this launch to the identity copied alongside the exact data drive
        // during provision. Reading the shared publication here would race a later
        // replacement and would prove the wrong bytes.
        let identity_path = cell.root.join("runtime-artifact.identity.json");
        let identity_bytes = tokio::fs::read(&identity_path).await.with_context(|| {
            format!(
                "cell {} has no attached runtime artifact identity at {} ({})",
                cell.id,
                identity_path.display(),
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        let identity =
            Self::validate_runtime_identity(&identity_bytes, &cell.image).with_context(|| {
                format!(
                    "cell {} has an invalid attached runtime artifact identity ({})",
                    cell.id,
                    hive_core::fault::NODE_IMAGE_MISSING
                )
            })?;
        let authorized = func.runtime_artifact.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "cell {} launch omitted the caller-authorized runtime artifact identity ({})",
                cell.id,
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            authorized == &identity,
            "cell {} attached runtime artifact identity changed after launch authorization ({})",
            cell.id,
            hive_core::fault::NODE_IMAGE_MISSING
        );

        // The caller derives this guest cwd from the same RuntimeArtifactSpec
        // delivered for the image. Never collapse a workspace app back to the
        // checkout root here.
        let func = func.clone();
        anyhow::ensure!(
            func.workdir
                .as_deref()
                .map(|workdir| workdir == DELIVERED_WORKDIR || workdir.starts_with("/workspace/"))
                .unwrap_or(false),
            "firecracker function is missing its validated runtime artifact workdir"
        );

        // Use the exact proof selected while this cell's immutable base bytes
        // were stable and copied. Re-reading the canonical base here would bind a
        // post-provision rootfs replacement to an already-running older guest.
        let rootfs_proof = self
            .cell_rootfs_proofs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&cell.id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cell {} has no proof for the exact rootfs bytes it booted ({})",
                    cell.id,
                    hive_core::fault::NODE_RUNTIME_MISSING
                )
            })?;

        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;
        perform_agent_handshake(&mut stream, &rootfs_proof).await?;
        let req = serde_json::to_vec(&AgentRequest::StartFunction(func.clone()))?;
        write_frame(&mut stream, &req).await?;

        // H1: the authenticated boot transcript must already match the exact
        // rootfs/agent proof. H2: the guest must now echo the exact mounted
        // runtime-artifact identity before FunctionReady can be accepted.
        let mut artifact_ready = false;
        loop {
            let frame = read_frame(&mut stream).await?;
            validate_agent_versioned_launch_event_frame(&frame).map_err(|error| {
                anyhow::anyhow!(
                    "{}: guest emitted a non-exact post-handshake event: {error}",
                    hive_core::fault::NODE_RUNTIME_MISSING
                )
            })?;
            let event: AgentEvent = serde_json::from_slice(&frame).map_err(|error| {
                anyhow::anyhow!(
                    "{}: guest emitted malformed post-handshake event: {error}",
                    hive_core::fault::NODE_RUNTIME_MISSING
                )
            })?;
            match event {
                AgentEvent::RuntimeArtifactReady(observed) => {
                    anyhow::ensure!(
                        !artifact_ready && observed == identity,
                        "guest runtime artifact identity does not match the attached image ({})",
                        hive_core::fault::NODE_IMAGE_MISSING
                    );
                    artifact_ready = true;
                }
                AgentEvent::FunctionReady => {
                    anyhow::ensure!(
                        artifact_ready,
                        "guest agent did not perform runtime artifact protocol v{} proof; rebuild the cell rootfs ({})",
                        RUNTIME_ARTIFACT_PROTOCOL_VERSION,
                        hive_core::fault::NODE_RUNTIME_MISSING
                    );
                    break;
                }
                AgentEvent::ProtocolFault(fault) => anyhow::bail!(
                    "{}: guest protocol fault {:?}: {}",
                    hive_core::fault::NODE_RUNTIME_MISSING,
                    fault.code,
                    fault.message
                ),
                AgentEvent::HandshakeReady(_) => anyhow::bail!(
                    "{}: guest sent a duplicate or out-of-order handshake reply",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
                AgentEvent::FunctionFault(fault) => match fault.code {
                    AgentFunctionFaultCode::NodeImageMissing => anyhow::bail!(
                        "{}: guest refused function start: {}",
                        hive_core::fault::NODE_IMAGE_MISSING,
                        fault.message
                    ),
                    AgentFunctionFaultCode::NodeRuntimeMissing => anyhow::bail!(
                        "{}: guest refused function start: {}",
                        hive_core::fault::NODE_RUNTIME_MISSING,
                        fault.message
                    ),
                },
                AgentEvent::FunctionError(error) => {
                    // The guest is authenticated, but this value is still the
                    // tenant process's stderr. Preserve it as diagnostics without
                    // letting marker-shaped tenant text enter downstream fault
                    // classifiers, which intentionally inspect returned errors.
                    tracing::warn!(
                        cell = %cell.id,
                        image = %cell.image,
                        tenant_start_diagnostic = %error,
                        "tenant process failed to start inside its authenticated cell"
                    );
                    anyhow::bail!(
                        "the deployment's own process failed to start inside its cell; check this \
                         deployment's logs, entrypoint and required env; the cell itself booted \
                         successfully ({})",
                        hive_core::fault::DEPLOYMENT_START_FAILED
                    )
                }
                unexpected => anyhow::bail!(
                    "{}: guest emitted out-of-order launch event {unexpected:?}",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            }
        }
        Ok(CellEndpoint::Vsock {
            uds: uds.clone(),
            port: CELL_FUNCTION_PORT,
        })
    }

    fn delivered_workdir(
        &self,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<Option<String>> {
        artifact.guest_workdir(DELIVERED_WORKDIR).map(Some)
    }

    async fn deliver_build(
        &self,
        image: &str,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.cfg.rootfs_dir).await?;
        let staged = crate::runtime_artifact::stage_sealed_runtime_artifact(
            artifact,
            &self.cfg.rootfs_dir.join(".artifact-staging"),
        )
        .await?;
        let identity = artifact.identity(image)?;
        anyhow::ensure!(
            staged.content_sha256() == identity.content_sha256,
            "verified runtime package materialized a different semantic identity"
        );
        staged.write_identity(&identity)?;

        let out = self.data_image_for(image);
        let sidecar = self.data_identity_for(image);
        let backup = Self::publication_backup_for(&out);
        let _artifact_guard = Self::artifact_lock_for(&out)?.lock().await;

        // A complete publication is immutable. Reusing the exact same identity is
        // idempotent; a platform-issued image id resolving to different content is
        // a hard identity collision and must never overwrite the prior bytes.
        if out.is_file() && sidecar.is_file() {
            let publication = Self::read_publication_at(&sidecar, image).await?;
            Self::verify_publication_image(&publication, &out).await?;
            anyhow::ensure!(
                publication.identity == identity,
                "runtime artifact image id {image:?} is already bound to different immutable content"
            );
            let keep = std::collections::HashSet::from([out.clone(), sidecar.clone()]);
            if let Err(error) =
                Self::cleanup_stale_publication_files(&self.cfg.rootfs_dir, &out, &sidecar, &keep)
            {
                tracing::warn!(
                    image,
                    error = %error,
                    "runtime artifact stale publication cleanup refused or failed"
                );
            }
            return Ok(());
        }

        // Recover only namespace states whose predecessor is unambiguous. Old
        // fixed-name `.publishing` files are never reused: a killed process may
        // still hold their inode, so every new attempt gets unique names and the
        // old paths are left to the guarded stale sweep after a complete pair
        // exists again.
        if !out.exists() && sidecar.exists() {
            anyhow::ensure!(
                Self::canonical_links_publication_temporary(
                    &self.cfg.rootfs_dir,
                    &sidecar
                )?,
                "runtime artifact has an incomplete sidecar not owned by a recoverable publication for image {image:?}"
            );
            remove_file_if_exists(&sidecar)?;
            sync_parent_blocking(&sidecar)?;
        }
        if !out.exists() && backup.exists() {
            let metadata = std::fs::symlink_metadata(&backup)?;
            anyhow::ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "runtime artifact legacy backup is not a regular file for image {image:?}"
            );
            std::fs::rename(&backup, &out)?;
            sync_parent_blocking(&out)?;
        } else if out.is_file() && !sidecar.exists() && backup.exists() {
            if same_file_identity(&out, &backup)? {
                remove_file_if_exists(&backup)?;
                sync_parent_blocking(&out)?;
            } else {
                anyhow::ensure!(
                    Self::canonical_links_publication_temporary(
                        &self.cfg.rootfs_dir,
                        &out
                    )?,
                    "runtime artifact has ambiguous legacy and partial publications for image {image:?}"
                );
                remove_file_if_exists(&out)?;
                sync_parent_blocking(&out)?;
                std::fs::rename(&backup, &out)?;
                sync_parent_blocking(&out)?;
            }
        } else if out.is_file()
            && !sidecar.exists()
            && !backup.exists()
            && Self::canonical_links_publication_temporary(&self.cfg.rootfs_dir, &out)?
        {
            remove_file_if_exists(&out)?;
            sync_parent_blocking(&out)?;
        }
        anyhow::ensure!(
            !backup.exists(),
            "runtime artifact has ambiguous legacy and partial publications for image {image:?}"
        );

        let legacy = out.is_file() && !sidecar.exists();
        anyhow::ensure!(
            legacy || (!out.exists() && !sidecar.exists()),
            "runtime artifact publication is incomplete or corrupt for image {image:?}"
        );

        let mut allocated = None;
        for _ in 0..32 {
            let token = Self::publication_temp_token();
            let tmp = Self::publication_temp_for(&out, &token);
            let tmp_sidecar = Self::publication_temp_for(&sidecar, &token);
            let image_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let publication_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&tmp_sidecar)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    remove_file_if_exists(&tmp)?;
                    sync_parent_blocking(&tmp)?;
                    continue;
                }
                Err(error) => {
                    remove_file_if_exists(&tmp)?;
                    sync_parent_blocking(&tmp)?;
                    return Err(error.into());
                }
            };
            let guard = match PublicationTempGuard::new(
                tmp.clone(),
                tmp_sidecar.clone(),
                out.clone(),
                sidecar.clone(),
                backup.clone(),
            ) {
                Ok(guard) => guard,
                Err(error) => {
                    remove_file_if_exists(&tmp)?;
                    remove_file_if_exists(&tmp_sidecar)?;
                    sync_parent_blocking(&tmp)?;
                    return Err(error);
                }
            };
            sync_parent_blocking(&tmp)?;
            allocated = Some((tmp, image_file, publication_file, guard));
            break;
        }
        let (tmp, image_file, publication_file, mut temp_guard) = allocated.ok_or_else(|| {
            anyhow::anyhow!("could not allocate a unique runtime artifact publication temporary")
        })?;

        // Selection, symlink resolution and all size/count limits completed before
        // this private image is created. The stage reports bytes from its held
        // descriptor, so sizing never reopens a repository-controlled pathname.
        let source_mib = staged.materialized_size_mib();
        let image_mib = source_mib
            .saturating_mul(3)
            .saturating_div(2)
            .saturating_add(512);
        let image_bytes = image_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow::anyhow!("runtime artifact image size overflow"))?;
        let image_file = tokio::fs::File::from_std(image_file);
        image_file.set_len(image_bytes).await?;
        image_file.sync_all().await?;
        drop(image_file);
        let mut mkfs = Command::new("mkfs.ext4");
        let staged_path = staged.inherited_path(&mut mkfs)?;
        let status = mkfs
            .args(["-F", "-q", "-d"])
            .arg(staged_path)
            .arg(&tmp)
            .env_clear()
            .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await?;
        anyhow::ensure!(
            status.success(),
            "mkfs.ext4 -d failed packing validated runtime artifact for image '{image}'"
        );
        let mut permissions = tokio::fs::metadata(&tmp).await?.permissions();
        permissions.set_readonly(true);
        tokio::fs::set_permissions(&tmp, permissions).await?;
        let image_file = tokio::fs::File::open(&tmp).await?;
        image_file.sync_all().await?;
        let (image_sha256, image_bytes) = hash_file_sha256(&tmp).await?;
        let publication = RuntimeArtifactPublication {
            schema: RUNTIME_ARTIFACT_PUBLICATION_SCHEMA,
            identity: identity.clone(),
            image_sha256,
            image_bytes,
        };
        let publication_bytes = serde_json::to_vec(&publication)?;
        anyhow::ensure!(
            publication_bytes.len() <= 4096,
            "runtime artifact publication descriptor exceeds 4096 bytes"
        );
        let mut publication_file = tokio::fs::File::from_std(publication_file);
        publication_file.write_all(&publication_bytes).await?;
        publication_file.sync_all().await?;
        drop(publication_file);

        Self::verify_publication_image(&publication, &tmp).await?;
        if legacy {
            // Preserve the prior inode under a durable name before removing the
            // canonical commit marker. There is no await between removal and the
            // new pair's durable links, so request cancellation can only run the
            // guard before or after this whole namespace transition.
            std::fs::hard_link(&out, &backup)
                .context("preserve legacy runtime artifact before publication")?;
            sync_parent_blocking(&backup)?;
            temp_guard.begin_publication(true)?;
            std::fs::remove_file(&out)?;
            sync_parent_blocking(&out)?;
        } else {
            temp_guard.begin_publication(false)?;
        }

        // Link the descriptor first and the image last. `out` is the commit marker:
        // before it exists provision attaches nothing; once it exists both names
        // are durable and hash-bound. These no-replace hard links and parent fsyncs
        // are synchronous so a dropped future cannot interrupt the critical pair.
        std::fs::hard_link(&temp_guard.temporary_identity, &sidecar)
            .context("publish runtime artifact identity")?;
        sync_parent_blocking(&sidecar)?;
        std::fs::hard_link(&tmp, &out).context("publish runtime artifact image")?;
        sync_parent_blocking(&out)?;
        Self::verify_publication_image(&publication, &out).await?;

        temp_guard.mark_publication_committed()?;
        temp_guard.commit()?;
        let keep = std::collections::HashSet::from([out.clone(), sidecar.clone()]);
        if let Err(error) =
            Self::cleanup_stale_publication_files(&self.cfg.rootfs_dir, &out, &sidecar, &keep)
        {
            tracing::warn!(
                image,
                error = %error,
                "runtime artifact stale publication cleanup refused or failed"
            );
        }
        Ok(())
    }

    async fn runtime_artifact_identity(
        &self,
        image: &str,
    ) -> anyhow::Result<Option<RuntimeArtifactIdentity>> {
        let data = self.data_image_for(image);
        anyhow::ensure!(
            data.is_file(),
            "node is missing the committed runtime artifact image for {image:?}: {} ({})",
            data.display(),
            hive_core::fault::NODE_IMAGE_MISSING
        );
        let publication = self.read_data_publication(image).await.with_context(|| {
            format!(
                "node has no valid committed runtime artifact identity for {image:?} ({})",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        Ok(Some(publication.identity))
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        self.cell_rootfs_proofs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&cell.id);
        let id = cell.id.clone();
        let root = cell.root.clone();
        let containers = self.containers.clone();
        let procs = self.procs.clone();
        let taps = self.taps.clone();
        let cleanup = tokio::spawn(async move {
            let container = containers.lock().await.remove(&id);
            if let Some(container) = container {
                container.terminate().await;
            }
            cleanup_firecracker_process_and_tap(&id, &procs, &taps).await;
            let _ = tokio::fs::remove_dir_all(root).await;
        });
        cleanup
            .await
            .map_err(|e| anyhow::anyhow!("firecracker cleanup task failed: {e}"))
    }

    async fn cpu_percent(&self, cell: &CellHandle) -> Option<f32> {
        // Sample the per-cell Firecracker VMM host process; its CPU tracks the
        // guest's vCPU work. Container cells (no microVM) have no VMM proc → None.
        let pid = {
            let procs = self.procs.lock().await;
            procs.get(&cell.id).and_then(|c| c.id())?
        };
        self.sampler.cpu_percent(pid, cell.resources.vcpus)
    }
}

/// Firecracker-specific microVM exec support (Sandboxes) — NOT part of the
/// generic [`CellBackend`] trait since it's meaningless for the mock/container
/// backends: a sandbox needs a long-lived cell that accepts MANY commands over
/// its lifetime (unlike `run_build`'s one-shot, self-destructing cell). Each
/// call opens its OWN fresh vsock connection to the already-booted agent (the
/// same `CONNECT`-handshake + length-prefixed-JSON transport `run_build` uses),
/// so multiple execs — and a kill targeting an earlier one — can be in flight
/// concurrently.
impl FirecrackerBackend {
    /// Start one argv command inside `cell` and stream its `AgentEvent`s back
    /// (`ExecOutput`* then one `ExecDone`) on an unbounded channel. Returns as
    /// soon as the connection is established — the caller drains the channel
    /// (blocking: read until `ExecDone`; detached: spawn a task that drains it).
    pub async fn exec_command(
        &self,
        cell: &CellHandle,
        req: hive_core::ExecRequest,
    ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<AgentEvent>> {
        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;
        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;
        let payload = serde_json::to_vec(&AgentRequest::Exec(req))?;
        write_frame(&mut stream, &payload).await?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let frame = match read_frame(&mut stream).await {
                    Ok(f) => f,
                    Err(_) => break, // connection closed — caller treats as done/killed
                };
                let ev: AgentEvent = match serde_json::from_slice(&frame) {
                    Ok(e) => e,
                    Err(_) => break,
                };
                let is_done = matches!(ev, AgentEvent::ExecDone { .. });
                if tx.send(ev).is_err() {
                    break; // receiver dropped (e.g. command was killed and caller stopped listening)
                }
                if is_done {
                    break;
                }
            }
        });
        Ok(rx)
    }

    /// Open one interactive pty session inside `cell` and return an
    /// `AgentEvent` receiver (`PtyOutput`* then one `PtyExited`) plus a
    /// [`PtySender`] the caller uses to push typed bytes / resize events back
    /// on the SAME connection — unlike `exec_command`, this is a duplex
    /// session, not a one-shot request/stream.
    pub async fn exec_pty(
        &self,
        cell: &CellHandle,
        req: hive_core::ExecPtyRequest,
    ) -> anyhow::Result<(tokio::sync::mpsc::UnboundedReceiver<AgentEvent>, PtySender)> {
        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;
        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;
        let payload = serde_json::to_vec(&AgentRequest::ExecPty(req))?;
        write_frame(&mut stream, &payload).await?;

        let (mut read_half, mut write_half) = stream.into_split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let frame = match read_frame(&mut read_half).await {
                    Ok(f) => f,
                    Err(_) => break, // connection closed — caller treats as exited
                };
                let ev: AgentEvent = match serde_json::from_slice(&frame) {
                    Ok(e) => e,
                    Err(_) => break,
                };
                let is_exited = matches!(ev, AgentEvent::PtyExited { .. });
                if tx.send(ev).is_err() {
                    break; // receiver dropped (caller stopped listening)
                }
                if is_exited {
                    break;
                }
            }
        });

        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<AgentRequest>();
        tokio::spawn(async move {
            while let Some(req) = in_rx.recv().await {
                let Ok(payload) = serde_json::to_vec(&req) else {
                    continue;
                };
                if write_frame(&mut write_half, &payload).await.is_err() {
                    break; // connection closed — further sends are silently dropped
                }
            }
        });

        Ok((rx, PtySender(in_tx)))
    }

    /// Signal a still-running exec (by the id it was started with) to stop.
    /// Opens a FRESH connection (the exec's own connection is busy streaming
    /// output) — the guest agent's process-global exec registry finds the
    /// child by id and sends it `SIGKILL` regardless of which connection asks.
    pub async fn kill_exec(&self, cell: &CellHandle, exec_id: &str) -> anyhow::Result<()> {
        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;
        let mut stream = connect_agent(uds, Duration::from_secs(10)).await?;
        let payload = serde_json::to_vec(&AgentRequest::KillExec {
            id: exec_id.to_string(),
        })?;
        write_frame(&mut stream, &payload).await?;
        // Wait for the ack (Pong) so the caller knows the signal was actually
        // delivered to the guest, not just queued on the wire.
        let _ = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await;
        Ok(())
    }
}

/// The write half of an [`FirecrackerBackend::exec_pty`] session — cheap to
/// clone, backed by an unbounded channel feeding the connection's dedicated
/// writer task, so a caller (the websocket handler) can hold one per browser
/// tab without juggling a raw stream.
#[derive(Clone)]
pub struct PtySender(tokio::sync::mpsc::UnboundedSender<AgentRequest>);

impl PtySender {
    pub fn input(&self, bytes: Vec<u8>) {
        let _ = self.0.send(AgentRequest::PtyInput { bytes });
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.0.send(AgentRequest::PtyResize { cols, rows });
    }
}

// ---- Firecracker REST API over its Unix socket ------------------------------

/// Minimal HTTP/1.1 `PUT` to the Firecracker API socket. Firecracker speaks
/// plain HTTP over a Unix domain socket; we avoid a full HTTP client dep and
/// write the request by hand, expecting a 2xx (usually 204).
async fn fc_put(api_sock: &PathBuf, path: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(body)?;
    let mut stream = UnixStream::connect(api_sock).await?;
    let req = format!(
        "PUT {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    // Firecracker's micro-http server keeps the connection open (it ignores our
    // `Connection: close`), so we must NOT read to EOF — parse headers, then
    // read exactly Content-Length bytes of body.
    let resp = read_http_response(&mut stream).await?;
    let head = String::from_utf8_lossy(&resp);
    let status_line = head.lines().next().unwrap_or("<no status>");
    let status_ok = status_line.contains(" 200")
        || status_line.contains(" 204")
        || status_line.contains(" 201");
    anyhow::ensure!(
        status_ok,
        "firecracker API PUT {path} failed: {status_line} | body: {}",
        head.split("\r\n\r\n").nth(1).unwrap_or("")
    );
    Ok(())
}

/// Read one HTTP response (status line + headers + Content-Length body) without
/// waiting for the connection to close.
async fn read_http_response(stream: &mut UnixStream) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        // Find end of headers.
        if let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buf[..hdr_end]).to_ascii_lowercase();
            let content_len = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = hdr_end + 4;
            if buf.len() >= body_start + content_len {
                return Ok(buf);
            }
        }
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .map_err(|_| anyhow::anyhow!("timed out reading firecracker API response"))??;
        anyhow::ensure!(n > 0, "firecracker API closed connection mid-response");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Host path for a build-cache key's tarball (key sanitized for the filesystem).
fn cache_path(cache_dir: &PathBuf, key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cache_dir.join(format!("{safe}.tar.gz"))
}

async fn wait_for_path(path: &PathBuf, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        // 5ms (was 20ms): a `stat()` on a not-yet-created socket path is a few
        // microseconds, so tightening this only adds a handful of extra syscalls
        // per cold start — but it shaves up to 15ms of pure polling-granularity
        // tail latency off EVERY cold start waiting on this socket to appear.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    anyhow::bail!("timed out waiting for {}", path.display())
}

/// Firecracker's own `--id` validation accepts only `[a-zA-Z0-9-]`, up to 64
/// chars, and rejects anything else by panicking (SIGABRT) straight out of
/// its own `main()` instead of a clean parse error — cell ids elsewhere in
/// this file (run-dir names, hashmap keys, vsock lookups) are untouched by
/// this and keep using the real `spec.id` (which may contain `_`, as every
/// sandbox cell id does).
fn firecracker_safe_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}

// ---- vsock host-initiated connection + framing ------------------------------

/// Host-initiated vsock connect: connect to the Firecracker vsock UDS, send the
/// `CONNECT <port>` handshake, and wait for `OK <peer_port>`. Retries until the
/// in-guest agent is listening (i.e. the cell has finished booting).
fn handshake_transcript_sha256(nonce: &str, proof: &AgentBootProof) -> String {
    let digest = Sha256::digest(agent_handshake_transcript(nonce, proof));
    let mut value = String::with_capacity(64);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

async fn fresh_agent_handshake_nonce() -> anyhow::Result<String> {
    let mut entropy = [0_u8; AGENT_HANDSHAKE_NONCE_BYTES];
    let mut random = tokio::fs::File::open("/dev/urandom")
        .await
        .context("open OS entropy for agent handshake")?;
    random
        .read_exact(&mut entropy)
        .await
        .context("read OS entropy for agent handshake")?;
    let mut nonce = String::with_capacity(AGENT_HANDSHAKE_NONCE_BYTES * 2);
    use std::fmt::Write as _;
    for byte in entropy {
        let _ = write!(nonce, "{byte:02x}");
    }
    Ok(nonce)
}

async fn perform_agent_handshake(
    stream: &mut UnixStream,
    rootfs: &RuntimeArtifactRootfsMetadata,
) -> anyhow::Result<()> {
    let nonce = fresh_agent_handshake_nonce().await.map_err(|error| {
        anyhow::anyhow!(
            "{}: cannot create fresh agent handshake challenge: {error}",
            hive_core::fault::NODE_RUNTIME_MISSING
        )
    })?;
    let expected = rootfs.agent_boot_proof();
    anyhow::ensure!(
        expected.agent_wire_protocol == AGENT_WIRE_PROTOCOL_VERSION
            && expected.agent_wire_capabilities == AGENT_WIRE_CAPABILITIES
            && expected.runtime_artifact_protocol == RUNTIME_ARTIFACT_PROTOCOL_VERSION,
        "{}: host attempted a handshake with an incompatible rootfs proof",
        hive_core::fault::NODE_RUNTIME_MISSING
    );
    let request = serde_json::to_vec(&AgentRequest::Handshake(AgentHandshake {
        nonce: nonce.clone(),
        expected_boot: expected.clone(),
    }))?;
    write_frame(stream, &request).await.map_err(|error| {
        anyhow::anyhow!(
            "{}: could not send the agent handshake: {error}",
            hive_core::fault::NODE_RUNTIME_MISSING
        )
    })?;
    let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(stream))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{}: guest did not answer the agent handshake within 5 seconds",
                hive_core::fault::NODE_RUNTIME_MISSING
            )
        })?
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: guest closed or malformed the agent handshake: {error}",
                hive_core::fault::NODE_RUNTIME_MISSING
            )
        })?;
    validate_agent_handshake_response_frame(&frame).map_err(|error| {
        anyhow::anyhow!(
            "{}: guest returned a non-exact agent-handshake frame: {error}",
            hive_core::fault::NODE_RUNTIME_MISSING
        )
    })?;
    let event: AgentEvent = serde_json::from_slice(&frame).map_err(|error| {
        anyhow::anyhow!(
            "{}: guest returned invalid agent-handshake JSON: {error}",
            hive_core::fault::NODE_RUNTIME_MISSING
        )
    })?;
    match event {
        AgentEvent::HandshakeReady(AgentHandshakeReady {
            nonce: observed_nonce,
            proof,
            transcript_sha256,
        }) => {
            let transcript = handshake_transcript_sha256(&nonce, &expected);
            anyhow::ensure!(
                observed_nonce == nonce
                    && proof == expected
                    && FirecrackerBackend::lowercase_sha256(&transcript_sha256)
                    && transcript_sha256 == transcript,
                "{}: guest handshake did not match the fresh exact rootfs/agent proof",
                hive_core::fault::NODE_RUNTIME_MISSING
            );
            Ok(())
        }
        AgentEvent::ProtocolFault(fault) => anyhow::bail!(
            "{}: guest refused the agent handshake with {:?}: {}",
            hive_core::fault::NODE_RUNTIME_MISSING,
            fault.code,
            fault.message
        ),
        unexpected => anyhow::bail!(
            "{}: guest returned out-of-order handshake event {unexpected:?}",
            hive_core::fault::NODE_RUNTIME_MISSING
        ),
    }
}

async fn connect_agent(uds: &str, timeout: Duration) -> anyhow::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err = String::from("unknown");
    while tokio::time::Instant::now() < deadline {
        match try_connect_once(uds).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = e.to_string();
                // 25ms (was 100ms): a failed vsock connect attempt (guest agent
                // not listening yet) is a cheap syscall that returns almost
                // instantly, so a tighter retry interval costs negligible extra
                // CPU — but it directly shrinks the worst-case tail latency this
                // polling loop adds on TOP of the microVM's real boot time, on
                // every single cold start.
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    // An unreachable in-guest agent is a NODE fault, not an app fault and not
    // saturation: the microVM booted but its PID1 never answered on the vsock
    // socket, which means the guest ROOTFS is wrong (no /sbin/hive-cell-agent,
    // or one that cannot run) — reprovision the image. Unmarked, this landed in
    // `classify_lease_error`'s catch-all and published CAPACITY_EXHAUSTED on the
    // fleet's dominant backend, sending operators to look for space on a node
    // that had plenty.
    anyhow::bail!(
        "{}: could not reach cell agent on {uds}: {last_err}",
        hive_core::fault::NODE_IMAGE_MISSING
    )
}

async fn try_connect_once(uds: &str) -> anyhow::Result<UnixStream> {
    let mut stream = UnixStream::connect(uds).await?;
    stream
        .write_all(format!("CONNECT {CELL_AGENT_PORT}\n").as_bytes())
        .await?;
    stream.flush().await?;

    // Read the "OK <port>\n" line one byte at a time (it's short).
    let mut reader = BufReader::new(&mut stream);
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        anyhow::ensure!(n == 1, "vsock handshake closed early");
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        anyhow::ensure!(line.len() < 64, "vsock handshake line too long");
    }
    let line = String::from_utf8_lossy(&line);
    anyhow::ensure!(line.starts_with("OK"), "vsock handshake rejected: {line}");
    Ok(stream)
}

/// Length-prefixed framing: 4-byte big-endian length, then JSON payload.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> anyhow::Result<()> {
    let len = (payload.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(n <= 64 * 1024 * 1024, "frame too large: {n}");
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real (no mocking) proof that `reflink_or_copy` produces byte-identical
    /// output to a plain copy, whether or not the host filesystem actually
    /// supports reflink — `/tmp` on most CI/dev Linux hosts is tmpfs (no
    /// reflink support), which exercises the fallback path for free; on a host
    /// where it IS supported, the `cp --reflink=auto` branch runs instead. Both
    /// must produce the same content, so this test is meaningful either way.
    #[tokio::test]
    async fn reflink_or_copy_produces_identical_content() {
        let dir = std::env::temp_dir().join(format!("hive-reflink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let payload = vec![0xABu8; 4096];
        std::fs::write(&src, &payload).unwrap();
        let dst = dir.join("dst.bin");
        reflink_or_copy(&src, &dst)
            .await
            .expect("copy must succeed");
        let got = std::fs::read(&dst).unwrap();
        assert_eq!(got, payload, "copied content must be byte-identical");

        // A second copy overwriting an existing destination must also succeed
        // (the cold-start path always copies into a freshly created run_dir, but
        // this guards against any future reuse assumption).
        std::fs::write(&src, vec![0xCDu8; 128]).unwrap();
        reflink_or_copy(&src, &dst)
            .await
            .expect("overwrite copy must succeed");
        assert_eq!(std::fs::read(&dst).unwrap(), vec![0xCDu8; 128]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reflink_or_copy_fails_cleanly_when_source_is_missing() {
        let dir =
            std::env::temp_dir().join(format!("hive-reflink-missing-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let result = reflink_or_copy(&dir.join("does-not-exist.bin"), &dir.join("dst.bin")).await;
        assert!(
            result.is_err(),
            "copying a missing source must return an error, never silently succeed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real, live smoke test proving the Sandboxes exec path works end-to-end
    /// against a REAL Firecracker microVM: provision a cell from the
    /// `sandbox-node22` rootfs, exec `node --version` over the NEW
    /// `AgentRequest::Exec`/`ExecOutput`/`ExecDone` guest-agent protocol, and
    /// assert a real exit code + real stdout come back — then terminate.
    /// Requires a genuine Firecracker-capable host (Linux + /dev/kvm, the
    /// `firecracker` binary, `/var/lib/hive/vmlinux`, and a
    /// `sandbox-node22.ext4` rootfs with the freshly-built agent baked in —
    /// exactly what this session provisioned on fc-virginia-3). Never runs in
    /// normal CI; opt in with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a real Firecracker host with a sandbox-node22 rootfs (see doc comment)"]
    async fn live_sandbox_exec_on_real_firecracker() {
        let mut cfg = FirecrackerConfig::default();
        // Matches this fleet's PVM boot-arg requirements (HIVE_FC_BOOT_ARGS on
        // the live node) — `nokaslr` + i8042 probe disables are needed on PVM.
        cfg.boot_args = "console=ttyS0 reboot=k panic=1 pci=off nokaslr i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd root=/dev/vda rw init=/sbin/hive-cell-agent".to_string();

        let backend = FirecrackerBackend::new(cfg);
        assert!(
            backend.is_supported(),
            "this host must have /dev/kvm + the firecracker binary"
        );

        let spec = CellSpec {
            id: CellId::from("sbx-livetest-1".to_string()),
            image: "sandbox-node22".to_string(),
            resources: hive_core::ResourceSpec {
                vcpus: 1,
                mem_mib: 512,
                disk_mib: 2048,
                timeout_secs: 60,
            },
            tenant: "personal".to_string(),
            container: None,
        };
        let handle = backend
            .provision(&spec)
            .await
            .expect("provision must succeed on a real FC host");
        assert!(
            handle.endpoint.is_some(),
            "a real microVM cell must have a vsock endpoint"
        );

        let req = hive_core::ExecRequest {
            id: "cmd-livetest-1".to_string(),
            cmd: "node".to_string(),
            args: vec!["--version".to_string()],
            cwd: String::new(),
            env: Default::default(),
            sudo: false,
            shell: false,
        };
        let mut rx = backend
            .exec_command(&handle, req)
            .await
            .expect("exec_command must start");

        let mut stdout = String::new();
        let mut exit_code: Option<Option<i32>> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(AgentEvent::ExecOutput { line, .. })) => {
                    stdout.push_str(&line);
                    stdout.push('\n');
                }
                Ok(Some(AgentEvent::ExecDone {
                    exit_code: code, ..
                })) => {
                    exit_code = Some(code);
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        backend.terminate(&handle).await.ok();

        assert_eq!(
            exit_code,
            Some(Some(0)),
            "node --version must exit 0 inside the real microVM; got stdout: {stdout:?}"
        );
        assert!(
            stdout.trim().starts_with('v'),
            "expected a real node version string (e.g. v22.x.x), got: {stdout:?}"
        );
    }
}
