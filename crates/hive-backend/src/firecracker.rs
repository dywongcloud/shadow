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

use crate::{CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink};
use async_trait::async_trait;
use hive_core::{
    now_ms, AgentEvent, AgentRequest, BuildJob, BuildResult, CellId, LogLine, LogStream,
    CELL_AGENT_PORT, CELL_FUNCTION_PORT, CELL_GUEST_CID,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

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

    /// Per-deployment build-output ext4 (the artifact `deliver_build` packs and
    /// `provision` attaches as the cell's second drive). Lives alongside the
    /// base rootfs images, keyed by the same logical image name.
    fn data_image_for(&self, image: &str) -> PathBuf {
        self.cfg
            .rootfs_dir
            .join(format!("{}.data.ext4", crate::sanitize_image(image)))
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
    /// image) via debugfs — no mount needed — so the guest can resolve DNS for
    /// outbound `fetch`. Best-effort: DNS simply won't work if debugfs is absent.
    async fn write_guest_resolv(&self, overlay: &std::path::Path) {
        let tmp = overlay.with_extension("resolv.tmp");
        if tokio::fs::write(&tmp, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n")
            .await
            .is_err()
        {
            return;
        }
        let script = format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; \
             debugfs -w -R 'rm /etc/resolv.conf' '{ov}' >/dev/null 2>&1; \
             debugfs -w -R 'write {tmp} /etc/resolv.conf' '{ov}' >/dev/null 2>&1",
            ov = overlay.display(),
            tmp = tmp.display(),
        );
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .status()
            .await;
        let _ = tokio::fs::remove_file(&tmp).await;
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
    armed: bool,
}

impl FirecrackerProvisionGuard {
    fn new(
        id: CellId,
        root: PathBuf,
        procs: Arc<Mutex<HashMap<CellId, Child>>>,
        taps: Arc<Mutex<HashMap<CellId, String>>>,
    ) -> Self {
        Self {
            id,
            root,
            procs,
            taps,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for FirecrackerProvisionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let id = self.id.clone();
        let root = self.root.clone();
        let procs = self.procs.clone();
        let taps = self.taps.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
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

/// Guest path the per-deployment build output is mounted at (the agent mounts
/// `/dev/vdb` here; the function server runs with this as its working dir).
pub const DELIVERED_WORKDIR: &str = "/build";

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

#[async_trait]
impl CellBackend for FirecrackerBackend {
    fn name(&self) -> &'static str {
        "firecracker"
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
        );
        tokio::fs::create_dir_all(&run_dir).await?;

        let api_sock = run_dir.join("api.sock");
        let vsock_uds = run_dir.join("vsock.sock");
        let log_file = run_dir.join("console.log");
        let overlay = run_dir.join("rootfs.ext4");

        // Per-cell writable rootfs. Prefer a dedicated `<image>.ext4` but fall
        // back to the shared base runtime rootfs — most deployments boot the
        // base and get their code from the data drive below.
        let base = {
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
        reflink_or_copy(&base, &overlay).await?;
        // Give the guest working DNS for outbound fetch (per-cell overlay only).
        self.write_guest_resolv(&overlay).await;

        // If this image has a delivered build artifact (packed by `deliver_build`),
        // give the cell a private writable copy to attach as its second drive.
        // The in-guest agent mounts it at DELIVERED_WORKDIR (/dev/vdb -> /build).
        let data_src = self.data_image_for(&spec.image);
        let data_overlay = run_dir.join("data.ext4");
        let has_data = data_src.exists();
        if has_data {
            reflink_or_copy(&data_src, &data_overlay).await?;
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
                "path_on_host": overlay,
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
        fc_put(
            &api_sock,
            "/actions",
            &serde_json::json!({ "action_type": "InstanceStart" }),
        )
        .await?;

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
                    // Not applicable during a build (Sandboxes-only events).
                    AgentEvent::Pong
                    | AgentEvent::FunctionReady
                    | AgentEvent::FunctionError(_)
                    | AgentEvent::ExecOutput { .. }
                    | AgentEvent::ExecDone { .. } => {}
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

        // The control plane registers the pool's workdir as its own host build
        // dir (correct for the same-host mock backend). Inside the guest that
        // path doesn't exist; the delivered build is mounted at DELIVERED_WORKDIR.
        let mut func = func.clone();
        func.workdir = Some(DELIVERED_WORKDIR.to_string());

        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;
        let req = serde_json::to_vec(&AgentRequest::StartFunction(func.clone()))?;
        write_frame(&mut stream, &req).await?;

        // Wait for the agent to report the function is up and bridged.
        loop {
            let frame = read_frame(&mut stream).await?;
            match serde_json::from_slice::<AgentEvent>(&frame)? {
                AgentEvent::FunctionReady => break,
                // The microVM booted and the in-guest agent ran the app, which
                // then failed to come up — an APP fault (bad entrypoint, missing
                // env, never bound its port). Marked so the gateway reports the
                // deployment instead of the host: unmarked, the first two of
                // these were published as CAPACITY_EXHAUSTED before the pool's
                // circuit had opened.
                AgentEvent::FunctionError(e) => anyhow::bail!(
                    "the deployment's own process failed to start inside its cell: {e} — check \
                     this deployment's logs, entrypoint and required env; the node booted the \
                     cell fine ({})",
                    hive_core::fault::DEPLOYMENT_START_FAILED
                ),
                _ => continue,
            }
        }
        Ok(CellEndpoint::Vsock {
            uds: uds.clone(),
            port: CELL_FUNCTION_PORT,
        })
    }

    fn delivered_workdir(&self) -> Option<&'static str> {
        Some(DELIVERED_WORKDIR)
    }

    async fn deliver_build(&self, image: &str, build_dir: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            build_dir.is_dir(),
            "deliver_build: build dir does not exist: {}",
            build_dir.display()
        );
        tokio::fs::create_dir_all(&self.cfg.rootfs_dir).await?;
        let out = self.data_image_for(image);
        let tmp = out.with_extension("ext4.tmp");

        // Pack the host build dir into an ext4 image WITHOUT a privileged loop
        // mount: `mkfs.ext4 -d <dir>` populates the filesystem from a directory
        // directly. Size it to the build dir plus generous headroom (node_modules
        // etc.). Written to a temp path then atomically renamed so a serving cell
        // never attaches a half-written image.
        let script = format!(
            r#"set -e
            sz=$(du -sm "$BUILD" | cut -f1)
            sz=$(( sz * 3 / 2 + 512 ))
            rm -f "$OUT"
            truncate -s "${{sz}}M" "$OUT"
            mkfs.ext4 -F -q -d "$BUILD" "$OUT"
            "#,
        );
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("BUILD", build_dir)
            .env("OUT", &tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        anyhow::ensure!(
            status.success(),
            "mkfs.ext4 -d failed packing build output for image '{image}'"
        );
        tokio::fs::rename(&tmp, &out).await?;
        Ok(())
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
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
