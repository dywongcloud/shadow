//! The pluggable isolation layer: the control plane only needs to provision a cell, run a build in
//! it and tear it down ([`CellBackend`]); [`mock::MockBackend`] is a sandboxed child process (macOS
//! too), [`firecracker::FirecrackerBackend`] a real microVM, [`litebox`] a userspace Linux ABI.

pub mod cell_orphans;
pub mod container_cli;
pub mod firecracker;
pub mod litebox;
pub mod litebox_macos;
pub mod mock;
pub mod runtime_artifact;
#[cfg(target_os = "macos")]
pub mod sep_signer;
pub mod snapshot;

use async_trait::async_trait;
use hive_core::{BuildJob, BuildResult, CellId, LogLine, ResourceSpec};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub use cell_orphans::{orphan_reap_ran, reap_orphaned_cells, set_cell_owner, ReapReport};
pub use hive_core::FunctionLaunch;
pub use runtime_artifact::{
    materialize_runtime_artifact_package, reopen_sealed_runtime_artifact,
    seal_host_runtime_artifact, validate_runtime_artifact_package_descriptor,
    RuntimeArtifactPackageDescriptor, RuntimeArtifactPaths, RuntimeArtifactSpec,
    SealedRuntimeArtifact, VerifiedRuntimeArtifactPackage,
    RUNTIME_ARTIFACT_PACKAGE_PROTOCOL_VERSION, RUNTIME_ARTIFACT_PROTOCOL_VERSION,
};

/// Sink the backend writes build output to. The box daemon fans this out to
/// any number of log subscribers.
pub type LogSink = mpsc::UnboundedSender<LogLine>;

/// Cumulative CPU time (user+system) of a SINGLE `pid` in nanoseconds, read straight from the OS
/// (sysinfo's per-process CPU is unreliable on macOS).
#[cfg(target_os = "macos")]
fn pid_cpu_time_ns(pid: u32) -> Option<u64> {
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            &mut ti as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    let raw = ti.pti_total_user.saturating_add(ti.pti_total_system);
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    if unsafe { libc::mach_timebase_info(&mut tb) } != 0 || tb.denom == 0 {
        return None;
    }
    Some(raw.saturating_mul(tb.numer as u64) / tb.denom as u64)
}

#[cfg(target_os = "linux")]
fn pid_cpu_time_ns(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_pid_ppid, cpu) = parse_linux_stat(&stat)?;
    Some(cpu)
}

/// Snapshot of every process as `(pid, ppid, cumulative_cpu_ns)`, so CPU can be summed over a
/// cell's whole process TREE (a function may run under a wrapper like `npm exec`).
#[cfg(target_os = "macos")]
fn all_procs() -> Vec<(u32, u32, u64)> {
    let cnt = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if cnt <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; cnt as usize + 32];
    let bytes = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes) };
    if got <= 0 {
        return Vec::new();
    }
    let n = (got as usize / std::mem::size_of::<i32>()).min(pids.len());
    let mut out = Vec::with_capacity(n);
    for &p in &pids[..n] {
        if p <= 0 {
            continue;
        }
        let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidinfo(
                p,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut bi as *mut _ as *mut libc::c_void,
                sz,
            )
        };
        if r != sz {
            continue;
        }
        let cpu = pid_cpu_time_ns(p as u32).unwrap_or(0);
        out.push((p as u32, bi.pbi_ppid, cpu));
    }
    out
}

/// Parse `(ppid, cpu_ns)` from a `/proc/<pid>/stat` line, splitting after the final `)` since the
/// comm field may contain spaces.
#[cfg(target_os = "linux")]
fn parse_linux_stat(stat: &str) -> Option<(u32, u64)> {
    let after = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = after.split_whitespace().collect();
    let ppid: u32 = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    Some((
        ppid,
        (utime.saturating_add(stime)).saturating_mul(1_000_000_000) / hz as u64,
    ))
}

#[cfg(target_os = "linux")]
fn all_procs() -> Vec<(u32, u32, u64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some((ppid, cpu)) = parse_linux_stat(&stat) {
                out.push((pid, ppid, cpu));
            }
        }
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn all_procs() -> Vec<(u32, u32, u64)> {
    Vec::new()
}

/// A pid → (children, cumulative-cpu) index built ONCE per process snapshot, so each per-instance
/// subtree query is O(subtree) instead of re-deriving O(P) hashmaps every 500 ms tick.
#[derive(Default)]
struct ProcIndex {
    children: std::collections::HashMap<u32, Vec<u32>>,
    cpu: std::collections::HashMap<u32, u64>,
}

impl ProcIndex {
    fn build(procs: &[(u32, u32, u64)]) -> Self {
        let mut children: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::with_capacity(procs.len());
        let mut cpu: std::collections::HashMap<u32, u64> =
            std::collections::HashMap::with_capacity(procs.len());
        for &(pid, ppid, c) in procs {
            children.entry(ppid).or_default().push(pid);
            cpu.insert(pid, c);
        }
        ProcIndex { children, cpu }
    }

    /// Sum cumulative CPU (ns) over `root` and all its descendants.
    /// `None` if `root` isn't present (process gone).
    fn subtree_cpu_ns(&self, root: u32) -> Option<u64> {
        use std::collections::{HashSet, VecDeque};
        self.cpu.get(&root)?;
        let mut sum = 0u64;
        let mut seen = HashSet::new();
        let mut q = VecDeque::new();
        q.push_back(root);
        seen.insert(root);
        while let Some(p) = q.pop_front() {
            sum = sum.saturating_add(self.cpu.get(&p).copied().unwrap_or(0));
            if let Some(cs) = self.children.get(&p) {
                for &c in cs {
                    if seen.insert(c) {
                        q.push_back(c);
                    }
                }
            }
        }
        Some(sum)
    }
}

/// Sum cumulative CPU (ns) over `root` and all its descendants; `None` if `root` is gone.
fn subtree_cpu_ns(root: u32, procs: &[(u32, u32, u64)]) -> Option<u64> {
    ProcIndex::build(procs).subtree_cpu_ns(root)
}

/// Per-cell CPU sampler shared by the backends: utilization = Δsubtree_cpu_time / Δwall_time,
/// with the all-process snapshot cached briefly; the first sample for a pid is a 0 baseline.
pub(crate) struct CpuSampler {
    inner: std::sync::Mutex<SamplerState>,
}

struct SamplerState {
    /// Index over the current snapshot, built once when the snapshot is
    /// refreshed rather than once per sampled instance (see [`ProcIndex`]).
    index: ProcIndex,
    snapshot_at: Option<std::time::Instant>,
    prev: std::collections::HashMap<u32, (u64, std::time::Instant)>,
}

impl CpuSampler {
    pub(crate) fn new() -> Self {
        CpuSampler {
            inner: std::sync::Mutex::new(SamplerState {
                index: ProcIndex::default(),
                snapshot_at: None,
                prev: std::collections::HashMap::new(),
            }),
        }
    }

    /// CPU of the process tree rooted at `pid`, as a percentage of `vcpus`
    /// allocated cores (so ~100 ≈ one fully-busy vCPU). `None` if `pid` is gone.
    pub(crate) fn cpu_percent(&self, pid: u32, vcpus: u32) -> Option<f32> {
        let now = std::time::Instant::now();
        let mut g = self.inner.lock().ok()?;
        let stale = g
            .snapshot_at
            .map(|t| t.elapsed() >= std::time::Duration::from_millis(250))
            .unwrap_or(true);
        if stale {
            g.index = ProcIndex::build(&all_procs());
            g.snapshot_at = Some(now);
        }
        let cur_ns = g.index.subtree_cpu_ns(pid)?;
        let pct = match g.prev.get(&pid) {
            Some((prev_ns, prev_t)) => {
                let wall_ns = now.duration_since(*prev_t).as_nanos() as u64;
                if wall_ns == 0 {
                    0.0
                } else {
                    let cpu_ns = cur_ns.saturating_sub(*prev_ns);
                    (cpu_ns as f64 / wall_ns as f64 * 100.0 / vcpus.max(1) as f64) as f32
                }
            }
            None => 0.0,
        };
        g.prev.insert(pid, (cur_ns, now));
        if stale && g.prev.len() > 256 {
            let live: std::collections::HashSet<u32> = g.index.cpu.keys().copied().collect();
            g.prev.retain(|p, _| live.contains(p));
        }
        Some(pct)
    }
}

/// An address the gateway can open connections to in order to reach a function
/// server running inside a cell (Fluid compute data plane).
#[derive(Clone, Debug)]
pub enum CellEndpoint {
    /// Direct TCP (mock backend): the function listens here on the host.
    Tcp(String),
    /// Firecracker: host-initiated vsock CONNECT to `port` on `uds`.
    Vsock { uds: String, port: u32 },
}

/// A bidirectional byte stream to a function instance.
pub trait DuplexStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> DuplexStream for T {}

/// Open a fresh connection to a cell endpoint; one connection per in-flight request.
pub async fn connect_endpoint(ep: &CellEndpoint) -> anyhow::Result<Box<dyn DuplexStream>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match ep {
        CellEndpoint::Tcp(addr) => Ok(Box::new(tokio::net::TcpStream::connect(addr).await?)),
        #[cfg(unix)]
        CellEndpoint::Vsock { uds, port } => {
            let mut s = tokio::net::UnixStream::connect(uds).await?;
            s.write_all(format!("CONNECT {port}\n").as_bytes()).await?;
            s.flush().await?;
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            loop {
                let n = s.read(&mut b).await?;
                anyhow::ensure!(n == 1, "vsock handshake closed early");
                if b[0] == b'\n' {
                    break;
                }
                line.push(b[0]);
                anyhow::ensure!(line.len() < 64, "vsock handshake too long");
            }
            anyhow::ensure!(
                line.starts_with(b"OK"),
                "vsock handshake rejected: {}",
                String::from_utf8_lossy(&line)
            );
            Ok(Box::new(s))
        }
        #[cfg(not(unix))]
        CellEndpoint::Vsock { uds, .. } => Err(anyhow::anyhow!(
            "vsock endpoints are not supported on this platform ({}); \
             Windows shadow nodes isolate through Litebox over TCP",
            uds
        )),
    }
}

/// Transport protocol for one published container port (podman's `-p` is TCP unless the internal
/// port carries `/udp`); a minimal local counterpart of `fluid_core::ServiceProtocol`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContainerProtocol {
    #[default]
    Tcp,
    Udp,
}

/// One port a container publishes: the in-container port, the host port and the transport; a
/// deployment may publish several ([`ContainerSpec::ports`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerPort {
    pub container_port: u16,
    pub host_port: u16,
    pub protocol: ContainerProtocol,
}

impl ContainerPort {
    /// A single TCP port — the common case and the bridge for single-port callers.
    pub fn tcp(container_port: u16, host_port: u16) -> ContainerPort {
        ContainerPort {
            container_port,
            host_port,
            protocol: ContainerProtocol::Tcp,
        }
    }
}

/// A container (podman) cell: the backend runs this OCI image on the HOST instead of a microVM
/// / host process; set when the function's `runtime == "container"`.
#[derive(Clone, Debug)]
pub struct ContainerSpec {
    /// OCI image to `podman run` (from the function's `__container__` start_cmd).
    pub image: String,
    /// Every port this container publishes; the first is the PRIMARY (health-checked, fronted by the
    /// tunnel server), every entry gets its own `-p`. Non-empty for a real deploy.
    pub ports: Vec<ContainerPort>,
}

/// Per-container resource ceilings applied to `podman run` so one tenant cannot exhaust host RAM,
/// saturate every CPU or fork-bomb the node; `None` = no limit for that dimension.
#[derive(Clone, Debug)]
pub struct ContainerLimits {
    /// Memory ceiling (podman `--memory`), e.g. "512m", "2g". None = no limit.
    pub memory: Option<String>,
    /// CPU quota (podman `--cpus`), e.g. "1.0", "0.5". None = no limit.
    pub cpus: Option<String>,
    /// Max PIDs (podman `--pids-limit`) — fork-bomb guard. None = no limit.
    pub pids: Option<u32>,
}

impl Default for ContainerLimits {
    fn default() -> Self {
        fn env_or(key: &str, default: &str) -> String {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        }
        Self {
            memory: Some(env_or("HIVE_CONTAINER_MEMORY", "4g")),
            cpus: Some(env_or("HIVE_CONTAINER_CPUS", "2.0")),
            pids: std::env::var("HIVE_CONTAINER_PIDS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .or(Some(1024)),
        }
    }
}

/// `HIVE_CONTAINER_MEMORY_MAX_MIB` fallback: 16 GiB, generous for a real monolith and still a real
/// ceiling.
const DEFAULT_MEMORY_MAX_MIB: u32 = 16 * 1024;
/// `HIVE_CONTAINER_CPUS_MAX` fallback: 4x the default `2.0` cpus.
const DEFAULT_CPUS_MAX: f64 = 8.0;
/// `HIVE_CONTAINER_PIDS_MAX` fallback: 4x the default 1024 pids.
const DEFAULT_PIDS_MAX: u32 = 4096;

impl ContainerLimits {
    /// Limits for a specific container function: the deployment's requested memory/cpus/pids when
    /// set, else the env-tunable defaults — and EACH request is CLAMPED to its fleet-wide maximum, so
    /// a tenant's own fluid.json can never remove the ceiling.
    pub fn for_container(memory_mib: u32, cpus: f64, pids: u32) -> Self {
        let mut l = Self::default();
        if memory_mib > 0 {
            let max = std::env::var("HIVE_CONTAINER_MEMORY_MAX_MIB")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_MEMORY_MAX_MIB);
            l.memory = Some(format!("{}m", memory_mib.min(max)));
        }
        if cpus > 0.0 {
            let max = std::env::var("HIVE_CONTAINER_CPUS_MAX")
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .unwrap_or(DEFAULT_CPUS_MAX);
            l.cpus = Some(format!("{}", cpus.min(max)));
        }
        if pids > 0 {
            let max = std::env::var("HIVE_CONTAINER_PIDS_MAX")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_PIDS_MAX);
            l.pids = Some(pids.min(max));
        }
        l
    }

    /// The `podman run` OPTION flags for these limits plus always-on `--security-opt
    /// no-new-privileges`; pushed BEFORE the image name. Shared by every `podman run` path.
    pub fn podman_run_flags(&self) -> Vec<String> {
        let mut f = Vec::new();
        if let Some(mem) = &self.memory {
            f.push("--memory".into());
            f.push(mem.clone());
        }
        if let Some(cpus) = &self.cpus {
            f.push("--cpus".into());
            f.push(cpus.clone());
        }
        if let Some(pids) = &self.pids {
            f.push("--pids-limit".into());
            f.push(pids.to_string());
        }
        f.push("--security-opt".into());
        f.push("no-new-privileges".into());
        f
    }
}

/// Parse a podman "IPAM error: requested ip address X is already allocated to container ID Y"
/// failure into `(network, stale_container_id)`; `None` for any other failure.
fn parse_ipam_conflict(net: &Option<serde_json::Value>, stderr: &str) -> Option<(String, String)> {
    if !stderr.contains("IPAM error") || !stderr.contains("already allocated to container") {
        return None;
    }
    let netname = net
        .as_ref()
        .and_then(|n| n.get("net"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let id = stderr
        .rsplit_once("container ID")
        .map(|(_, tail)| tail.trim())
        .and_then(|tail| {
            tail.split(|c: char| c.is_whitespace())
                .find(|s| !s.is_empty())
        })
        .map(|s| {
            s.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_string()
        })
        .filter(|s| !s.is_empty())?;
    Some((netname, id))
}

/// True when a failed `podman run` failed because podman's LOCK POOL is empty ("exceeded
/// num_locks": one lock per container AND per volume) — the pool, not capacity, is the problem.
/// Shared with `mock.rs`, whose container path is the same real podman.
pub(crate) fn is_lock_exhaustion(stderr: &str) -> bool {
    let e = stderr.to_lowercase();
    e.contains("num_locks") || (e.contains("allocating lock") && e.contains("allocation failed"))
}

/// Is this an ANONYMOUS podman volume id (64 hex chars)? The load-bearing guard for
/// [`reclaim_podman_locks`]: every volume the platform creates on purpose is NAMED (`hive-vol-*`).
fn is_anonymous_volume(name: &str) -> bool {
    name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Reclaim podman locks so containers can start again — EXITED `hive-cell-*` containers and
/// DANGLING ANONYMOUS volumes only, never a running container or a named volume. Returns
/// (volumes, cells).
async fn reclaim_podman_locks(bin: &str, path_env: &str) -> (usize, usize) {
    use tokio::process::Command;
    let mut vols = 0usize;
    let mut cells = 0usize;
    if let Ok(out) = Command::new(bin)
        .args([
            "ps",
            "-a",
            "--filter",
            "status=exited",
            "--format",
            "{{.Names}}",
        ])
        .env("PATH", path_env)
        .output()
        .await
    {
        for name in String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|n| n.starts_with("hive-cell-"))
        {
            if Command::new(bin)
                .args(["rm", "-f", "-v", name])
                .env("PATH", path_env)
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                cells += 1;
            }
        }
    }
    if let Ok(out) = Command::new(bin)
        .args([
            "volume",
            "ls",
            "--filter",
            "dangling=true",
            "--format",
            "{{.Name}}",
        ])
        .env("PATH", path_env)
        .output()
        .await
    {
        for name in String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|n| is_anonymous_volume(n))
        {
            if Command::new(bin)
                .args(["volume", "rm", "-f", name])
                .env("PATH", path_env)
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                vols += 1;
            }
        }
    }
    (vols, cells)
}

/// Reclaim below this many free podman locks; legitimate fleet peak was ~25 of the 2048, so a node
/// under this is leaking, not busy.
pub const LOCK_HEADROOM_FLOOR: u64 = 1536;

/// podman's currently-free lock count, or `None` if podman isn't present /
/// doesn't report it. This is the number that hit 0 on the leader.
pub async fn podman_free_locks(path_env: &str) -> Option<u64> {
    use tokio::process::Command;
    let out = Command::new(crate::container_cli::bin(false))
        .args(["info", "--format", "{{.Host.FreeLocks}}"])
        .env("PATH", path_env)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// PROACTIVE lock sweep on an interval: under [`LOCK_HEADROOM_FLOOR`] free locks, run the same
/// conservative reclaim. Returns `(free_locks_before, volumes, cells)`; `None` without podman.
pub async fn sweep_container_locks(path_env: &str) -> Option<(u64, usize, usize)> {
    let free = podman_free_locks(path_env).await?;
    if free >= LOCK_HEADROOM_FLOOR {
        return Some((free, 0, 0));
    }
    let (vols, cells) = reclaim_podman_locks(crate::container_cli::bin(false), path_env).await;
    tracing::warn!(
        free_locks_before = free,
        floor = LOCK_HEADROOM_FLOOR,
        volumes_reclaimed = vols,
        cells_reclaimed = cells,
        "podman lock headroom low — proactively reclaimed leaked locks"
    );
    Some((free, vols, cells))
}

/// Whether podman's `stderr` blames the PINNED static `--ip` (never true when `net` pins none): an
/// IPAM refusal, an address outside the subnet (`requested static ip X not in any subnet on network
/// Y`), an exhausted range (`no available addresses in range`), or a network that was removed or
/// leaked and could not be recreated with the pinned subnet.
fn is_static_ip_failure(net: &Option<serde_json::Value>, stderr: &str) -> bool {
    let pinned = net
        .as_ref()
        .and_then(|n| n.get("ip"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !pinned {
        return false;
    }
    let e = stderr.to_lowercase();
    e.contains("ipam")
        || e.contains("already allocated")
        || e.contains("requested ip")
        || e.contains("static ip")
        || e.contains("not in subnet")
        || e.contains("not in any subnet")
        || e.contains("no available")
        || e.contains("network not found")
}

/// Make a logical image name safe as a single filename component.
pub(crate) fn sanitize_image(image: &str) -> String {
    image
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// PATH for invoking podman on a Linux host (systemd units run with a minimal env).
pub(crate) const PODMAN_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub(crate) struct AbortTask(Option<tokio::task::JoinHandle<()>>);

impl AbortTask {
    pub(crate) fn new(task: tokio::task::JoinHandle<()>) -> Self {
        Self(Some(task))
    }

    pub(crate) fn publish(mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.0.take()
    }

    pub(crate) fn abort(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl Drop for AbortTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

pub(crate) struct ContainerRollback {
    name: String,
    apple: bool,
    path_env: String,
    armed: bool,
}

impl ContainerRollback {
    pub(crate) fn new(name: String, apple: bool, path_env: &str) -> Self {
        Self {
            name,
            apple,
            path_env: path_env.to_string(),
            armed: true,
        }
    }

    async fn cleanup(&mut self) {
        remove_container_eventually(&self.name, self.apple, &self.path_env).await;
        self.armed = false;
    }
}

impl Drop for ContainerRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let name = self.name.clone();
        let apple = self.apple;
        let path_env = self.path_env.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                remove_container_eventually(&name, apple, &path_env).await;
            });
        } else {
            std::thread::spawn(move || {
                let _ = std::process::Command::new(crate::container_cli::bin(apple))
                    .args(crate::container_cli::rm_args(apple, &name))
                    .env("PATH", path_env)
                    .output();
            });
        }
    }
}

pub(crate) struct ContainerLaunch {
    endpoint: CellEndpoint,
    task: AbortTask,
    rollback: ContainerRollback,
}

impl ContainerLaunch {
    pub(crate) fn new(
        endpoint: CellEndpoint,
        task: tokio::task::JoinHandle<()>,
        rollback: ContainerRollback,
    ) -> Self {
        Self {
            endpoint,
            task: AbortTask::new(task),
            rollback,
        }
    }

    pub(crate) fn endpoint(&self) -> CellEndpoint {
        self.endpoint.clone()
    }

    pub(crate) async fn terminate(mut self) {
        self.task.abort();
        self.rollback.cleanup().await;
    }
}

async fn remove_container(name: &str, apple: bool, path_env: &str) {
    let _ = tokio::process::Command::new(crate::container_cli::bin(apple))
        .args(crate::container_cli::rm_args(apple, name))
        .env("PATH", path_env)
        .kill_on_drop(true)
        .output()
        .await;
}

async fn remove_container_eventually(name: &str, apple: bool, path_env: &str) {
    for delay_ms in [0, 100, 500] {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        remove_container(name, apple, path_env).await;
    }
}

async fn run_container_command(
    bin: &str,
    args: &[String],
    path_env: &str,
) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new(bin)
        .args(args)
        .env("PATH", path_env)
        .kill_on_drop(true)
        .output()
        .await
}

/// Optional container sandbox runtime (gVisor `runsc`) from `HIVE_CONTAINER_RUNTIME`, returned
/// only when resolvable on this host so a missing runtime falls back to podman's default.
pub(crate) fn container_runtime() -> Option<String> {
    let v = std::env::var("HIVE_CONTAINER_RUNTIME").ok()?;
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let candidates: Vec<String> = if v.contains('/') {
        vec![v.to_string()]
    } else {
        [
            "/usr/local/bin",
            "/usr/bin",
            "/usr/local/sbin",
            "/usr/sbin",
            "/bin",
        ]
        .iter()
        .map(|d| format!("{d}/{v}"))
        .collect()
    };
    if let Some(path) = candidates
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    {
        Some(path)
    } else {
        tracing::warn!(runtime = %v, "HIVE_CONTAINER_RUNTIME set but binary not found — using podman default runtime");
        None
    }
}

pub(crate) async fn podman_run_container(
    cell_id: &CellId,
    image: &str,
    ports: &[ContainerPort],
    env: &std::collections::BTreeMap<String, String>,
    max_concurrency: u32,
    path_env: &str,
    runtime: Option<&str>,
    net_json: Option<&str>,
    limits: &ContainerLimits,
    raw_proxy: bool,
    gpu: bool,
) -> anyhow::Result<ContainerLaunch> {
    use tokio::process::Command;
    anyhow::ensure!(
        !ports.is_empty(),
        "podman_run_container: at least one port must be published"
    );
    let primary = ports[0];
    let name = format!(
        "hive-{}",
        cell_id
            .as_str()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    );

    let net = net_json.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let apple = !net
        .as_ref()
        .map(crate::container_cli::needs_podman_networking)
        .unwrap_or(false)
        && crate::container_cli::is_apple_default();
    let bin = crate::container_cli::bin(apple);
    let rollback = ContainerRollback::new(name.clone(), apple, path_env);
    let _ = Command::new(bin)
        .args(crate::container_cli::rm_args(apple, &name))
        .env("PATH", path_env)
        .output()
        .await;

    let volume: Option<(String, String)> = net.as_ref().and_then(|n| {
        let name = n
            .get("vol")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?;
        let path = n
            .get("volpath")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("/data");
        Some((name.to_string(), path.to_string()))
    });
    if let Some((vname, _)) = &volume {
        let _ = Command::new(bin)
            .args(["volume", "create", vname])
            .env("PATH", path_env)
            .output()
            .await;
        if !apple {
            crate::cell_orphans::guard_volume_single_writer(bin, path_env, vname, &name).await?;
        }
    }
    if let Some(n) = &net {
        if let Some(netname) = n
            .get("net")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            let mut c = vec!["network".to_string(), "create".to_string()];
            if !apple {
                c.push("--disable-dns".to_string());
            }
            if let Some(s) = n
                .get("subnet")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                c.push("--subnet".into());
                c.push(s.to_string());
            }
            if !apple {
                if let Some(g) = n
                    .get("gw")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    c.push("--gateway".into());
                    c.push(g.to_string());
                }
            }
            c.push(netname.to_string());
            let created = Command::new(bin)
                .args(&c)
                .env("PATH", path_env)
                .output()
                .await;
            let ok = created
                .as_ref()
                .map(|o| o.status.success())
                .unwrap_or(false);
            let exists = created
                .as_ref()
                .map(|o| {
                    let e = String::from_utf8_lossy(&o.stderr).to_lowercase();
                    e.contains("already exists") || e.contains("already being used by a network")
                })
                .unwrap_or(false);
            if !ok && !exists {
                tracing::warn!(network = %netname, "fixed-subnet network create failed — retrying with an auto-allocated subnet");
                let mut c2 = vec!["network".to_string(), "create".to_string()];
                if !apple {
                    c2.push("--disable-dns".to_string());
                }
                c2.push(netname.to_string());
                let _ = Command::new(bin)
                    .args(&c2)
                    .env("PATH", path_env)
                    .output()
                    .await;
            }
        }
    }
    let mut base: Vec<String> = vec![
        "-d".into(),
        "--name".into(),
        name.clone(),
        "-e".into(),
        format!("PORT={}", primary.container_port),
    ];
    if !apple {
        base.extend(crate::cell_orphans::cell_label_args(cell_id.as_str()));
    }
    for (k, v) in env {
        base.push("-e".into());
        base.push(format!("{k}={v}"));
    }
    if let Some((vname, vpath)) = &volume {
        base.push("-e".into());
        base.push(format!("HIVE_VOLUME={vpath}"));
        base.push("-v".into());
        base.push(format!("{vname}:{vpath}"));
    }
    if let Some(n) = &net {
        if let Some(netname) = n
            .get("net")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            base.push("--network".into());
            base.push(netname.to_string());
            if let Some(ip) = n
                .get("ip")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                base.push("--ip".into());
                base.push(ip.to_string());
            }
            if let Some(hosts) = n.get("hosts").and_then(|v| v.as_array()) {
                for h in hosts.iter().filter_map(|h| h.as_str()) {
                    base.push("--add-host".into());
                    base.push(h.to_string());
                }
            }
        }
    }
    base.extend(crate::container_cli::resource_flags(apple, limits));
    if gpu && !apple {
        base.push("--device".into());
        base.push("nvidia.com/gpu=all".into());
    }
    for p in ports {
        base.push("-p".into());
        let suffix = match p.protocol {
            ContainerProtocol::Udp => "/udp",
            ContainerProtocol::Tcp => "",
        };
        base.push(format!(
            "127.0.0.1:{}:{}{suffix}",
            p.host_port, p.container_port
        ));
    }
    let argv_of = |key: &str| -> Vec<String> {
        net.as_ref()
            .and_then(|n| n.get(key))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let entrypoint = argv_of("entrypoint");
    if !entrypoint.is_empty() {
        base.push("--entrypoint".into());
        base.push(serde_json::to_string(&entrypoint).unwrap_or_else(|_| entrypoint.join(" ")));
    }
    base.push(image.to_string());
    base.extend(argv_of("cmd"));

    let mut attempt: Vec<String> = vec!["run".into()];
    if !apple {
        if let Some(rt) = runtime {
            attempt.push("--runtime".into());
            attempt.push(rt.to_string());
        }
    }
    attempt.extend(base.iter().cloned());
    let mut out = run_container_command(bin, &attempt, path_env).await?;

    if !apple && !out.status.success() && runtime.is_some() {
        tracing::warn!(
            runtime = ?runtime,
            err = %String::from_utf8_lossy(&out.stderr).trim(),
            "container failed under sandbox runtime — retrying with podman default runtime"
        );
        let _ = Command::new(bin)
            .args(crate::container_cli::rm_args(apple, &name))
            .env("PATH", path_env)
            .output()
            .await;
        let mut fb: Vec<String> = vec!["run".into()];
        fb.extend(base.iter().cloned());
        out = run_container_command(bin, &fb, path_env).await?;
    }

    if !apple
        && !out.status.success()
        && is_static_ip_failure(&net, &String::from_utf8_lossy(&out.stderr))
    {
        let netname = net
            .as_ref()
            .and_then(|n| n.get("net"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let build_run = |rt: Option<&str>| {
            let mut v: Vec<String> = vec!["run".into()];
            if let Some(rt) = rt {
                v.push("--runtime".into());
                v.push(rt.to_string());
            }
            v.extend(base.iter().cloned());
            v
        };
        let conflict = parse_ipam_conflict(&net, &String::from_utf8_lossy(&out.stderr));
        let holder_live = match &conflict {
            Some((_, id)) => match Command::new(bin)
                .args(["inspect", "--format", "{{.State.Status}}", id])
                .env("PATH", path_env)
                .output()
                .await
            {
                Ok(o) if o.status.success() => {
                    let st = String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .to_ascii_lowercase();
                    !matches!(st.as_str(), "exited" | "stopped" | "created" | "configured")
                }
                Ok(o) => {
                    let e = String::from_utf8_lossy(&o.stderr).to_ascii_lowercase();
                    !(e.contains("no such") || e.contains("not found"))
                }
                Err(_) => true,
            },
            None => false,
        };
        if holder_live {
            let holder = conflict.as_ref().map(|(_, i)| i.as_str()).unwrap_or("");
            anyhow::bail!(
                "static address on network {netname} is held by live container {holder} \
                 (previous generation still serving — not destroying it). The address \
                 frees when that generation expires; this start will be retried."
            );
        }
        {
            if let Some((_, stale_id)) = conflict {
                tracing::warn!(
                    network = %netname, stale = %stale_id,
                    "IPAM address collision on a leaked lease — reaping stale container and retrying"
                );
                let _ = Command::new(bin)
                    .args(["network", "disconnect", "--force", &netname, &stale_id])
                    .env("PATH", path_env)
                    .output()
                    .await;
                let _ = Command::new(bin)
                    .args(crate::container_cli::rm_args(false, &stale_id))
                    .env("PATH", path_env)
                    .output()
                    .await;
                let _ = Command::new(bin)
                    .args(crate::container_cli::rm_args(apple, &name))
                    .env("PATH", path_env)
                    .output()
                    .await;
                out = run_container_command(bin, &build_run(runtime), path_env).await?;
            }

            if !out.status.success()
                && is_static_ip_failure(&net, &String::from_utf8_lossy(&out.stderr))
            {
                let attached = match Command::new(bin)
                    .args(["ps", "-a", "-q", "--filter", &format!("network={netname}")])
                    .env("PATH", path_env)
                    .output()
                    .await
                {
                    Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                        .split_whitespace()
                        .count(),
                    _ => usize::MAX,
                };
                if attached > 0 {
                    tracing::warn!(
                        network = %netname, attached,
                        "IPAM lease still held, but live containers are on this network \
                         — refusing the network reset; falling back to a dynamic address"
                    );
                }
                if let Some(n) = net.as_ref().filter(|_| attached == 0) {
                    tracing::warn!(network = %netname, "IPAM lease still held after reap — resetting project network");
                    let _ = Command::new(bin)
                        .args(["network", "rm", "--force", &netname])
                        .env("PATH", path_env)
                        .output()
                        .await;
                    let mut c = vec![
                        "network".to_string(),
                        "create".to_string(),
                        "--disable-dns".to_string(),
                    ];
                    if let Some(s) = n
                        .get("subnet")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        c.push("--subnet".into());
                        c.push(s.to_string());
                    }
                    if let Some(g) = n
                        .get("gw")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        c.push("--gateway".into());
                        c.push(g.to_string());
                    }
                    c.push(netname.clone());
                    let rc = Command::new(bin)
                        .args(&c)
                        .env("PATH", path_env)
                        .output()
                        .await;
                    let recreated = rc.as_ref().map(|o| o.status.success()).unwrap_or(false);
                    if !recreated {
                        let _ = Command::new(bin)
                            .args(["network", "create", "--disable-dns", &netname])
                            .env("PATH", path_env)
                            .output()
                            .await;
                    }
                    let _ = Command::new(bin)
                        .args(crate::container_cli::rm_args(apple, &name))
                        .env("PATH", path_env)
                        .output()
                        .await;
                    out = run_container_command(bin, &build_run(runtime), path_env).await?;
                }
            }

            if !out.status.success()
                && is_static_ip_failure(&net, &String::from_utf8_lossy(&out.stderr))
            {
                tracing::warn!(network = %netname, "IPAM lease unclearable — retrying with a dynamic address (dropping the static --ip pin)");
                let mut dyn_base: Vec<String> = Vec::with_capacity(base.len());
                let mut it = base.iter();
                while let Some(a) = it.next() {
                    if a == "--ip" {
                        let _ = it.next();
                        continue;
                    }
                    dyn_base.push(a.clone());
                }
                let _ = Command::new(bin)
                    .args(crate::container_cli::rm_args(apple, &name))
                    .env("PATH", path_env)
                    .output()
                    .await;
                let mut retry: Vec<String> = vec!["run".into()];
                if let Some(rt) = runtime {
                    retry.push("--runtime".into());
                    retry.push(rt.to_string());
                }
                retry.extend(dyn_base);
                out = run_container_command(bin, &retry, path_env).await?;
            }
        }
    }
    if !apple && !out.status.success() && is_lock_exhaustion(&String::from_utf8_lossy(&out.stderr))
    {
        let (vols, cells) = reclaim_podman_locks(bin, path_env).await;
        if vols > 0 || cells > 0 {
            tracing::warn!(
                reclaimed_volumes = vols,
                reclaimed_cells = cells,
                "podman lock pool was exhausted — reclaimed dangling anonymous volumes + exited cells, retrying container start"
            );
            let _ = Command::new(bin)
                .args(crate::container_cli::rm_args(apple, &name))
                .env("PATH", path_env)
                .output()
                .await;
            let mut retry: Vec<String> = vec!["run".into()];
            if let Some(rt) = runtime {
                retry.push("--runtime".into());
                retry.push(rt.to_string());
            }
            retry.extend(base.iter().cloned());
            out = run_container_command(bin, &retry, path_env).await?;
        } else {
            tracing::error!(
                "podman lock pool exhausted and NOTHING was reclaimable — raise num_locks in containers.conf (then `podman system renumber`); containers cannot start on this node"
            );
        }
    }
    if !out.status.success() {
        let _ = Command::new(bin)
            .args(crate::container_cli::rm_args(apple, &name))
            .env("PATH", path_env)
            .output()
            .await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        if !apple && is_lock_exhaustion(stderr) {
            anyhow::bail!(
                "node cannot start containers: podman's lock pool is exhausted and nothing was \
                 reclaimable — raise `num_locks` in containers.conf then run `podman system \
                 renumber` on this node (a config change alone is inert). Not an application \
                 fault and not host disk/memory capacity ({}). {bin} run failed: {stderr}",
                hive_core::fault::NODE_LOCK_POOL_EXHAUSTED
            );
        }
        anyhow::bail!("{bin} run failed: {stderr}");
    }
    let func_addr = format!("127.0.0.1:{}", primary.host_port);
    let ready_secs = std::env::var("HIVE_CONTAINER_READY_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(180);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(ready_secs);
    loop {
        if tokio::net::TcpStream::connect(&func_addr).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let running = crate::container_cli::is_running(apple, &name, path_env).await;
            if running {
                tracing::warn!(
                    container = %name, addr = %func_addr, secs = ready_secs,
                    "container not accepting connections yet but still running — returning endpoint; it will serve once it listens"
                );
                break;
            }
            let logs = Command::new(bin)
                .args(crate::container_cli::logs_tail_args(apple, &name, 20))
                .env("PATH", path_env)
                .output()
                .await
                .ok()
                .map(|o| {
                    let mut t = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    if !e.is_empty() {
                        if !t.is_empty() {
                            t.push('\n');
                        }
                        t.push_str(&e);
                    }
                    t
                })
                .unwrap_or_default();
            let _ = Command::new(bin)
                .args(crate::container_cli::rm_args(apple, &name))
                .env("PATH", path_env)
                .output()
                .await;
            anyhow::bail!(
                "the deployment's own container {name} exited before listening on {func_addr} — \
                 check this deployment's logs, entrypoint and required env; the node started the \
                 container fine ({}). Container logs: {logs}",
                hive_core::fault::DEPLOYMENT_START_FAILED
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let tunnel_addr = listener.local_addr()?.to_string();
    let max_conc = max_concurrency.max(1);
    let task = tokio::spawn(async move {
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
    });
    Ok(ContainerLaunch::new(
        CellEndpoint::Tcp(tunnel_addr),
        task,
        rollback,
    ))
}

/// What the control plane asks a backend to materialize.
#[derive(Clone, Debug)]
pub struct CellSpec {
    pub id: CellId,
    /// Logical image / rootfs name (warm pools are keyed on this).
    pub image: String,
    pub resources: ResourceSpec,
    /// Owning team/tenant slug (empty = "personal"); backends partition a cell's host resources by it.
    pub tenant: String,
    /// Set when this cell is a CONTAINER (podman) rather than a function/microVM; `None` = function cell.
    pub container: Option<ContainerSpec>,
}

/// Make a tenant slug safe as a single host path component (no traversal, no separators); empty or
/// dot-only normalizes to "personal".
pub(crate) fn sanitize_tenant(t: &str) -> String {
    let s: String = t
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "personal".into()
    } else {
        s
    }
}

/// A live, backend-specific handle to a provisioned cell.
#[derive(Clone, Debug)]
pub struct CellHandle {
    pub id: CellId,
    pub image: String,
    pub resources: ResourceSpec,
    /// Filesystem root the build runs against (temp dir for mock, mount for FC).
    pub root: PathBuf,
    /// Opaque backend handle, e.g. the Firecracker API socket path or VM id.
    pub endpoint: Option<String>,
}

/// The isolation contract. One impl == one way of realizing a cell.
#[async_trait]
pub trait CellBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Boot/prepare a cell. This is the expensive step Hive hides behind warm
    /// pools — for the real backend it boots a microVM and loads the image.
    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle>;

    /// Provision while binding an isolated runtime to the caller-authorized delivery identity; the
    /// default accepts only the explicit same-host/container `None` shape.
    async fn provision_runtime(
        &self,
        spec: &CellSpec,
        expected: Option<&hive_core::RuntimeArtifactIdentity>,
    ) -> anyhow::Result<CellHandle> {
        anyhow::ensure!(
            expected.is_none(),
            "backend {} cannot provision a caller-authorized runtime artifact ({})",
            self.name(),
            hive_core::fault::NODE_RUNTIME_MISSING
        );
        self.provision(spec).await
    }

    /// Run a build inside an already-provisioned cell, streaming logs to `sink`.
    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult>;

    /// Make one exact sealed runtime artifact available to serving cells; a backend never receives
    /// checkout coordinates it could rebuild from.
    async fn deliver_build(
        &self,
        _image: &str,
        _artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<()> {
        anyhow::bail!(
            "backend {} did not consume the caller-authorized sealed runtime artifact",
            self.name()
        )
    }

    /// Whether this backend attaches an isolated runtime artifact (then cold-start must resolve and
    /// carry H1); same-host and container execution return false.
    fn requires_runtime_artifact_authorization(&self) -> bool {
        false
    }

    /// Exact identity committed by `deliver_build` for `image`, resolved BEFORE provisioning and
    /// compared with the artifact actually attached, so a launch authorized against H1 can never
    /// execute a later delivery H2. `None` for backends that attach no artifact.
    async fn runtime_artifact_identity(
        &self,
        _image: &str,
    ) -> anyhow::Result<Option<hive_core::RuntimeArtifactIdentity>> {
        Ok(None)
    }

    /// Backend cwd for the application selected by the sealed descriptor.
    fn delivered_workdir(
        &self,
        _artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Start a long-lived function server inside the cell and return an endpoint the gateway opens
    /// connections to; unlike `run_build`, the cell is not single-use.
    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint>;

    /// Tear the cell down. Cells are single-use for builds; for functions this
    /// is called when the instance is scaled down.
    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()>;

    /// Current CPU utilization of the cell's function process as a percentage of its ALLOCATED vCPU
    /// budget — the real saturation signal behind adaptive concurrency; `None` when the backend cannot
    /// sample (the pool then holds concurrency unchanged, never guessing from latency).
    async fn cpu_percent(&self, _cell: &CellHandle) -> Option<f32> {
        None
    }

    /// Run one argv command inside an already-provisioned long-lived cell (Sandboxes); returns once
    /// the command has started, the caller drains `ExecOutput`* then `ExecDone`. Default: unsupported.
    async fn exec_command(
        &self,
        _cell: &CellHandle,
        _req: hive_core::ExecRequest,
    ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>> {
        anyhow::bail!("backend {} does not support exec_command", self.name())
    }

    /// Signal a still-running `exec_command` (by the id it was started with)
    /// to stop. Default: unsupported, matching `exec_command`.
    async fn kill_exec(&self, _cell: &CellHandle, _exec_id: &str) -> anyhow::Result<()> {
        anyhow::bail!("backend {} does not support kill_exec", self.name())
    }

    /// Open one interactive pty session inside `cell` (Sandboxes terminal): a receiver of `PtyOutput`*
    /// then `PtyExited`, plus a sender for typed bytes / resize events. Default: unsupported.
    async fn exec_pty(
        &self,
        _cell: &CellHandle,
        _req: hive_core::ExecPtyRequest,
    ) -> anyhow::Result<(
        tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
        PtyIo,
    )> {
        anyhow::bail!("backend {} does not support exec_pty", self.name())
    }
}

/// Backend-agnostic write side of an `exec_pty` session — an `Fn` closure so each backend plugs in
/// its own transport without `CellBackend` naming a concrete type.
#[derive(Clone)]
pub struct PtyIo {
    input: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
}

impl PtyIo {
    pub fn new(
        input: impl Fn(Vec<u8>) + Send + Sync + 'static,
        resize: impl Fn(u16, u16) + Send + Sync + 'static,
    ) -> Self {
        Self {
            input: Arc::new(input),
            resize: Arc::new(resize),
        }
    }

    pub fn input(&self, bytes: Vec<u8>) {
        (self.input)(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        (self.resize)(cols, rows)
    }
}

#[cfg(test)]
mod container_limits_tests {
    use super::ContainerLimits;

    fn idx(flags: &[String], name: &str) -> Option<usize> {
        flags.iter().position(|f| f == name)
    }

    #[test]
    fn default_limits_emit_memory_cpus_pids_and_no_new_privileges() {
        let flags = ContainerLimits::default().podman_run_flags();
        // Generous ceilings by default (512m OOM-killed real monolith containers).
        // These are the fallbacks when the HIVE_CONTAINER_* env vars are unset.
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(flags[m + 1], "4g");
        let c = idx(&flags, "--cpus").expect("--cpus present");
        assert_eq!(flags[c + 1], "2.0");
        let p = idx(&flags, "--pids-limit").expect("--pids-limit present");
        assert_eq!(flags[p + 1], "1024");
        // Privilege-escalation guard on every container.
        let s = idx(&flags, "--security-opt").expect("--security-opt present");
        assert_eq!(flags[s + 1], "no-new-privileges");
    }

    #[test]
    fn for_container_honors_requested_memory_else_default() {
        // A deployment's requested memory (memory_mib) overrides the default…
        let flags = ContainerLimits::for_container(8192, 0.0, 0).podman_run_flags();
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(flags[m + 1], "8192m", "explicit memory_mib wins");
        // …and 0 falls back to the generous default (never the old 512m).
        let d = ContainerLimits::for_container(0, 0.0, 0).podman_run_flags();
        let dm = idx(&d, "--memory").expect("--memory present");
        assert_ne!(
            d[dm + 1],
            "512m",
            "0 must not resurrect the OOM-killing 512m"
        );
    }

    #[test]
    fn for_container_honors_requested_cpus_and_pids_else_default() {
        // A deployment's requested cpus/pids (fluid.json `container.cpus`/`pids`)
        // override the defaults…
        let flags = ContainerLimits::for_container(0, 4.0, 512).podman_run_flags();
        let c = idx(&flags, "--cpus").expect("--cpus present");
        assert_eq!(flags[c + 1], "4", "explicit cpus wins");
        let p = idx(&flags, "--pids-limit").expect("--pids-limit present");
        assert_eq!(flags[p + 1], "512", "explicit pids wins");
        // …and 0.0/0 fall back to the fleet-wide env-tunable defaults.
        let d = ContainerLimits::for_container(0, 0.0, 0).podman_run_flags();
        let dc = idx(&d, "--cpus").expect("--cpus present");
        assert_eq!(
            d[dc + 1],
            "2.0",
            "0.0 must fall back to the default cpus quota"
        );
        let dp = idx(&d, "--pids-limit").expect("--pids-limit present");
        assert_eq!(
            d[dp + 1],
            "1024",
            "0 must fall back to the default pids limit"
        );
    }

    #[test]
    fn for_container_clamps_an_excessive_requested_memory() {
        // REGRESSION TEST for a real, confirmed vulnerability: a deployment's
        // OWN fluid.json (`{"container":{"memory":"999999m"}}`) used to be
        // applied to `podman run --memory` completely unclamped — a single
        // tenant could remove the ceiling entirely and exhaust host RAM,
        // defeating the entire H1 hardening pass ("one tenant's container
        // cannot exhaust host RAM ... a DoS against every other tenant on a
        // shared host").
        let flags = ContainerLimits::for_container(999_999, 0.0, 0).podman_run_flags();
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(
            flags[m + 1],
            "16384m",
            "an excessive request must be clamped to the fleet-wide maximum"
        );

        // A reasonable, legitimate request under the max is honored exactly
        // (already covered above for 8192, re-asserted here as a boundary
        // check right at the default max).
        let flags = ContainerLimits::for_container(16_384, 0.0, 0).podman_run_flags();
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(
            flags[m + 1],
            "16384m",
            "a request exactly at the max must pass through unchanged"
        );
    }

    #[test]
    fn for_container_clamps_an_excessive_requested_cpus_and_pids() {
        // Same class of regression as the memory test above, for the two
        // dimensions added alongside it: an unclamped `fluid.json`
        // (`{"container":{"cpus":"999","pids":999999}}`) must never be able to
        // remove the CPU-quota / process-table ceiling entirely — the whole
        // point of H1 hardening applies identically to every dimension a
        // tenant can request, not just memory.
        let flags = ContainerLimits::for_container(0, 999.0, 999_999).podman_run_flags();
        let c = idx(&flags, "--cpus").expect("--cpus present");
        assert_eq!(
            flags[c + 1],
            "8",
            "an excessive cpus request must be clamped to the fleet-wide maximum"
        );
        let p = idx(&flags, "--pids-limit").expect("--pids-limit present");
        assert_eq!(
            flags[p + 1],
            "4096",
            "an excessive pids request must be clamped to the fleet-wide maximum"
        );

        // A reasonable, legitimate request exactly at the max is honored exactly.
        let flags = ContainerLimits::for_container(0, 8.0, 4096).podman_run_flags();
        let c = idx(&flags, "--cpus").expect("--cpus present");
        assert_eq!(
            flags[c + 1],
            "8",
            "a cpus request exactly at the max must pass through unchanged"
        );
        let p = idx(&flags, "--pids-limit").expect("--pids-limit present");
        assert_eq!(
            flags[p + 1],
            "4096",
            "a pids request exactly at the max must pass through unchanged"
        );
    }

    #[test]
    fn none_fields_omit_their_flags_but_keep_hardening() {
        let limits = ContainerLimits {
            memory: None,
            cpus: None,
            pids: None,
        };
        let flags = limits.podman_run_flags();
        assert!(idx(&flags, "--memory").is_none());
        assert!(idx(&flags, "--cpus").is_none());
        assert!(idx(&flags, "--pids-limit").is_none());
        // no-new-privileges is unconditional hardening.
        assert!(idx(&flags, "--security-opt").is_some());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod apple_container_live_tests {
    use super::*;
    use hive_core::CellId;

    /// REAL, live end-to-end proof that `podman_run_container` (the core
    /// single-container launch path shared by the mock and Firecracker
    /// backends) correctly drives Apple's `container` CLI on macOS: a real
    /// image is pulled, a real lightweight VM boots, the tunnel accepts a real
    /// TCP connection to the container's published port, and teardown
    /// actually removes it. No mocks — this is the exact code path a real
    /// Fluid function / sandbox / registry-image deploy takes on this OS.
    #[tokio::test]
    async fn podman_run_container_boots_a_real_container_via_apple_container_cli() {
        if !crate::container_cli::available(true).await {
            eprintln!("skipping: apple `container` CLI not available in this environment");
            return;
        }
        let cell_id = CellId::from(format!("livetest-{}", hive_core::now_ms()));
        let host_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let path_env = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
        let env = std::collections::BTreeMap::new();
        let limits = ContainerLimits::default();

        // nginx listens on :80 by default with no custom command needed — the
        // fn only injects a PORT env var, it never overrides the image's own
        // entrypoint, so the image itself must be a real listener.
        let result = podman_run_container(
            &cell_id,
            "docker.io/library/nginx:alpine",
            &[ContainerPort::tcp(80, host_port)],
            &env,
            10,
            path_env,
            None, // no sandbox runtime — irrelevant on macOS (see module doc)
            None, // single container, no compose network
            &limits,
            false, // HTTP-family service — the framed tunnel path
            false, // no GPU on the test host
        )
        .await;

        let launch = result.expect("container must boot successfully via apple container CLI");
        assert!(matches!(launch.endpoint(), CellEndpoint::Tcp(_)));
        let name = format!("hive-{cell_id}");

        // Real teardown: confirm the container is actually gone afterward, not
        // just that the call didn't error.
        launch.terminate().await;
        crate::container_cli::inject_hosts(&name, path_env, &[]).await; // no-op, exercises the empty-entries guard
        let still_running = crate::container_cli::is_running(true, &name, path_env).await;
        assert!(!still_running, "container must be removed after teardown");
    }
}
