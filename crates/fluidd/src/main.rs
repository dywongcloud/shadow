//! `fluidd` — the serving daemon: Fluid compute pool + public gateway + admin
//! API. Runs functions in the same cells as Hive (mock processes anywhere, or
//! Firecracker microVMs inside Lima).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use fluid_compute::{Fluid, FluidConfig};
use fluid_gateway::Gateway;
use hive_backend::{
    firecracker::{FirecrackerBackend, FirecrackerConfig},
    mock::{MockBackend, MockConfig},
    CellBackend,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Mock,
    Firecracker,
}

#[derive(Parser, Debug)]
#[command(name = "fluidd", about = "Fluid compute serving daemon (gateway + pool)")]
struct Args {
    /// Public address (user traffic).
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Admin address (fluidctl).
    #[arg(long, default_value = "127.0.0.1:8786")]
    admin: SocketAddr,
    /// Isolation backend for function instances.
    #[arg(long, value_enum, default_value_t = Backend::Mock)]
    backend: Backend,
    /// Image / rootfs used for function cells (firecracker backend).
    #[arg(long, default_value = "default")]
    image: String,

    // firecracker paths (ignored for mock)
    #[arg(long, default_value = "/usr/local/bin/firecracker")]
    fc_bin: String,
    #[arg(long, default_value = "/var/lib/hive/vmlinux")]
    fc_kernel: String,
    #[arg(long, default_value = "/var/lib/hive/rootfs")]
    fc_rootfs_dir: String,
    #[arg(long, default_value = "/var/lib/hive/run")]
    fc_run_dir: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,fluid_compute=debug".into()),
        )
        .init();

    let args = Args::parse();

    let backend: Arc<dyn CellBackend> = match args.backend {
        Backend::Mock => Arc::new(MockBackend::new(MockConfig {
            root: std::env::temp_dir().join("fluid-cells"),
            provision_latency: Duration::from_millis(300),
            cache_root: std::env::temp_dir().join("fluid-cache"),
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
                tracing::warn!("firecracker backend not ready (need Linux + /dev/kvm + binary)");
            }
            Arc::new(fc)
        }
    };

    let fluid = Fluid::start(backend, FluidConfig::default());
    let gw = Gateway::new(fluid, args.image.clone());

    // Run both servers concurrently.
    let public = fluid_gateway::serve_public(gw.clone(), args.listen);
    let admin = fluid_gateway::serve_admin(gw.clone(), args.admin);
    tokio::try_join!(public, admin)?;
    Ok(())
}
