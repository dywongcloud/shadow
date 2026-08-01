//! Real host resource accounting (no mocks): per-node CPU/memory/disk capacity
//! and live usage via `sysinfo`, plus cumulative network transfer from the OS NIC
//! counters. Each node reports its capacity into `NodeInfo`; the cluster total is
//! the sum across live nodes (see `admin::resources`).

use sysinfo::{Disks, Networks, System};

/// Static capacity of this host: (cpu_cores, mem_total_mb, disk_total_gb).
/// Read once at startup and published on the node's `NodeInfo`.
pub fn capacity() -> (u32, u64, u64) {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let mut sys = System::new();
    sys.refresh_memory();
    let mem_total_mb = sys.total_memory() / 1024 / 1024; // bytes -> MiB
    let disks = Disks::new_with_refreshed_list();
    // Root/primary volume capacity (sum distinct mounts, bytes -> GiB).
    let disk_total_bytes: u64 = disks
        .list()
        .iter()
        .map(|d| d.total_space())
        .max()
        .unwrap_or(0);
    (cores, mem_total_mb, disk_total_bytes / 1024 / 1024 / 1024)
}

/// GPUs on this host, probed ONCE at boot (mirrors `capacity()`'s once-at-boot
/// shape and `geolocate()`'s override-then-probe pattern).
///
/// Returns `(count, model, total_vram_mb)`. A host with no NVIDIA driver, no
/// `nvidia-smi`, or no GPUs returns `(0, None, 0)` — absence is a normal state,
/// never an error, so a CPU-only node's boot is unaffected. `HIVE_GPUS`
/// (`count,model,vram_mb`, e.g. `2,Tesla T4,30720`) overrides the probe for
/// dev/testing exactly like `HIVE_GEO` overrides geolocation.
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
    // `--query-gpu` CSV: one line per GPU, "name, vram_mib".
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

/// Free space on this host's largest data volume, GiB.
///
/// Split out of `live()` because placement needs this figure on a cheap,
/// frequent cadence and must not pay `live()`'s mandatory
/// `MINIMUM_CPU_UPDATE_INTERVAL` sleep (it samples CPU twice) just to read a
/// disk number.
pub fn disk_free_gb() -> u64 {
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .map(|d| d.available_space())
        .max()
        .unwrap_or(0)
        / 1024
        / 1024
        / 1024
}

/// MEASURED free VRAM on this host, MiB — summed across every GPU, straight
/// from the driver.
///
/// The GPU pool previously derived free VRAM purely by arithmetic:
/// `total - (per_instance_reserve * live_gpu_instances)`. That drifts from
/// reality in BOTH directions, and was measured doing so on 2026-07-31:
/// fc-sanjose-gpu-1 was reported as having 30 GiB reserved while `nvidia-smi`
/// showed 105 MiB actually in use per card (phantom reservations from instance
/// counts that no longer reflect live processes), and fc-sanjose-gpu-2's real
/// `llama-server` VRAM was invisible to the accounting entirely because
/// inference endpoints are not serverless function instances. Pooling decisions
/// — "does this model fit on the coordinator, or do we need `--rpc` members?" —
/// were therefore being made against a number that matched neither the hardware
/// nor the workload.
///
/// Returns `None` when there is no usable `nvidia-smi` (non-GPU host, or the
/// query failed), so callers can fall back to the reservation estimate rather
/// than treating an absent probe as "zero free".
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
