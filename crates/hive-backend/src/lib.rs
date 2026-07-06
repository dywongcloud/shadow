//! `hive-backend` — the pluggable isolation layer.
//!
//! In real Hive a **cell** is a Firecracker microVM on KVM. The control plane,
//! warm pool, scheduler, and lifecycle logic do not care *how* a cell is
//! realized — they only need to provision one, run a build in it, and tear it
//! down. That contract is [`CellBackend`].
//!
//! Two implementations ship here:
//!
//! * [`mock::MockBackend`] — a cell is a sandboxed child-process build in a temp
//!   dir. Runs on macOS/M-series today, so the whole control plane is testable
//!   without virtualization.
//! * [`firecracker::FirecrackerBackend`] — a cell is a real aarch64 Firecracker
//!   microVM, intended to run inside a Lima VM with nested virtualization.

pub mod firecracker;
pub mod mock;

use async_trait::async_trait;
use hive_core::{BuildJob, BuildResult, CellId, LogLine, ResourceSpec};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub use hive_core::FunctionLaunch;

/// Sink the backend writes build output to. The box daemon fans this out to
/// any number of log subscribers.
pub type LogSink = mpsc::UnboundedSender<LogLine>;

/// Cumulative CPU time (user+system) consumed by a SINGLE `pid` since it started,
/// in nanoseconds. Read straight from the OS (no third-party sampler — sysinfo's
/// per-process CPU is unreliable on macOS), so the delta we compute is exact.
#[cfg(target_os = "macos")]
fn pid_cpu_time_ns(pid: u32) -> Option<u64> {
    // proc_pidinfo(PROC_PIDTASKINFO) returns task times in MACH absolute-time
    // units, not nanoseconds — on Apple Silicon a unit is ~41.67ns. Convert via
    // mach_timebase_info (numer/denom), else CPU% reads ~42× too low.
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTASKINFO, 0, &mut ti as *mut _ as *mut libc::c_void, size)
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

/// Snapshot of every process as `(pid, ppid, cumulative_cpu_ns)`. Lets us sum CPU
/// over a cell's whole process TREE — a function may run under a wrapper (e.g.
/// `npm exec next start` spawns `next-server`), so sampling only the direct child
/// would miss the real work. (On Firecracker the cell is one VMM process whose CPU
/// already covers the guest, so its subtree is just itself.)
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
            libc::proc_pidinfo(p, libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut libc::c_void, sz)
        };
        if r != sz {
            continue;
        }
        let cpu = pid_cpu_time_ns(p as u32).unwrap_or(0);
        out.push((p as u32, bi.pbi_ppid, cpu));
    }
    out
}

/// Parse `(ppid, cpu_ns)` from a `/proc/<pid>/stat` line. The comm field (2) may
/// contain spaces/`)`, so we split after the final `)`: then state(0), ppid(1),
/// …, utime(11), stime(12).
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
    Some((ppid, (utime.saturating_add(stime)).saturating_mul(1_000_000_000) / hz as u64))
}

#[cfg(target_os = "linux")]
fn all_procs() -> Vec<(u32, u32, u64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
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

/// Sum cumulative CPU (ns) over `root` and all its descendants in `procs`.
/// `None` if `root` isn't present (process gone).
fn subtree_cpu_ns(root: u32, procs: &[(u32, u32, u64)]) -> Option<u64> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut cpu: HashMap<u32, u64> = HashMap::new();
    for &(pid, ppid, c) in procs {
        children.entry(ppid).or_default().push(pid);
        cpu.insert(pid, c);
    }
    cpu.get(&root)?; // root must exist
    let mut sum = 0u64;
    let mut seen = HashSet::new();
    let mut q = VecDeque::new();
    q.push_back(root);
    seen.insert(root);
    while let Some(p) = q.pop_front() {
        sum = sum.saturating_add(cpu.get(&p).copied().unwrap_or(0));
        if let Some(cs) = children.get(&p) {
            for &c in cs {
                if seen.insert(c) {
                    q.push_back(c);
                }
            }
        }
    }
    Some(sum)
}

/// Per-cell CPU sampler shared by the backends for `cpu_percent` (#2). Computes
/// utilization as `Δsubtree_cpu_time / Δwall_time` — exact, no dependency on a
/// sampler library's internal timing. The all-process snapshot is cached briefly
/// so many per-cell queries within one autoscaler tick share one enumeration. The
/// first sample for a pid returns 0 (baseline only); steady-state is accurate.
pub(crate) struct CpuSampler {
    inner: std::sync::Mutex<SamplerState>,
}

struct SamplerState {
    snapshot: Vec<(u32, u32, u64)>,
    snapshot_at: Option<std::time::Instant>,
    prev: std::collections::HashMap<u32, (u64, std::time::Instant)>,
}

impl CpuSampler {
    pub(crate) fn new() -> Self {
        CpuSampler {
            inner: std::sync::Mutex::new(SamplerState {
                snapshot: Vec::new(),
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
        // Refresh the all-process snapshot at most ~4×/s; per-cell queries in one
        // tick reuse it.
        let stale = g.snapshot_at.map(|t| t.elapsed() >= std::time::Duration::from_millis(250)).unwrap_or(true);
        if stale {
            g.snapshot = all_procs();
            g.snapshot_at = Some(now);
        }
        let cur_ns = subtree_cpu_ns(pid, &g.snapshot)?;
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

/// Open a fresh connection to a cell endpoint. One connection per in-flight
/// request keeps proxying simple; in-function concurrency = many of these open
/// to the same instance at once.
pub async fn connect_endpoint(ep: &CellEndpoint) -> anyhow::Result<Box<dyn DuplexStream>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match ep {
        CellEndpoint::Tcp(addr) => Ok(Box::new(tokio::net::TcpStream::connect(addr).await?)),
        CellEndpoint::Vsock { uds, port } => {
            let mut s = tokio::net::UnixStream::connect(uds).await?;
            s.write_all(format!("CONNECT {port}\n").as_bytes()).await?;
            s.flush().await?;
            // Consume the "OK <peer_port>\n" handshake line.
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
    }
}

/// A container (podman) cell: instead of a microVM / host process, the backend
/// runs this OCI image directly via podman on the HOST. Set when the deployment's
/// function is `runtime == "container"`. Lets Firecracker nodes run containers on
/// the host (outside the microVM) — not just the mock backend.
#[derive(Clone, Debug)]
pub struct ContainerSpec {
    /// OCI image to `podman run` (from the function's `__container__` start_cmd).
    pub image: String,
    /// Port the container listens on inside (host port is allocated + mapped).
    pub port: u16,
}

/// Per-container resource ceilings applied to `podman run` so one tenant's
/// container cannot exhaust host RAM, saturate every CPU, or fork-bomb the node
/// (a DoS against every other tenant on a shared host). `None` on a field = no
/// limit for that dimension. See [`ContainerLimits::default`] for the safe
/// defaults applied to every deployment today; these can later be made
/// per-deployment configurable via project settings.
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
        // Ceilings, NOT reservations (podman `--memory` only bounds peak use), so a
        // generous default is safe: a small app uses little, while a real monolith
        // container (monorepo dev server, bundled front+back) isn't OOM-killed. Still
        // bounds a runaway/hostile container on a shared host (the H1 DoS guard).
        // 512m was far too low and OOM-killed legitimate apps (exit 137). Env-tunable
        // fleet-wide; per-deployment override via fluid.json `container.memory`.
        fn env_or(key: &str, default: &str) -> String {
            std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
        }
        Self {
            memory: Some(env_or("HIVE_CONTAINER_MEMORY", "4g")),
            cpus: Some(env_or("HIVE_CONTAINER_CPUS", "2.0")),
            pids: std::env::var("HIVE_CONTAINER_PIDS").ok().and_then(|s| s.trim().parse().ok()).or(Some(1024)),
        }
    }
}

impl ContainerLimits {
    /// Limits for a specific container function: honor the deployment's requested
    /// memory (`memory_mib`, from fluid.json `container.memory` / project settings)
    /// when set (>0), else the generous, env-tunable default. CPU/PID from default.
    /// The requested value is CLAMPED to a fleet-wide maximum — without this, a
    /// tenant's own `fluid.json` (`{"container":{"memory":"999999m"}}`) could
    /// remove the ceiling entirely (H1's whole point was that ONE tenant's
    /// container must never be able to exhaust host RAM and starve every other
    /// co-located tenant on a shared node).
    pub fn for_container(memory_mib: u32) -> Self {
        let mut l = Self::default();
        if memory_mib > 0 {
            let max = std::env::var("HIVE_CONTAINER_MEMORY_MAX_MIB")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(16_384); // 16 GiB — generous for a real monolith, still a real ceiling
            l.memory = Some(format!("{}m", memory_mib.min(max)));
        }
        l
    }

    /// The `podman run` OPTION flags for these limits plus always-on privilege
    /// hardening (`--security-opt no-new-privileges`). Must be pushed BEFORE the
    /// image name in the args vector (they're run options, not container CMD args).
    /// Shared by every `podman run` path so limits can't drift between backends.
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
        // Prevent suid binaries inside the container from escalating privileges
        // (free defense-in-depth; applied to every container).
        f.push("--security-opt".into());
        f.push("no-new-privileges".into());
        f
    }
}

/// Run an OCI image as a detached podman container on the HOST, then front it with
/// the Fluid tunnel server (so in-function concurrency + the gateway proxy work the
/// same as any other cell). Shared by the mock + Firecracker backends so Firecracker
/// nodes can run containers outside their microVMs. Returns the podman container
/// name (for teardown), the tunnel endpoint, and the accept-loop task handle.
pub(crate) async fn podman_run_container(
    cell_id: &CellId,
    image: &str,
    internal_port: u16,
    host_port: u16,
    env: &std::collections::BTreeMap<String, String>,
    max_concurrency: u32,
    path_env: &str,
    // Optional OCI runtime (e.g. gVisor's `runsc` for stronger sandboxing). `None`
    // uses podman's default (crun/runc). A name or absolute path podman accepts.
    runtime: Option<&str>,
    // Optional multi-service (docker-compose) network config — a JSON object from the
    // deploy's `start_cmd[3]`: { net, subnet, gw, ip, hosts: ["name:ip", …] }. All of
    // a deployment's services join one **DNS-less** podman network with STATIC IPs and
    // `/etc/hosts` (`--add-host`) entries for every sibling, so they reach each other
    // by service name WITHOUT podman's aardvark-dns (which binds the bridge gateway's
    // :53 and would collide with the node's Seer DNS on 0.0.0.0:53). `None` =
    // standalone single container — unchanged behavior.
    net_json: Option<&str>,
    // Per-container resource ceilings (memory/cpus/pids) applied as `podman run`
    // flags so a single tenant can't DoS the shared host. See [`ContainerLimits`].
    limits: &ContainerLimits,
) -> anyhow::Result<(String, CellEndpoint, tokio::task::JoinHandle<()>)> {
    use tokio::process::Command;
    let name = format!("hive-{}", cell_id.as_str().replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
    // Clear any stale container from a prior cell at this id (kill_on_drop can't).
    let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;

    let net = net_json.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    // Automatic persistent volume: the run-config JSON (start_cmd[3]) may carry a
    // stable `vol` name + `volpath` mount point. Every container gets a host-backed
    // named volume (≥1 GB, effectively bounded only by host disk) mounted there, so
    // its data survives instance restarts / scale-to-zero. The name is keyed per
    // project (not per cell), so redeploys keep the data. `podman volume create` is
    // idempotent — an "already exists" on a re-run is expected and fine.
    let volume: Option<(String, String)> = net.as_ref().and_then(|n| {
        let name = n.get("vol").and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
        let path = n.get("volpath").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("/data");
        Some((name.to_string(), path.to_string()))
    });
    if let Some((vname, _)) = &volume {
        let _ = Command::new("podman").args(["volume", "create", vname]).env("PATH", path_env).output().await;
    }
    // Idempotently create the per-deployment DNS-LESS network (no aardvark → no :53
    // collision with Seer DNS). "already exists" / overlap on a re-run is fine.
    if let Some(n) = &net {
        if let Some(netname) = n.get("net").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            let mut c = vec!["network".to_string(), "create".to_string(), "--disable-dns".to_string()];
            if let Some(s) = n.get("subnet").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                c.push("--subnet".into());
                c.push(s.to_string());
            }
            if let Some(g) = n.get("gw").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                c.push("--gateway".into());
                c.push(g.to_string());
            }
            c.push(netname.to_string());
            let _ = Command::new("podman").args(&c).env("PATH", path_env).output().await;
        }
    }
    // Base `podman run` args (everything after the subcommand, sans --runtime). The
    // port is published on 127.0.0.1 ONLY — the container is never exposed to the
    // internet directly; it's reached solely via the gateway/ngrok for the deployment.
    let mut base: Vec<String> = vec![
        "-d".into(), "--name".into(), name.clone(),
        "-e".into(), format!("PORT={internal_port}"),
    ];
    for (k, v) in env {
        base.push("-e".into());
        base.push(format!("{k}={v}"));
    }
    // Mount the automatic persistent volume + tell the app where it is ($HIVE_VOLUME).
    if let Some((vname, vpath)) = &volume {
        base.push("-e".into());
        base.push(format!("HIVE_VOLUME={vpath}"));
        base.push("-v".into());
        base.push(format!("{vname}:{vpath}"));
    }
    // Shared network with a static IP + sibling host entries (multi-service only).
    if let Some(n) = &net {
        if let Some(netname) = n.get("net").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            base.push("--network".into());
            base.push(netname.to_string());
            if let Some(ip) = n.get("ip").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
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
    // Resource ceilings + privilege drop — DoS / escalation defense-in-depth.
    // These are `podman run` OPTIONS, so they must precede the image name below.
    base.extend(limits.podman_run_flags());
    base.push("-p".into());
    base.push(format!("127.0.0.1:{host_port}:{internal_port}"));
    base.push(image.to_string());

    // Attempt 1: with the requested sandbox runtime (e.g. gVisor `runsc`) if any.
    let mut attempt: Vec<String> = vec!["run".into()];
    if let Some(rt) = runtime {
        attempt.push("--runtime".into());
        attempt.push(rt.to_string());
    }
    attempt.extend(base.iter().cloned());
    let mut out = Command::new("podman").args(&attempt).env("PATH", path_env).output().await?;

    // Non-breaking fallback: if a sandbox runtime was requested but the container
    // couldn't start under it (a gVisor incompatibility), retry on podman's DEFAULT
    // runtime so the deployment still serves. Isolation degrades to the default for
    // that one container; logged so the degradation is visible.
    if !out.status.success() && runtime.is_some() {
        tracing::warn!(
            runtime = ?runtime,
            err = %String::from_utf8_lossy(&out.stderr).trim(),
            "container failed under sandbox runtime — retrying with podman default runtime"
        );
        let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;
        let mut fb: Vec<String> = vec!["run".into()];
        fb.extend(base.iter().cloned());
        out = Command::new("podman").args(&fb).env("PATH", path_env).output().await?;
    }
    if !out.status.success() {
        // CRITICAL leak fix: a FAILED `podman run` can still leave a partial
        // `created` container holding one of podman's 2048 container LOCKS (e.g.
        // when the failure IS "exceeded num_locks", or an image-pull/port error
        // after the name+storage were allocated). Every cold-start uses a unique
        // cell name, so without this `rm` the failures pile up in `created` state
        // and exhaust the lock pool fleet-wide → CAPACITY_EXHAUSTED on ALL
        // deployments (not just the one that failed). Reclaim the lock here.
        let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;
        anyhow::bail!("podman run failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // Wait for the container's port to accept connections (image pull + boot).
    // Budget is env-tunable (`HIVE_CONTAINER_READY_SECS`, default 180) — a heavy
    // monolith (monorepo dev server: webpack + tsc + nodemon, esp. under gVisor)
    // can take minutes to compile before it listens, and the old hard 90s deadline
    // killed it mid-boot → keep-warm restarted the slow boot → it never converged
    // ("builds + deploys but never loads"). CRITICAL: on deadline we do NOT kill a
    // container that is still RUNNING — we hand back the endpoint and let it finish
    // booting (early requests 502 until the port opens, then it serves). We only
    // bail if the container has actually EXITED (a real crash).
    let func_addr = format!("127.0.0.1:{host_port}");
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
            let running = Command::new("podman")
                .args(["inspect", "-f", "{{.State.Running}}", &name])
                .env("PATH", path_env)
                .output()
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
                .unwrap_or(false);
            if running {
                // Still booting — keep it and return the endpoint (don't restart-loop).
                tracing::warn!(
                    container = %name, addr = %func_addr, secs = ready_secs,
                    "container not accepting connections yet but still running — returning endpoint; it will serve once it listens"
                );
                break;
            }
            // Actually crashed/exited → surface it.
            let logs = Command::new("podman").args(["logs", "--tail", "20", &name]).env("PATH", path_env).output().await
                .ok().map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string()).unwrap_or_default();
            let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;
            anyhow::bail!("container {name} exited before listening on {func_addr}: {logs}");
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
                        fluid_tunnel::TunnelServer::serve(conn, local, max_conc).await;
                    });
                }
                Err(_) => break,
            }
        }
    });
    Ok((name, CellEndpoint::Tcp(tunnel_addr), task))
}

/// Stop + remove a podman container by name (cell teardown). Best-effort.
pub(crate) async fn podman_stop_container(name: &str, path_env: &str) {
    let _ = tokio::process::Command::new("podman")
        .args(["rm", "-f", name])
        .env("PATH", path_env)
        .output()
        .await;
}

/// What the control plane asks a backend to materialize.
#[derive(Clone, Debug)]
pub struct CellSpec {
    pub id: CellId,
    /// Logical image / rootfs name (warm pools are keyed on this).
    pub image: String,
    pub resources: ResourceSpec,
    /// Owning team/tenant slug (empty = "personal"). Lets a backend partition a
    /// cell's host resources per tenant (the mock backend nests cell workdirs
    /// under the tenant; Firecracker nests its per-cell run dir likewise).
    pub tenant: String,
    /// Set when this cell is a CONTAINER (podman) rather than a function/microVM.
    /// The backend runs it via host podman; Firecracker uses this to skip booting a
    /// microVM and run the container on the host instead. `None` = function cell.
    pub container: Option<ContainerSpec>,
}

/// Make a tenant slug safe to use as a single host path component (no traversal,
/// no separators) so an odd/hostile team name can't escape a backend's cells
/// root. Empty / dot-only normalizes to "personal".
pub(crate) fn sanitize_tenant(t: &str) -> String {
    let s: String = t
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') { "personal".into() } else { s }
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

    /// Run a build inside an already-provisioned cell, streaming logs to `sink`.
    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult>;

    /// Make a built deployment available to the cells that will serve it.
    ///
    /// The control plane builds on its own host filesystem (`build_dir`), then
    /// serves via `provision` + `start_function`. For a same-host backend (mock,
    /// child process) the serving cell already sees `build_dir`, so this is a
    /// no-op. For an isolated backend (Firecracker microVM) the guest cannot see
    /// the host's `build_dir`, so this packs it into a per-`image` artifact that
    /// `provision` later attaches to the cell. Called once per deployment, keyed
    /// by the same `image` the function pool will provision with.
    async fn deliver_build(&self, _image: &str, _build_dir: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// The guest path a delivered build is mounted at inside a cell (so the
    /// control plane can register the function pool's workdir to match). For
    /// same-host backends the workdir is the host `build_dir` (returns `None`,
    /// meaning "use the build dir as-is"); Firecracker mounts it at a fixed
    /// guest path.
    fn delivered_workdir(&self) -> Option<&'static str> {
        None
    }

    /// Start a long-lived function server inside the cell and return an endpoint
    /// the gateway can open connections to (Fluid compute serving path). Unlike
    /// `run_build`, the cell is NOT single-use: it stays alive serving many
    /// concurrent requests until the pool decides to scale it down.
    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint>;

    /// Tear the cell down. Cells are single-use for builds; for functions this
    /// is called when the instance is scaled down.
    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()>;

    /// Current CPU utilization of this cell's function process, as a percentage of
    /// the cell's ALLOCATED CPU budget (so ~100 ≈ one fully-busy vCPU for a 1-vCPU
    /// cell, normalized by `cell.resources.vcpus`). This is the REAL saturation
    /// signal that drives adaptive concurrency (#2): the pool lowers per-instance
    /// concurrency when this is high (a CPU-bound function) so new requests scale
    /// OUT instead of piling onto a hot instance, and raises it back when there's
    /// headroom (an I/O-bound function keeps the full ceiling). Returns `None` when
    /// the backend can't sample — the pool then holds concurrency unchanged, never
    /// guessing from latency (which would wrongly penalize I/O-bound work).
    async fn cpu_percent(&self, _cell: &CellHandle) -> Option<f32> {
        None
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
        let flags = ContainerLimits::for_container(8192).podman_run_flags();
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(flags[m + 1], "8192m", "explicit memory_mib wins");
        // …and 0 falls back to the generous default (never the old 512m).
        let d = ContainerLimits::for_container(0).podman_run_flags();
        let dm = idx(&d, "--memory").expect("--memory present");
        assert_ne!(d[dm + 1], "512m", "0 must not resurrect the OOM-killing 512m");
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
        let flags = ContainerLimits::for_container(999_999).podman_run_flags();
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(flags[m + 1], "16384m", "an excessive request must be clamped to the fleet-wide maximum");

        // A reasonable, legitimate request under the max is honored exactly
        // (already covered above for 8192, re-asserted here as a boundary
        // check right at the default max).
        let flags = ContainerLimits::for_container(16_384).podman_run_flags();
        let m = idx(&flags, "--memory").expect("--memory present");
        assert_eq!(flags[m + 1], "16384m", "a request exactly at the max must pass through unchanged");
    }

    #[test]
    fn none_fields_omit_their_flags_but_keep_hardening() {
        let limits = ContainerLimits { memory: None, cpus: None, pids: None };
        let flags = limits.podman_run_flags();
        assert!(idx(&flags, "--memory").is_none());
        assert!(idx(&flags, "--cpus").is_none());
        assert!(idx(&flags, "--pids-limit").is_none());
        // no-new-privileges is unconditional hardening.
        assert!(idx(&flags, "--security-opt").is_some());
    }
}
