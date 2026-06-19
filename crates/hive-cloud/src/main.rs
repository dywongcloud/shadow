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
mod dnsserver;
mod docstore;
mod edge;
mod git;
mod gitops;
mod lease;
mod guardian;
mod identity;
mod incidents;
mod metrics;
mod notifications;
mod persist;
mod project_settings;
mod resources;
mod secrets;
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
    /// Region id for this node. Default "auto" derives it from the node's real
    /// geolocation (e.g. a node in Los Angeles → "los-angeles"); pass an explicit
    /// value to override.
    #[arg(long, default_value = "auto")]
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

    // Auto-detect this node's real-world location (IP geolocation) so it reports
    // its true position for the regions map + the function-region picker.
    let geo = geolocate().await;
    if let Some(g) = &geo {
        tracing::info!(city = %g.2, country = %g.3, lat = g.0, lon = g.1, "node geolocated");
    }
    // The node's REGION reflects where it actually is. When `--region` is left at
    // the default ("auto"), derive a stable id from the geolocation (e.g. a node
    // in Los Angeles → "los-angeles") instead of a hard-coded label like "iad1".
    // Co-located nodes share the id (same region, multiple nodes).
    let region = if args.region == "auto" {
        region_id_from_geo(geo.as_ref())
    } else {
        args.region.clone()
    };

    // Serving (Fluid) + builds (Hive control plane).
    let fluid = Fluid::start(backend.clone(), FluidConfig::default());
    let gw = Gateway::new(fluid.clone(), args.image.clone());
    let hive = Hive::start(
        HiveConfig {
            hive_id: format!("hive-{}", region).into(),
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
        ConcurrencyLimiter::new(region.clone(), plan).with_burst(args.burst_limit, 10_000),
    );
    let router = Router::new();
    let cron = Arc::new(CronScheduler::new());
    let workflows = WorkflowEngine::new();
    let public_base = format!("http://{}", args.listen);
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
        region: region.clone(),
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
        region.clone(),
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

    // Tier 4: store the iroh endpoint for outbound dialing, and accept inbound P2P
    // tunnels — serving each to THIS node's gateway (so the request is routed to the
    // right local deployment). This makes deployments reachable over QUIC across NATs.
    if let Some(ep) = iroh_ep.clone() {
        *cloud.iroh.write() = Some(ep.clone());
        let gateway_addr = args.listen.to_string();
        tokio::spawn(hive_p2p::serve_tunnels(ep, gateway_addr, 256));
        tracing::info!(gateway = %args.listen, "iroh P2P tunnel server accepting peer connections");
    }

    // Initial cluster reconcile (single-node: this node is leader).
    cloud.cluster.reconcile(cloud.registry.nodes().into_iter().map(|n| n.id).collect());

    // Background loops: cron scheduler + peer gossip.
    spawn_cron_loop(cloud.clone());
    spawn_cluster_loop(cloud.clone());
    spawn_lease_loop(cloud.clone());

    // Warm the always-on GuardianDB (durable, iroh-replicated state store) so it
    // is live before the first snapshot. Best-effort; never blocks boot.
    guardian::init_background();

    // Authoritative DNS server (answers the platform's own records). Non-privileged
    // port by default so it runs without root; set HIVE_DNS_ADDR=0.0.0.0:53 in prod.
    {
        let dns_addr: SocketAddr = std::env::var("HIVE_DNS_ADDR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "127.0.0.1:5354".parse().unwrap());
        let c = cloud.clone();
        tokio::spawn(async move {
            if let Err(e) = dnsserver::serve(c, dns_addr).await {
                tracing::warn!(error=%e, %dns_addr, "DNS server failed to bind (continuing without it)");
            }
        });
    }
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

    // Production TLS: terminate HTTPS on the gateway (same edge pipeline). Uses a
    // real cert from HIVE_TLS_CERT/HIVE_TLS_KEY (PEM paths) when set, else a
    // generated self-signed cert for local dev. Runs ALONGSIDE plain HTTP.
    let tls_public = public.clone();
    let tls_addr: SocketAddr = std::env::var("HIVE_TLS_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:8443".parse().unwrap());
    tokio::spawn(async move {
        if let Err(e) = serve_tls(tls_public, tls_addr).await {
            tracing::warn!(error=%e, %tls_addr, "TLS listener failed (continuing with HTTP)");
        }
    });

    tracing::info!(region=%region, node=%args.name, public=%args.listen, admin=%args.admin, tls=%tls_addr, "hive-cloud node up");

    let pub_srv = serve(public, args.listen, "public");
    let adm_srv = serve(admin_router, args.admin, "admin");
    tokio::try_join!(pub_srv, adm_srv)?;
    Ok(())
}

/// Derive a stable, human-readable region id from a node's geolocation, so a
/// node's region reflects where it actually is (e.g. "los-angeles") rather than a
/// hard-coded label. Co-located nodes resolve to the same id (one region, many
/// nodes). Falls back to "local" when geolocation is unavailable (offline).
fn region_id_from_geo(geo: Option<&(f64, f64, String, String)>) -> String {
    let slug = |s: &str| {
        s.trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    };
    if let Some((_, _, city, _country)) = geo {
        let c = slug(city);
        if !c.is_empty() {
            return c;
        }
    }
    "local".to_string()
}

/// Terminate HTTPS for the gateway. Loads a real cert/key (PEM) from
/// HIVE_TLS_CERT + HIVE_TLS_KEY when both are set (production), otherwise
/// generates a self-signed cert for `localhost`/`*.localhost` (local dev).
async fn serve_tls(app: axum::Router, addr: SocketAddr) -> anyhow::Result<()> {
    use axum_server::tls_rustls::RustlsConfig;
    // Install the ring crypto provider for rustls (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = match (std::env::var("HIVE_TLS_CERT"), std::env::var("HIVE_TLS_KEY")) {
        (Ok(cert_path), Ok(key_path)) if !cert_path.is_empty() && !key_path.is_empty() => {
            tracing::info!(cert=%cert_path, "TLS using provided certificate");
            RustlsConfig::from_pem_file(cert_path, key_path).await?
        }
        _ => {
            let names = vec!["localhost".to_string(), "*.localhost".to_string()];
            let certified = rcgen::generate_simple_self_signed(names)?;
            let cert_pem = certified.cert.pem();
            let key_pem = certified.key_pair.serialize_pem();
            tracing::info!("TLS using generated self-signed certificate (dev; set HIVE_TLS_CERT/KEY for production)");
            RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes()).await?
        }
    };
    tracing::info!(%addr, "HTTPS (TLS) gateway listening");
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await?;
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

/// Container placement: every few seconds, for each container deployment this node
/// holds, compute the preferred owner (rendezvous hash over LIVE holders) and
/// either acquire/renew our fenced lease (if we're preferred) or release it (so the
/// preferred node can take over). A short liveness window gives fast failover.
fn spawn_lease_loop(cloud: Arc<CloudState>) {
    use std::collections::HashSet;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let self_id = cloud.node_name.clone();
            let region = cloud.region.clone();
            let now = now_ms();
            // Live nodes = self + peers seen within 12s (fast failover detection).
            let live: HashSet<String> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| n.is_self || now.saturating_sub(n.last_seen_ms) < 12_000)
                .map(|n| n.id)
                .collect();
            let holders = cloud.container_holders.read().clone();
            for key in cloud.gw.container_projects() {
                // The live nodes that can actually run this container (self + peers
                // that gossiped they hold it).
                let mut h: Vec<String> = holders.get(&key).cloned().unwrap_or_default();
                if !h.contains(&self_id) {
                    h.push(self_id.clone());
                }
                h.retain(|n| live.contains(n));
                if h.is_empty() {
                    h.push(self_id.clone());
                }
                match crate::lease::hrw_owner(&key, &h) {
                    Some(pref) if pref == self_id => {
                        if let Some(l) = cloud.leases.acquire_or_renew(&key, &self_id, &region, 10_000) {
                            tracing::debug!(key=%key, epoch=l.epoch, "holding container lease");
                        }
                    }
                    Some(_) => cloud.leases.release(&key, &self_id),
                    None => {}
                }
            }
        }
    });
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
            // Container holders, seeded with this node's own container deployments.
            let mut holders: HashMap<String, Vec<String>> = HashMap::new();
            for key in cloud.gw.container_projects() {
                holders.entry(key).or_default().push(cloud.node_name.clone());
            }
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
                            // Which container deployments this peer holds → lease election set.
                            if let Some(cs) = v.get("containers").and_then(|x| x.as_array()) {
                                for k in cs.iter().filter_map(|x| x.as_str()) {
                                    holders.entry(k.to_string()).or_default().push(node_id.clone());
                                }
                            }
                        }
                    }
                }
                // Converge container leases (highest fencing epoch wins).
                if let Ok(resp) = cloud
                    .http
                    .get(format!("{peer}/v1/leases"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    if let Ok(leases) = resp.json::<Vec<crate::lease::ContainerLease>>().await {
                        for l in leases {
                            cloud.leases.merge(l);
                        }
                    }
                }
            }
            *cloud.peer_routes.write() = routes;
            *cloud.container_holders.write() = holders;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
