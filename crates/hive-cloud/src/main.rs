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
mod dns;
mod docstore;
mod edge;
mod git;
mod gitops;
#[cfg(feature = "guardian")]
mod guardian;
mod identity;
mod incidents;
mod metrics;
mod notifications;
mod persist;
mod project_settings;
mod resources;
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
use hive_backend::firecracker::{FirecrackerBackend, FirecrackerConfig};
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

    // Shared isolation backend. The ONLY component that is allowed to be mocked
    // (and only when a real microVM host isn't available): we auto-select the real
    // Firecracker microVM backend whenever the host supports it (Linux + /dev/kvm),
    // and fall back to the sandboxed child-process MockBackend otherwise (e.g. local
    // dev on macOS). `HIVE_FORCE_MOCK=1` forces the mock for local development.
    let force_mock = std::env::var("HIVE_FORCE_MOCK").map(|v| v == "1").unwrap_or(false);
    let firecracker = FirecrackerBackend::new(FirecrackerConfig::default());
    let backend: Arc<dyn CellBackend> = if firecracker.is_supported() && !force_mock {
        tracing::info!("isolation backend: Firecracker microVM (real, Linux + /dev/kvm)");
        Arc::new(firecracker)
    } else {
        if force_mock {
            tracing::warn!("isolation backend: MockBackend (HIVE_FORCE_MOCK=1) — runtime is mocked for local development");
        } else {
            tracing::warn!("isolation backend: MockBackend (sandboxed child process) — real microVMs need Linux + /dev/kvm; this is expected for local dev. ALL OTHER subsystems run for real.");
        }
        Arc::new(MockBackend::new(MockConfig {
            root: std::env::temp_dir().join("hive-cloud-cells"),
            provision_latency: Duration::from_millis(200),
            cache_root: std::env::temp_dir().join("hive-cloud-cache"),
        }))
    };

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
    // Auto-detect this node's real-world location (IP geolocation) so it reports
    // its true position for the regions map + the function-region picker.
    let geo = geolocate().await;
    if let Some(g) = &geo {
        tracing::info!(city = %g.2, country = %g.3, lat = g.0, lon = g.1, "node geolocated");
    }
    let cap = resources::capacity();
    // Tier 4: bind a REAL iroh P2P endpoint (QUIC + relay/DNS discovery) so this
    // node has a real peer id and can serve/accept Hive tunnels across networks.
    // Best-effort with a timeout: if it can't bind (offline), the node still boots
    // and the HTTP mesh keeps working.
    let iroh_ep = match tokio::time::timeout(Duration::from_secs(8), hive_p2p::bind()).await {
        Ok(Ok(ep)) => {
            tracing::info!(peer_id = %ep.id(), "iroh P2P endpoint bound (real QUIC mesh)");
            Some(ep)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "iroh bind failed — P2P transport disabled (HTTP mesh still routes)");
            None
        }
        Err(_) => {
            tracing::warn!("iroh bind timed out — P2P transport disabled (HTTP mesh still routes)");
            None
        }
    };
    let me = NodeInfo {
        id: args.name.clone(),
        name: args.name.clone(),
        region: args.region.clone(),
        public_url: public_base.clone(),
        peer_id: iroh_ep.as_ref().map(|e| e.id().to_string()),
        iroh_addr: iroh_ep.as_ref().and_then(hive_p2p::addr_json),
        last_seen_ms: now_ms(),
        is_self: true,
        latency_ms: 0,
        healthy: true,
        lat: geo.as_ref().map(|g| g.0),
        lon: geo.as_ref().map(|g| g.1),
        city: geo.as_ref().map(|g| g.2.clone()),
        country: geo.as_ref().map(|g| g.3.clone()),
        cpu_cores: cap.0,
        mem_total_mb: cap.1,
        disk_total_gb: cap.2,
    };
    tracing::info!(cores = cap.0, mem_mb = cap.1, disk_gb = cap.2, "node host capacity");
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

    // Record mesh peers so the build cache can be pulled P2P from other nodes.
    *cloud.peers.write() = args.peers.clone();

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

/// Best-effort IP geolocation at startup → (lat, lon, city, country). Uses the
/// free ip-api.com endpoint with a short timeout; returns None on any failure so
/// a node always boots even offline. Override with HIVE_GEO="lat,lon,city,country".
async fn geolocate() -> Option<(f64, f64, String, String)> {
    if let Ok(manual) = std::env::var("HIVE_GEO") {
        let parts: Vec<&str> = manual.splitn(4, ',').collect();
        if parts.len() == 4 {
            if let (Ok(lat), Ok(lon)) = (parts[0].trim().parse(), parts[1].trim().parse()) {
                return Some((lat, lon, parts[2].trim().to_string(), parts[3].trim().to_string()));
            }
        }
    }
    let client = reqwest::Client::new();
    let resp = client
        .get("http://ip-api.com/json/?fields=status,lat,lon,city,country")
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        return None;
    }
    Some((
        v.get("lat")?.as_f64()?,
        v.get("lon")?.as_f64()?,
        v.get("city")?.as_str()?.to_string(),
        v.get("country")?.as_str()?.to_string(),
    ))
}

async fn serve(router: axum::Router, addr: SocketAddr, label: &str) -> anyhow::Result<()> {
    let mut listeners = vec![tokio::net::TcpListener::bind(addr).await?];
    tracing::info!(%addr, "{label} listening");
    // For loopback, ALSO bind the other IP family on the same port. Browsers
    // resolve `*.localhost` to ::1 (IPv6) first, so a v4-only bind makes deploys
    // unreachable in the browser even though 127.0.0.1 works for curl/CLI.
    if addr.ip().is_loopback() {
        let alt: SocketAddr = match addr.ip() {
            std::net::IpAddr::V4(_) => (std::net::Ipv6Addr::LOCALHOST, addr.port()).into(),
            std::net::IpAddr::V6(_) => (std::net::Ipv4Addr::LOCALHOST, addr.port()).into(),
        };
        match tokio::net::TcpListener::bind(alt).await {
            Ok(l) => { tracing::info!(%alt, "{label} also listening (dual-stack loopback)"); listeners.push(l); }
            Err(e) => tracing::warn!(%alt, error=%e, "{label} could not bind alt loopback address"),
        }
    }
    let mut tasks = Vec::new();
    for l in listeners {
        let r = router.clone();
        tasks.push(tokio::spawn(async move { axum::serve(l, r).await }));
    }
    for t in tasks {
        t.await??;
    }
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
    use std::collections::HashMap;
    tokio::spawn(async move {
        loop {
            // Rebuild the cross-node routing table from scratch each cycle so stale
            // routes (peers that no longer host a deployment) age out.
            let mut routes: HashMap<String, Vec<crate::state::PeerRoute>> = HashMap::new();
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
                let mut rtt = 0u64;
                if let Ok(resp) = cloud
                    .http
                    .get(format!("{peer}/v1/nodes"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    // Round-trip latency to this peer (for anycast selection).
                    rtt = now_ms().saturating_sub(t0);
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
                // Learn which deployments this peer serves → routing table.
                if let Ok(resp) = cloud
                    .http
                    .get(format!("{peer}/v1/serve-hosts"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        let node_id = v.get("node").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let region = v.get("region").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let gateway = v.get("gateway").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if !gateway.is_empty() && node_id != cloud.node_name {
                            if let Some(hosts) = v.get("hosts").and_then(|x| x.as_array()) {
                                for h in hosts.iter().filter_map(|x| x.as_str()) {
                                    routes.entry(h.to_string()).or_default().push(crate::state::PeerRoute {
                                        node_id: node_id.clone(),
                                        region: region.clone(),
                                        gateway: gateway.clone(),
                                        latency_ms: rtt,
                                        healthy: true,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            *cloud.peer_routes.write() = routes;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
