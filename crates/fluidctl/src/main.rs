//! `fluidctl` — deploy and inspect Fluid deployments.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use fluid_core::{DeployRequest, DeploymentInfo, Manifest};

#[derive(Parser, Debug)]
#[command(name = "fluidctl", about = "Deploy to a Fluid daemon")]
struct Cli {
    /// Admin URL of the Fluid daemon.
    #[arg(long, env = "FLUID_ADMIN", default_value = "http://127.0.0.1:8786")]
    admin: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Deploy a directory containing a fluid.json manifest.
    Deploy {
        /// Path to the deployment directory.
        dir: PathBuf,
    },
    /// List deployments.
    Ls,
    /// Show Fluid pool stats (instances, in-flight per function).
    Stats,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let http = reqwest::Client::new();

    match cli.cmd {
        Cmd::Deploy { dir } => {
            let dir = std::fs::canonicalize(&dir)?;
            let manifest_path = dir.join("fluid.json");
            let raw = std::fs::read_to_string(&manifest_path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", manifest_path.display()))?;
            let manifest = Manifest::from_json(&raw)?;
            let req = DeployRequest {
                root: dir.to_string_lossy().into_owned(),
                manifest,
                creator: Some("cli".into()),
                git: None,
                production: true,
                project_incarnation: None,
            };
            let info: DeploymentInfo = http
                .post(format!("{}/deployments", cli.admin))
                .json(&req)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("deployed {} (project '{}')", info.id, info.project);
            println!("  functions: {}", info.functions.join(", "));
            println!("  alias:     {}", info.alias);
            println!(
                "  try:       curl -H 'Host: {}' http://127.0.0.1:8787/",
                info.alias
            );
        }
        Cmd::Ls => {
            let list: Vec<DeploymentInfo> = http
                .get(format!("{}/deployments", cli.admin))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for d in list {
                println!(
                    "{}  project={}  functions=[{}]  alias={}",
                    d.id,
                    d.project,
                    d.functions.join(","),
                    d.alias
                );
            }
        }
        Cmd::Stats => {
            let stats: serde_json::Value = http
                .get(format!("{}/stats", cli.admin))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
    }
    Ok(())
}
