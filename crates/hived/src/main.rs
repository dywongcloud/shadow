//! `hived` — runs a single Hive node: control plane + per-Hive API, backed by
//! either the mock backend (runs anywhere) or the Firecracker backend (inside
//! a Lima VM with nested virtualization). For the MVP one process plays every
//! role (control plane, box daemon, cell daemon are collapsed); the boundaries
//! live in the type system (the `CellBackend` trait, the proto messages).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use hive_backend::{
    firecracker::{FirecrackerBackend, FirecrackerConfig},
    mock::{MockBackend, MockConfig},
    CellBackend,
};
use hive_controlplane::{BoxConfig, Hive, HiveConfig};
use hive_core::{HiveId, ResourceSpec};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Mock,
    Firecracker,
}

#[derive(Parser, Debug)]
#[command(name = "hived", about = "A Hive node (control plane + API)")]
struct Args {
    /// Address for the per-Hive HTTP API.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// Isolation backend for cells.
    #[arg(long, value_enum, default_value_t = Backend::Mock)]
    backend: Backend,

    /// Number of boxes in this Hive.
    #[arg(long, default_value_t = 2)]
    boxes: usize,

    /// vCPUs per box.
    #[arg(long, default_value_t = 16)]
    box_vcpus: u32,

    /// Memory (MiB) per box.
    #[arg(long, default_value_t = 32 * 1024)]
    box_mem_mib: u32,

    /// Warm-pool targets as `IMAGE=COUNT`, repeatable. e.g. --warm node:20=2
    #[arg(long = "warm", value_parser = parse_warm)]
    warm: Vec<(String, usize)>,

    /// Max concurrent builds across the Hive.
    #[arg(long, default_value_t = 8)]
    max_concurrent: usize,

    /// Hive id label.
    #[arg(long, default_value = "hive-local")]
    hive_id: String,

    /// (mock) simulated cold-provision latency in milliseconds.
    #[arg(long, default_value_t = 800)]
    mock_provision_ms: u64,

    /// (firecracker) path to the firecracker binary.
    #[arg(long, default_value = "/usr/local/bin/firecracker")]
    fc_bin: String,
    /// (firecracker) guest kernel (vmlinux) image.
    #[arg(long, default_value = "/var/lib/hive/vmlinux")]
    fc_kernel: String,
    /// (firecracker) directory of per-image base rootfs files.
    #[arg(long, default_value = "/var/lib/hive/rootfs")]
    fc_rootfs_dir: String,
    /// (firecracker) disk-backed dir for per-cell runtime state (NOT tmpfs).
    #[arg(long, default_value = "/var/lib/hive/run")]
    fc_run_dir: String,
}

fn parse_warm(s: &str) -> Result<(String, usize), String> {
    let (img, count) = s
        .rsplit_once('=')
        .ok_or_else(|| format!("expected IMAGE=COUNT, got '{s}'"))?;
    let n: usize = count.parse().map_err(|_| format!("bad count in '{s}'"))?;
    Ok((img.to_string(), n))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hive_controlplane=debug".into()),
        )
        .init();

    let args = Args::parse();

    let warm_targets: BTreeMap<String, usize> = args.warm.iter().cloned().collect();

    let cfg = HiveConfig {
        hive_id: HiveId::from(args.hive_id.clone()),
        boxes: (0..args.boxes)
            .map(|_| BoxConfig {
                vcpus: args.box_vcpus,
                mem_mib: args.box_mem_mib,
            })
            .collect(),
        warm_targets,
        default_warm_target: 0, // only pre-warm explicitly-listed images
        warm_spec: ResourceSpec::default(),
        warm_idle_ttl: Duration::from_secs(120),
        max_concurrent_builds: args.max_concurrent,
        autoscaler_interval: Duration::from_millis(500),
    };

    let backend: Arc<dyn CellBackend> = match args.backend {
        Backend::Mock => Arc::new(MockBackend::new(MockConfig {
            root: std::env::temp_dir().join("hive-cells"),
            provision_latency: Duration::from_millis(args.mock_provision_ms),
            cache_root: std::env::temp_dir().join("hive-cache"),
            receipts_dir: std::env::temp_dir().join("hive-cells"),
        })),
        Backend::Firecracker => {
            let fc = FirecrackerBackend::new(FirecrackerConfig {
                firecracker_bin: args.fc_bin.clone().into(),
                kernel_image: args.fc_kernel.clone().into(),
                rootfs_dir: args.fc_rootfs_dir.clone().into(),
                run_dir: args.fc_run_dir.clone().into(),
                ..FirecrackerConfig::default()
            });
            if !fc.is_supported() {
                tracing::warn!(
                    "firecracker backend selected but environment is not ready \
                     (need Linux + /dev/kvm + firecracker binary). \
                     Provisioning will fail until you run inside the Lima VM. \
                     See scripts/ for setup."
                );
            }
            Arc::new(fc)
        }
    };

    let hive = Hive::start(cfg, backend);
    hive_api::serve(hive, args.listen).await
}
