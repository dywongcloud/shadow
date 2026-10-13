//! Real host resource accounting (no mocks): per-node CPU/memory/disk capacity and live usage via
//! `sysinfo`, plus cumulative network transfer from the OS NIC counters. Each node reports its
//! capacity into `NodeInfo`; the cluster total is the sum across live nodes (see
//! `admin::resources`).

use sysinfo::{Disks, Networks, System};

/// Static capacity of this host: (cpu_cores, mem_total_mb, disk_total_gb).
/// Read once at startup and published on the node's `NodeInfo`.
/// The disk figure is the capacity of the largest single mount, not a sum across mounts.
pub fn capacity() -> (u32, u64, u64) {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let mut sys = System::new();
    sys.refresh_memory();
    let mem_total_mb = sys.total_memory() / 1024 / 1024;
    let disks = Disks::new_with_refreshed_list();
    let disk_total_bytes: u64 = disks
        .list()
        .iter()
        .map(|d| d.total_space())
        .max()
        .unwrap_or(0);
    (cores, mem_total_mb, disk_total_bytes / 1024 / 1024 / 1024)
}

/// The capabilities placement consumes from one observation of the active backend. Keeping them in
/// one value makes both boot publication and periodic refresh use one backend/config/image verdict.
#[derive(Clone, Debug)]
pub struct RuntimeCapabilities {
    pub wasm_runtime: Option<bool>,
    /// Can this node's active backend actually run a `Runtime::Bun` function? See
    /// `NodeInfo::bun_runtime` for the full contract: `Some(false)` always on Litebox regardless of
    /// what is staged (its pinned upstream syscall shim panics on Bun's own boot probe).
    pub bun_runtime: Option<bool>,
    pub runtime_artifact_protocol: Option<u16>,
    /// Package managers / toolchains this node can execute in its BUILD lane, probed rather than
    /// assumed (see [`BUILD_TOOLCHAINS`]). `None` = UNREPORTED, and, deliberately unlike
    /// `wasm_runtime`/`bun_runtime` (where `None` means NOT CAPABLE), it means UNKNOWN and is
    /// ADMITTED by placement. The asymmetry is about the failure direction: a missing package
    /// manager has a SUBSTITUTE (npm ships with node), so an unreported node is not guaranteed to
    /// fail and the build lane degrades honestly instead, while excluding every unreported peer
    /// would empty the candidate set mid-rollout. Firecracker reports `None` on purpose: its builds
    /// run inside the builder image, which this host can neither see nor stat.
    pub build_toolchains: Option<Vec<String>>,
}

/// The backend selected by `main`, including the exact Firecracker image identity that was passed
/// to the backend constructor. This is deliberately not reconstructed from environment variables:
/// an env value can describe an alternate image while the selected backend still boots its
/// configured base image, and capability advertising must follow the latter.
#[derive(Clone)]
pub struct RuntimeCapabilitySource {
    backend: RuntimeCapabilityBackend,
}

#[derive(Clone)]
enum RuntimeCapabilityBackend {
    Firecracker {
        backend: std::sync::Arc<hive_backend::firecracker::FirecrackerBackend>,
        wasmer_marker: std::path::PathBuf,
        bun_marker: std::path::PathBuf,
    },
    Litebox(std::sync::Arc<hive_backend::litebox::LiteboxBackend>),
    Mock,
}

impl RuntimeCapabilitySource {
    /// Builds the source for a Firecracker node. The Wasmer/Bun markers are published by
    /// `build-rootfs.sh` next to the exact base rootfs, named with the same image-name
    /// normalization as `FirecrackerBackend::rootfs_for`.
    pub fn firecracker(
        backend: std::sync::Arc<hive_backend::firecracker::FirecrackerBackend>,
        config: &hive_backend::firecracker::FirecrackerConfig,
    ) -> Self {
        let image = config
            .base_image
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        Self {
            backend: RuntimeCapabilityBackend::Firecracker {
                backend,
                wasmer_marker: config.rootfs_dir.join(format!("{image}.wasmer")),
                bun_marker: config.rootfs_dir.join(format!("{image}.bun")),
            },
        }
    }

    pub fn litebox(backend: std::sync::Arc<hive_backend::litebox::LiteboxBackend>) -> Self {
        Self {
            backend: RuntimeCapabilityBackend::Litebox(backend),
        }
    }

    pub fn mock() -> Self {
        Self {
            backend: RuntimeCapabilityBackend::Mock,
        }
    }

    /// Observes Wasmer, Bun, runtime-artifact and build-toolchain support from the active backend.
    ///
    /// Firecracker's positive Wasmer/Bun answers are each conditional on the same exact rootfs
    /// content proof provisioning checks, so a marker for any alternate image, a missing or
    /// replaced proof, or rootfs bytes that no longer match the proof cannot produce a positive
    /// capability. Its builds run INSIDE the pinned builder image, never against this host's PATH,
    /// so it reports no build toolchains (`None`: unknown, admitted by placement; the image lane is
    /// probed live at build time instead). Litebox builds run as HOST processes, so the host PATH
    /// is exactly the filesystem the build commands exec against and its `build_toolchains` come
    /// from it.
    ///
    /// Litebox NEVER answers `true` for Bun regardless of what marker or PATH entry exists: its
    /// pinned upstream syscall shim panics (`unimplemented!()`) on Bun's own boot-time
    /// `readlink("/proc/self/fd/3")` probe, so advertising capability there would turn every Bun
    /// cold start into an uncontrolled guest panic instead of an honest placement refusal. This is
    /// a fixed backend limitation, not a probe result (see `LiteboxBackend`'s module doc and
    /// `crate::litebox`'s pre-spawn refusal for the belt-and-braces half). Mock may truthfully exec
    /// a host Wasmer/Bun, but it is never an isolated runtime-artifact backend.
    pub async fn detect(&self) -> RuntimeCapabilities {
        match &self.backend {
            RuntimeCapabilityBackend::Firecracker {
                backend,
                wasmer_marker,
                bun_marker,
            } => match backend.base_runtime_artifact_protocol().await {
                Ok(protocol) => RuntimeCapabilities {
                    wasm_runtime: Some(regular_file(wasmer_marker)),
                    bun_runtime: Some(regular_file(bun_marker)),
                    runtime_artifact_protocol: Some(protocol),
                    build_toolchains: None,
                },
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        code = hive_core::fault::NODE_RUNTIME_MISSING,
                        "runtime capabilities disabled: the active guest rootfs has no valid exact-image proof"
                    );
                    RuntimeCapabilities {
                        wasm_runtime: Some(false),
                        bun_runtime: Some(false),
                        runtime_artifact_protocol: None,
                        build_toolchains: None,
                    }
                }
            },
            RuntimeCapabilityBackend::Litebox(backend) if backend.is_supported() => {
                RuntimeCapabilities {
                    wasm_runtime: Some(which_on_path("wasmer").is_some()),
                    bun_runtime: Some(false),
                    runtime_artifact_protocol: Some(hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION),
                    build_toolchains: Some(detect_host_build_toolchains()),
                }
            }
            RuntimeCapabilityBackend::Litebox(_) => RuntimeCapabilities {
                wasm_runtime: Some(false),
                bun_runtime: Some(false),
                runtime_artifact_protocol: None,
                build_toolchains: None,
            },
            RuntimeCapabilityBackend::Mock => RuntimeCapabilities {
                wasm_runtime: Some(which_on_path("wasmer").is_some()),
                bun_runtime: Some(which_on_path("bun").is_some()),
                runtime_artifact_protocol: None,
                build_toolchains: Some(detect_host_build_toolchains()),
            },
        }
    }
}

fn regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// First match for `name` on the process PATH. Deliberately the same resolution the spawned child
/// gets (it inherits this process's PATH), so a hit here means the child really can exec it.
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file())
}

/// Package managers / toolchains a repository BUILD may invoke, in the canonical probe order, which
/// is also the order they appear in `NodeInfo::build_toolchains`, so every observer reads the same
/// list. PROBED, NEVER ASSUMED, and probed against the filesystem the build ACTUALLY execs against:
/// `git.rs` picks the build lane by BACKEND (mock/litebox run repository commands as plain HOST
/// processes, firecracker inside the pinned builder image), so a host PATH probe is authoritative
/// for the host-exec lane and MEANINGLESS for the image lane, which is why
/// `RuntimeCapabilities::build_toolchains` is `None` on firecracker: the image's toolchain is
/// asserted by provisioning (`ansible/roles/build_executor`, whose live probe requires `node npm
/// corepack bun python3 go`) and re-probed at build time through the same session that will run the
/// command, never inferred from the host.
pub const BUILD_TOOLCHAINS: [(&str, u32); 7] = [
    ("node", 1),
    ("npm", 1 << 1),
    ("pnpm", 1 << 2),
    ("yarn", 1 << 3),
    ("bun", 1 << 4),
    ("go", 1 << 5),
    ("python3", 1 << 6),
];

/// Bit for `name`, or `None` when it is not a toolchain the build lane probes
/// (a bare-word command naming anything else is never substituted).
pub fn build_toolchain_bit(name: &str) -> Option<u32> {
    BUILD_TOOLCHAINS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, bit)| *bit)
}

/// Renders a probe bitmask back into canonical names (the wire form `NodeInfo::build_toolchains`
/// carries, so `/v1/nodes` reads as `["node","npm"]` and not as an opaque integer).
pub fn build_toolchain_names(bits: u32) -> Vec<String> {
    BUILD_TOOLCHAINS
        .iter()
        .filter(|(_, bit)| bits & bit != 0)
        .map(|(tool, _)| (*tool).to_string())
        .collect()
}

/// Probes the HOST PATH: authoritative for the host-exec build lane (mock/litebox), where
/// repository commands run as plain child processes inheriting this process's PATH.
pub fn detect_host_build_toolchains() -> Vec<String> {
    BUILD_TOOLCHAINS
        .iter()
        .filter(|(tool, _)| which_on_path(tool).is_some())
        .map(|(tool, _)| (*tool).to_string())
        .collect()
}

/// A `/bin/sh` script whose EXIT CODE is the build-toolchain bitmask. It probes whatever filesystem
/// the build lane execs against (the host checkout for mock/litebox, the inside of the isolated
/// builder image for firecracker) through the very session that will run the repository commands,
/// so the answer cannot disagree with the command's own resolution. Deliberately exit-code based:
/// the one build primitive available (`IsolatedBuild::run`) reports an exit status and streams
/// stdout into the tenant's build log, so a stdout-based probe would either need a second channel
/// or leak into that log. It is `if`-guarded, so it is safe under `set -e` and `set -u`.
pub fn build_toolchain_probe_script() -> String {
    let mut script = String::from("bits=0");
    for (tool, bit) in BUILD_TOOLCHAINS {
        script.push_str(&format!(
            "\nif command -v {tool} >/dev/null 2>&1; then bits=$((bits + {bit})); fi"
        ));
    }
    script.push_str("\nexit $bits\n");
    script
}

/// Substitute preference for an ABSENT package manager: the first of these that the build lane
/// actually has. `npm` leads because it ships with node, the one toolchain every node that can
/// build at all is guaranteed to carry, so a missing `bun`/`pnpm`/`yarn` degrades to a working
/// install instead of failing the build.
pub const PACKAGE_MANAGER_SUBSTITUTES: [&str; 3] = ["npm", "pnpm", "yarn"];

/// The package manager to use instead of the absent `requested` one, given the toolchains the build
/// lane actually has. A manager that IS present needs no substitute (answering would let a caller
/// swap a working toolchain for a different one), and `None` when no substitute exists, which the
/// caller must report as a build-environment fault naming the missing binary, never as the tenant's
/// application failing.
pub fn package_manager_substitute(requested: &str, available: &[String]) -> Option<&'static str> {
    if available.iter().any(|a| a == requested) {
        return None;
    }
    PACKAGE_MANAGER_SUBSTITUTES
        .iter()
        .copied()
        .find(|candidate| *candidate != requested && available.iter().any(|a| a == candidate))
}

/// GPUs on this host, probed ONCE at boot (mirrors `capacity()`'s once-at-boot shape and
/// `geolocate()`'s override-then-probe pattern). Returns `(count, model, total_vram_mb)`, parsed
/// from `nvidia-smi --query-gpu` CSV (one `name, vram_mib` line per GPU). A host with no NVIDIA
/// driver, no `nvidia-smi`, or no GPUs returns `(0, None, 0)`: absence is a normal state, never an
/// error, so a CPU-only node's boot is unaffected. `HIVE_GPUS` (`count,model,vram_mb`, e.g.
/// `2,Tesla T4,30720`) overrides the probe for dev/testing exactly like `HIVE_GEO` overrides
/// geolocation.
pub fn detect_gpus() -> (u32, Option<String>, u64) {
    if let Ok(v) = std::env::var("HIVE_GPUS") {
        let mut it = v.splitn(3, ',');
        let count = it
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let model = it
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let vram = it
            .next()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        return (count, model, vram);
    }
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let Ok(out) = out else { return (0, None, 0) };
    if !out.status.success() {
        return (0, None, 0);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut count = 0u32;
    let mut model: Option<String> = None;
    let mut vram_mb = 0u64;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut parts = line.rsplitn(2, ',');
        let mem = parts
            .next()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let name = parts.next().map(str::trim).unwrap_or("");
        count += 1;
        vram_mb += mem;
        if model.is_none() && !name.is_empty() {
            model = Some(name.to_string());
        }
    }
    (count, model, vram_mb)
}

/// Live usage snapshot of this host.
#[derive(serde::Serialize)]
pub struct LiveUsage {
    pub cpu_pct: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub disk_free_gb: u64,
    pub disk_total_gb: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

/// Sample live host usage. CPU% needs two reads spaced by the OS minimum interval.
pub async fn live() -> LiveUsage {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
    sys.refresh_cpu_all();
    sys.refresh_memory();

    let cpu_pct = sys.global_cpu_usage();
    let mem_total_mb = sys.total_memory() / 1024 / 1024;
    let mem_used_mb = sys.used_memory() / 1024 / 1024;

    let disks = Disks::new_with_refreshed_list();
    let disk_total_gb = disks
        .list()
        .iter()
        .map(|d| d.total_space())
        .max()
        .unwrap_or(0)
        / 1024
        / 1024
        / 1024;
    let disk_free_gb = disks
        .list()
        .iter()
        .map(|d| d.available_space())
        .max()
        .unwrap_or(0)
        / 1024
        / 1024
        / 1024;

    let nets = Networks::new_with_refreshed_list();
    let mut net_rx_bytes = 0u64;
    let mut net_tx_bytes = 0u64;
    for (_, n) in nets.list() {
        net_rx_bytes += n.total_received();
        net_tx_bytes += n.total_transmitted();
    }

    LiveUsage {
        cpu_pct,
        mem_used_mb,
        mem_total_mb,
        disk_free_gb,
        disk_total_gb,
        net_rx_bytes,
        net_tx_bytes,
    }
}

/// Free space on this host's largest data volume, GiB. Split out of `live()` because placement
/// needs this figure on a cheap, frequent cadence and must not pay `live()`'s mandatory
/// `MINIMUM_CPU_UPDATE_INTERVAL` sleep (it samples CPU twice) just to read a disk number.
///
/// A measured value is clamped to at least 1 because 0 is the UNKNOWN sentinel (a pre-upgrade peer
/// or a failed probe): a genuinely full disk would truncate to 0 GiB and gossip as 'unknown', and
/// placement's admit-on-unknown rule would then route deployments and builds onto a node with zero
/// bytes free. The clamp keeps the honest value under any sane placement floor while the sentinel
/// stays reserved for 'no probe'.
pub fn disk_free_gb() -> u64 {
    let disks = Disks::new_with_refreshed_list();
    match disks.list().iter().map(|d| d.available_space()).max() {
        Some(bytes) => (bytes / 1024 / 1024 / 1024).max(1),
        None => 0,
    }
}

/// MEASURED free VRAM on this host, MiB, summed across every GPU straight from the driver. The GPU
/// pool used to derive free VRAM purely by arithmetic (`total - per_instance_reserve *
/// live_gpu_instances`), which drifts from reality in BOTH directions: phantom reservations from
/// instance counts that no longer reflect live processes, and real `llama-server` VRAM invisible to
/// the accounting because inference endpoints are not serverless function instances. Pooling
/// decisions ('does this model fit on the coordinator, or do we need `--rpc` members?') were
/// therefore made against a number that matched neither the hardware nor the workload.
///
/// Returns `None` when there is no usable `nvidia-smi` (non-GPU host, or the query failed), so
/// callers can fall back to the reservation estimate rather than treating an absent probe as 'zero
/// free'.
pub fn measured_gpu_free_mb() -> Option<u64> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut total = 0u64;
    let mut seen = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Ok(mb) = line.parse::<u64>() {
            total += mb;
            seen = true;
        }
    }
    seen.then_some(total)
}
