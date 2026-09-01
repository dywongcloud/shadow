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
#[command(
    name = "fluidd",
    about = "Fluid compute serving daemon (gateway + pool)"
)]
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

#[cfg(unix)]
async fn shutdown_signal() -> anyhow::Result<&'static str> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result?;
            Ok("ctrl-c")
        }
        _ = terminate.recv() => Ok("SIGTERM"),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> anyhow::Result<&'static str> {
    tokio::signal::ctrl_c().await?;
    Ok("ctrl-c")
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
            receipts_dir: std::env::temp_dir().join("fluid-cells"),
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
    let public_gateway = gw.clone();
    let admin_gateway = gw.clone();
    let mut servers = tokio::spawn(async move {
        tokio::try_join!(
            fluid_gateway::serve_public(public_gateway, args.listen),
            fluid_gateway::serve_admin(admin_gateway, args.admin),
        )
        .map(|_| ())
    });

    let server_result = tokio::select! {
        joined = &mut servers => match joined {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!("fluidd server task failed: {error}")),
        },
        signal = shutdown_signal() => match signal {
            Ok(signal) => {
                tracing::info!(signal, "fluidd shutdown requested");
                Ok(())
            }
            Err(error) => Err(error),
        },
    };
    if !servers.is_finished() {
        servers.abort();
        let _ = servers.await;
    }

    let shutdown_result = gw.shutdown().await;
    match (server_result, shutdown_result) {
        (Ok(()), Ok(terminated)) => {
            tracing::info!(terminated, "fluidd shutdown complete");
            Ok(())
        }
        (Err(server), Ok(_)) => Err(server),
        (Ok(()), Err(shutdown)) => Err(shutdown),
        (Err(server), Err(shutdown)) => Err(anyhow::anyhow!(
            "fluidd server failed: {server:#}; shutdown also failed: {shutdown:#}"
        )),
    }
}
