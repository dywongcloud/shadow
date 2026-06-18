//! Real host resource accounting (no mocks): per-node CPU/memory/disk capacity
//! and live usage via `sysinfo`, plus cumulative network transfer from the OS NIC
//! counters. Each node reports its capacity into `NodeInfo`; the cluster total is
//! the sum across live nodes (see `admin::resources`).

use sysinfo::{Disks, Networks, System};

/// Static capacity of this host: (cpu_cores, mem_total_mb, disk_total_gb).
/// Read once at startup and published on the node's `NodeInfo`.
pub fn capacity() -> (u32, u64, u64) {
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let mut sys = System::new();
    sys.refresh_memory();
    let mem_total_mb = sys.total_memory() / 1024 / 1024; // bytes -> MiB
    let disks = Disks::new_with_refreshed_list();
    // Root/primary volume capacity (sum distinct mounts, bytes -> GiB).
    let disk_total_bytes: u64 = disks.list().iter().map(|d| d.total_space()).max().unwrap_or(0);
    (cores, mem_total_mb, disk_total_bytes / 1024 / 1024 / 1024)
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
    let disk_total_gb = disks.list().iter().map(|d| d.total_space()).max().unwrap_or(0) / 1024 / 1024 / 1024;
    let disk_free_gb = disks.list().iter().map(|d| d.available_space()).max().unwrap_or(0) / 1024 / 1024 / 1024;

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
