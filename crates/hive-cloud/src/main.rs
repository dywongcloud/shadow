//! `hive-cloud` — one node of a unified, multi-MacBook cloud.
//!
//! A single binary that runs, behind one public gateway + one admin API:
//! builds & sandbox (Hive control plane), serving with Fluid compute, an edge
//! pipeline (WAF, bot management, CDN), cron, workflows, previews, and a
//! region-aware node registry that meshes with peer nodes over HTTP gossip.

mod admin;
mod apikeys;
mod audit;
mod auth;
mod billing;
mod cluster;
mod databases;
mod edge;
mod git;
#[cfg(feature = "guardian")]
mod guardian;
mod identity;
mod incidents;
mod metrics;
mod notifications;
mod persist;
mod project_settings;
mod securelink;
mod state;
mod teams;
mod webhooks;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use fluid_compute::{Fluid, FluidConfig};
use fluid_gateway::Gateway;
use hive_backend::mock::{MockBackend, MockConfig};
use hive_backend::CellBackend;
use hive_controlplane::{BoxConfig, Hive, HiveConfig};
use hive_core::now_ms;
use hive_edge::{
    workflows::WorkflowStep, BotManager, CdnCache, ConcurrencyLimiter, CronScheduler, NodeInfo,
    NodeRegistry, Plan, Router, Waf, WorkflowEngine,
};

use state::CloudState;

#[derive(Parser, Debug)]
#[command(name = "hive-cloud", about = "A unified cloud node (builds + serving + edge + cron + workflows)")]
struct Args {
    /// Region label, e.g. sfo1, iad1, fra1.
    #[arg(long, default_value = "dev1")]
    region: String,
    /// Unique node name across the cloud.
    #[arg(long, default_value = "node-a")]
    name: String,
    /// Public gateway address (user traffic).
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Admin/control API address (dashboard + CLI + mesh).
    #[arg(long, default_value = "127.0.0.1:8786")]
    admin: SocketAddr,
    /// Peer node admin URLs to mesh with (repeatable), e.g. http://192.168.1.20:8786
    #[arg(long = "peer")]
    peers: Vec<String>,
    /// Image/rootfs for function & build cells.
    #[arg(long, default_value = "default")]
    image: String,
    /// Plan (sets max concurrency: hobby/pro=30k, enterprise=100k).
    #[arg(long, default_value = "pro")]
    plan: String,
    /// Per-region burst concurrency limit (executions per 10s).
    #[arg(long, default_value_t = 1000)]
    burst_limit: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    // Shared isolation backend (mock here; swap for firecracker inside Lima).
    let backend: Arc<dyn CellBackend> = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join("hive-cloud-cells"),
        provision_latency: Duration::from_millis(200),
        cache_root: std::env::temp_dir().join("hive-cloud-cache"),
    }));

    // Serving (Fluid) + builds (Hive control plane).
    let fluid = Fluid::start(backend.clone(), FluidConfig::default());
    let gw = Gateway::new(fluid.clone(), args.image.clone());
    let hive = Hive::start(
        HiveConfig {
            hive_id: format!("hive-{}", args.region).into(),
            boxes: vec![BoxConfig::default(), BoxConfig::default()],
            ..HiveConfig::default()
        },
        backend.clone(),
    );

    // Edge subsystems.
    let waf = Waf::new();
    let bot = Arc::new(BotManager::new());
    let cdn = Arc::new(CdnCache::new());
    let plan = match args.plan.to_lowercase().as_str() {
        "hobby" => Plan::Hobby,
        "enterprise" => Plan::Enterprise,
        _ => Plan::Pro,
    };
    let limiter = Arc::new(
        ConcurrencyLimiter::new(args.region.clone(), plan).with_burst(args.burst_limit, 10_000),
    );
    let router = Router::new();
    let cron = Arc::new(CronScheduler::new());
    let workflows = WorkflowEngine::new();
    let public_base = format!("http://{}", args.listen);
    let me = NodeInfo {
        id: args.name.clone(),
        name: args.name.clone(),
        region: args.region.clone(),
        public_url: public_base.clone(),
        peer_id: None,
        last_seen_ms: now_ms(),
        is_self: true,
        latency_ms: 0,
        healthy: true,
    };
    let registry = NodeRegistry::new(me);

    let cloud = CloudState::new(
        args.region.clone(),
        args.name.clone(),
        public_base.clone(),
        waf,
        bot,
        cdn,
        limiter,
        router,
        registry,
        cron,
        workflows,
        gw.clone(),
        fluid,
        hive,
    );

    // Restore persisted platform state from disk (deployments, settings, WAF…).
    persist::restore(&cloud, persist::load());

    // Initial cluster reconcile (single-node: this node is leader).
    cloud.cluster.reconcile(cloud.registry.nodes().into_iter().map(|n| n.id).collect());

    // Background loops: cron scheduler + peer gossip.
    spawn_cron_loop(cloud.clone());
    spawn_cluster_loop(cloud.clone());
    if !args.peers.is_empty() {
        spawn_gossip_loop(cloud.clone(), args.peers.clone());
    }

    // Public gateway, wrapped in the edge pipeline.
    let public = fluid_gateway::public_router(gw.clone())
        .layer(axum::middleware::from_fn_with_state(cloud.clone(), edge::edge_pipeline));
    let admin_router = admin::router(cloud.clone())
        .layer(axum::middleware::from_fn(auth::require_auth));
    if auth::enforced() {
        tracing::info!("JWT auth enforced on admin mutations (HIVE_JWT_SECRET set)");
    }

    tracing::info!(region=%args.region, node=%args.name, public=%args.listen, admin=%args.admin, "hive-cloud node up");

    let pub_srv = serve(public, args.listen, "public");
    let adm_srv = serve(admin_router, args.admin, "admin");
    tokio::try_join!(pub_srv, adm_srv)?;
    Ok(())
}

async fn serve(router: axum::Router, addr: SocketAddr, label: &str) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "{label} listening");
    axum::serve(l, router).await?;
    Ok(())
}

/// Invoke a deployment route on this node's own public gateway (used by cron &
/// workflows). `deployment` is the Host subdomain (project or deployment id).
async fn invoke(
    cloud: &Arc<CloudState>,
    deployment: &str,
    path: &str,
) -> anyhow::Result<(u16, String)> {
    let url = format!("{}{}", cloud.public_base, path);
    let resp = cloud
        .http
        .get(url)
        .header("host", format!("{deployment}.localhost"))
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

/// Build the workflow step invoker (each step hits a function route).
pub fn wf_invoker(cloud: Arc<CloudState>) -> hive_edge::StepInvoker {
    Arc::new(move |step: WorkflowStep| {
        let cloud = cloud.clone();
        let fut: Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> = Box::pin(async move {
            let (status, body) = invoke(&cloud, &step.deployment, &step.path).await?;
            anyhow::ensure!(status < 500, "step {} -> HTTP {status}", step.name);
            Ok(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()))
        });
        fut
    })
}

fn spawn_cluster_loop(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let members: Vec<String> = cloud.registry.nodes().into_iter().map(|n| n.id).collect();
            cloud.cluster.reconcile(members);
        }
    });
}

fn spawn_cron_loop(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let due = cloud.cron.tick(now_ms());
            for job in due {
                let cloud = cloud.clone();
                tokio::spawn(async move {
                    let res = invoke(&cloud, &job.deployment, &job.path).await;
                    let (status, detail) = match res {
                        Ok((s, _)) => (s, format!("cron {} -> {s}", job.name)),
                        Err(e) => (0, format!("cron {} error: {e}", job.name)),
                    };
                    let ev = cloud.event(&cloud.region, "CRON", &job.deployment, &job.path, status, "cron", &detail);
                    cloud.record(ev);
                });
            }
        }
    });
}

fn spawn_gossip_loop(cloud: Arc<CloudState>, peers: Vec<String>) {
    tokio::spawn(async move {
        loop {
            for peer in &peers {
                // Announce ourselves, and learn the peer's view of the cloud.
                let me = cloud.registry.me();
                let _ = cloud
                    .http
                    .post(format!("{peer}/v1/nodes/announce"))
                    .json(&me)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await;
                let t0 = now_ms();
                if let Ok(resp) = cloud
                    .http
                    .get(format!("{peer}/v1/nodes"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    // Round-trip latency to this peer (for anycast selection).
                    let rtt = now_ms().saturating_sub(t0);
                    if let Ok(nodes) = resp.json::<Vec<NodeInfo>>().await {
                        // The peer lists itself first (its `me()`); record its latency.
                        let peer_self_id = nodes.first().map(|n| n.id.clone());
                        for n in nodes {
                            if n.id != cloud.node_name {
                                cloud.registry.upsert_peer(n);
                            }
                        }
                        if let Some(pid) = peer_self_id {
                            cloud.registry.set_health(&pid, rtt, true);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
