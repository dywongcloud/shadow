//! The `guardian-sentinel-server` binary.
//!
//! Opens a GuardianDB over a `data-dir` and serves the administration RPC on a
//! loopback socket. This process is the **owner** of the storage; the redb file
//! lock means only it may hold the `data-dir` open — tools like the TUI panel
//! attach over the socket (see `docs/ADMIN_RPC_PLAN.md`).
//!
//! Usage:
//!   cargo run --features sentinel --bin guardian-sentinel-server
//!   cargo run --features sentinel --bin guardian-sentinel-server -- --addr 127.0.0.1:15433 --data-dir ./guardian_data

use std::path::PathBuf;
use std::sync::Arc;

use guardian_db::sentinel::{
    AdminContext, AdminSource, DEFAULT_ADDR, EmbeddedSource, open_owned, serve,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,guardian_db=info".to_string()),
        )
        .init();

    let mut addr = DEFAULT_ADDR.to_string();
    let mut data_dir = PathBuf::from("./guardian_data");
    // Token gating action ops. Falls back to the GUARDIAN_ADMIN_TOKEN env var.
    let mut token = std::env::var("GUARDIAN_ADMIN_TOKEN").ok();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "--data-dir" | "-d" => {
                if let Some(v) = args.next() {
                    data_dir = PathBuf::from(v);
                }
            }
            "--token" | "-t" => {
                if let Some(v) = args.next() {
                    token = Some(v);
                }
            }
            "--help" | "-h" => {
                println!(
                    "guardian-sentinel-server — administration RPC for a live GuardianDB\n\n\
                     Usage: guardian-sentinel-server [--addr 127.0.0.1:15433] [--data-dir ./guardian_data] [--token <t>]\n\n\
                     With --token (or GUARDIAN_ADMIN_TOKEN), clients must authenticate before any op.\n\
                     This process owns the data-dir (redb lock); connect tools over the socket."
                );
                return Ok(());
            }
            other => eprintln!("ignoring unknown argument: {other}"),
        }
    }

    // Open the storage. This process becomes the single owner of the data-dir.
    let (db, client) = open_owned(&data_dir).await?;

    let ctx = AdminContext::with_data_dir(db, client, data_dir.clone());
    // Reopen stores created in earlier sessions (G1) so clients see them again.
    ctx.reopen_stores().await;
    let source: Arc<dyn AdminSource> = Arc::new(EmbeddedSource::new(ctx));

    tracing::info!(
        "guardian admin RPC listening on {addr} (data-dir {}, auth {})",
        data_dir.display(),
        if token.is_some() { "on" } else { "off" }
    );
    serve(&addr, source, token).await?;
    Ok(())
}
