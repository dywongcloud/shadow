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
}

impl FirecrackerBackend {
    pub fn new(cfg: FirecrackerConfig) -> Self {
        let procs = Arc::new(Mutex::new(HashMap::new()));
        // Self-management GC: periodically reap orphaned per-cell run dirs — the
        // multi-GB microVM overlays left behind when a cell's firecracker process
        // is gone (e.g. after a node restart). Without this, dead overlays
        // accumulate and exhaust host disk (the SJ outage). Only dirs with no live
        // process AND untouched for a grace period are removed, so an in-flight
        // provision (dir created before the process registers) is never reaped.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let procs_gc = procs.clone();
            let run_dir = cfg.run_dir.clone();
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                    Self::gc_orphans(&run_dir, &procs_gc).await;
                }
            });
        }
        FirecrackerBackend {
            cfg,
            procs,
            taps: Arc::new(Mutex::new(HashMap::new())),
            net_idx: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            nat_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Remove run dirs for cells that are NOT live (no entry in `procs`) and whose
    /// dir hasn't been modified in the last 5 minutes (so an in-flight provision is
    /// safe). Frees orphaned overlay disk. Best-effort.
    async fn gc_orphans(run_dir: &std::path::Path, procs: &Arc<Mutex<HashMap<CellId, Child>>>) {
        let live: std::collections::HashSet<String> =
            procs.lock().await.keys().map(|c| c.to_string()).collect();
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
        // Floor: a microVM overlay pair (rootfs + data) plus slack. ~3 GiB.
        const FLOOR_BYTES: u64 = 3 * 1024 * 1024 * 1024;
        let base = if self.cfg.run_dir.exists() { self.cfg.run_dir.as_path() } else { std::path::Path::new("/") };
        if Self::disk_free_bytes(base) >= FLOOR_BYTES {
            return Ok(());
        }
        tracing::warn!("disk below floor before provision — running orphan GC");
        Self::gc_orphans(&self.cfg.run_dir, &self.procs).await;
        let free = Self::disk_free_bytes(base);
        anyhow::ensure!(
            free >= FLOOR_BYTES,
            "no capacity: host disk critically low ({} MiB free, need {} MiB) after GC",
            free / (1024 * 1024),
            FLOOR_BYTES / (1024 * 1024)
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
        self.cfg.rootfs_dir.join(format!("{}.ext4", sanitize_image(image)))
    }

    /// Per-deployment build-output ext4 (the artifact `deliver_build` packs and
    /// `provision` attaches as the cell's second drive). Lives alongside the
    /// base rootfs images, keyed by the same logical image name.
    fn data_image_for(&self, image: &str) -> PathBuf {
        self.cfg.rootfs_dir.join(format!("{}.data.ext4", sanitize_image(image)))
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
        let script = r#"
            export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH
            sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1
            if command -v iptables >/dev/null 2>&1; then
              iptables -t nat -C POSTROUTING -s 172.16.0.0/16 -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s 172.16.0.0/16 -j MASQUERADE
              iptables -C FORWARD -s 172.16.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -s 172.16.0.0/16 -j ACCEPT
              iptables -C FORWARD -d 172.16.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -d 172.16.0.0/16 -j ACCEPT
            elif command -v nft >/dev/null 2>&1; then
              nft add table ip hive_nat 2>/dev/null
              nft 'add chain ip hive_nat post { type nat hook postrouting priority 100 ; }' 2>/dev/null
              nft flush chain ip hive_nat post 2>/dev/null
              nft add rule ip hive_nat post ip saddr 172.16.0.0/16 masquerade 2>/dev/null
              nft 'add chain ip hive_nat fwd { type filter hook forward priority 0 ; }' 2>/dev/null
              nft flush chain ip hive_nat fwd 2>/dev/null
              nft add rule ip hive_nat fwd ip saddr 172.16.0.0/16 accept 2>/dev/null
              nft add rule ip hive_nat fwd ip daddr 172.16.0.0/16 accept 2>/dev/null
            fi"#;
        let _ = Command::new("/bin/sh").arg("-c").arg(script).output().await;
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
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        self.taps.lock().await.insert(id.clone(), tap.clone());
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
        if tokio::fs::write(&tmp, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n").await.is_err() {
            return;
        }
        let script = format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; \
             debugfs -w -R 'rm /etc/resolv.conf' '{ov}' >/dev/null 2>&1; \
             debugfs -w -R 'write {tmp} /etc/resolv.conf' '{ov}' >/dev/null 2>&1",
            ov = overlay.display(),
            tmp = tmp.display(),
        );
        let _ = Command::new("/bin/sh").arg("-c").arg(&script).status().await;
        let _ = tokio::fs::remove_file(&tmp).await;
    }

    /// Tear down a cell's TAP (best-effort).
    async fn teardown_cell_net(&self, id: &CellId) {
        if let Some(tap) = self.taps.lock().await.remove(id) {
            let _ = Command::new("/bin/sh").arg("-c").arg(format!("export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {tap} 2>/dev/null")).status().await;
        }
    }
}

/// Per-cell egress networking config (host TAP + guest boot args).
struct CellNet {
    tap: String,
    mac: String,
    ip_cmdline: String,
}

/// Guest path the per-deployment build output is mounted at (the agent mounts
/// `/dev/vdb` here; the function server runs with this as its working dir).
pub const DELIVERED_WORKDIR: &str = "/build";

fn sanitize_image(image: &str) -> String {
    image
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect()
}

#[async_trait]
impl CellBackend for FirecrackerBackend {
    fn name(&self) -> &'static str {
        "firecracker"
    }

    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        anyhow::ensure!(
            self.is_supported(),
            "firecracker backend unavailable: need Linux + /dev/kvm + {} (run inside the Lima VM)",
            self.cfg.firecracker_bin.display()
        );
        // Self-manage disk before allocating multi-GB overlays: GC orphans + refuse
        // if still critically low, so we never boot into a near-full filesystem.
        self.ensure_disk_headroom().await?;

        // Per-tenant run dir (`<run_dir>/<tenant>/<cell-id>`) so each team's VM
        // sockets / overlays / console logs are isolated on the host. (The VM
        // itself is already isolated by its own kernel + per-cell vsock; this
        // partitions the host-side artifacts too.) Empty tenant => "personal".
        let tenant = if spec.tenant.trim().is_empty() { "personal" } else { spec.tenant.as_str() };
        let run_dir = self.cfg.run_dir.join(crate::sanitize_tenant(tenant)).join(spec.id.as_str());
        tokio::fs::create_dir_all(&run_dir).await?;

        let api_sock = run_dir.join("api.sock");
        let vsock_uds = run_dir.join("vsock.sock");
        let log_file = run_dir.join("console.log");
        let overlay = run_dir.join("rootfs.ext4");

        // Per-cell writable rootfs. Real Hive uses CoW; a copy is the simple,
        // correct equivalent for a study implementation. Prefer a dedicated
        // `<image>.ext4` but fall back to the shared base runtime rootfs — most
        // deployments boot the base and get their code from the data drive below.
        let base = {
            let per_image = self.rootfs_for(&spec.image);
            if per_image.exists() { per_image } else { self.rootfs_for(&self.cfg.base_image) }
        };
        anyhow::ensure!(
            base.exists(),
            "rootfs missing for image '{}' and base '{}': {}",
            spec.image,
            self.cfg.base_image,
            base.display()
        );
        tokio::fs::copy(&base, &overlay).await?;
        // Give the guest working DNS for outbound fetch (per-cell overlay only).
        self.write_guest_resolv(&overlay).await;

        // If this image has a delivered build artifact (packed by `deliver_build`),
        // give the cell a private writable copy to attach as its second drive.
        // The in-guest agent mounts it at DELIVERED_WORKDIR (/dev/vdb -> /build).
        let data_src = self.data_image_for(&spec.image);
        let data_overlay = run_dir.join("data.ext4");
        let has_data = data_src.exists();
        if has_data {
            tokio::fs::copy(&data_src, &data_overlay).await?;
        }

        // Spawn the Firecracker process bound to a fresh API socket.
        let _ = tokio::fs::remove_file(&api_sock).await;
        let console = std::fs::File::create(&log_file)?;
        let child = Command::new(&self.cfg.firecracker_bin)
            .arg("--api-sock")
            .arg(&api_sock)
            .arg("--id")
            .arg(spec.id.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::from(console.try_clone()?))
            .stderr(Stdio::from(console))
            .kill_on_drop(true)
            .spawn()?;
        self.procs.lock().await.insert(spec.id.clone(), child);

        // Wait for the API socket to appear.
        wait_for_path(&api_sock, Duration::from_secs(5)).await?;

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
                    AgentEvent::Pong
                    | AgentEvent::FunctionReady
                    | AgentEvent::FunctionError(_) => {}
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
                AgentEvent::FunctionError(e) => anyhow::bail!("function failed to start: {e}"),
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
        anyhow::ensure!(status.success(), "mkfs.ext4 -d failed packing build output for image '{image}'");
        tokio::fs::rename(&tmp, &out).await?;
        Ok(())
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        if let Some(mut child) = self.procs.lock().await.remove(&cell.id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.teardown_cell_net(&cell.id).await;
        let _ = tokio::fs::remove_dir_all(&cell.root).await;
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
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    cache_dir.join(format!("{safe}.tar.gz"))
}

async fn wait_for_path(path: &PathBuf, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("timed out waiting for {}", path.display())
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
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    anyhow::bail!("could not reach cell agent on {uds}: {last_err}")
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
pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, payload: &[u8]) -> anyhow::Result<()> {
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
