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
mod db_gateway;
mod db_replicate;
mod db_rest;
mod dns;
mod dnsserver;
mod docstore;
mod edge;
mod enterprise;
mod enterprise_api;
mod git;
mod gossip;
mod discovery;
mod gitops;
mod lease;
mod guardian;
mod identity;
mod incidents;
mod integrations;
mod metrics;
mod microfrontends;
mod microfrontends_api;
mod notifications;
mod persist;
mod project_settings;
mod resources;
mod resp;
mod retry;
mod sandboxes;
mod sandboxes_api;
mod sandboxes_platform;
mod schedule;
mod world;
mod world_queue;
mod secrets;
mod vercel_dns;
mod acme;
mod securelink;
mod svcgraph;
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
    let firecracker = Arc::new(FirecrackerBackend::new(fc_cfg));
    // Backend kind ("firecracker"|"mock") captured alongside the backend — gossiped
    // so the placement scheduler only auto-targets real-microVM nodes (never the
    // local/mock Mac nodes). `sandbox_fc` retains the CONCRETE type (Sandboxes'
    // exec/kill methods are Firecracker-specific, not part of the generic
    // `CellBackend` trait object every other subsystem sees).
    let sandbox_fc_supported = firecracker.is_supported() && !force_mock;
    let sandbox_fc: Option<Arc<FirecrackerBackend>> = sandbox_fc_supported.then(|| firecracker.clone());
    let (backend, backend_name): (Arc<dyn CellBackend>, &'static str) =
        if sandbox_fc_supported {
            tracing::info!("isolation backend: Firecracker microVM (real, Linux + /dev/kvm)");
            (firecracker, "firecracker")
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
        sandbox_fc,
    );

    // Restore persisted platform state from disk (deployments, settings, WAF…).
    persist::restore(&cloud, persist::load());
    // Start the coalescing background persister: after this, persist() marks dirty
    // + wakes the writer instead of fsync-ing the whole state on the request thread.
    persist::spawn_persister(cloud.clone());
    // Graceful-shutdown flush: on SIGTERM/SIGINT (e.g. `systemctl restart`) write the
    // latest state synchronously so a restart loses nothing from the coalescing window.
    {
        let flush_cloud = cloud.clone();
        tokio::spawn(async move {
            let _ = &flush_cloud; // keep state alive for the flush
            let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut intr = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = intr.recv() => {}
            }
            tracing::info!("shutdown signal → flushing platform state");
            persist::flush_blocking();
            std::process::exit(0);
        });
    }
    // Start the enterprise SIEM streamer: audit entries for teams with SIEM
    // enabled are POSTed (best-effort, async) to their configured endpoint.
    enterprise::spawn_siem_streamer(cloud.enterprise.clone(), cloud.http.clone());
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
        // MESH HOT-JOIN: a not-yet-trusted node presents HMAC(HIVE_JWT_SECRET, its
        // OWN endpoint id) over a dedicated join stream; the id is the QUIC
        // connection's authenticated remote identity, so a valid proof admits
        // exactly that key into the trust set — no allowlist edit, no restart
        // anywhere. Only offered when the fleet secret is configured (fail-closed:
        // without it, untrusted connections are dropped exactly as before).
        let join_handler: Option<hive_p2p::JoinHandler> = std::env::var("HIVE_JWT_SECRET")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|secret| {
                let cloud = cloud.clone();
                let h: hive_p2p::JoinHandler = std::sync::Arc::new(move |remote_id: String, node_json: Vec<u8>, proof: String| {
                    let cloud = cloud.clone();
                    let secret = secret.clone();
                    Box::pin(async move {
                        let expect = crate::admin::hmac_sha256_hex(secret.as_bytes(), remote_id.as_bytes());
                        // Constant-time-ish compare is overkill here (proof is an
                        // HMAC over a public value; the secret never leaves HMAC),
                        // but never admit on empty/short input.
                        if proof.len() != expect.len() || proof != expect {
                            tracing::warn!(peer = %remote_id, "REJECTED mesh join: invalid proof");
                            return Vec::new();
                        }
                        let Ok(node) = serde_json::from_slice::<NodeInfo>(&node_json) else {
                            tracing::warn!(peer = %remote_id, "REJECTED mesh join: unparseable NodeInfo");
                            return Vec::new();
                        };
                        // The announced iroh identity must BE the proven QUIC identity —
                        // a valid proof must not admit a NodeInfo that routes elsewhere.
                        let announced_eid = node.iroh_addr.as_deref().and_then(hive_p2p::endpoint_id_from_addr_json);
                        if announced_eid.as_deref() != Some(remote_id.as_str()) {
                            tracing::warn!(peer = %remote_id, announced = ?announced_eid, "REJECTED mesh join: NodeInfo iroh identity mismatch");
                            return Vec::new();
                        }
                        if let Ok(mut t) = cloud.trusted_peer_ids.write() {
                            t.insert(remote_id.clone());
                        }
                        if let Some(addr) = node.iroh_addr.clone() {
                            cloud.peer_iroh.write().insert(remote_id.clone(), (remote_id.clone(), addr));
                        }
                        let name = node.name.clone();
                        cloud.registry.upsert_peer(node);
                        cloud.audit.record("_global", "mesh", "join", "node", &name, &format!("endpoint {remote_id} admitted via join proof"));
                        tracing::info!(peer = %remote_id, node = %name, "mesh join ADMITTED (hot-join, key-addressed)");
                        serde_json::to_vec(&cloud.registry.nodes()).unwrap_or_default()
                    })
                });
                h
            });
        tokio::spawn(hive_p2p::serve_tunnels_with_join(ep, gateway_addr, 256, trust, Some(gossip_handler), join_handler));
        tracing::info!(gateway = %args.listen, "iroh P2P tunnel server accepting peer connections (join surface on)");
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
    guardian::set_node_name(&args.name);
    guardian::init_background();
    // Restore-on-rollback guard: if the local snapshot regressed (older than the
    // GuardianDB replica — the failure that silently dropped shoomoo's env vars +
    // reset billing), adopt the replica. Web3 data-sovereignty: the replicated,
    // content-addressed copy outranks a regressed local file.
    guardian::spawn_restore_guard(cloud.clone());

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
    // ALWAYS spawn the gossip loop: targets are now DYNAMIC (recomputed every
    // round from CLI peers + seeds + the persisted/learned key-addressed roster),
    // so a node started with zero peer config still gossips the moment a peer
    // joins INTO it (inbound join populates peer_iroh) or the guardian-replicated
    // roster lands. A zero-target round is a no-op.
    spawn_gossip_loop(cloud.clone(), args.peers.clone(), seed_targets.clone());
    // Roster fallback from GuardianDB (iroh-docs, replicated): when the local
    // peer_iroh.json was lost (wiped data dir) and no seeds are configured, adopt
    // the replicated roster so the node still rejoins the mesh by KEY. Best-effort,
    // never blocks boot; existing entries are never clobbered.
    {
        let c = cloud.clone();
        tokio::spawn(async move {
            for _ in 0..12 {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if !c.peer_iroh.read().is_empty() {
                    return; // seeds/CLI/live gossip already populated it
                }
                if let Some(bytes) = guardian::get("mesh/roster").await {
                    if let Ok(map) = serde_json::from_slice::<std::collections::HashMap<String, (String, String)>>(&bytes) {
                        let me = c.registry.me().peer_id;
                        let mut pi = c.peer_iroh.write();
                        for (k, v) in map {
                            if Some(&v.0) == me.as_ref() {
                                continue;
                            }
                            pi.entry(k).or_insert(v);
                        }
                        tracing::info!(entries = pi.len(), "mesh roster adopted from GuardianDB replica");
                        return;
                    }
                }
            }
        });
    }
    // Active full-mesh health probing: direct, parallel probes of every public peer so
    // down-detection is fast (sub-interval) rather than transitive-gossip + staleness.
    spawn_health_loop(cloud.clone());

    // Eager full-mesh trunking: proactively keep a live iroh trunk to EVERY healthy
    // peer (not just the ones we directly gossip), so cross-node requests reuse a
    // warm trunk instead of paying a cold dial/holepunch on the critical path.
    spawn_trunk_warmer(cloud.clone());

    // Billing meter loop: periodically converts measured fleet compute usage into
    // charges (usage → rate card → ledger → invoice). Runs whether Stripe is
    // configured or not (mock or real). Web3 decentralization: the loop is spawned
    // on EVERY node, and each tick the acting meter is ELECTED from live membership
    // (lowest healthy cryptographic iroh identity — see `Cluster::billing_leader`)
    // with automatic failover; no hardcoded privileged node. A 2-tick stability
    // window keeps a flapping health view from double-charging during transitions.
    // `HIVE_BILLING_COORDINATOR_NODE` remains as an explicit manual PIN override.
    spawn_billing_meter_loop(cloud.clone());

    // Managed World Queue delivery loop (hive-native Queue for the Vercel WDK
    // World interface -- no external queue dependency).
    tokio::spawn(crate::world_queue::run_delivery_loop(cloud.clone(), cloud.world_queue.clone()));

    // Vercel DNS reconciler (ngrok retirement): leader-elected loop publishing
    // healthy node IPs to api.{platform}/*.{apps} via the Vercel API. No-op in
    // ngrok ingress mode (unless HIVE_DNS_RECONCILE=1 forces it for testing).
    vercel_dns::spawn_reconciler(cloud.clone());

    // ACME (Let's Encrypt DNS-01 via Vercel): leader issues/renews the wildcard
    // bundles; every node syncs them from the replicated store and hot-swaps the
    // SNI resolver. No-ops entirely in ngrok ingress mode.
    acme::spawn_acme(cloud.clone());
    acme::spawn_cert_sync(cloud.clone());

    // Public gateway, wrapped in the edge pipeline.
    let public = fluid_gateway::public_router(gw.clone())
        .layer(axum::middleware::from_fn_with_state(cloud.clone(), edge::edge_pipeline));
    // Connection-level DoS bounds on the control plane (the admin router has no
    // streaming/SSE endpoints and deploys enqueue-then-return, so a bounded
    // per-request timeout and body cap are safe — unlike the public gateway,
    // which streams tenant responses and already caps request bodies at 16 MiB
    // + per-IP rate-limits in the edge pipeline). Env-tunable.
    let admin_max_body: usize = std::env::var("HIVE_ADMIN_MAX_BODY_MIB")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(64) * 1024 * 1024;
    let admin_req_timeout = Duration::from_secs(
        std::env::var("HIVE_ADMIN_REQUEST_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120),
    );
    let admin_router = admin::router(cloud.clone())
        .layer(axum::middleware::from_fn(auth::require_auth))
        .layer(axum::middleware::from_fn(admin::admin_rate_limit))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(admin_max_body))
        .layer(tower_http::timeout::TimeoutLayer::new(admin_req_timeout));
    if auth::enforced() {
        tracing::info!("JWT auth enforced on admin mutations (HIVE_JWT_SECRET set)");
    }

    // Host-based dispatch (real-DNS ingress): on the shared public listener,
    // `Host: api.{platform_domain}` routes to the ADMIN router (the platform
    // API), everything else — `*.{apps_domain}` etc. — to the deployment edge
    // pipeline. Only active when `HIVE_INGRESS != ngrok`; in ngrok mode the
    // public listener is byte-identical to today. Exposing the admin API on a
    // public host REQUIRES JWT enforcement — refuse to split otherwise.
    let public = if cloud.ingress != "ngrok" {
        if !auth::enforced() {
            tracing::error!(
                "HIVE_INGRESS={} requires HIVE_JWT_SECRET (the admin API becomes publicly addressable at api.{}); keeping single-router listener",
                cloud.ingress, cloud.platform_domain
            );
            public
        } else {
            let api_host = format!("api.{}", cloud.platform_domain);
            // Ops/admin console host — the operator surface, distinct from the
            // developer/API-key `api.` host (both currently reach the admin router).
            let admin_host = format!("admin.{}", cloud.platform_domain);
            // Dashboard hosts (apex + www): reverse-proxied to the dashboard
            // origin (`HIVE_DASHBOARD_UPSTREAM`, e.g. the ngrok origin until the
            // dashboard moves onto a node). Empty upstream = no dashboard hosts.
            let dash_upstream = std::env::var("HIVE_DASHBOARD_UPSTREAM").ok().map(|v| v.trim().trim_end_matches('/').to_string()).filter(|v| !v.is_empty());
            let dash_hosts = vec![cloud.platform_domain.clone(), format!("www.{}", cloud.platform_domain)];
            tracing::info!(%api_host, %admin_host, dashboard = ?dash_upstream, "host-based dispatch active (api/admin hosts → admin router; apex/www → dashboard proxy)");
            host_switch_router(cloud.clone(), api_host, admin_host, dash_hosts, dash_upstream, cloud.http.clone(), admin_router.clone(), public)
        }
    } else {
        public
    };

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

    // Per-tenant DB gateway (Neon/Upstash model): Postgres :5432 + Redis :6379
    // TLS-SNI proxies to each tenant DB's container. Spawns only when the gateway
    // is enabled (HIVE_DB_DOMAIN set); the wildcard `*.{db_domain}` cert comes from
    // the same ACME-managed SNI resolver. High ports (>1024) — no capability needed.
    db_gateway::spawn(cloud.clone());

    // Real-DNS ingress listeners (ngrok retirement): a public HTTPS listener with
    // the hot-swappable SNI resolver (wildcard apps cert + api cert; ACME-managed)
    // and a port-80 listener that only 301s to https. Only bound when
    // `HIVE_INGRESS != ngrok`. Low ports need CAP_NET_BIND_SERVICE (see RUNBOOK) —
    // bind failures are logged, never fatal, so a dev box still boots.
    if cloud.ingress != "ngrok" {
        let https_addr: SocketAddr = std::env::var("HIVE_HTTPS_LISTEN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "0.0.0.0:443".parse().unwrap());
        let http_addr: SocketAddr = std::env::var("HIVE_HTTP_LISTEN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "0.0.0.0:80".parse().unwrap());
        let https_router = public.clone();
        tokio::spawn(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let cfg = axum_server::tls_rustls::RustlsConfig::from_config(acme::server_config());
            tracing::info!(%https_addr, "public HTTPS listener (SNI resolver, ACME-managed certs)");
            if let Err(e) = axum_server::bind_rustls(https_addr, cfg)
                .serve(https_router.into_make_service_with_connect_info::<SocketAddr>())
                .await
            {
                tracing::error!(error = %e, %https_addr, "HTTPS listener failed (check CAP_NET_BIND_SERVICE / port availability)");
            }
        });
        // Port 80: redirect-only (no content ever served in cleartext).
        tokio::spawn(async move {
            let redirect = axum::Router::new().fallback(|req: axum::http::Request<axum::body::Body>| async move {
                let host = req
                    .headers()
                    .get(axum::http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
                    .or_else(|| req.uri().host().map(|h| h.to_string()))
                    .unwrap_or_default()
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let path_q = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
                axum::response::Redirect::permanent(&format!("https://{host}{path_q}"))
            });
            tracing::info!(%http_addr, "port-80 listener (301 → https only)");
            match tokio::net::TcpListener::bind(http_addr).await {
                Ok(l) => {
                    if let Err(e) = axum::serve(l, redirect).await {
                        tracing::error!(error = %e, "port-80 redirect listener failed");
                    }
                }
                Err(e) => tracing::error!(error = %e, %http_addr, "cannot bind port 80 (check CAP_NET_BIND_SERVICE)"),
            }
        });
    }

    tracing::info!(region=%region, node=%args.name, public=%args.listen, admin=%args.admin, tls=%tls_addr, "hive-cloud node up");

    let pub_srv = serve(public, args.listen, "public");
    let adm_srv = serve(admin_router, args.admin, "admin");
    tokio::try_join!(pub_srv, adm_srv)?;
    Ok(())
}

/// One listener, split by Host (real-DNS ingress): `api.{platform_domain}` (the
/// PLATFORM API + API-key surface) AND `admin.{platform_domain}` (the ops/admin
/// console surface) → the admin router; the dashboard hosts → the dashboard
/// proxy; anything else → the deployment edge pipeline. Implemented as a fallback
/// handler that oneshots into the matching inner router, so the x-hive-proxied
/// loop guard, WS upgrade path and everything else inside each router are
/// untouched. Host matching is case-insensitive and strips `:port`. api and admin
/// share one router today (same auth); the split is by HOSTNAME so `api.` reads
/// as the developer API and `admin.` as the operator console, and the two can
/// diverge (separate auth/route sets) without touching this dispatch.
fn host_switch_router(
    cloud: Arc<CloudState>,
    api_host: String,
    admin_host: String,
    dash_hosts: Vec<String>,
    dash_upstream: Option<String>,
    http: reqwest::Client,
    admin: axum::Router,
    public: axum::Router,
) -> axum::Router {
    use axum::{body::Body, http::Request};
    let handler = move |req: Request<Body>| {
        let cloud = cloud.clone();
        let admin = admin.clone();
        let public = public.clone();
        let api_host = api_host.clone();
        let admin_host = admin_host.clone();
        let dash_hosts = dash_hosts.clone();
        let dash_upstream = dash_upstream.clone();
        let http = http.clone();
        async move {
            // HTTP/2 carries the authority in the URI pseudo-header, NOT a Host
            // header — read both or h2 requests dispatch as host="" (the bug that
            // sent api/apex traffic into the public router).
            let host = req
                .headers()
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .or_else(|| req.uri().host().map(|h| h.to_string()))
                .unwrap_or_default()
                .split(':')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if host == api_host || host == admin_host {
                // Pass the MATCHED host so a leader-forward pins to the right SNI
                // (the platform cert covers both api. and admin.).
                return admin_ingress(cloud, admin, host, req).await;
            }
            // Dashboard hosts: reverse-proxy to the configured origin so
            // `shadw.cloud` replaces the ngrok dashboard URL immediately (the
            // origin stays an internal hop until the dashboard moves on-node).
            if let (true, Some(up)) = (dash_hosts.iter().any(|h| *h == host), dash_upstream.as_ref()) {
                return dashboard_proxy(&http, up, req).await;
            }
            match tower::ServiceExt::oneshot(public, req).await {
                Ok(resp) => resp,
                Err(never) => match never {},
            }
        }
    };
    axum::Router::new().fallback(handler)
}

/// Regional AdminAPI ingress for `api.{platform_domain}`. Runs on EVERY healthy
/// node — clients reach the nearest via health-aware DNS. It authenticates the
/// request, then serves locally IF this node is the control-plane leader, else
/// forwards to the leader over HTTPS (pinned to the leader's IP, SNI = api host).
/// A loop-guard header prevents re-forwarding. First-slice policy: after auth,
/// forward ALL requests (reads + writes) to the leader. Entirely dormant unless a
/// node runs with `HIVE_INGRESS!=ngrok` AND `HIVE_JWT_SECRET` set (see caller).
async fn admin_ingress(
    cloud: Arc<CloudState>,
    admin: axum::Router,
    api_host: String,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    async fn serve_local(admin: axum::Router, req: axum::http::Request<axum::body::Body>) -> axum::response::Response {
        match tower::ServiceExt::oneshot(admin, req).await {
            Ok(resp) => resp,
            Err(never) => match never {},
        }
    }
    // Loop guard + anti-forgery: a request carrying the internal forward marker is
    // served locally ONLY if we are (still) the control-plane leader. The marker
    // rides the same public `api` host, so a client can forge it — we therefore
    // never trust its mere presence to place a write on a non-leader. If leadership
    // changed mid-flight, or a client forged the marker toward a non-leader, refuse
    // mutations with 503 (the client retries and re-resolves to the current
    // leader); reads may serve locally best-effort. This closes both the spoof
    // (forced write on a non-leader) and the stale-forward split-brain, while still
    // terminating any forward chain (a forwarded request is never re-forwarded).
    if req.headers().contains_key("x-hive-admin-forwarded") {
        if cloud.is_control_plane_leader() {
            return serve_local(admin, req).await;
        }
        if matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH") {
            return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not control-plane leader").into_response();
        }
        return serve_local(admin, req).await;
    }
    // Auth-first: with enforcement on, reject a mutation lacking a valid token
    // BEFORE forwarding (fail fast). Reads pass (the leader re-verifies anyway).
    if crate::auth::enforced() {
        let is_mutation = matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
        let path = req.uri().path();
        let open = path == "/healthz" || path == "/v1/token" || path == "/v1/git/webhook";
        if is_mutation && !open {
            let ok = crate::auth::extract_token(req.headers()).and_then(|t| crate::auth::verify(&t).ok()).is_some();
            if !ok {
                return (axum::http::StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
            }
        }
    }
    // READS ARE FULLY DISTRIBUTED: every node serves GET/HEAD from its own
    // gossip-replicated state (eventually-consistent, last-writer-wins — the
    // same converged view the loopback admin always served). Forwarding reads
    // to the leader (the original first-slice policy) put a cross-ocean RTT in
    // front of EVERY dashboard fetch and made one node a global choke point —
    // one wedged/far leader read as "the platform is down" even though every
    // node held the data. Only MUTATIONS still serialize through the leader.
    //
    // `/v1/token` is exempt even though it's a POST: minting is pure HS256
    // signing with the fleet-shared secret — no state is written — so login
    // must never depend on leader reachability or distance.
    let is_mutation = matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    if !is_mutation || req.uri().path() == "/v1/token" {
        return serve_local(admin, req).await;
    }
    // Leader serves locally.
    if cloud.is_control_plane_leader() {
        return serve_local(admin, req).await;
    }
    // Forward to the leader. Build the SNI-pinned client up front: a leader with no
    // reachable IP — OR a malformed registry IP that won't parse to a socket addr —
    // must FAIL CLOSED (never fall back to plain DNS, which could send the write
    // anywhere). In that case avoid split-brain: serve reads locally (best effort),
    // refuse mutations with 503.
    let leader_ip = cloud.leader_node().and_then(|n| n.public_ip.clone().or(n.public_ip6.clone()));
    if let Some(ip) = leader_ip {
        if let Some(client) = leader_client(&ip, &api_host) {
            return admin_forward_to_leader(client, &api_host, req).await;
        }
    }
    if matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH") {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "control-plane leader unreachable").into_response()
    } else {
        serve_local(admin, req).await
    }
}

/// A no-redirect reqwest client that resolves `api_host` to a specific leader
/// IP:443 — so the forward deterministically hits the leader with a valid SNI +
/// cert (the wildcard/api bundle covers `api_host`). Cached per (ip, host).
/// Returns `None` when `ip` doesn't parse to an address or the client can't be
/// built, so the caller FAILS CLOSED rather than falling back to plain DNS (which
/// would resolve `api_host` to an arbitrary node and mis-route the forward).
fn leader_client(ip: &str, api_host: &str) -> Option<reqwest::Client> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, reqwest::Client>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = format!("{ip}|{api_host}");
    if let Some(c) = cache.lock().unwrap().get(&key) {
        return Some(c.clone());
    }
    let addr = std::net::SocketAddr::new(ip.parse::<std::net::IpAddr>().ok()?, 443);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(api_host, addr)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    cache.lock().unwrap().insert(key, client.clone());
    Some(client)
}

/// Forward an admin request to the leader over HTTPS (pinned to the leader IP,
/// SNI/Host = api host). Adds the loop-guard header; preserves method, path+query,
/// headers (incl. Authorization/Cookie), and body; streams the response back.
async fn admin_forward_to_leader(
    client: reqwest::Client,
    api_host: &str,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (axum::http::StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let path_q = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let url = format!("https://{api_host}{path_q}");
    let method = match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return (axum::http::StatusCode::METHOD_NOT_ALLOWED, "bad method").into_response(),
    };
    let mut rb = client.request(method, &url).header("x-hive-admin-forwarded", "1");
    for (k, v) in parts.headers.iter() {
        let n = k.as_str().to_ascii_lowercase();
        if matches!(n.as_str(), "host" | "content-length" | "connection") {
            continue;
        }
        rb = rb.header(k, v);
    }
    rb = rb.header(reqwest::header::HOST, api_host).body(body.to_vec());
    match rb.send().await {
        Ok(resp) => {
            let mut out = axum::http::Response::builder().status(resp.status().as_u16());
            for (k, v) in resp.headers().iter() {
                let n = k.as_str().to_ascii_lowercase();
                if matches!(n.as_str(), "connection" | "transfer-encoding" | "content-length") {
                    continue;
                }
                out = out.header(k.as_str(), v.as_bytes());
            }
            let bytes = resp.bytes().await.unwrap_or_default();
            out.body(axum::body::Body::from(bytes))
                .unwrap_or_else(|_| (axum::http::StatusCode::BAD_GATEWAY, "bad gateway").into_response())
        }
        Err(_) => (axum::http::StatusCode::BAD_GATEWAY, "control-plane leader forward failed").into_response(),
    }
}

/// Minimal streaming reverse proxy for the dashboard hosts. Forwards method,
/// path+query, headers (sans hop-by-hop + Host — the upstream needs ITS OWN
/// Host to route, e.g. an ngrok origin) and body; streams the response back.
///
/// REDIRECTS ARE NEVER FOLLOWED server-side (a dedicated no-redirect client):
/// auth flows (Clerk dev-browser handshake) must bounce the BROWSER, not the
/// proxy. 3xx Location values (and their percent-encoded forms inside query
/// strings) that reference the upstream origin are rewritten to the public
/// host, so the user stays on shadw.cloud through the whole auth loop.
async fn dashboard_proxy(
    http: &reqwest::Client,
    upstream: &str,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let _ = http; // proxying uses a dedicated NO-REDIRECT client (below)
    static NOFOLLOW: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let http = NOFOLLOW.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client")
    });
    let public_host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| req.uri().host().map(|h| h.to_string()))
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();
    let upstream_host = upstream.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string();
    let path_q = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let url = format!("{upstream}{path_q}");
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (axum::http::StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let method = match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return (axum::http::StatusCode::METHOD_NOT_ALLOWED, "bad method").into_response(),
    };
    let mut rb = http.request(method, &url).body(body.to_vec());
    for (k, v) in parts.headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(name.as_str(), "host" | "connection" | "transfer-encoding" | "content-length" | "upgrade" | "keep-alive" | "accept-encoding" | "x-forwarded-host" | "x-forwarded-proto") {
            continue;
        }
        rb = rb.header(k, v);
    }
    // Tell the app what the PUBLIC origin is (Next/Clerk build absolute URLs).
    rb = rb.header("x-forwarded-host", &public_host).header("x-forwarded-proto", "https");
    match rb.send().await {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16()).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let mut builder = axum::http::Response::builder().status(status);
            for (k, v) in resp.headers().iter() {
                let name = k.as_str().to_ascii_lowercase();
                if matches!(name.as_str(), "connection" | "transfer-encoding" | "content-length" | "content-encoding") {
                    continue;
                }
                // Keep the user on the PUBLIC host across auth bounces: rewrite
                // upstream-origin references in Location (raw + percent-encoded
                // inside query params like Clerk's redirect_url).
                if name == "location" {
                    if let Ok(loc) = v.to_str() {
                        let enc_up = upstream_host.replace(':', "%3A").replace('/', "%2F");
                        let enc_pub = public_host.replace(':', "%3A").replace('/', "%2F");
                        let rewritten = loc
                            .replace(&format!("//{upstream_host}"), &format!("//{public_host}"))
                            .replace(&enc_up, &enc_pub);
                        if let Ok(hv) = axum::http::HeaderValue::from_str(&rewritten) {
                            builder = builder.header(k, hv);
                            continue;
                        }
                    }
                }
                builder = builder.header(k, v);
            }
            let stream = resp.bytes_stream();
            builder
                .body(axum::body::Body::from_stream(stream))
                .map(|r| r.into_response())
                .unwrap_or_else(|_| (axum::http::StatusCode::BAD_GATEWAY, "proxy build failed").into_response())
        }
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, format!("dashboard origin unreachable: {e}")).into_response(),
    }
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
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
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
        let r = router.clone().into_make_service_with_connect_info::<SocketAddr>();
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
                // Project-level kill switch: the schedule still advances (jobs
                // keep being created/updated/deleted on deploy, matching
                // Vercel's semantics), but a disabled project's jobs don't
                // actually fire.
                if !cloud.projects.cron_enabled(&job.deployment) {
                    continue;
                }
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

/// Loop-local partials returned by one peer's gossip sync, merged after the
/// concurrent `join_all` (the direct cloud.* store writes already happened inside
/// the task via internally-synchronized stores).
#[derive(Default)]
struct PeerSync {
    /// (host, route) pairs learned from this peer's serve-hosts.
    routes: Vec<(String, crate::state::PeerRoute)>,
    /// This peer's fleet deployments (node_id, list) for the dashboard view.
    fleet: Option<(String, Vec<fluid_core::DeploymentInfo>)>,
    /// (container key, holder node_id) pairs for lease election.
    holders: Vec<(String, String)>,
    /// The peer's node id, if it was reached this round (drives the route TTL merge).
    seen: Option<String>,
}

/// Sync ONE peer: announce ourselves, learn its nodes/routes/deployments, and
/// converge zkauth/enterprise/lease state. All cloud.* writes go through
/// internally-synchronized stores so many of these run concurrently safely;
/// loop-local data is returned as a [`PeerSync`] to merge after `join_all`.
async fn sync_one_peer(cloud: Arc<CloudState>, peer: String, me_bytes: Vec<u8>) -> PeerSync {
    let mut out = PeerSync::default();
    let _ = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_POST, "/v1/nodes/announce", &me_bytes).await;
    let t0 = now_ms();
    let mut rtt = 0u64;
    let mut nodes_bytes = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/nodes", &[]).await;
    // MESH HOT-JOIN (client side): a gossip failure to this target may mean we are
    // not yet in ITS trust set (first contact of a brand-new node, or a wiped trust
    // roster). When the fleet secret is configured, present a join proof —
    // HMAC(secret, OUR endpoint id) — over the dedicated join stream; on admission
    // the reply is the peer's full node roster, consumed exactly like /v1/nodes.
    // The dial is by KEY (the eid/seed mapping in peer_iroh); no IP involved.
    if nodes_bytes.is_none() {
        if let Ok(secret) = std::env::var("HIVE_JWT_SECRET") {
            if !secret.trim().is_empty() {
                let me_id = cloud.registry.me().peer_id;
                let target = {
                    // fetch() may have evicted the mapping on failure; fall back to
                    // re-deriving it (seed keys/eids ARE the identity).
                    let pi = cloud.peer_iroh.read();
                    pi.get(&peer).cloned()
                }
                .or_else(|| {
                    let k = peer.strip_prefix("seed:").unwrap_or(&peer);
                    (k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit()))
                        .then(|| (k.to_string(), format!("{{\"id\":\"{k}\",\"addrs\":[]}}")))
                });
                let pool = cloud.mesh.read().clone();
                if let (Some(me_id), Some((node_id, addr)), Some(pool)) = (me_id, target, pool) {
                    let proof = crate::admin::hmac_sha256_hex(secret.as_bytes(), me_id.as_bytes());
                    let attempt = tokio::time::timeout(
                        Duration::from_secs(5),
                        pool.join_request(&node_id, &addr, &me_bytes, &proof),
                    )
                    .await;
                    if let Ok(Ok(bytes)) = attempt {
                        if !bytes.is_empty() {
                            tracing::info!(peer = %node_id, "mesh join accepted — roster received");
                            // Restore the transport mapping fetch() evicted.
                            cloud.peer_iroh.write().entry(peer.clone()).or_insert((node_id, addr));
                            nodes_bytes = Some(bytes);
                        }
                    }
                }
            }
        }
    }
    if let Some(bytes) = nodes_bytes {
        rtt = now_ms().saturating_sub(t0);
        if let Ok(nodes) = serde_json::from_slice::<Vec<NodeInfo>>(&bytes) {
            let peer_self = nodes.first().cloned();
            if let Some(ps) = &peer_self {
                if let Some(addr) = ps.iroh_addr.clone() {
                    cloud.peer_iroh.write().insert(peer.clone(), (ps.id.clone(), addr));
                }
            }
            let peer_self_id = peer_self.map(|n| n.id);
            for n in nodes {
                if n.id != cloud.node_name {
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
                if !peer.starts_with("seed:") {
                    cloud.node_admins.write().insert(pid, peer.clone());
                }
                cloud.mark_gossip_ok();
            }
        }
    } else {
        let id = cloud
            .peer_iroh
            .read()
            .get(&peer)
            .map(|(id, _)| id.clone())
            .or_else(|| {
                // Key-addressed targets: the target string IS the endpoint id (or a
                // seed:<id>), so a failed round still marks health even after
                // fetch() evicted the transport mapping.
                let k = peer.strip_prefix("seed:").unwrap_or(&peer);
                (k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit())).then(|| k.to_string())
            });
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            cloud.registry.set_health(&id, 0, false);
        }
    }
    if let Some(bytes) = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/serve-hosts", &[]).await {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let node_id = v.get("node").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let region = v.get("region").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let gateway = v.get("gateway").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if !gateway.is_empty() && node_id != cloud.node_name {
                out.seen = Some(node_id.clone());
                if let Some(hosts) = v.get("hosts").and_then(|x| x.as_array()) {
                    for h in hosts.iter().filter_map(|x| x.as_str()) {
                        out.routes.push((
                            h.to_string(),
                            crate::state::PeerRoute {
                                node_id: node_id.clone(),
                                region: region.clone(),
                                gateway: gateway.clone(),
                                latency_ms: rtt,
                                healthy: true,
                                last_seen_ms: now_ms(),
                            },
                        ));
                    }
                }
                if let Some(cs) = v.get("containers").and_then(|x| x.as_array()) {
                    for k in cs.iter().filter_map(|x| x.as_str()) {
                        out.holders.push((k.to_string(), node_id.clone()));
                    }
                }
            }
        }
    }
    if let Some(bytes) = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/fleet-deployments", &[]).await {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let node_id = v.get("node").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if !node_id.is_empty() && node_id != cloud.node_name {
                if let Some(deps) = v.get("deployments") {
                    if let Ok(list) = serde_json::from_value::<Vec<fluid_core::DeploymentInfo>>(deps.clone()) {
                        out.fleet = Some((node_id, list));
                    }
                }
            }
        }
    }
    #[cfg(feature = "zkauth")]
    if let Some(bytes) = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/zkauth/roster-export", &[]).await {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            crate::zkauth::ingest_peer_export(&v);
        }
    }
    if let Some(bytes) = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/enterprise/edge-export", &[]).await {
        if let Ok(exp) = serde_json::from_slice::<crate::enterprise::EdgeExport>(&bytes) {
            cloud.enterprise.ingest_peer_edge(&peer, exp);
        }
    }
    if let Some(bytes) = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/leases", &[]).await {
        if let Ok(leases) = serde_json::from_slice::<Vec<crate::lease::ContainerLease>>(&bytes) {
            for l in leases {
                cloud.leases.merge(l);
            }
        }
    }
    out
}

fn spawn_gossip_loop(cloud: Arc<CloudState>, peers: Vec<String>, seeds: Vec<(String, String, String)>) {
    use std::collections::HashMap;
    // STATIC gossip targets = the configured --peer URLs (warm path, via persisted
    // peer_iroh or HTTP) PLUS the bootstrap seed keys (always-available iroh
    // rendezvous). The seed keys carry the cold/wiped node until its warm
    // peer_iroh is repopulated. DYNAMIC targets (hot-join) are added per-round
    // below from the registry + persisted key-addressed roster.
    let mut targets = peers.clone();
    for (key, _, _) in &seeds {
        if !targets.contains(key) {
            targets.push(key.clone());
        }
    }
    tokio::spawn(async move {
        // Content hash of the last roster replicated into GuardianDB, so the
        // (5s-cadence) loop only writes the replicated doc when it CHANGES.
        let mut roster_hash: u64 = 0;
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
            // DYNAMIC target set (hot-join, key-addressed): every round, dial the
            // union of the static targets, every REGISTRY node with an iroh addr
            // (learned transitively from any peer's /v1/nodes, or via an inbound
            // join/announce), and every persisted key-addressed roster entry.
            // Dedup by ENDPOINT ID; peers are dialed by KEY (iroh resolves the
            // address via discovery/relay/holepunch — never by IP). This is what
            // makes a new node visible fleet-wide with ZERO restarts: it joins one
            // seed, the seed's /v1/nodes carries it everywhere, and every node's
            // next round dials it first-hand.
            let targets: Vec<String> = {
                let mut round: Vec<String> = targets.clone();
                let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
                if let Some(me) = cloud.registry.me().peer_id.clone() {
                    covered.insert(me);
                }
                {
                    let pi = cloud.peer_iroh.read();
                    for t in &round {
                        if let Some((nid, _)) = pi.get(t) {
                            covered.insert(nid.clone());
                        }
                    }
                }
                // Registry-derived (freshest addr wins — a rejoined node's new addr
                // must replace a stale persisted one).
                let mut adds: Vec<(String, String)> = Vec::new();
                for n in cloud.registry.nodes() {
                    if n.is_self {
                        continue;
                    }
                    let Some(addr) = n.iroh_addr else { continue };
                    let Some(eid) = hive_p2p::endpoint_id_from_addr_json(&addr) else { continue };
                    if covered.insert(eid.clone()) {
                        adds.push((eid, addr));
                    }
                }
                {
                    let mut pi = cloud.peer_iroh.write();
                    for (eid, addr) in &adds {
                        pi.insert(eid.clone(), (eid.clone(), addr.clone()));
                        round.push(eid.clone());
                    }
                }
                // Persisted roster continuity: eid-keyed entries not covered above
                // (e.g. right after a restart, before the registry re-converges).
                {
                    let pi = cloud.peer_iroh.read();
                    for (k, (nid, _)) in pi.iter() {
                        if k.len() == 64
                            && k.chars().all(|c| c.is_ascii_hexdigit())
                            && covered.insert(nid.clone())
                        {
                            round.push(k.clone());
                        }
                    }
                }
                round.truncate(64); // bound the per-round dial fan-out
                round
            };
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
            // Announce + learn each peer's view CONCURRENTLY. The per-peer cloud.* writes
            // (registry, peer_iroh, node_admins, trusted ids, enterprise, leases, zkauth)
            // all go through internally-synchronized stores, so they're race-free across
            // tasks; only the loop-local maps are merged from the returned partials. This
            // overlaps the network waits so one slow peer no longer serializes the rest.
            let me = cloud.registry.me();
            let me_bytes = serde_json::to_vec(&me).unwrap_or_default();
            let partials = futures::future::join_all(
                targets.iter().map(|peer| sync_one_peer(cloud.clone(), peer.clone(), me_bytes.clone())),
            )
            .await;
            for pr in partials {
                if let Some(n) = pr.seen { seen_nodes.insert(n); }
                for (h, route) in pr.routes { routes.entry(h).or_default().push(route); }
                if let Some((nid, list)) = pr.fleet { fleet.insert(nid, list); }
                for (k, nid) in pr.holders { holders.entry(k).or_default().push(nid); }
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
            // Roster hygiene: bound unbounded growth over a long uptime. Keep every
            // entry the registry still vouches for (its own 30s staleness already
            // prunes dead nodes) PLUS the static config-derived targets (CLI peers /
            // bootstrap seeds — these must survive even while their node is briefly
            // down, unlike a transitively-learned entry); drop everything else once
            // the map exceeds the cap, oldest-looking (registry-unknown) first. A
            // dead/unreachable roster entry can never wedge the loop regardless (H4
            // per-phase iroh timeouts bound every individual dial).
            const ROSTER_CAP: usize = 256;
            {
                let mut pi = cloud.peer_iroh.write();
                if pi.len() > ROSTER_CAP {
                    let keep: std::collections::HashSet<String> = cloud
                        .registry
                        .nodes()
                        .into_iter()
                        .filter_map(|n| n.iroh_addr.as_deref().and_then(hive_p2p::endpoint_id_from_addr_json))
                        .chain(targets.iter().cloned())
                        .collect();
                    pi.retain(|k, (nid, _)| keep.contains(k) || keep.contains(nid));
                }
            }
            // Persist the gossip-transport map so the next restart bootstraps iroh
            // gossip from disk (no SSH tunnel needed for rendezvous). This map IS
            // the mesh roster: key-addressed (endpoint ids), never IPs.
            let roster_json = {
                let pi = cloud.peer_iroh.read();
                crate::persist::save_peer_iroh(&pi);
                serde_json::to_vec(&*pi).unwrap_or_default()
            };
            // Replicate the roster through GuardianDB (iroh-docs) so a node whose
            // local file is lost can re-adopt it from the replicated store. Only
            // written when the content actually changes (rosters are stable).
            let h = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                roster_json.hash(&mut hasher);
                hasher.finish()
            };
            if h != roster_hash && !roster_json.is_empty() {
                roster_hash = h;
                tokio::spawn(async move { crate::guardian::put("mesh/roster", roster_json).await });
            }
            // Seed GuardianDB's OWN iroh client with every currently-known peer
            // address from the main mesh's gossip-derived registry (re-asserted
            // every round, same rationale as the bootstrap-seed re-assertion
            // above): GuardianDB's separate iroh client otherwise depends
            // entirely on n0's public discovery to ever learn a peer's address,
            // which this platform's cloud hosts can't reliably use (confirmed:
            // frozen, non-converging per-node key counts; a permanently-hung
            // live read). Best-effort, spawned so a slow/failed seed round never
            // blocks the gossip loop's own cadence.
            let guardian_addrs: Vec<String> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self)
                .filter_map(|n| n.iroh_addr)
                .collect();
            if !guardian_addrs.is_empty() {
                tokio::spawn(async move { crate::guardian::seed_known_peers(&guardian_addrs).await });
            }
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

/// Eager full-mesh trunking (proactive). Keeps a live iroh QUIC trunk to EVERY
/// healthy peer — not just the ones this node directly gossips — so a cross-node
/// request reuses a warm trunk instead of paying a cold dial/holepunch on the
/// critical path (the dial cost moves here, off-request). Runs a hair under the 15s
/// QUIC keepalive so an established trunk never lapses between passes; a missing or
/// dead one (peer just restarted) is re-dialed within a tick. Warms in PARALLEL so
/// one slow holepunch can't serialize the rest, and `warm`'s connect is H4-bounded
/// so a dead peer can't wedge the loop. Config: `HIVE_TRUNK_WARM_INTERVAL` (s, def 10).
fn spawn_trunk_warmer(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_TRUNK_WARM_INTERVAL", 10));
    tracing::info!(?interval, "eager mesh trunk warmer");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let pool = match cloud.mesh.read().clone() {
                Some(p) => p,
                None => continue, // iroh transport not bound yet
            };
            // Every healthy peer with a known iroh address → ensure a live trunk.
            let peers: Vec<(String, String)> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self && n.healthy)
                .filter_map(|n| n.iroh_addr.map(|a| (n.id, a)))
                .collect();
            if peers.is_empty() {
                continue;
            }
            let mut handles = Vec::with_capacity(peers.len());
            for (id, addr) in peers {
                let pool = pool.clone();
                handles.push(tokio::spawn(async move { pool.warm(&id, &addr).await }));
            }
            let mut warmed = 0usize;
            for h in handles {
                if matches!(h.await, Ok(true)) {
                    warmed += 1;
                }
            }
            let trunks = pool.trunk_count().await;
            tracing::debug!(warmed, trunks, "trunk warmer pass");
        }
    });
}

/// Billing meter loop — the metering→billing pipeline. On a fixed interval it pulls
/// the fleet-wide per-function usage stats (local + every peer over the mesh),
/// aggregates them per tenant, and feeds the cumulative totals to the billing store,
/// which charges only the DELTA since the last reading (delta·rate-card → ledger →
/// invoice). Idempotent-ish: counter resets (pool recycle / node restart) are handled
/// by the store's meter. Runs mock or real — Stripe only affects top-up checkout, not
/// metering. Config: `HIVE_BILLING_METER_INTERVAL` (s, def 60).
fn spawn_billing_meter_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_BILLING_METER_INTERVAL", 60));
    tracing::info!(?interval, "billing meter loop (usage → charges → invoices; leader-elected)");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Election state: how many consecutive ticks THIS node has been the elected
        // meter, and whether it acted last tick (for transition logging).
        let mut leader_ticks: u32 = 0;
        let mut was_acting = false;
        let manual_pin = std::env::var("HIVE_BILLING_COORDINATOR_NODE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        loop {
            tick.tick().await;
            // Who should meter this tick? Manual pin wins when set; otherwise the
            // ELECTED leader (lowest healthy cryptographic identity in the mesh).
            let elected = match &manual_pin {
                Some(pin) => Some(pin.clone()),
                None => crate::cluster::Cluster::billing_leader(&cloud.registry.nodes()),
            };
            let am_leader = elected.as_deref() == Some(cloud.node_name.as_str());
            leader_ticks = if am_leader { leader_ticks.saturating_add(1) } else { 0 };
            // Stability window: act only after 2 consecutive leader ticks, so two
            // nodes with briefly divergent health views can't both charge a delta.
            let acting = am_leader && leader_ticks >= 2;
            if acting != was_acting {
                tracing::info!(
                    elected = elected.as_deref().unwrap_or("none"),
                    acting,
                    "billing meter leadership changed (elected coordinator, auto-failover)"
                );
                was_acting = acting;
            }
            if !acting {
                continue;
            }
            let stats = admin::fleet_function_stats(&cloud).await;
            if stats.is_empty() {
                continue;
            }
            // Aggregate cumulative usage per tenant across the whole fleet.
            let mut totals: std::collections::HashMap<String, billing::UsageTotals> =
                std::collections::HashMap::new();
            for s in &stats {
                let t = totals.entry(s.tenant.clone()).or_default();
                t.active_cpu_ms = t.active_cpu_ms.saturating_add(s.active_cpu_ms);
                t.mem_gb_hr_milli =
                    t.mem_gb_hr_milli.saturating_add((s.memory_gb_hrs * 1000.0) as u64);
                t.requests = t.requests.saturating_add(s.requests);
            }
            let mut charged_any = 0u64;
            for (tenant, tot) in totals {
                charged_any += cloud.billing.meter_usage(&tenant, tot);
            }
            if charged_any > 0 {
                tracing::debug!(cents = charged_any, "billing meter charged usage");
            }
        }
    });
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
