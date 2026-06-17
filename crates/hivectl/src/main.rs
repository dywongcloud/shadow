//! `hivectl` — client for a Hive node's API. Submit builds, watch logs, inspect
//! cluster state.

use std::collections::BTreeMap;

use clap::{Parser, Subcommand};
use futures::StreamExt;
use hive_core::{BuildJob, ClusterStatus, JobView, LogLine, LogStream, ResourceSpec, SubmitResponse};

#[derive(Parser, Debug)]
#[command(name = "hivectl", about = "Client for a Hive node")]
struct Cli {
    /// Base URL of the Hive API.
    #[arg(long, env = "HIVE_SERVER", default_value = "http://127.0.0.1:8080")]
    server: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Submit a build job.
    Submit {
        /// Base image / rootfs name (matches a warm pool when configured).
        #[arg(long, default_value = "default")]
        image: String,
        /// Git repo to clone (optional).
        #[arg(long, default_value = "")]
        repo: String,
        /// Git ref (branch/tag/sha).
        #[arg(long, default_value = "HEAD")]
        git_ref: String,
        /// Build command, repeatable. Runs in order; stops on first failure.
        #[arg(long = "command", short = 'c')]
        commands: Vec<String>,
        /// Environment variable, repeatable, as KEY=VALUE.
        #[arg(long = "env", value_parser = parse_env)]
        env: Vec<(String, String)>,
        #[arg(long, default_value_t = 2)]
        vcpus: u32,
        #[arg(long, default_value_t = 2048)]
        mem_mib: u32,
        #[arg(long, default_value_t = 8192)]
        disk_mib: u32,
        #[arg(long, default_value_t = 900)]
        timeout_secs: u64,
        /// Build-cache key (e.g. a lockfile hash). Restores/saves --cache-path.
        #[arg(long)]
        cache_key: Option<String>,
        /// Directory to cache (relative to work dir), repeatable.
        #[arg(long = "cache-path")]
        cache_paths: Vec<String>,
        /// Stream logs and wait for completion (exit code mirrors the build).
        #[arg(long)]
        follow: bool,
    },
    /// Show a single job.
    Get { job_id: String },
    /// Stream a job's logs.
    Logs {
        job_id: String,
        /// Keep streaming until the build finishes.
        #[arg(long, default_value_t = true)]
        follow: bool,
    },
    /// Show whole-cluster status.
    Status,
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got '{s}'"))?;
    Ok((k.to_string(), v.to_string()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let http = reqwest::Client::new();

    match cli.cmd {
        Cmd::Submit {
            image,
            repo,
            git_ref,
            commands,
            env,
            vcpus,
            mem_mib,
            disk_mib,
            timeout_secs,
            cache_key,
            cache_paths,
            follow,
        } => {
            let env_map: BTreeMap<String, String> = env.into_iter().collect();
            let mut builder = BuildJob::builder(image)
                .repo(repo, git_ref)
                .commands(commands)
                .resources(ResourceSpec {
                    vcpus,
                    mem_mib,
                    disk_mib,
                    timeout_secs,
                });
            if let Some(key) = cache_key {
                builder = builder.cache(key, cache_paths);
            }
            for (k, v) in env_map {
                builder = builder.env(k, v);
            }
            let job = builder.build();

            let resp: SubmitResponse = http
                .post(format!("{}/v1/jobs", cli.server))
                .json(&job)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("submitted job {}", resp.job_id);

            if follow {
                stream_logs(&http, &cli.server, resp.job_id.as_str()).await?;
                let view = get_job(&http, &cli.server, resp.job_id.as_str()).await?;
                print_job_summary(&view);
                std::process::exit(view.exit_code.unwrap_or(0).max(0));
            }
        }
        Cmd::Get { job_id } => {
            let view = get_job(&http, &cli.server, &job_id).await?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Cmd::Logs { job_id, follow } => {
            if follow {
                stream_logs(&http, &cli.server, &job_id).await?;
            } else {
                // one-shot: stream consumes until done anyway in this MVP
                stream_logs(&http, &cli.server, &job_id).await?;
            }
        }
        Cmd::Status => {
            let status: ClusterStatus = http
                .get(format!("{}/v1/status", cli.server))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            print_status(&status);
        }
    }

    Ok(())
}

async fn get_job(http: &reqwest::Client, server: &str, id: &str) -> anyhow::Result<JobView> {
    Ok(http
        .get(format!("{server}/v1/jobs/{id}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Read the NDJSON log stream, splitting across chunk boundaries.
async fn stream_logs(http: &reqwest::Client, server: &str, id: &str) -> anyhow::Result<()> {
    let resp = http
        .get(format!("{server}/v1/jobs/{id}/logs"))
        .send()
        .await?
        .error_for_status()?;

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            if let Ok(log) = serde_json::from_slice::<LogLine>(line) {
                print_log(&log);
            }
        }
    }
    Ok(())
}

fn print_log(l: &LogLine) {
    match l.stream {
        LogStream::Stdout => println!("{}", l.line),
        LogStream::Stderr => eprintln!("{}", l.line),
        LogStream::System => println!("\x1b[2m» {}\x1b[0m", l.line),
    }
}

fn print_job_summary(v: &JobView) {
    println!(
        "\njob {} → {:?} (exit={:?}, provision_latency={}ms)",
        v.id,
        v.state,
        v.exit_code,
        v.provision_latency_ms.unwrap_or(0)
    );
}

fn print_status(s: &ClusterStatus) {
    println!("Hive {}  (queued: {})", s.hive, s.queued);
    println!("\nBoxes:");
    for b in &s.boxes {
        println!(
            "  {}  vcpu {}/{}  mem {}/{} MiB  cells {} (warm {})",
            b.id, b.vcpus_used, b.vcpus_total, b.mem_used_mib, b.mem_total_mib, b.cells, b.warm_cells
        );
    }
    println!("\nCells:");
    for c in &s.cells {
        println!(
            "  {}  [{:?}]  image={}  job={}",
            c.id,
            c.state,
            c.image,
            c.job.as_ref().map(|j| j.to_string()).unwrap_or_else(|| "-".into())
        );
    }
    println!("\nJobs:");
    for j in &s.jobs {
        println!(
            "  {}  [{:?}]  image={}  latency={}ms",
            j.id,
            j.state,
            j.image,
            j.provision_latency_ms.unwrap_or(0)
        );
    }
}
