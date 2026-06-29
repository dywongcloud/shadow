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
mod compose;
mod databases;
mod dns;
mod dnsserver;
mod docstore;
mod edge;
mod git;
mod gossip;
mod discovery;
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
mod retry;
mod schedule;
mod world;
mod secrets;
mod securelink;
#[cfg(feature = "zkauth")]
mod zkauth;
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
    // Guest kernel cmdline override. Some hosts need extra args — e.g. PVM
    // (software-virtualized KVM on a cloud VM) wants the i8042 probes disabled:
    //   HIVE_FC_BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off i8042.noaux \
    //     i8042.nomux i8042.nopnp i8042.dumbkbd root=/dev/vda rw init=/sbin/hive-cell-agent"
    // Must keep `init=/sbin/hive-cell-agent` so the cell agent runs as PID 1.
    let mut fc_cfg = FirecrackerConfig::default();
    if let Ok(ba) = std::env::var("HIVE_FC_BOOT_ARGS") {
        if !ba.trim().is_empty() {
            fc_cfg.boot_args = ba;
        }
    }
    let firecracker = FirecrackerBackend::new(fc_cfg);
    // Backend kind ("firecracker"|"mock") captured alongside the backend — gossiped
    // so the placement scheduler only auto-targets real-microVM nodes (never the
    // local/mock Mac nodes).
    let (backend, backend_name): (Arc<dyn CellBackend>, &'static str) =
        if firecracker.is_supported() && !force_mock {
            tracing::info!("isolation backend: Firecracker microVM (real, Linux + /dev/kvm)");
            (Arc::new(firecracker), "firecracker")
        } else {
            if force_mock {
                tracing::warn!("isolation backend: MockBackend (HIVE_FORCE_MOCK=1) — runtime is mocked for local development");
            } else {
                tracing::warn!("isolation backend: MockBackend (sandboxed child process) — real microVMs need Linux + /dev/kvm; this is expected for local dev. ALL OTHER subsystems run for real.");
            }
            (
                Arc::new(MockBackend::new(MockConfig {
                    root: std::env::temp_dir().join("hive-cloud-cells"),
                    provision_latency: Duration::from_millis(200),
                    cache_root: std::env::temp_dir().join("hive-cloud-cache"),
                })),
                "mock",
            )
        };
    let backend_name = backend_name.to_string();

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
    // Persistent iroh identity: a stable EndpointId across restarts so peers' cached
    // addresses stay valid (enables gossip-over-iroh + retiring SSH tunnels).
    let iroh_key_path = persist::data_dir().join("iroh_secret.key");
    // Cold-start bootstrap seeds: stable public nodes a wiped/fresh node rendezvous
    // with over iroh (no SSH, no prior state). From `HIVE_BOOTSTRAP_PEERS` (CSV) or a
    // `$HIVE_DATA/bootstrap_peers` file. Registered with iroh so seeds dial by NodeId.
    let bootstrap_seeds = {
        let csv = std::env::var("HIVE_BOOTSTRAP_PEERS").ok().filter(|s| !s.trim().is_empty()).or_else(|| {
            std::fs::read_to_string(persist::data_dir().join("bootstrap_peers"))
                .ok()
                .map(|s| s.replace(['\n', '\r'], ","))
        });
        csv.map(|c| hive_p2p::parse_bootstrap_seeds(&c)).unwrap_or_default()
    };
    // Self-hosted discovery (Seer): pkarr relay URLs the node publishes to + resolves
    // from, instead of depending on n0's public pkarr/DNS. Added alongside n0 (the
    // mesh keeps working if Seer is down). Run Seer itself with HIVE_SEER_ADDR.
    let discovery_urls: Vec<String> = std::env::var("HIVE_DISCOVERY_DNS")
        .ok()
        .map(|s| s.split(',').map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect())
        .unwrap_or_default();
    // HIVE_DISCOVERY_N0=0 drops n0's public pkarr/DNS (Seer-only discovery, n0 relay
    // kept). Default keeps n0 (Seer additive).
    let n0_discovery = std::env::var("HIVE_DISCOVERY_N0").map(|v| v != "0" && v != "false").unwrap_or(true);
    let iroh_ep = match tokio::time::timeout(Duration::from_secs(8), hive_p2p::bind_full(Some(iroh_key_path), &bootstrap_seeds, &discovery_urls, n0_discovery)).await {
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
    // Reachable PUBLIC IP for the client-facing DNS (Seer). Authoritative source is
    // HIVE_PUBLIC_IP (set on nodes with a real inbound-reachable address). These cloud
    // nodes sit behind 1:1 NAT (private NIC IP), so the public IP can't be sniffed off
    // the interface — it MUST be configured. `HIVE_PUBLIC_IP=auto` opts into ip-api
    // detection (correct for 1:1-NAT cloud nodes; do NOT use on home-NAT'd nodes, where
    // the detected IP is the ISP gateway, not reachable inbound). Unset → None (NAT-safe).
    let public_ip = resolve_public_ip(geo.as_ref().and_then(|g| g.4.clone()));
    let public_ip6 = std::env::var("HIVE_PUBLIC_IP6").ok().and_then(|s| {
        s.trim().parse::<std::net::Ipv6Addr>().ok().filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
    }).map(|ip| ip.to_string());
    if let Some(ip) = &public_ip {
        tracing::info!(%ip, ip6 = ?public_ip6, "node public IP (advertised to client DNS / Seer)");
    }
    let me = NodeInfo {
        id: args.name.clone(),
        name: args.name.clone(),
        region: region.clone(),
        public_url: public_base.clone(),
        public_ip,
        public_ip6,
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
        backend: backend_name.clone(),
    };
    tracing::info!(cores = cap.0, mem_mb = cap.1, disk_gb = cap.2, backend = %backend_name, "node host capacity");
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
    // Seed the gossip-transport map from disk so we can reach peers over iroh
    // immediately on restart (bootstrap without the HTTP-over-SSH tunnels). Stable
    // persistent iroh identities keep these addresses valid across restarts.
    *cloud.peer_iroh.write() = persist::load_peer_iroh();

    // Cold-start bootstrap: turn the seeds into always-available iroh gossip targets.
    // Exclude ourselves (a seed list may include this node), key them as `seed:<id>`,
    // and pre-seed `peer_iroh` so the gossip loop dials them over iroh even with an
    // empty/wiped warm map. These are re-asserted each round (so the timeout+evict
    // can't permanently drop a flaky seed) and added to the gossip target list.
    let self_iroh_id = iroh_ep.as_ref().map(|e| e.id().to_string());
    let seed_targets: Vec<(String, String, String)> = bootstrap_seeds
        .iter()
        .filter(|s| self_iroh_id.as_deref() != Some(s.node_id.as_str()))
        .map(|s| (format!("seed:{}", s.node_id), s.node_id.clone(), s.addr_json.clone()))
        .collect();
    {
        let mut pi = cloud.peer_iroh.write();
        for (key, nid, addr) in &seed_targets {
            pi.entry(key.clone()).or_insert_with(|| (nid.clone(), addr.clone()));
        }
    }
    if !seed_targets.is_empty() {
        tracing::info!(seeds = seed_targets.len(), "cold-start bootstrap seeds registered");
    }

    // Record mesh peers so the build cache can be pulled P2P from other nodes.
    *cloud.peers.write() = args.peers.clone();

    // Tier 4: store the iroh endpoint for outbound dialing, and accept inbound P2P
    // tunnels — serving each to THIS node's gateway (so the request is routed to the
    // right local deployment). This makes deployments reachable over QUIC across NATs.
    if let Some(ep) = iroh_ep.clone() {
        *cloud.iroh.write() = Some(ep.clone());
        // Pooled cross-node transport: reuse one QUIC connection per peer, a new
        // stream per request (built here, alongside the endpoint it dials with).
        *cloud.mesh.write() = Some(hive_p2p::PeerPool::new(ep.clone()));
        let gateway_addr = args.listen.to_string();
        // #20 peer trust: enforce the allowlist only when HIVE_PEER_TRUST is set
        // (opt-in — default keeps the mesh open, no behavior change). When on, the
        // accept loop rejects any peer whose iroh identity isn't in the trust set.
        let enforce_trust = std::env::var("HIVE_PEER_TRUST")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        let trust = enforce_trust.then(|| cloud.trusted_peer_ids.clone());
        if enforce_trust {
            tracing::info!(
                trusted = cloud.trusted_peer_ids.read().map(|s| s.len()).unwrap_or(0),
                "iroh P2P peer-trust enforcement ENABLED (#20)"
            );
        }
        // Serve control-plane gossip over the same iroh mesh (the inbound side of
        // the HTTP-over-SSH → QUIC migration). Always provided; peers only use it
        // when THEY have HIVE_GOSSIP_IROH on. The connection trust gate (#20) still
        // applies, so gossip is authenticated by the peer's iroh identity.
        let gossip_handler = crate::gossip::handler(cloud.clone());
        tokio::spawn(hive_p2p::serve_tunnels(ep, gateway_addr, 256, trust, Some(gossip_handler)));
        tracing::info!(gateway = %args.listen, "iroh P2P tunnel server accepting peer connections");
    }

    // Initial cluster reconcile (single-node: this node is leader).
    cloud.cluster.reconcile(cloud.registry.nodes().into_iter().map(|n| n.id).collect());

    // Background loops: cron scheduler + peer gossip.
    spawn_cron_loop(cloud.clone());
    spawn_cluster_loop(cloud.clone());
    spawn_lease_loop(cloud.clone());

    // Self-management GC: reap stale clone/build working dirs under /tmp/hive-deploys
    // every 10 min (dirs untouched >30 min are dead builds), so build scratch never
    // exhausts host disk. Pairs with the firecracker orphan-overlay GC. Skips dirs
    // that still back a live deployment.
    let gc_cloud = cloud.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let n = crate::git::gc_build_dirs(&gc_cloud, Duration::from_secs(1800)).await;
            if n > 0 {
                tracing::info!(removed = n, "gc: cleaned stale build dirs");
            }
        }
    });

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
    // Discovery (Plane B, node↔node): the platform's own pkarr relay — serves/accepts
    // self-verifying iroh NodeAddr records so the fleet resolves peers on platform-owned
    // infra instead of n0. Bind with HIVE_DISCOVERY_ADDR (run on stable PUBLIC nodes).
    // NOTE: distinct from Seer (Plane A, client→node DNS, in dnsserver.rs). HIVE_SEER_ADDR
    // is still accepted as a deprecated alias for the bind addr (the names once collided).
    if let Some(disc_addr) = std::env::var("HIVE_DISCOVERY_ADDR")
        .or_else(|_| std::env::var("HIVE_SEER_ADDR"))
        .ok()
        .and_then(|s| s.parse::<SocketAddr>().ok())
    {
        tokio::spawn(async move {
            if let Err(e) = discovery::serve(disc_addr, discovery::DiscoveryStore::new()).await {
                tracing::warn!(error=%e, %disc_addr, "discovery (pkarr relay) failed to bind");
            }
        });
    }
    if !args.peers.is_empty() || !seed_targets.is_empty() {
        spawn_gossip_loop(cloud.clone(), args.peers.clone(), seed_targets.clone());
    }
    // Active full-mesh health probing: direct, parallel probes of every public peer so
    // down-detection is fast (sub-interval) rather than transitive-gossip + staleness.
    spawn_health_loop(cloud.clone());

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
fn region_id_from_geo(geo: Option<&(f64, f64, String, String, Option<String>)>) -> String {
    let slug = |s: &str| {
        s.trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    };
    if let Some((_, _, city, _country, _ip)) = geo {
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

/// Resolve this node's reachable PUBLIC IPv4 for the client-facing DNS (Seer):
/// - `HIVE_PUBLIC_IP=<ipv4>` → that address (authoritative; validated, never 0.0.0.0/loopback).
/// - `HIVE_PUBLIC_IP=auto`   → the ip-api-detected external IP (`detected`), iff it's a real
///   public address. Correct for 1:1-NAT cloud nodes; NOT for home-NAT'd nodes.
/// - unset → `None`: the node advertises no public IP and is excluded from client DNS answers
///   (the NAT-safe default — a browser must only ever get a node it can actually reach).
fn resolve_public_ip(detected: Option<String>) -> Option<String> {
    let is_public_v4 = |ip: &std::net::Ipv4Addr| {
        !ip.is_unspecified() && !ip.is_loopback() && !ip.is_private() && !ip.is_link_local()
    };
    match std::env::var("HIVE_PUBLIC_IP").ok().map(|s| s.trim().to_string()) {
        Some(v) if v.eq_ignore_ascii_case("auto") => {
            detected.and_then(|s| s.parse::<std::net::Ipv4Addr>().ok()).filter(is_public_v4).map(|ip| ip.to_string())
        }
        Some(v) if !v.is_empty() => {
            v.parse::<std::net::Ipv4Addr>().ok().filter(|ip| !ip.is_unspecified() && !ip.is_loopback()).map(|ip| ip.to_string())
        }
        _ => None,
    }
}

/// Best-effort IP geolocation at startup → (lat, lon, city, country, public_ip). Uses
/// the free ip-api.com endpoint with a short timeout; returns None on any failure so
/// a node always boots even offline. Override with HIVE_GEO="lat,lon,city,country".
/// The 5th tuple element is the detected external IP (ip-api `query`), used only when
/// `HIVE_PUBLIC_IP=auto` (see `resolve_public_ip`).
async fn geolocate() -> Option<(f64, f64, String, String, Option<String>)> {
    if let Ok(manual) = std::env::var("HIVE_GEO") {
        let parts: Vec<&str> = manual.splitn(4, ',').collect();
        if parts.len() == 4 {
            if let (Ok(lat), Ok(lon)) = (parts[0].trim().parse(), parts[1].trim().parse()) {
                return Some((lat, lon, parts[2].trim().to_string(), parts[3].trim().to_string(), None));
            }
        }
    }
    let client = reqwest::Client::new();
    let resp = client
        .get("http://ip-api.com/json/?fields=status,lat,lon,city,country,query")
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
        v.get("query").and_then(|q| q.as_str()).map(|s| s.to_string()),
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
    use std::collections::{HashMap, HashSet};
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            // Drain superseded deployments' keep-warm pools — only the production
            // deployment of each project stays warm (the rest scale to zero).
            cloud.gw.reconcile_keepwarm();
            let self_id = cloud.node_name.clone();
            let region = cloud.region.clone();
            let now = now_ms();
            // Live nodes = self + peers seen within 12s (fast failover detection).
            let nodes_now = cloud.registry.nodes();
            let live: HashSet<String> = nodes_now
                .iter()
                .filter(|n| n.is_self || now.saturating_sub(n.last_seen_ms) < 12_000)
                .map(|n| n.id.clone())
                .collect();
            // node -> region, so the election can be region-constrained. Every node
            // sees the same gossiped regions → all compute the same owner.
            let node_region: HashMap<String, String> =
                nodes_now.iter().map(|n| (n.id.clone(), n.region.clone())).collect();
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
                // Region-constrain the election for region-pinned containers: a
                // `regions:["virginia"]` container must only ever be owned by a
                // holder IN an allowed region — otherwise a non-region holder could
                // win the rendezvous hash and serve from the wrong region. Fall back
                // to the unconstrained set only if NO holder is in an allowed region
                // (availability beats strict pinning when mis-placed).
                let regions = cloud.projects.get(&key).functions.regions;
                if !regions.is_empty() {
                    let allowed: Vec<String> = h
                        .iter()
                        .filter(|n| {
                            node_region
                                .get(*n)
                                .map(|r| regions.iter().any(|ar| ar.eq_ignore_ascii_case(r)))
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    if !allowed.is_empty() {
                        h = allowed;
                    }
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

fn spawn_gossip_loop(cloud: Arc<CloudState>, peers: Vec<String>, seeds: Vec<(String, String, String)>) {
    use std::collections::HashMap;
    // Gossip targets = the configured --peer URLs (warm path, via persisted peer_iroh
    // or HTTP) PLUS the bootstrap seed keys (always-available iroh rendezvous). The
    // seed keys carry the cold/wiped node until its warm peer_iroh is repopulated.
    let mut targets = peers.clone();
    for (key, _, _) in &seeds {
        if !targets.contains(key) {
            targets.push(key.clone());
        }
    }
    tokio::spawn(async move {
        loop {
            // Re-assert the bootstrap seeds into peer_iroh each round: the gossip
            // timeout+evict drops a stale/dead target's entry, but seeds are
            // config-derived rendezvous points we must keep retrying — so re-add any
            // that were evicted (without clobbering a fresher learned address).
            {
                let mut pi = cloud.peer_iroh.write();
                for (key, nid, addr) in &seeds {
                    pi.entry(key.clone()).or_insert_with(|| (nid.clone(), addr.clone()));
                }
            }
            // Rebuild the cross-node routing table from scratch each cycle so stale
            // routes (peers that no longer host a deployment) age out.
            let mut routes: HashMap<String, Vec<crate::state::PeerRoute>> = HashMap::new();
            // #24: node ids successfully gossiped this round — drives the route TTL
            // merge so a transient miss to a healthy peer doesn't drop its routes.
            let mut seen_nodes: std::collections::HashSet<String> = std::collections::HashSet::new();
            // Deployments hosted on each peer (name -> list), for the fleet-wide
            // dashboard deployment view.
            let mut fleet: HashMap<String, Vec<fluid_core::DeploymentInfo>> = HashMap::new();
            // Container holders, seeded with this node's own container deployments.
            let mut holders: HashMap<String, Vec<String>> = HashMap::new();
            // Rebuild replicated zkauth rosters from scratch each cycle (so peer
            // revocations converge); each peer's export is merged in below.
            #[cfg(feature = "zkauth")]
            crate::zkauth::clear_peer_cache();
            for key in cloud.gw.container_projects() {
                holders.entry(key).or_default().push(cloud.node_name.clone());
            }
            for peer in &targets {
                // Announce ourselves, and learn the peer's view of the cloud — over
                // iroh QUIC when enabled+known, else HTTP-over-SSH (bootstrap/fallback).
                let me = cloud.registry.me();
                let me_bytes = serde_json::to_vec(&me).unwrap_or_default();
                let _ = gossip::fetch(&cloud, peer, hive_p2p::GOSSIP_POST, "/v1/nodes/announce", &me_bytes).await;
                let t0 = now_ms();
                let mut rtt = 0u64;
                if let Some(bytes) = gossip::fetch(&cloud, peer, hive_p2p::GOSSIP_GET, "/v1/nodes", &[]).await {
                    // Round-trip latency to this peer (for anycast selection).
                    rtt = now_ms().saturating_sub(t0);
                    if let Ok(nodes) = serde_json::from_slice::<Vec<NodeInfo>>(&bytes) {
                        // The peer lists itself first (its `me()`); record its latency.
                        let peer_self = nodes.first().cloned();
                        // Record the peer's iroh address so the NEXT round can gossip
                        // to it over QUIC instead of HTTP-over-SSH.
                        if let Some(ps) = &peer_self {
                            if let Some(addr) = ps.iroh_addr.clone() {
                                cloud.peer_iroh.write().insert(peer.clone(), (ps.id.clone(), addr));
                            }
                        }
                        let peer_self_id = peer_self.map(|n| n.id);
                        for n in nodes {
                            if n.id != cloud.node_name {
                                // #20: trust the iroh identity of every fleet node we
                                // learn over this (operator-controlled) gossip channel.
                                if let Some(addr) = n.iroh_addr.as_deref() {
                                    if let Some(eid) = hive_p2p::endpoint_id_from_addr_json(addr) {
                                        if let Ok(mut t) = cloud.trusted_peer_ids.write() {
                                            t.insert(eid);
                                        }
                                    }
                                }
                                cloud.registry.upsert_peer(n);
                            }
                        }
                        if let Some(pid) = peer_self_id {
                            cloud.registry.set_health(&pid, rtt, true);
                            // Remember how to reach this node's admin (its peer URL),
                            // so the placement scheduler can dispatch deploys to it.
                            // Skip synthetic seed keys (`seed:<id>`) — they're an iroh
                            // rendezvous handle, not an HTTP admin URL; don't overwrite
                            // a real --peer admin URL with one.
                            if !peer.starts_with("seed:") {
                                cloud.node_admins.write().insert(pid, peer.clone());
                            }
                            // Control-plane sync succeeded (#25) — clears degraded state.
                            cloud.mark_gossip_ok();
                        }
                    }
                } else {
                    // Direct probe to this peer FAILED → mark it unhealthy so Seer (client
                    // DNS) and anycast immediately stop handing browsers a node they can't
                    // reach. Self-heals: the next successful round flips it back healthy.
                    // We can't read the id from a failed response, so resolve it from the
                    // iroh map learned on a prior round (keyed by this peer handle).
                    let id = cloud.peer_iroh.read().get(peer).map(|(id, _)| id.clone());
                    if let Some(id) = id.filter(|s| !s.is_empty()) {
                        cloud.registry.set_health(&id, 0, false);
                    }
                }
                // Learn which deployments this peer serves → routing table.
                if let Some(bytes) = gossip::fetch(&cloud, peer, hive_p2p::GOSSIP_GET, "/v1/serve-hosts", &[]).await
                {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        let node_id = v.get("node").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let region = v.get("region").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let gateway = v.get("gateway").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if !gateway.is_empty() && node_id != cloud.node_name {
                            // #24: this peer was reached this round → its routing is
                            // authoritative (used by the TTL merge below).
                            seen_nodes.insert(node_id.clone());
                            if let Some(hosts) = v.get("hosts").and_then(|x| x.as_array()) {
                                for h in hosts.iter().filter_map(|x| x.as_str()) {
                                    routes.entry(h.to_string()).or_default().push(crate::state::PeerRoute {
                                        node_id: node_id.clone(),
                                        region: region.clone(),
                                        gateway: gateway.clone(),
                                        latency_ms: rtt,
                                        healthy: true,
                                        last_seen_ms: now_ms(),
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
                // Pull this peer's deployments → fleet-wide dashboard view.
                if let Some(bytes) = gossip::fetch(&cloud, peer, hive_p2p::GOSSIP_GET, "/v1/fleet-deployments", &[]).await
                {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        let node_id = v.get("node").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if !node_id.is_empty() && node_id != cloud.node_name {
                            if let Some(deps) = v.get("deployments") {
                                if let Ok(list) = serde_json::from_value::<Vec<fluid_core::DeploymentInfo>>(deps.clone()) {
                                    fleet.insert(node_id, list);
                                }
                            }
                        }
                    }
                }
                // Replicate the peer's public zkauth roster so previews placed on
                // THIS node verify against the same ring the home node minted.
                #[cfg(feature = "zkauth")]
                if let Some(bytes) = gossip::fetch(&cloud, peer, hive_p2p::GOSSIP_GET, "/v1/zkauth/roster-export", &[]).await
                {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        crate::zkauth::ingest_peer_export(&v);
                    }
                }
                // Converge container leases (highest fencing epoch wins).
                if let Some(bytes) = gossip::fetch(&cloud, peer, hive_p2p::GOSSIP_GET, "/v1/leases", &[]).await
                {
                    if let Ok(leases) = serde_json::from_slice::<Vec<crate::lease::ContainerLease>>(&bytes) {
                        for l in leases {
                            cloud.leases.merge(l);
                        }
                    }
                }
            }
            // #24: TTL-merge routes so a route from a peer we briefly couldn't reach
            // this round survives (up to ROUTE_TTL_MS) instead of vanishing and
            // 404-ing the deployment; reached peers' routes are still authoritative.
            let merged = {
                let prev = cloud.peer_routes.read().clone();
                crate::state::merge_routes_ttl(&prev, routes, &seen_nodes, now_ms(), crate::state::ROUTE_TTL_MS)
            };
            *cloud.peer_routes.write() = merged;
            // TTL-merge fleet deployments too (same rationale as routes): a single missed
            // gossip fetch to a peer must NOT wipe its projects from the dashboard's
            // workflows/runs/deployments views. Carry forward an alive-but-unreached
            // node's deployments; drop only nodes that have aged out of the registry.
            let alive: std::collections::HashSet<String> =
                cloud.registry.nodes().into_iter().map(|n| n.name).collect();
            let merged_deps = {
                let prev = cloud.peer_deployments.read().clone();
                crate::state::merge_deployments_ttl(&prev, fleet, &alive)
            };
            *cloud.peer_deployments.write() = merged_deps;
            *cloud.container_holders.write() = holders;
            // Persist the gossip-transport map so the next restart bootstraps iroh
            // gossip from disk (no SSH tunnel needed for rendezvous).
            crate::persist::save_peer_iroh(&cloud.peer_iroh.read());
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Parse a positive-u64 env var with a default (clamped to >= 1).
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.trim().parse::<u64>().ok()).filter(|&v| v > 0).unwrap_or(default).max(1)
}

/// Decide the health write from a probe result + the running consecutive-miss count.
/// Returns `(new_miss_count, write)` where `write` is `Some(true)`/`Some(false)` to
/// call `set_health`, or `None` to leave the current health untouched. Success →
/// reset + healthy (fast recovery). Failure → only flip unhealthy after `threshold`
/// CONSECUTIVE misses (a single dropped/slow probe never flaps a node).
fn health_decision(prev_misses: u32, ok: bool, threshold: u32) -> (u32, Option<bool>) {
    if ok {
        (0, Some(true))
    } else {
        let m = prev_misses + 1;
        if m >= threshold { (m, Some(false)) } else { (m, None) }
    }
}

/// Active full-mesh health probing (the fast path for down-detection). Every node
/// directly probes every OTHER public node in PARALLEL on a short interval, so health
/// — up AND down — is owned by a direct probe (sub-`interval` flips) instead of
/// transitive gossip + the ~30s staleness drain. Scope = public-IP nodes only: NAT'd
/// nodes are reachable solely via relay, are already excluded from client DNS (the
/// `public_ip` gate in `lb_records`), and stay on the staleness model so relay-probe
/// jitter can't churn their health or spam logs. `nodes()`'s 30s staleness drop stays
/// the backstop for a peer that's both unprobeable and gone. Config:
/// `HIVE_HEALTH_INTERVAL` (s, def 5), `HIVE_HEALTH_TIMEOUT` (s, def 2),
/// `HIVE_HEALTH_FAIL_THRESHOLD` (consecutive misses, def 2).
fn spawn_health_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_HEALTH_INTERVAL", 5));
    let timeout = Duration::from_secs(env_u64("HIVE_HEALTH_TIMEOUT", 2));
    let threshold = env_u64("HIVE_HEALTH_FAIL_THRESHOLD", 2) as u32;
    tracing::info!(?interval, ?timeout, threshold, "active health probing (public nodes)");
    tokio::spawn(async move {
        let mut misses: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        loop {
            tokio::time::sleep(interval).await;
            // No mesh transport bound yet → skip the round (never false-flag a peer).
            if cloud.mesh.read().is_none() {
                continue;
            }
            // Probe set: every OTHER node with a public IP + a resolvable iroh address.
            let targets: Vec<(String, String, String)> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self && n.public_ip.is_some())
                .filter_map(|n| Some((n.id.clone(), n.peer_id.clone()?, n.iroh_addr.clone()?)))
                .collect();
            if targets.is_empty() {
                continue;
            }
            // Probe ALL targets concurrently — a dead/slow peer must not delay the rest.
            let results = futures::future::join_all(targets.into_iter().map(|(name, id, addr)| {
                let cloud = cloud.clone();
                async move { (name, gossip::probe(&cloud, &id, &addr, timeout).await) }
            }))
            .await;
            let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (name, rtt) in results {
                live.insert(name.clone());
                let prev = *misses.get(&name).unwrap_or(&0);
                let (next, write) = health_decision(prev, rtt.is_some(), threshold);
                misses.insert(name.clone(), next);
                match write {
                    Some(true) => cloud.registry.set_health(&name, rtt.unwrap_or(0), true),
                    Some(false) => {
                        tracing::debug!(node = %name, misses = next, "peer marked unhealthy (probe)");
                        cloud.registry.set_health(&name, 0, false);
                    }
                    None => {}
                }
            }
            // Forget miss-counters for nodes no longer in the probe set (relocated/gone).
            misses.retain(|k, _| live.contains(k));
        }
    });
}

#[cfg(test)]
mod health_tests {
    use super::health_decision;

    #[test]
    fn threshold_prevents_single_probe_flapping() {
        let threshold = 2;
        // First miss (< threshold): stay as-is (no write), counter = 1.
        let (m, w) = health_decision(0, false, threshold);
        assert_eq!(m, 1);
        assert_eq!(w, None, "a single dropped probe must NOT flip the node");
        // Second consecutive miss (== threshold): flip unhealthy.
        let (m, w) = health_decision(m, false, threshold);
        assert_eq!(m, 2);
        assert_eq!(w, Some(false), "Nth consecutive miss flips unhealthy");
        // A success resets the counter and restores health immediately.
        let (m, w) = health_decision(m, true, threshold);
        assert_eq!(m, 0, "success resets the miss counter");
        assert_eq!(w, Some(true), "success → healthy (fast recovery)");
    }

    #[test]
    fn threshold_one_flips_on_first_miss() {
        let (m, w) = health_decision(0, false, 1);
        assert_eq!((m, w), (1, Some(false)));
    }
}
