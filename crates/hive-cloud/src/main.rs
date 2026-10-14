//! `hive-cloud`: one node of the Hive cloud. A single binary runs, behind one public gateway and
//! one admin API, the control plane (builds, sandboxes), Fluid compute serving, the edge pipeline
//! (WAF, bot management, CDN), cron, workflows, previews and a region-aware node registry that
//! meshes with its peers.

mod acme;
mod admin;
mod apikeys;
mod app_discovery;
mod audit;
mod auth;
mod billing;
mod bounded_round;
mod browser_admission;
mod browser_artifacts;
mod browser_esm;
mod browser_db;
mod browser_db_rest;
mod browser_metering;
mod browser_presence;
mod build_coordinates;
mod build_executor;
mod build_registry;
mod cloudflare_dns;
mod project_inventory;
mod cluster;
mod coi;
mod compose;
mod databases;
mod db_gateway;
mod db_replicate;
mod db_rest;
mod dedicated_ipv4_listener;
mod deployment_ledger;
mod dht_probe;
mod discovery;
mod dns;
mod dns_geo;
mod dns_probe;
mod dnsserver;
mod docstore;
mod drive_api;
mod drive_blobs;
mod drive_webdav;
mod edge;
mod enterprise;
mod enterprise_api;
mod event_log;
mod geoip;
mod git;
mod github_app_auth;
mod gitops;
mod gossip;
mod gossip_round;
mod gpu_pool;
mod guardian;
mod health;
mod hrana;
mod hrana_proto;
mod musql;
mod musql_migrate;
mod sqlite_engine;
mod identity;
mod incidents;
mod runtime_watch;
mod bulkhead;
mod inference;
mod integrations;
mod integrity_signer;
mod leadership;
mod lease;
mod listener_audit;
mod memwatch;
mod mesh_raw;
mod mesh_shell;
mod meshwatch;
mod metrics;
mod microfrontends;
mod microfrontends_api;
mod notifications;
mod persist;
mod prewarm;
mod production_deployments;
mod project_settings;
mod push;
mod queues;
mod queues_api;
mod raw_ports;
mod raw_proxy;
mod relational;
mod repository_build;
mod resources;
mod resp;
mod resp_cache;
mod restart_audit;
mod retry;
mod runtime_artifact_transfer;
mod runtime_artifact_transfer_fs;
mod runtime_artifact_transfer_sender;
mod runtime_artifact_transfer_service;
mod runtime_artifact_transfer_store;
mod runtime_artifact_transfer_wire;
mod sandboxes;
mod sandboxes_api;
mod sandboxes_platform;
mod schedule;
mod secrets;
mod securelink;
mod sqlite_pool;
mod state;
mod storage_api;
mod storage_broker;
mod store_follower;
mod store_sync;
mod supervise;
mod svcgraph;
mod teams;
mod tenancy_reconcile;
mod tencent_eip;
mod udp_relay;
mod vercel_dns;
mod webhooks;
mod workspace;
mod world;
mod world_queue;
#[cfg(feature = "zkauth")]
mod zkauth;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use fluid_compute::{Fluid, FluidConfig};
use fluid_gateway::Gateway;
use hive_backend::firecracker::{FirecrackerBackend, FirecrackerConfig};
use hive_backend::litebox::LiteboxBackend;
use hive_backend::mock::{MockBackend, MockConfig};
use hive_backend::CellBackend;
use hive_controlplane::{BoxConfig, Hive, HiveConfig};
use hive_core::now_ms;
use hive_edge::{
    workflows::WorkflowStep, BotManager, CdnCache, ConcurrencyLimiter, CronScheduler, NodeInfo,
    NodeRegistry, Plan, Router, Waf, WorkflowEngine,
};

use state::CloudState;

/// The embedded relay's `AccessControl`: denylist-shaped, so it denies only the (handshake-PROVEN)
/// endpoint ids that this node's `browser_admissions` store has revoked and allows everyone else,
/// every fleet node relaying through the same server included. Routing-layer revocation only stops
/// NEW invocations from picking a revoked target; this is what keeps a revoked browser endpoint
/// from reconnecting and sitting on the relay. `cloud` is a deferred cell because the relay starts
/// before `CloudState` exists; it holds a `Weak` so this impl never keeps `CloudState` alive, and
/// while the cell is unfilled or already dropped the relay fails OPEN.
#[derive(Debug)]
struct BrowserRelayAccess {
    cloud: Arc<std::sync::OnceLock<std::sync::Weak<CloudState>>>,
}

impl iroh_relay::server::AccessControl for BrowserRelayAccess {
    async fn on_connect(
        &self,
        request: &iroh_relay::server::ClientRequest,
    ) -> iroh_relay::server::Access {
        let Some(cloud) = self.cloud.get().and_then(|w| w.upgrade()) else {
            return iroh_relay::server::Access::Allow;
        };
        let endpoint_id = request.endpoint_id().to_string();
        if cloud
            .browser_admissions
            .is_denied(&endpoint_id, hive_core::now_ms())
        {
            return iroh_relay::server::Access::Deny {
                reason: Some("browser admission revoked".to_string()),
            };
        }
        iroh_relay::server::Access::Allow
    }
}

/// Linux builds run on jemalloc with the profiler compiled in but switched off (`malloc_conf`
/// below), so a heap profile can be taken from a live node (`admin::heap_profile` turns sampling
/// on) with no rebuild or restart; the steady-state cost is jemalloc versus system malloc and
/// nothing more.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// `prof:true` compiles the profiler in and `prof_active:false` keeps it off (no sampling, no
/// per-allocation bookkeeping); `lg_prof_sample:19` samples about every 512 KiB, the usual
/// production-safe rate.
#[cfg(target_os = "linux")]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:false,lg_prof_sample:19\0";

/// The public HTTPS listener's `axum_server::Handle`, set once when the listener starts so the
/// SIGTERM handler can drain it. Unset under ngrok ingress, which has no listener here; the
/// shutdown path treats that as "nothing to drain", not an error.
static SHUTDOWN_HTTPS_HANDLE: std::sync::OnceLock<axum_server::Handle> = std::sync::OnceLock::new();

#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum RestartReason {
    MemoryPressure = 1,
    MeshIsolation = 2,
    MeshDegradation = 3,
    /// meshwatch's `establishment_wedge` trigger: the endpoint stopped forming new connections in
    /// either direction while warm trunks kept the fleet audible.
    EstablishmentWedge = 4,
}

impl RestartReason {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::MemoryPressure),
            2 => Some(Self::MeshIsolation),
            3 => Some(Self::MeshDegradation),
            4 => Some(Self::EstablishmentWedge),
            _ => None,
        }
    }

    /// The reason's name in `restart_history.json` (`RestartRecord::reason`)
    /// — what per-reason restart rate caps count.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MemoryPressure => "memory_pressure",
            Self::MeshIsolation => "mesh_isolation",
            Self::MeshDegradation => "mesh_degradation",
            Self::EstablishmentWedge => "establishment_wedge",
        }
    }

    /// The per-reason restart policy: at most `cap` restarts inside `window_ms`, counted from
    /// `restart_history.json` across boots ([`ControlledRestart::request`] persists the reason
    /// before the graceful tail starts); `None` = uncapped. Memory pressure is uncapped because the
    /// kernel ends that process anyway; the isolation and degradation triggers re-arm only after
    /// the NEW process converged (or saw a peer, or ran its boot budget), so only the establishment
    /// trigger needs the cap as its loop guard.
    fn rate_cap(self) -> Option<(usize, u64)> {
        match self {
            Self::MemoryPressure | Self::MeshIsolation | Self::MeshDegradation => None,
            Self::EstablishmentWedge => Some((1, 6 * 60 * 60 * 1000)),
        }
    }

    /// Whether an automatic restart for this reason may happen NOW; `Err` names the first shared
    /// gate that holds it back. [`ControlledRestart::request`] is the one chokepoint, so no current
    /// or future trigger can skip them: the cell-orphan interlock
    /// (`hive_backend::orphan_reap_ran()`, false on macOS and under `HIVE_CELL_ORPHAN_REAP=0`, so
    /// those nodes only WARN; exempt for memory pressure, which ends the process anyway), then the
    /// reason's [`rate_cap`](Self::rate_cap), which fails CLOSED when the persisted history cannot
    /// be trusted.
    pub(crate) fn admissible(self) -> Result<(), String> {
        if !matches!(self, Self::MemoryPressure) && !hive_backend::orphan_reap_ran() {
            return Err(
                "interlock: the cell-orphan reaper has not confirmed this boot clean".into(),
            );
        }
        if let Some((cap, window_ms)) = self.rate_cap() {
            match restart_audit::controlled_restarts_within(self.as_str(), window_ms) {
                Ok(n) if n < cap => {}
                Ok(n) => {
                    return Err(format!(
                        "rate cap: {n} restart(s) for this reason within the last {} h",
                        window_ms / 3_600_000
                    ))
                }
                Err(why) => return Err(format!("rate cap cannot be verified ({why})")),
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ControlledRestart {
    inner: Arc<ControlledRestartInner>,
}

struct ControlledRestartInner {
    reason: std::sync::atomic::AtomicU8,
    notify: tokio::sync::Notify,
}

impl ControlledRestart {
    const SEALED: u8 = 1 << 7;
    const REASON_MASK: u8 = !Self::SEALED;

    fn new() -> Self {
        Self {
            inner: Arc::new(ControlledRestartInner {
                reason: std::sync::atomic::AtomicU8::new(0),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Request the controlled exit-17 for `reason`; `false` when [`RestartReason::admissible`]
    /// refuses (WARN at most once per 5 min per reason) or another reason already won the latch,
    /// and the caller keeps watching.
    pub(crate) fn request(&self, reason: RestartReason) -> bool {
        if let Err(blocker) = reason.admissible() {
            use std::sync::atomic::AtomicU64;
            static LAST_WARN_MS: [AtomicU64; 5] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];
            let last = &LAST_WARN_MS[reason as usize];
            let now = hive_core::now_ms();
            if now.saturating_sub(last.load(std::sync::atomic::Ordering::Relaxed)) >= 300_000 {
                last.store(now, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    reason = reason.as_str(),
                    held_by = %blocker,
                    "controlled restart REFUSED: a restart trigger fired but a shared gate holds \
                     it back; the node keeps running and the trigger keeps watching"
                );
            }
            return false;
        }
        if self
            .inner
            .reason
            .compare_exchange(
                0,
                reason as u8,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        restart_audit::note_controlled_restart(reason.as_str());
        self.inner.notify.notify_one();
        true
    }

    fn reason(&self) -> Option<RestartReason> {
        let state = self.inner.reason.load(std::sync::atomic::Ordering::Acquire);
        RestartReason::from_u8(state & Self::REASON_MASK)
    }

    /// Atomically closes the request latch and consumes the winning reason: a watchdog racing this
    /// boundary either wins before the seal and changes the exit code, or sees the sealed state and
    /// gets `false`, never a successful request after main committed to exit zero.
    fn seal(&self) -> Option<RestartReason> {
        let state = self
            .inner
            .reason
            .fetch_or(Self::SEALED, std::sync::atomic::Ordering::AcqRel);
        RestartReason::from_u8(state & Self::REASON_MASK)
    }

    async fn wait(&self) -> RestartReason {
        loop {
            if let Some(reason) = self.reason() {
                return reason;
            }
            self.inner.notify.notified().await;
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "hive-cloud",
    about = "A unified cloud node (builds + serving + edge + cron + workflows)"
)]
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
    /// Port for this node's OWN embedded iroh-relay server (HTTP relay + NAT
    /// fallback for the mesh). Default 3341 — one above the standalone `iroh-relay`
    /// binaries' :3340 on bkk/va/sj, so an embedded listener never collides with a
    /// standalone one still running on the same host during the transition.
    /// `HIVE_OWN_RELAY_PORT` overrides this at runtime.
    #[arg(long, default_value_t = 3341)]
    relay_port: u16,
    /// Operator diagnostic: PROVE that the given addresses serve the delegated
    /// zone as authoritative nameservers, from THIS host, then exit without
    /// starting a node. Runs the exact code the prover loop runs
    /// (`dns_probe::probe_nameserver`) — asking the question with a second
    /// implementation is how the diagnostic and the decision quietly diverge.
    /// Zone from `HIVE_DEPLOY_ZONE`; `HIVE_DNS_PROBE_SUBNETS` (comma-separated
    /// CIDRs) adds client subnets to ask on behalf of. Non-zero exit if any
    /// target fails, so it composes into a shell check.
    #[arg(long = "dns-probe", value_delimiter = ',')]
    dns_probe: Vec<String>,
    /// Operator diagnostic (same family as `--dns-probe`): resolve the given
    /// 64-hex iroh endpoint id(s) through the PUBLIC mainline DHT only, from
    /// this host, then exit without starting a node. Nothing else is in the
    /// path — no bootstrap seeds, no Seer pkarr relay, no cached
    /// `peer_iroh.json` — so a hit is proof the target's pkarr record is live
    /// on the DHT and this host's egress can read it, and a "the mesh
    /// converged anyway" explanation is structurally unavailable. Budget from
    /// `HIVE_DHT_PROBE_TIMEOUT_MS` (default 30s, retried — a cold routing
    /// table legitimately misses the first attempts). Non-zero exit on a miss.
    #[arg(long = "dht-probe", value_delimiter = ',')]
    dht_probe: Vec<String>,
    /// Operator diagnostic (same family as `--dns-probe`): run one Hrana
    /// pipeline through BOTH managed-SQLite engines on the same data —
    /// rusqlite first, then a `musql-convert`ed copy through the musqld
    /// sidecar — and print both envelopes plus whether they are identical.
    /// This is how "the engine swap is invisible on the wire" gets measured
    /// instead of asserted. Needs `HIVE_SQLITE_ENGINE=musql`,
    /// `HIVE_MUSQLD_BIN`, `HIVE_MUSQL_CONVERT_BIN` and a scratch `HIVE_DATA`;
    /// touches nothing else. Non-zero exit if the envelopes differ.
    #[arg(long = "sqlite-engine-probe")]
    sqlite_engine_probe: bool,
    /// Operator diagnostic (same family as `--dns-probe`/`--dht-probe`): parse a
    /// compose file through the REAL `compose::parse_compose` the build pipeline
    /// uses and print exactly what the platform will run and route — every
    /// service, its full port list, which single port `/` reaches, the resolved
    /// `command`/`entrypoint` argv, and which ports get no public ingress.
    ///
    /// This exists because the failure it diagnoses was undiagnosable: a compose
    /// service publishing several ports had every port after the first discarded
    /// in silence, so a MinIO console on `:9001` simply did not exist as far as
    /// the platform was concerned and the only symptom available to the user was
    /// a closed connection. Reads a file; binds nothing, joins no mesh, needs no
    /// node. Non-zero exit on a parse error.
    #[arg(long = "compose-probe")]
    compose_probe: Option<std::path::PathBuf>,
    /// Operator diagnostic (same family as `--dns-probe`): seed the ACME
    /// DNS-01 challenge store at boot with `<fqdn>=<txt value>` — through the
    /// SAME `AcmeChallengeStore::insert` the real issuance path calls — then
    /// serve normally, so `dig TXT <fqdn>` against `HIVE_DNS_ADDR` proves the
    /// Seer challenge-answer path end-to-end without burning a real ACME
    /// order. The seed ages out on the store's own TTL like any challenge.
    #[arg(long = "acme-txt-selftest")]
    acme_txt_selftest: Option<String>,
    /// Operator diagnostic: run Litebox's Tier-2 functional smoke test
    /// (`LiteboxBackend::smoke_test` — TWO real checks: the syscall
    /// rewriter, then a full per-cell-TUN + patched-litebox + bind-shim
    /// network round trip) then exit without starting a node. BRING-UP
    /// ONLY: never run this against a node already carrying live traffic —
    /// it creates a real (throwaway) TUN device, mirrors
    /// `pvm_run_smoke_test`'s gating (AGENTS.md "PVM kernels"). A pass on
    /// BOTH checks is what licenses an operator to set
    /// `HIVE_LITEBOX_VERIFIED=1` on this host; this flag never sets it
    /// itself. Non-zero exit on failure, so it composes into a shell check.
    #[arg(long = "litebox-probe")]
    litebox_probe: bool,
    /// Debug-build-only destructive diagnostic against HIVE_DATA. Intended only
    /// for disposable stores; release binaries do not expose this flag.
    #[cfg(debug_assertions)]
    #[arg(long = "guardian-lifecycle-diagnostic")]
    guardian_lifecycle_diagnostic: bool,
    /// Focused debug-build-only real-store witness for compression plus the
    /// replication-writer retention cadence under continuous snapshot traffic.
    /// Requires disposable HIVE_DATA, HIVE_GUARDIAN_WRITER_CADENCE_DIAGNOSTIC=1,
    /// and HIVE_GUARDIAN_PART_REAP_CHECK_SECS<=2. Release binaries omit it.
    #[cfg(debug_assertions)]
    #[arg(long = "guardian-writer-cadence-diagnostic")]
    guardian_writer_cadence_diagnostic: bool,
}

/// Tokio worker stack size, the one reason for an explicit runtime instead of `#[tokio::main]`:
/// `run_build`'s async pipeline compiles to poll frames large enough to overflow tokio's default 2
/// MiB worker stack (every deploy on a debug binary died
/// `thread 'tokio-rt-worker' has overflowed its stack`, and release binaries sit close enough to
/// the edge that the same class killed nodes under real load). It is virtual address space per
/// worker, committed only when touched, so it costs nothing.
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .build()
        .expect("build tokio runtime")
        .block_on(async_main())
}

/// Whether a Seer bind is reachable by external resolvers on :53: a wildcard bind, or a specific
/// non-loopback address while `HIVE_PUBLIC_IP` is set (the platform publishes it, and without a
/// public IP nothing forwards :53 to this node). The specific-address case matters: it is the only
/// bind that leaves wildcard :53 free for netavark/aardvark-dns, which provisioning the isolated
/// BuildExecutor needs, and requiring a wildcard bind silently dropped such nodes from the geo/api
/// NS set, so the reconciler HELD the delegation. Loopback and dev binds never count: advertising
/// one would publish a black hole in the delegated zone.
fn dns_bind_reaches_internet(addr: &str) -> bool {
    if addr.rsplit(':').next() != Some("53") {
        return false;
    }
    let host = addr.rsplit_once(':').map(|(host, _)| host).unwrap_or("");
    if host == "127.0.0.1" || host == "::1" || host == "[::1]" || host.starts_with("127.") {
        return false;
    }
    if host == "0.0.0.0" || host == "::" || host == "[::]" {
        return true;
    }
    std::env::var("HIVE_PUBLIC_IP")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// The gateway URL this node gossips as `gateway`, which peers use for HTTP mesh routing:
/// `HIVE_PUBLIC_IP` when set, because the `--listen` bind address (usually 0.0.0.0) would make
/// every cross-node proxy dial localhost and silently break the mesh.
fn gateway_base_url(args: &Args) -> String {
    let port = args.listen.port();
    match std::env::var("HIVE_PUBLIC_IP")
        .ok()
        .map(|s| s.trim().to_string())
    {
        Some(v) if !v.is_empty() && v != "0.0.0.0" => format!("http://{}:{}", v, port),
        _ => format!("http://{}", args.listen),
    }
}

/// Installs the process-level rustls `CryptoProvider` (ring). It must run before any rustls user:
/// the dependency tree links both `ring` and `aws-lc-rs`, so rustls cannot pick a default on its
/// own and a task that gets there first panics (witnessed on the ngrok-ingress path, where the dead
/// task leaked a redb open handle and wedged guardian init until the next restart). Later installs
/// are idempotent no-ops.
fn install_ring_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Per-client relay ingress backstop (`HIVE_RELAY_CLIENT_BPS`, `0` disables), far above what a
/// relayed control-plane peer or tunnel pulls, so only an abusive client is clipped. The relay is
/// public-facing and browser/WASM iroh clients are relay-ONLY by construction, so their traffic
/// always lands on it.
const RELAY_CLIENT_BYTES_PER_SECOND: u64 = 16 * 1024 * 1024;
/// Burst allowance as a multiple of the steady relay rate, so a short legitimate spike (a deploy
/// artifact moving over a relayed trunk) is not clipped.
const RELAY_CLIENT_BURST_FACTOR: u32 = 2;
/// Default hard deadline for the whole graceful stop (`HIVE_SHUTDOWN_DEADLINE_SECS`); it must stay
/// below systemd's `TimeoutStopSec` (90 s in hive-node.service) so the exit is ours, never
/// SIGKILL's.
const SHUTDOWN_DEADLINE_DEFAULT_SECS: u64 = 75;
/// Connection-level DoS bounds on the control plane (`HIVE_ADMIN_MAX_BODY_MIB`,
/// `HIVE_ADMIN_REQUEST_TIMEOUT_SECS`): the admin router has no streaming/SSE endpoints and deploys
/// enqueue-then-return, so a per-request timeout and a body cap are safe, unlike the public
/// gateway, which streams tenant responses and is bounded by the edge pipeline instead (16 MiB
/// request bodies, per-IP rate limits).
const ADMIN_MAX_BODY_MIB_DEFAULT: usize = 64;
const ADMIN_REQUEST_TIMEOUT_SECS_DEFAULT: u64 = 120;
/// Period of the self-management GC that reaps stale clone/build working dirs under the deploy
/// roots (`$HIVE_DATA/deploys` and the legacy `/tmp/hive-deploys`, skipping dirs that still back a
/// live deployment) and orphaned `Building…` placeholders, so build scratch never exhausts host
/// disk (it pairs with the firecracker orphan-overlay GC).
const BUILD_GC_INTERVAL: Duration = Duration::from_secs(600);
/// A build working dir untouched for this long is a dead build.
const STALE_BUILD_DIR_AGE: Duration = Duration::from_secs(1800);

async fn async_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    runtime_watch::spawn(tokio::runtime::Handle::current());
    install_ring_crypto_provider();
    let args = Args::parse();

    #[cfg(debug_assertions)]
    if args.guardian_writer_cadence_diagnostic {
        println!("{}", guardian::writer_cadence_diagnostic().await?);
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if args.guardian_lifecycle_diagnostic {
        println!("{}", guardian::lifecycle_diagnostic().await?);
        return Ok(());
    }

    if !args.dns_probe.is_empty() {
        return dns_probe::run_cli(&args.dns_probe).await;
    }
    if !args.dht_probe.is_empty() {
        return dht_probe::run_cli(&args.dht_probe).await;
    }
    if args.sqlite_engine_probe {
        return crate::musql_migrate::run_probe().await;
    }
    if let Some(path) = &args.compose_probe {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let services = compose::parse_compose(&text)?;
        let primary = compose::primary_service(&services).map(|s| s.name.clone());
        println!("{} — {} service(s)", path.display(), services.len());
        for svc in &services {
            let is_primary = primary.as_deref() == Some(svc.name.as_str());
            let src = match (&svc.build, &svc.image) {
                (Some(b), _) => format!("build {}", b.context),
                (None, Some(i)) => format!("image {i}"),
                _ => "(no image or build)".into(),
            };
            println!(
                "\n  {}{}\n    source     {src}\n    ports      {}",
                svc.name,
                if is_primary {
                    "  [PRIMARY — serves /]"
                } else {
                    ""
                },
                if svc.all_ports.is_empty() {
                    "(none declared; defaults to 8080/http)".to_string()
                } else {
                    svc.all_ports
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            format!(
                                "{}/{}{}{}",
                                p.container,
                                p.protocol,
                                match p.host {
                                    Some(h) => format!(" published:{h}"),
                                    None => " internal".to_string(),
                                },
                                if i == 0 { " <- routed at /" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
            if let Some(e) = &svc.entrypoint {
                println!("    entrypoint {e:?}");
            }
            if let Some(c) = &svc.command {
                println!("    command    {c:?}");
            }
            let published = svc.all_ports.iter().filter(|p| p.host.is_some()).count();
            let internal = svc
                .all_ports
                .len()
                .saturating_sub(published)
                .saturating_sub(usize::from(published == 0 && !svc.all_ports.is_empty()));
            if published > 0 || internal > 0 {
                println!(
                    "    NOTE       {published} published port(s) get a public raw-TCP \
                     allocation preferring the literal host port; internal-only ports are \
                     reachable from sibling services, not publicly."
                );
            }
        }
        println!(
            "\npublic entrypoint: {}",
            primary.as_deref().unwrap_or("(none)")
        );
        return Ok(());
    }
    if args.litebox_probe {
        let be = hive_backend::litebox::LiteboxBackend::default();
        match be.smoke_test().await {
            Ok(()) => {
                println!(
                    "litebox smoke test: PASS — both the syscall rewriter AND a full real HTTP \
                     round trip through the per-cell-TUN + patched-litebox + bind-shim networking \
                     pipeline succeeded. Safe to set HIVE_LITEBOX_VERIFIED=1 on this host, per what \
                     hive_backend::litebox's module doc's \"Networking\" section documents this \
                     covers (Node/Bun only — Python is not covered yet)."
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!("litebox smoke test: FAIL — {e}");
                std::process::exit(1);
            }
        }
    }

    restart_audit::audit_boot(&args.name);

    let firecracker_suppressed = std::env::var("HIVE_FORCE_MOCK")
        .map(|v| v == "1")
        .unwrap_or(false);
    let mut fc_cfg = FirecrackerConfig::default();
    if let Ok(ba) = std::env::var("HIVE_FC_BOOT_ARGS") {
        if !ba.trim().is_empty() {
            fc_cfg.boot_args = ba;
        }
    }
    let firecracker = Arc::new(FirecrackerBackend::new(fc_cfg.clone()));
    let firecracker_runtime_capabilities =
        resources::RuntimeCapabilitySource::firecracker(firecracker.clone(), &fc_cfg);
    let sandbox_fc_supported = firecracker.is_supported() && !firecracker_suppressed;
    let litebox = Arc::new(LiteboxBackend::default());
    let litebox_verified = std::env::var("HIVE_LITEBOX_VERIFIED")
        .map(|v| v == "1")
        .unwrap_or(false);
    let litebox_supported = litebox.is_supported() && litebox_verified && !sandbox_fc_supported;
    let sandbox_backend: Option<Arc<dyn CellBackend>> = if sandbox_fc_supported {
        Some(firecracker.clone())
    } else if litebox_supported {
        Some(litebox.clone())
    } else {
        None
    };
    let sandbox_firecracker: Option<Arc<FirecrackerBackend>> =
        sandbox_fc_supported.then(|| firecracker.clone());
    let litebox_runtime_capabilities = resources::RuntimeCapabilitySource::litebox(litebox.clone());
    let (backend, backend_name, runtime_capability_source): (
        Arc<dyn CellBackend>,
        &'static str,
        resources::RuntimeCapabilitySource,
    ) = if sandbox_fc_supported {
        tracing::info!("isolation backend: Firecracker microVM (real, Linux + /dev/kvm)");
        (firecracker, "firecracker", firecracker_runtime_capabilities)
    } else if litebox_supported {
        tracing::warn!(
            "isolation backend: Litebox (unprivileged syscall sandbox, HIVE_LITEBOX_VERIFIED=1) \
             — real microVMs unavailable/suppressed on this host. NOT a hardware isolation \
             boundary; see hive_backend::litebox module doc for the full security posture."
        );
        (litebox, "litebox", litebox_runtime_capabilities)
    } else {
        if firecracker_suppressed && !litebox_verified {
            tracing::warn!("isolation backend: MockBackend (HIVE_FORCE_MOCK=1, no verified Litebox) — runtime is mocked for local development");
        } else if firecracker_suppressed {
            tracing::warn!("isolation backend: MockBackend (HIVE_FORCE_MOCK=1) — Litebox verified but its runner binary is missing on this host (Tier 1 check failed)");
        } else {
            tracing::warn!("isolation backend: MockBackend (sandboxed child process) — real microVMs need Linux + /dev/kvm; this is expected for local dev. ALL OTHER subsystems run for real.");
        }
        (
            Arc::new(MockBackend::new(MockConfig {
                root: std::env::temp_dir().join("hive-cloud-cells"),
                provision_latency: Duration::from_millis(200),
                cache_root: std::env::temp_dir().join("hive-cloud-cache"),
                receipts_dir: crate::persist::data_dir().join("runtime-artifacts-v1"),
            })),
            "mock",
            resources::RuntimeCapabilitySource::mock(),
        )
    };
    let backend_name = backend_name.to_string();

    hive_backend::set_cell_owner(&args.name, &crate::persist::data_dir());
    let reap_urgent = if std::env::var("HIVE_CELL_ORPHAN_REAP")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
    {
        let path_env = git::podman_path_env();
        let (urgent_tx, urgent_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let report = hive_backend::reap_orphaned_cells(
                hive_backend::container_cli::bin(false),
                &path_env,
                urgent_tx,
            )
            .await;
            log_cell_reap(&report, started.elapsed());
        });
        Some(urgent_rx)
    } else {
        tracing::warn!(
            "cell orphan reap: disabled (HIVE_CELL_ORPHAN_REAP=0) -- cells leaked by earlier \
             boots keep running beside this one's; orphan_reap_ran() stays false"
        );
        None
    };

    let geo = geolocate().await;
    if let Some(g) = &geo {
        tracing::info!(city = %g.2, country = %g.3, lat = g.0, lon = g.1, "node geolocated");
    }
    let region = if args.region == "auto" {
        region_id_from_geo(geo.as_ref())
    } else {
        args.region.clone()
    };

    if let Some(urgent) = reap_urgent {
        if tokio::time::timeout(CELL_REAP_BOOT_WAIT, urgent).await.is_err() {
            tracing::warn!(
                wait_s = CELL_REAP_BOOT_WAIT.as_secs(),
                "cell orphan reap: listing + duplicate-writer kills still running -- booting on; \
                 the reap finishes in the background and the per-volume launch guard covers \
                 every volume-mounting launch until it is confirmed"
            );
        }
    }

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
    let public_base = gateway_base_url(&args);
    let cap = resources::capacity();
    let gpus = resources::detect_gpus();
    let runtime_capabilities = runtime_capability_source.detect().await;
    let wasm_rt = runtime_capabilities.wasm_runtime;
    let bun_rt = runtime_capabilities.bun_runtime;
    let runtime_artifact_protocol = runtime_capabilities.runtime_artifact_protocol;
    let build_toolchains = runtime_capabilities.build_toolchains.clone();
    let build_isolation_protocol = match build_executor::init_installed().await {
        Ok(protocol) => {
            tracing::info!(protocol, "BuildExecutor live probe passed");
            Some(protocol)
        }
        Err(error) => {
            tracing::warn!(
                code = ?error.code,
                operation = error.operation,
                detail = error.detail(),
                "BUILD_ISOLATION_UNAVAILABLE: repository builds disabled on this node"
            );
            None
        }
    };
    let iroh_key_path = persist::data_dir().join("iroh_secret.key");
    let bootstrap_seeds = {
        let csv = std::env::var("HIVE_BOOTSTRAP_PEERS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::fs::read_to_string(persist::data_dir().join("bootstrap_peers"))
                    .ok()
                    .map(|s| s.replace(['\n', '\r'], ","))
            });
        csv.map(|c| hive_p2p::parse_bootstrap_seeds(&c))
            .unwrap_or_default()
    };
    let discovery_urls: Vec<String> = std::env::var("HIVE_DISCOVERY_DNS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let n0_discovery = std::env::var("HIVE_DISCOVERY_N0")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);
    let iroh_ep = match tokio::time::timeout(
        Duration::from_secs(8),
        hive_p2p::bind_full(
            Some(iroh_key_path),
            &bootstrap_seeds,
            &discovery_urls,
            n0_discovery,
        ),
    )
    .await
    {
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
    let public_ip = resolve_public_ip(geo.as_ref().and_then(|g| g.4.clone()));
    let public_ip6 = std::env::var("HIVE_PUBLIC_IP6")
        .ok()
        .and_then(|s| {
            s.trim()
                .parse::<std::net::Ipv6Addr>()
                .ok()
                .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
        })
        .map(|ip| ip.to_string());
    if let Some(ip) = &public_ip {
        tracing::info!(%ip, ip6 = ?public_ip6, "node public IP (advertised to client DNS / Seer)");
    }

    let relay_port: u16 = std::env::var("HIVE_OWN_RELAY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(args.relay_port);
    let relay_bind: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        relay_port,
    );
    let browser_relay_access_cell: Arc<std::sync::OnceLock<std::sync::Weak<CloudState>>> =
        Arc::new(std::sync::OnceLock::new());
    let mut embedded_relay_cfg = iroh_relay::server::ServerConfig::default();
    let mut relay_cfg = iroh_relay::server::RelayConfig::new(relay_bind);
    relay_cfg.access = Arc::new(BrowserRelayAccess {
        cloud: browser_relay_access_cell.clone(),
    });
    let relay_bps = env_u64("HIVE_RELAY_CLIENT_BPS", RELAY_CLIENT_BYTES_PER_SECOND);
    if let Some(bps) = std::num::NonZeroU32::new(relay_bps.min(u32::MAX as u64) as u32) {
        let mut rl = iroh_relay::server::ClientRateLimit::new(bps);
        rl.max_burst_bytes =
            std::num::NonZeroU32::new(bps.get().saturating_mul(RELAY_CLIENT_BURST_FACTOR));
        relay_cfg.limits.client_rx = Some(rl);
        tracing::info!(
            bytes_per_second = bps.get(),
            "embedded relay: per-client rate limit armed"
        );
    } else {
        tracing::warn!("embedded relay: per-client rate limit DISABLED (HIVE_RELAY_CLIENT_BPS=0)");
    }
    embedded_relay_cfg.relay = Some(relay_cfg);
    embedded_relay_cfg.quic = None;
    embedded_relay_cfg.metrics_addr = None;
    let relay_server = match tokio::time::timeout(
        Duration::from_secs(5),
        iroh_relay::server::Server::spawn(embedded_relay_cfg),
    )
    .await
    {
        Ok(Ok(server)) => {
            tracing::info!(addr = ?server.http_addr(), "embedded iroh-relay server bound");
            Some(server)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, port = relay_port, "embedded iroh-relay server failed to bind (continuing without it)");
            None
        }
        Err(_) => {
            tracing::warn!(
                port = relay_port,
                "embedded iroh-relay server bind timed out (continuing without it)"
            );
            None
        }
    };
    let own_relay_url: Option<String> = relay_server
        .as_ref()
        .and(public_ip.as_ref())
        .map(|ip| format!("http://{ip}:{relay_port}"));
    if let Some(url) = &own_relay_url {
        tracing::info!(%url, "this node's relay_url (advertised via gossip)");
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
        guardian_iroh_addr: None,
        relay_url: None,
        dns_ns: std::env::var("HIVE_DNS_ADDR")
            .ok()
            .filter(|addr| dns_bind_reaches_internet(addr)),
        dns_api: {
            let ns_ok = std::env::var("HIVE_DNS_ADDR")
                .ok()
                .map(|a| dns_bind_reaches_internet(&a))
                .unwrap_or(false);
            ns_ok
                && std::env::var("HIVE_PLATFORM_DOMAIN")
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
        },
        dns_attest: Vec::new(),
        dashboard: false,
        cp_epoch: 1,
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
        disk_free_gb: crate::resources::disk_free_gb(),
        gpu_free_mb: crate::resources::measured_gpu_free_mb(),
        started_ms: crate::restart_audit::started_ms(),
        oom_restarts_24h: crate::restart_audit::oom_restarts_24h(),
        unclean_restarts_24h: crate::restart_audit::unclean_restarts_24h(),
        last_oom_ms: crate::restart_audit::last_oom_ms(),
        backend: backend_name.clone(),
        gpu_count: gpus.0,
        wasm_runtime: wasm_rt,
        bun_runtime: bun_rt,
        runtime_artifact_protocol,
        build_toolchains: build_toolchains.clone(),
        build_isolation_protocol,
        artifact_transfer_protocol: None,
        musql_engine: None,
        gpu_model: gpus.1.clone(),
        gpu_vram_mb: gpus.2,
        provider: std::env::var("HIVE_CLOUD_PROVIDER")
            .ok()
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty()),
        private_addr: resolve_private_addr(),
    };
    tracing::info!(
        cores = cap.0, mem_mb = cap.1, disk_gb = cap.2, backend = %backend_name,
        gpus = gpus.0, gpu_model = gpus.1.as_deref().unwrap_or("-"), gpu_vram_mb = gpus.2,
        wasm_runtime = wasm_rt.unwrap_or(false),
        bun_runtime = bun_rt.unwrap_or(false),
        runtime_artifact_protocol = ?runtime_artifact_protocol,
        build_toolchains = %build_toolchains
            .as_deref()
            .map(|toolchains| toolchains.join(","))
            .unwrap_or_else(|| "unreported".to_string()),
        "node host capacity"
    );
    if wasm_rt != Some(true) {
        tracing::info!(
            backend = %backend_name,
            "no wasmer runtime on the filesystem this node's functions exec against — \
             Runtime::Wasmer deployments will not be placed here (see the active-backend capability probe)"
        );
    }
    if bun_rt != Some(true) {
        tracing::info!(
            backend = %backend_name,
            "no bun runtime on the filesystem this node's functions exec against — \
             Runtime::Bun deployments will not be placed here (see the active-backend capability probe)"
        );
    }
    match build_toolchains.as_ref() {
        None => tracing::info!(
            backend = %backend_name,
            "build toolchains not probed on this node's build filesystem — \
             package-manager selection will not be placement-filtered here"
        ),
        Some(toolchains) => tracing::info!(
            backend = %backend_name,
            toolchains = %toolchains.join(","),
            "build toolchains probed on the filesystem this node's builds exec against"
        ),
    }
    let registry = NodeRegistry::new(me);
    if crate::musql::spawn() {
        registry.set_self_musql_engine(crate::musql::probe());
    }
    registry.set_self_relay_url(own_relay_url.clone());

    crate::secrets::audit_at_rest();

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
        sandbox_firecracker,
        sandbox_backend,
    );
    let _ = browser_relay_access_cell.set(Arc::downgrade(&cloud));
    cloud
        .fluid
        .set_execution_observer(crate::deployment_ledger::LedgerExecutionObserver::new(
            cloud.deployment_ledger.clone(),
        ));
    if cloud.runtime_artifact_transfer.enabled() {
        cloud.registry.set_self_artifact_transfer_protocol(Some(
            crate::runtime_artifact_transfer_wire::PROTOCOL_VERSION,
        ));
    }

    fluid_gateway::set_public_apps_domain(&cloud.apps_domain);

    if let Some((fqdn, value)) = args
        .acme_txt_selftest
        .as_deref()
        .and_then(|s| s.split_once('='))
    {
        cloud.acme_challenges.insert(fqdn, value);
        tracing::info!(%fqdn, "ACME DNS-01 selftest TXT seeded into the challenge store");
    }

    persist::restore(&cloud, persist::load());
    persist::spawn_persister(cloud.clone());
    deployment_ledger::spawn_outbox(cloud.clone());
    spawn_metrics_persist_loop(cloud.clone());
    let controlled_restart = ControlledRestart::new();
    {
        let flush_cloud = cloud.clone();
        let flush_node_name = args.name.clone();
        let shutdown_restart = controlled_restart.clone();
        tokio::spawn(async move {
            let terminate = async {
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(mut signal) => {
                        signal.recv().await;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to install SIGTERM handler");
                        std::future::pending::<()>().await;
                    }
                }
            };
            let interrupt = async {
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                    Ok(mut signal) => {
                        signal.recv().await;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to install SIGINT handler");
                        std::future::pending::<()>().await;
                    }
                }
            };
            let requested_reason = tokio::select! {
                _ = terminate => None,
                _ = interrupt => None,
                reason = shutdown_restart.wait() => Some(reason),
            };
            match requested_reason {
                Some(reason) => tracing::error!(
                    ?reason,
                    "controlled restart requested; beginning graceful shutdown"
                ),
                None => tracing::info!("shutdown signal received; beginning graceful shutdown"),
            }
            {
                let deadline = Duration::from_secs(env_u64(
                    "HIVE_SHUTDOWN_DEADLINE_SECS",
                    SHUTDOWN_DEADLINE_DEFAULT_SECS,
                ));
                let code = if requested_reason.is_some() { 17 } else { 0 };
                let spawned = std::thread::Builder::new()
                    .name("hive-shutdown-deadline".into())
                    .spawn(move || {
                        std::thread::sleep(deadline);
                        tracing::error!(
                            ?deadline,
                            exit_code = code,
                            "graceful shutdown exceeded its hard deadline — forcing exit now"
                        );
                        std::process::exit(code);
                    });
                if let Err(error) = spawned {
                    tracing::error!(
                        %error,
                        "could not spawn the shutdown hard-deadline thread; systemd's TimeoutStopSec is the only backstop"
                    );
                }
            }
            let grace = Duration::from_secs(env_u64("HIVE_SHUTDOWN_GRACE_SECS", 15));
            if let Some(handle) = SHUTDOWN_HTTPS_HANDLE.get() {
                tracing::info!(?grace, "shutdown requested → draining public listener (in-flight requests + cell tunnels)");
                handle.graceful_shutdown(Some(grace));
                let drain_started = std::time::Instant::now();
                loop {
                    let open = handle.connection_count();
                    if open == 0 || drain_started.elapsed() >= grace {
                        tracing::info!(
                            open_connections = open,
                            elapsed_ms = drain_started.elapsed().as_millis() as u64,
                            "public listener drained"
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            tracing::info!("shutdown requested → draining runtime artifact transfers");
            if let Err(error) = flush_cloud.runtime_artifact_transfer.shutdown().await {
                tracing::error!(
                    %error,
                    "runtime artifact transfer worker did not drain cleanly; continuing shutdown"
                );
            }
            tracing::info!("shutdown requested → flushing platform state");
            let final_guardian_generation = match persist::flush_blocking() {
                Ok(generation) => generation,
                Err(error) => {
                    tracing::error!(%error, "platform-state shutdown flush failed; Guardian shutdown will not await an unconfirmed generation");
                    None
                }
            };
            restart_audit::mark_clean_exit(&flush_node_name);
            flush_cloud.dns_geo.flush_blocking();
            if let Err(error) = crate::guardian::shutdown(final_guardian_generation).await {
                tracing::error!(%error, "Guardian shutdown did not complete cleanly; exiting anyway");
            }
            // Own statement so the lock guard drops BEFORE the await below: holding it across an
            // await makes this future non-Send.
            let endpoint = flush_cloud.iroh.read().clone();
            if let Some(ep) = endpoint {
                let budget = Duration::from_secs(env_u64("HIVE_ENDPOINT_CLOSE_SECS", 3));
                match tokio::time::timeout(budget, ep.close()).await {
                    Ok(()) => tracing::info!("iroh endpoint closed cleanly — peers notified"),
                    Err(_) => {
                        tracing::warn!(?budget, "iroh endpoint close timed out; exiting anyway")
                    }
                }
            }
            let restart_reason = shutdown_restart.seal();
            if let Some(reason) = restart_reason {
                tracing::error!(
                    ?reason,
                    exit_code = 17,
                    "controlled restart shutdown complete"
                );
            }
            std::process::exit(if restart_reason.is_some() { 17 } else { 0 });
        });
    }
    enterprise::spawn_siem_streamer(cloud.enterprise.clone());
    *cloud.peer_iroh.write() = persist::load_peer_iroh();

    let self_iroh_id = iroh_ep.as_ref().map(|e| e.id().to_string());
    let seed_targets: Vec<(String, String, String)> = bootstrap_seeds
        .iter()
        .filter(|s| self_iroh_id.as_deref() != Some(s.node_id.as_str()))
        .map(|s| {
            (
                format!("seed:{}", s.node_id),
                s.node_id.clone(),
                s.addr_json.clone(),
            )
        })
        .collect();
    {
        let mut pi = cloud.peer_iroh.write();
        for (key, nid, addr) in &seed_targets {
            pi.entry(key.clone())
                .or_insert_with(|| (nid.clone(), addr.clone()));
        }
    }
    if !seed_targets.is_empty() {
        tracing::info!(
            seeds = seed_targets.len(),
            "cold-start bootstrap seeds registered"
        );
    }

    *cloud.peers.write() = args.peers.clone();

    if let Some(ep) = iroh_ep.clone() {
        *cloud.iroh.write() = Some(ep.clone());
        *cloud.mesh.write() = Some(hive_p2p::PeerPool::new(ep.clone()));
        let browser_pool = hive_p2p::BrowserPool::new(ep.clone());
        *cloud.browser_mesh.write() = Some(browser_pool.clone());
        gw.set_browser_invoker(std::sync::Arc::new(move |target, request| {
            let browser_pool = browser_pool.clone();
            Box::pin(async move {
                browser_pool
                    .invoke(
                        &target.endpoint_id,
                        &target.addr_json,
                        &target.digest,
                        &request,
                    )
                    .await
                    .map_err(|error| fluid_gateway::BrowserInvokeFailure {
                        sent: error.sent,
                        message: error.message,
                    })
            })
        }));
        {
            let cloud_for_resolver = cloud.clone();
            gw.set_browser_claims_resolver(std::sync::Arc::new(move |headers| {
                let token = crate::auth::extract_token(headers)?;
                crate::auth::verify(&token)
                    .ok()
                    .or_else(|| crate::auth::api_key_claims(&cloud_for_resolver, &token))
                    .map(|claims| crate::admin::norm(&claims.tenant).to_string())
            }));
        }
        *cloud.relay_set.write() = Some(hive_p2p::RelaySet::new(ep.clone()));
        let gateway_addr = args.listen.to_string();
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
        let gossip_handler = crate::gossip::handler(cloud.clone());
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
                        if !crate::admin::ct_eq(&proof, &expect) {
                            tracing::warn!(peer = %remote_id, "REJECTED mesh join: invalid proof");
                            return Vec::new();
                        }
                        let Ok(node) = serde_json::from_slice::<NodeInfo>(&node_json) else {
                            tracing::warn!(peer = %remote_id, "REJECTED mesh join: unparseable NodeInfo");
                            return Vec::new();
                        };
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
                        cloud.registry.upsert_peer_self_report(node);
                        cloud.audit.record("_global", "mesh", "join", "node", &name, &format!("endpoint {remote_id} admitted via join proof"));
                        tracing::info!(peer = %remote_id, node = %name, "mesh join ADMITTED (hot-join, key-addressed)");
                        serde_json::to_vec(&cloud.registry.nodes()).unwrap_or_default()
                    })
                });
                h
            });
        let raw_resolver = crate::mesh_raw::resolver(cloud.clone());
        let admission_cloud = cloud.clone();
        let browser_admission: hive_p2p::BrowserAdmissionHandler =
            std::sync::Arc::new(move |endpoint_id: String| {
                let cloud = admission_cloud.clone();
                Box::pin(async move {
                    crate::browser_admission::endpoint_admitted(&cloud, &endpoint_id).await
                })
            });
        let browser_crr = std::env::var("HIVE_BROWSER_DB_LISTEN")
            .ok()
            .filter(|v| v.trim() == "0")
            .map_or_else(
                || Some(crate::browser_db::crr_sync_handler(&cloud)),
                |_| None,
            );
        hive_p2p::set_browser_meter(Some(crate::browser_metering::meter_handler(&cloud)));
        tokio::spawn(hive_p2p::serve_tunnels_full(
            ep,
            gateway_addr,
            256,
            trust,
            Some(gossip_handler),
            join_handler,
            Some(raw_resolver),
            Some(browser_admission),
            browser_crr,
            &[crate::store_sync::SNAPSHOT_PATH_PREFIX],
        ));
        tracing::info!(gateway = %args.listen, "iroh P2P tunnel server accepting peer connections (join + raw-target surfaces on)");
    }

    let _ = cloud.control_plane_leader();
    cloud.registry.set_self_cp_epoch(cloud.cluster.epoch());

    spawn_cron_loop(cloud.clone());
    spawn_cluster_loop(cloud.clone());
    spawn_guardian_snapshot_loop(cloud.clone());
    spawn_guardian_reap_loop(cloud.clone());
    spawn_lease_loop(cloud.clone());
    spawn_sandbox_timeout_reap_loop(cloud.clone());

    let gc_cloud = cloud.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(BUILD_GC_INTERVAL).await;
            let n = crate::git::gc_build_dirs(&gc_cloud, STALE_BUILD_DIR_AGE).await;
            if n > 0 {
                tracing::info!(removed = n, "gc: cleaned stale build dirs");
            }
            let p = crate::git::reap_orphan_placeholders(&gc_cloud).await;
            if p > 0 {
                tracing::info!(removed = p, "gc: reaped orphaned Building… placeholders");
            }
        }
    });

    guardian::set_node_name(&args.name);
    guardian::set_boot_seed_peers(
        crate::persist::load_peer_guardian_addr()
            .into_values()
            .collect(),
    );
    guardian::init_background();
    {
        let cloud = cloud.clone();
        tokio::spawn(async move {
            relational::init_schema().await;
            let existing: Vec<(String, String, String, u64)> = cloud
                .projects
                .snapshot()
                .into_iter()
                .map(|(project, s)| (project, s.team, s.build.root_dir, s.updated_ms))
                .collect();
            relational::backfill_projects(existing).await;
        });
    }
    guardian::spawn_restore_guard(cloud.clone());

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
    // Discovery (Plane B, node-to-node pkarr relay) is distinct from Seer (Plane A, client-to-node
    // DNS, dnsserver.rs); HIVE_SEER_ADDR is a deprecated alias for its bind address (the names once
    // collided).
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
    spawn_gossip_loop(cloud.clone(), args.peers.clone(), seed_targets.clone());
    {
        let c = cloud.clone();
        tokio::spawn(async move {
            for _ in 0..12 {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if !c.peer_iroh.read().is_empty() {
                    return;
                }
                if let Some(bytes) = guardian::get("mesh/roster").await {
                    if let Ok(map) = serde_json::from_slice::<
                        std::collections::HashMap<String, (String, String)>,
                    >(&bytes)
                    {
                        let me = c.registry.me().peer_id;
                        let mut pi = c.peer_iroh.write();
                        for (k, v) in map {
                            if Some(&v.0) == me.as_ref() {
                                continue;
                            }
                            pi.entry(k).or_insert(v);
                        }
                        tracing::info!(
                            entries = pi.len(),
                            "mesh roster adopted from GuardianDB replica"
                        );
                        return;
                    }
                }
            }
        });
    }
    spawn_health_loop(cloud.clone());

    spawn_trunk_warmer(cloud.clone());

    crate::prewarm::spawn(cloud.clone());

    spawn_relay_sync_loop(cloud.clone());

    spawn_anti_entropy_loop(cloud.clone());
    spawn_geo_refresh(cloud.registry.clone());
    spawn_disk_refresh(cloud.registry.clone(), runtime_capability_source.clone());
    spawn_memory_pressure_alarm();

    spawn_billing_meter_loop(cloud.clone());

    spawn_promotion_reconcile_loop(cloud.clone());

    spawn_relational_mirror_loop(cloud.clone());
    store_follower::spawn(cloud.clone());

    crate::push::spawn_push_dispatcher(cloud.clone());

    tokio::spawn(crate::world_queue::run_delivery_loop(
        cloud.clone(),
        cloud.world_queue.clone(),
    ));

    tokio::spawn(crate::queues::spawn_delivery_loop(cloud.clone()));

    dns_probe::spawn_ns_prober(cloud.clone());

    {
        let cloud = cloud.clone();
        let upstream = std::env::var("HIVE_DASHBOARD_UPSTREAM")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());
        let budget_ms: u64 = std::env::var("HIVE_DASHBOARD_PROBE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500);
        tokio::spawn(async move {
            let Some(up) = upstream else { return };
            loop {
                let ok = match tokio::time::timeout(
                    std::time::Duration::from_millis(budget_ms),
                    cloud.http.get(format!("{up}/")).send(),
                )
                .await
                {
                    Ok(Ok(resp)) => resp.status().is_success() || resp.status().is_redirection(),
                    _ => false,
                };
                cloud.registry.set_self_dashboard(ok);
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    vercel_dns::spawn_reconciler(cloud.clone());

    acme::spawn_acme(cloud.clone());
    acme::spawn_cert_sync(cloud.clone());
    acme::spawn_custom_cert_loop(cloud.clone());
    databases::spawn_db_reconcile(cloud.clone());
    crate::admin::spawn_domain_verify_loop(cloud.clone());
    inference::spawn_reconcile(cloud.clone());
    world::spawn_world_reconcile(cloud.clone());

    git::spawn_git_poll_reconcile(cloud.clone());

    production_deployments::spawn_node_death_reconcile(cloud.clone());

    spawn_container_lock_sweep();

    memwatch::spawn(cloud.hive.clone(), controlled_restart.clone());

    meshwatch::spawn(cloud.clone(), controlled_restart.clone());
    tenancy_reconcile::spawn(cloud.clone());
    listener_audit::spawn(cloud.clone(), |c| {
        leadership::may_act(c, leadership::Job::ListenerAudit)
    });
    incidents::spawn_reconciler(cloud.clone());
    spawn_deletion_reconcile_loop(cloud.clone());

    restart_audit::spawn(args.name.clone(), cloud.clone());

    browser_artifacts::spawn_gc_loop(cloud.clone());
    browser_db::spawn_reconcile(cloud.clone());
    browser_db::spawn_fleet_sync(cloud.clone());
    browser_admission::spawn_roll_call(cloud.clone());

    let public = fluid_gateway::public_router(gw.clone()).layer(
        axum::middleware::from_fn_with_state(cloud.clone(), edge::edge_pipeline),
    );
    let admin_max_body: usize = std::env::var("HIVE_ADMIN_MAX_BODY_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ADMIN_MAX_BODY_MIB_DEFAULT)
        * 1024
        * 1024;
    let admin_req_timeout = Duration::from_secs(
        std::env::var("HIVE_ADMIN_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(ADMIN_REQUEST_TIMEOUT_SECS_DEFAULT),
    );
    let admin_router = admin::router(cloud.clone())
        .merge(crate::guardian::routes().with_state(cloud.clone()))
        .layer(axum::middleware::from_fn(admin_cache_headers))
        .layer(axum::middleware::from_fn_with_state(
            cloud.clone(),
            auth::require_auth,
        ))
        .layer(axum::middleware::from_fn(admin::admin_rate_limit))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            admin_max_body,
        ))
        .layer(tower_http::timeout::TimeoutLayer::new(admin_req_timeout));
    if auth::enforced() {
        tracing::info!("JWT auth enforced on admin mutations (HIVE_JWT_SECRET set)");
    }

    let dash_hosts = vec![
        cloud.platform_domain.clone(),
        format!("www.{}", cloud.platform_domain),
    ];
    let public = if cloud.ingress != "ngrok" {
        if !auth::enforced() {
            tracing::error!(
                "HIVE_INGRESS={} requires HIVE_JWT_SECRET (the admin API becomes publicly addressable at api.{}); keeping single-router listener",
                cloud.ingress, cloud.platform_domain
            );
            public
        } else {
            let api_host = format!("api.{}", cloud.platform_domain);
            let admin_host = format!("admin.{}", cloud.platform_domain);
            let webhook_host = format!("webhook.{}", cloud.platform_domain);
            let dash_upstream = std::env::var("HIVE_DASHBOARD_UPSTREAM")
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| !v.is_empty());
            tracing::info!(%api_host, %admin_host, %webhook_host, dashboard = ?dash_upstream, "host-based dispatch active (api/admin/webhook hosts → admin router; apex/www → dashboard proxy)");
            host_switch_router(
                cloud.clone(),
                api_host,
                admin_host,
                webhook_host,
                dash_hosts.clone(),
                dash_upstream,
                cloud.http.clone(),
                admin_router.clone(),
                public,
            )
        }
    } else {
        public
    };

    // Layered once, after host dispatch and before every clone below (tls_public, the
    // dedicated-IPv4 listeners, plain HTTP), so all four public listeners agree on one COOP/COEP
    // policy (see `coi`).
    let public = coi::layer(public, coi::Config::from_env(&dash_hosts));

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

    db_gateway::spawn(cloud.clone());
    cloudflare_dns::spawn_db_dns(cloud.clone());

    raw_proxy::spawn(cloud.clone());

    udp_relay::spawn(cloud.clone());

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
        let https_shutdown_handle = axum_server::Handle::new();
        SHUTDOWN_HTTPS_HANDLE
            .set(https_shutdown_handle.clone())
            .ok();
        tokio::spawn(async move {
            install_ring_crypto_provider();
            let cfg = axum_server::tls_rustls::RustlsConfig::from_config(acme::server_config());
            tracing::info!(%https_addr, "public HTTPS listener (SNI resolver, ACME-managed certs)");
            if let Err(e) = axum_server::bind_rustls(https_addr, cfg)
                .handle(https_shutdown_handle)
                .serve(https_router.into_make_service_with_connect_info::<SocketAddr>())
                .await
            {
                tracing::error!(error = %e, %https_addr, "HTTPS listener failed (check CAP_NET_BIND_SERVICE / port availability)");
            }
        });
        let challenge_cloud = cloud.clone();
        tokio::spawn(async move {
            let cloud = challenge_cloud;
            let redirect =
                axum::Router::new().fallback(move |req: axum::http::Request<axum::body::Body>| {
                    let cloud = cloud.clone();
                    async move {
                        use axum::response::IntoResponse as _;
                        let path_q = req
                            .uri()
                            .path_and_query()
                            .map(|p| p.as_str().to_string())
                            .unwrap_or_else(|| "/".into());
                        if let Some(token) = path_q
                            .split('?')
                            .next()
                            .unwrap_or("")
                            .strip_prefix("/.well-known/acme-challenge/")
                        {
                            return match cloud.acme_http01.lookup(token) {
                                Some(body) => (
                                    axum::http::StatusCode::OK,
                                    [(
                                        axum::http::header::CONTENT_TYPE,
                                        "text/plain; charset=utf-8",
                                    )],
                                    body,
                                )
                                    .into_response(),
                                None => {
                                    if let Some(leader) = cloud.leader_forward_target() {
                                        if let Some(ip) = cloud
                                            .registry
                                            .nodes()
                                            .into_iter()
                                            .find(|n| n.name == leader && n.healthy)
                                            .and_then(|n| {
                                                n.public_ip.clone().filter(|ip| !ip.is_empty())
                                            })
                                        {
                                            let url = format!("http://{ip}{path_q}");
                                            if let Ok(r) = cloud
                                                .http
                                                .get(&url)
                                                .timeout(std::time::Duration::from_secs(5))
                                                .send()
                                                .await
                                            {
                                                if r.status().is_success() {
                                                    if let Ok(body) = r.text().await {
                                                        return (
                                                            axum::http::StatusCode::OK,
                                                            [(
                                                                axum::http::header::CONTENT_TYPE,
                                                                "text/plain; charset=utf-8",
                                                            )],
                                                            body,
                                                        )
                                                            .into_response();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    (axum::http::StatusCode::NOT_FOUND, "unknown challenge")
                                        .into_response()
                                }
                            };
                        }
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
                        axum::response::Redirect::permanent(&format!("https://{host}{path_q}"))
                            .into_response()
                    }
                });
            tracing::info!(%http_addr, "port-80 listener (301 → https only)");
            match tokio::net::TcpListener::bind(http_addr).await {
                Ok(l) => {
                    if let Err(e) = axum::serve(l, redirect).await {
                        tracing::error!(error = %e, "port-80 redirect listener failed");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, %http_addr, "cannot bind port 80 (check CAP_NET_BIND_SERVICE)")
                }
            }
        });

        dedicated_ipv4_listener::spawn(cloud.clone(), public.clone());
    }

    tracing::info!(region=%region, node=%args.name, public=%args.listen, admin=%args.admin, tls=%tls_addr, "hive-cloud node up");

    let admin_router = admin_router.layer(axum::middleware::from_fn_with_state(
        (cloud.clone(), format!("api.{}", cloud.platform_domain)),
        admin_loopback_forward,
    ));

    let pub_srv = serve(public, args.listen, "public");
    let adm_srv = serve(admin_router, args.admin, "admin");
    tokio::try_join!(pub_srv, adm_srv)?;
    Ok(())
}

/// Paths that must never be forwarded to the control-plane leader by either leader gate, for two
/// DISTINCT reasons. OWNER-routed: a managed SQLite database is a FILE on `Database::host_node` and
/// `hrana::serve` proxies to that node itself, so sending its pipeline POSTs to the leader first
/// would add a hop that still has to proxy, pin an interactive transaction's baton to whichever
/// node was leader when the stream opened (a leadership change mid-transaction strands it) and
/// deadlock the owner-proxy hop (the mesh envelope `/v1/databases/<id>/hrana-mesh` is itself a POST
/// and would bounce back to the leader instead of reaching the owner it was addressed to). The same
/// holds for browser_db's libsql/Hrana + Upstash REST surface (`/v1/browser-db/`, owner-routed to
/// `browser_db::rest_owner_for_project`, computed because there is no `Database` record to carry a
/// host_node), runtime artifact transfers (addressed to the immutable request's exact target node:
/// a chunk sent to the leader would mutate a different host's journal or fail WrongTarget, and make
/// resumable delivery impossible across leader changes) and sandbox owner RPCs
/// (`/v1/internal/sandboxes/`, addressed by the LEADER to the exact node that holds or must
/// provision the cell: bouncing them back re-runs the create on a node with no exec backend, or
/// deadlocks the hop). OWNER-FREE: shadw drive (`/v1/drive/*`, REST and WebDAV) needs no owner,
/// because every write lands in `relational::drive_*`, GuardianDB's CRDT last-write-wins store,
/// safe to apply from ANY node; leader-forwarding it cost a round trip per PUT/DELETE and bought no
/// correctness (the cause of slow WebDAV copies of many small files). WebDAV's non-CRUD methods
/// already bypassed the gate, since `is_mutation_request` only recognizes POST/PUT/DELETE/PATCH.
fn owner_routed(path: &str) -> bool {
    path.starts_with("/v1/sqlite/")
        || (path.starts_with("/v1/databases/") && path.ends_with("/hrana-mesh"))
        || (path.starts_with("/v1/databases/") && path.ends_with("/studio-mesh"))
        || path.starts_with("/v1/drive/")
        || path.starts_with("/v1/browser-db/")
        || (path.starts_with("/v1/projects/") && path.ends_with("/browser-db/rest-mesh"))
        || path.starts_with("/v1/runtime-artifact-transfer/v1/")
        || path.starts_with("/v1/internal/sandboxes/")
}

/// Whether the request mutates state (POST/PUT/DELETE/PATCH), the only kind that serializes through
/// the control-plane leader; WebDAV's non-CRUD methods (MKCOL/COPY/MOVE/PROPFIND/LOCK/UNLOCK) are
/// not classified as mutations.
fn is_mutation_request(req: &axum::http::Request<axum::body::Body>) -> bool {
    matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH")
}

/// Loopback-admin mutation forwarding (the `admin_ingress` leader rule applied to the raw admin
/// listener, which the dashboard's /cloud proxy targets: before this, a dashboard-driven mutation
/// was handled by whichever node hosted the dashboard and never reached the leader; witnessed as an
/// admission granted on a follower that never appeared on the leader). Reads serve locally;
/// mutations on a non-leader forward to the current leader over the SNI-pinned client, the exact
/// helper `admin_ingress` uses, never a second bespoke path. A request marked
/// `x-hive-admin-forwarded` (forwarded by a peer) is applied only on the CURRENT leader (503
/// anywhere else: a forged marker or a mid-flight leadership change, so the client retries and
/// re-resolves) and one marked `x-hive-internal` (service hop) is always served locally;
/// `/v1/token` is stateless HS256 minting and must never depend on leader reachability.
async fn admin_loopback_forward(
    axum::extract::State((cloud, api_host)): axum::extract::State<(
        std::sync::Arc<state::CloudState>,
        String,
    )>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_mutation = is_mutation_request(&req);
    if req.headers().contains_key("x-hive-admin-forwarded") {
        if is_mutation && !cloud.is_control_plane_leader() {
            return not_leader_refusal();
        }
        return next.run(req).await;
    }
    if !is_mutation
        || req.uri().path() == "/v1/token"
        || owner_routed(req.uri().path())
        || req.headers().contains_key("x-hive-internal")
        || cloud.is_control_plane_leader()
    {
        return next.run(req).await;
    }
    admin_forward_to_leader(&cloud, &api_host, req).await
}

/// One listener, split by Host (real-DNS ingress): `api.{platform_domain}` (the PLATFORM API +
/// API-key surface), `admin.{platform_domain}` (the ops/admin console) and
/// `webhook.{platform_domain}` (the incoming GitOps/OpenEdge build-notification receiver,
/// `OPENEDGE_WEBHOOK_URL`) go to the admin router, the dashboard hosts to the dashboard proxy (each
/// node's own self-hosted dashboard on loopback, `HIVE_DASHBOARD_UPSTREAM`, never an external
/// tunnel), and anything else to the deployment edge pipeline. It is a fallback handler that
/// `oneshot`s into the matching inner router, so the x-hive-proxied loop guard, the WS upgrade path
/// and everything inside each router stay untouched. Host matching is case-insensitive and strips
/// `:port`; HTTP/2 carries the authority in the URI, not a Host header, so both are read (reading
/// only Host sent api/apex traffic into the public router). `api-<region>.<domain>` is the
/// region-pinned API surface (its DNS record names ONE region's nodes) and dispatches like `api.`,
/// matched by shape rather than enumerated (an unpublished api-<x> host simply does not resolve)
/// with a real label boundary, so `api-evil-shadw.cloud`-style hosts cannot match a bare
/// `ends_with`. The MATCHED host is passed on so a leader forward pins the right SNI.
/// api/admin/webhook share one router today (same auth); the split is by hostname so each reads as
/// its own surface and they can diverge without touching this dispatch.
fn host_switch_router(
    cloud: Arc<CloudState>,
    api_host: String,
    admin_host: String,
    webhook_host: String,
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
        let webhook_host = webhook_host.clone();
        let dash_hosts = dash_hosts.clone();
        let dash_upstream = dash_upstream.clone();
        let http = http.clone();
        async move {
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
            let region_api = api_host
                .strip_prefix("api.")
                .map(|domain| {
                    let suffix = format!(".{domain}");
                    host.starts_with("api-")
                        && host.ends_with(&suffix)
                        && host.len() > "api-".len() + suffix.len()
                        && !host["api-".len()..host.len() - suffix.len()].contains('.')
                })
                .unwrap_or(false);
            if host == api_host || host == admin_host || host == webhook_host || region_api {
                return admin_ingress(cloud, admin, host, req).await;
            }
            if let (true, Some(up)) = (
                dash_hosts.iter().any(|h| *h == host),
                dash_upstream.as_ref(),
            ) {
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

/// Explicit Cache-Control on every admin GET response (previously NO `/v1/*` JSON carried any, so
/// every intermediary guessed), TENANT-SAFE BY CONSTRUCTION with two classes only: global
/// non-tenant catalogs (`/v1/regions*`, `/v1/frameworks`) get
/// `public, s-maxage=3600, stale-while-revalidate=86400` (shared caches may serve them to everyone
/// and keep serving stale while revalidating; browsers get a short `max-age` so a catalog change
/// still lands quickly), and everything else gets `private, no-store`, so no shared cache
/// (CDN/proxy) can store tenant data and the cross-tenant cache-bleed class is eliminated
/// structurally. The tenant-keyed stale-while-revalidate layer lives in the dashboard client
/// (`ui/lib/api.ts`, keys `${tenant}|${path}`, purged on every mutation and team switch), the layer
/// that can actually key on the VERIFIED tenant. Handlers that set their own Cache-Control (project
/// thumbnails) keep it.
async fn admin_cache_headers(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_get = req.method() == axum::http::Method::GET;
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    if is_get
        && !resp
            .headers()
            .contains_key(axum::http::header::CACHE_CONTROL)
    {
        let public_catalog =
            path == "/v1/frameworks" || path == "/v1/regions" || path.starts_with("/v1/regions/");
        let value = if public_catalog {
            "public, max-age=300, s-maxage=3600, stale-while-revalidate=86400"
        } else {
            "private, no-store"
        };
        if let Ok(v) = axum::http::HeaderValue::from_str(value) {
            resp.headers_mut()
                .insert(axum::http::header::CACHE_CONTROL, v);
        }
    }
    resp
}

/// Regional AdminAPI ingress for `api.{platform_domain}` (also `admin.`, `webhook.` and
/// `api-<region>.`): it runs on EVERY healthy node, which clients reach through health-aware DNS.
/// Reads are FULLY DISTRIBUTED: every node serves GET/HEAD locally from its own gossip-replicated
/// state, because forwarding them to the leader put a cross-ocean RTT in front of every dashboard
/// fetch and made one far or wedged leader read as "the platform is down". MUTATIONS serialize
/// through the control-plane leader: with auth enforced a mutation without a valid JWT or dashboard
/// API key is rejected BEFORE forwarding (the leader re-verifies either; JWT-only here made API
/// keys silently read-only), then it is served locally on the leader or forwarded over HTTPS,
/// SNI-pinned to a registry IP, failing CLOSED (never plain DNS, which could send the write
/// anywhere) with a 503 rather than applying it here, which would split the brain. `/v1/token` is
/// exempt (stateless HS256 minting, so login never depends on leader reachability) and so are
/// owner-routed paths. A request carrying `x-hive-admin-forwarded` is applied ONLY if this node is
/// (still) the leader: the marker rides the public api host and can be forged, so its presence
/// never places a write on a non-leader. A mutation there gets 503 (the client retries and
/// re-resolves); a forwarded mutation stamped with a control-plane epoch BEHIND ours is fenced (the
/// sender missed a promotion), answering our epoch in `x-hive-cp-epoch-current` so a well-behaved
/// forwarder can max-merge and re-stamp (provably not applied, so the retry is safe; a genuinely
/// superseded sender never converges); reads still serve locally. Entirely dormant unless a node
/// runs with `HIVE_INGRESS!=ngrok` AND `HIVE_JWT_SECRET` set.
async fn admin_ingress(
    cloud: Arc<CloudState>,
    admin: axum::Router,
    api_host: String,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    async fn serve_local(
        admin: axum::Router,
        req: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response {
        match tower::ServiceExt::oneshot(admin, req).await {
            Ok(resp) => resp,
            Err(never) => match never {},
        }
    }
    if req.headers().contains_key("x-hive-admin-forwarded") {
        let is_mutation = is_mutation_request(&req);
        if is_mutation {
            let sender_epoch = req
                .headers()
                .get("x-hive-cp-epoch")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(e) = sender_epoch {
                let ours = cloud.cluster.epoch();
                if e < ours {
                    tracing::warn!(
                        sender_epoch = e,
                        local_epoch = ours,
                        "rejected forwarded mutation with stale control-plane epoch (fenced)"
                    );
                    let mut resp = (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        STALE_EPOCH_BODY,
                    )
                        .into_response();
                    if let Ok(v) = axum::http::HeaderValue::from_str(&ours.to_string()) {
                        resp.headers_mut().insert("x-hive-cp-epoch-current", v);
                    }
                    return resp;
                }
            }
        }
        if cloud.is_control_plane_leader() {
            return serve_local(admin, req).await;
        }
        if is_mutation {
            return not_leader_refusal();
        }
        return serve_local(admin, req).await;
    }
    if crate::auth::enforced() {
        let is_mutation = is_mutation_request(&req);
        let path = req.uri().path();
        // Keep in sync with auth::require_auth's `open` list: these paths authenticate by something
        // other than a platform JWT (zkauth internal secret, per-database or per-project bearer,
        // WebDAV Basic).
        let open = path == "/healthz"
            || path == "/v1/token"
            || path == "/v1/git/webhook"
            || path == "/v1/zkauth/register"
            || path == "/v1/zkauth/preview-proof"
            || path.starts_with("/v1/sqlite/")
            || path.starts_with("/v1/browser-db/")
            || path.starts_with("/v1/drive/webdav/");
        if is_mutation && !open {
            let ok = crate::auth::extract_token(req.headers())
                .map(|t| {
                    crate::auth::verify(&t).is_ok()
                        || crate::auth::api_key_claims(&cloud, &t).is_some()
                })
                .unwrap_or(false);
            if !ok {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token",
                )
                    .into_response();
            }
        }
    }
    let is_mutation = is_mutation_request(&req);
    if !is_mutation || owner_routed(req.uri().path()) || req.uri().path() == "/v1/token" {
        return serve_local(admin, req).await;
    }
    if cloud.is_control_plane_leader() {
        return serve_local(admin, req).await;
    }
    admin_forward_to_leader(&cloud, &api_host, req).await
}

/// The EXACT body a node returns when a forwarded mutation lands on it and it is not the
/// control-plane leader. One constant, three users (the two refusal sites and
/// `is_not_leader_refusal`): a forwarder has to RECOGNISE this answer to route around it, and a
/// drift between emitter and matcher would silently reinstate the dead end this constant exists to
/// close.
const NOT_LEADER_BODY: &str = "not control-plane leader";

/// The refusal itself. Load-bearing: it is emitted BEFORE `next.run` / `serve_local`, so a caller
/// that receives it knows the request was PROVABLY not applied, the only reason re-sending it to
/// another candidate is safe (an ambiguous failure, such as a mid-flight transport error, is never
/// retried).
fn not_leader_refusal() -> axum::response::Response {
    use axum::response::IntoResponse;
    (axum::http::StatusCode::SERVICE_UNAVAILABLE, NOT_LEADER_BODY).into_response()
}

/// True for exactly the response [`not_leader_refusal`] produces.
fn is_not_leader_refusal(status: u16, body: &[u8]) -> bool {
    status == axum::http::StatusCode::SERVICE_UNAVAILABLE.as_u16()
        && std::str::from_utf8(body).is_ok_and(|s| s.trim() == NOT_LEADER_BODY)
}

/// The EXACT body the epoch fence in `admin_ingress`'s forwarded-marker branch returns when a
/// forwarded mutation's sender epoch is behind the receiver's; one constant for the emitter and
/// [`is_stale_epoch_refusal`], the same drift discipline as [`NOT_LEADER_BODY`].
const STALE_EPOCH_BODY: &str = "stale control-plane epoch (ownership changed); retry";

/// True for exactly the response the `admin_ingress` epoch fence produces. Like
/// [`not_leader_refusal`], it is emitted BEFORE the receiver's router runs, so the mutation was
/// provably not applied and re-sending it with a converged epoch is safe.
fn is_stale_epoch_refusal(status: u16, body: &[u8]) -> bool {
    status == axum::http::StatusCode::SERVICE_UNAVAILABLE.as_u16()
        && std::str::from_utf8(body).is_ok_and(|s| s.trim() == STALE_EPOCH_BODY)
}

/// Bounded count of same-candidate re-stamps after a receiver discloses a newer epoch (a bump that
/// landed mid-forward, or one gossip delivered during the round trip). A bump storm faster than the
/// forward RTT, or a sender that can never converge, still ends in the refusal: the fence's
/// protection is a bound, not a single shot.
const MAX_STALE_EPOCH_RETRIES: u32 = 3;

/// A no-redirect reqwest client that resolves `api_host` to a specific leader IP:443, so the
/// forward deterministically hits the leader with a valid SNI and cert (the wildcard/api bundle
/// covers `api_host`); cached per (ip, host). `None` when `ip` does not parse or the client cannot
/// be built, so the caller FAILS CLOSED instead of falling back to plain DNS (which would resolve
/// `api_host` to an arbitrary node and mis-route the forward). The connect timeout is separate
/// from, and far tighter than, the overall request timeout: a candidate whose address black-holes
/// must not burn the whole 30 s budget before the next candidate is tried.
pub(crate) fn leader_client(ip: &str, api_host: &str) -> Option<reqwest::Client> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, reqwest::Client>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = format!("{ip}|{api_host}");
    if let Some(c) = cache.lock().unwrap().get(&key) {
        return Some(c.clone());
    }
    let addr = std::net::SocketAddr::new(ip.parse::<std::net::IpAddr>().ok()?, 443);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(api_host, addr)
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    cache.lock().unwrap().insert(key, client.clone());
    Some(client)
}

/// Ordered (node name, dialable IP) candidates for a leader forward. A list, not one address:
/// `control_plane_leader()` resolves the owner from the CALLING node's own per-observer registry
/// view, and a 30 s gossip gap still makes this node fall through `HIVE_CP_OWNER_CHAIN` to the next
/// entry, which does NOT agree it is the owner and answers the forwarded write with
/// `not control-plane leader`, dead-ending a write the fleet could have accepted (measured: every
/// follower logged 6-9 owner changes in 3 h while the leader logged ZERO, surfacing to browser-node
/// donors as a raw `not control-plane leader`). Order: this node's believed leader first, then the
/// remaining curated chain entries in chain order (the operator-controlled set where ownership can
/// legitimately sit). Addresses come from the LOCAL registry and are deliberately NOT
/// health-filtered (the stale health verdict is the input that just misled us); self is skipped,
/// since a round trip to our own public address can only refuse again.
pub(crate) fn leader_forward_candidates(cloud: &Arc<CloudState>) -> Vec<(String, String)> {
    let nodes = cloud.registry.nodes();
    let addr_of = |name: &str| -> Option<String> {
        nodes
            .iter()
            .find(|n| n.name == name)
            .and_then(|n| n.public_ip.clone().or_else(|| n.public_ip6.clone()))
            .filter(|ip| !ip.trim().is_empty())
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |name: String, ip: String| {
        if name != cloud.node_name && !out.iter().any(|(n, _)| *n == name) {
            out.push((name, ip));
        }
    };
    if let Some(leader) = cloud.leader_node() {
        if let Some(ip) = leader
            .public_ip
            .clone()
            .or_else(|| leader.public_ip6.clone())
        {
            push(leader.name.clone(), ip);
        }
    }
    for entry in crate::cluster::Cluster::owner_chain_from_env() {
        if let Some(ip) = addr_of(&entry) {
            push(entry, ip);
        }
    }
    out
}

/// Forwards an admin MUTATION to the control-plane leader over HTTPS (pinned to a registry IP,
/// SNI/Host = api host), adding the loop-guard header and the epoch fencing token (the receiver
/// refuses the write if that epoch is BEHIND its own, because the sender's view of ownership is
/// stale) and preserving method, path+query, headers (Authorization and Cookie included) and body
/// verbatim, so the leader re-runs the FULL auth path and re-derives the tenant from the caller's
/// own token: this hop grants nothing and asserts no identity. It walks
/// [`leader_forward_candidates`] and moves to the next on exactly three provably-not-applied
/// outcomes: the `not control-plane leader` refusal, the stale-epoch refusal (retried in place
/// after adopting the epoch the receiver discloses, so a mid-flight bump converges
/// deterministically instead of racing gossip, bounded by [`MAX_STALE_EPOCH_RETRIES`], then walked
/// like any other refusal, since epoch views are per-observer like health) and a CONNECT failure
/// (nothing was sent). A refusal is kept as the answer of last resort. Any other answer, a
/// mid-flight transport error included, is returned as-is, because re-sending a mutation that may
/// already have been applied is worse than surfacing the failure. With every candidate refused or
/// none dialable it fails CLOSED: it never applies the mutation locally, the split-brain the
/// single-writer rule exists to prevent.
async fn admin_forward_to_leader(
    cloud: &Arc<CloudState>,
    api_host: &str,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (axum::http::StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response()
        }
    };
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let url = format!("https://{api_host}{path_q}");
    let method = match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            return (axum::http::StatusCode::METHOD_NOT_ALLOWED, "bad method").into_response()
        }
    };
    let mut cp_epoch = cloud.cluster.epoch();
    let candidates = leader_forward_candidates(cloud);
    let last = candidates.len().saturating_sub(1);
    let mut refused: Option<axum::response::Response> = None;
    let mut stale_retries = 0u32;
    'candidates: for (i, (name, ip)) in candidates.iter().enumerate() {
        let Some(client) = leader_client(ip, api_host) else {
            continue;
        };
        loop {
            let mut rb = client
                .request(method.clone(), &url)
                .header("x-hive-admin-forwarded", "1")
                .header("x-hive-cp-epoch", cp_epoch.to_string());
            for (k, v) in parts.headers.iter() {
                let n = k.as_str().to_ascii_lowercase();
                if matches!(n.as_str(), "host" | "content-length" | "connection") {
                    continue;
                }
                rb = rb.header(k, v);
            }
            rb = rb
                .header(reqwest::header::HOST, api_host)
                .body(body.to_vec());
            match rb.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let disclosed_epoch = resp
                        .headers()
                        .get("x-hive-cp-epoch-current")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    let mut out = axum::http::Response::builder().status(status);
                    for (k, v) in resp.headers().iter() {
                        let n = k.as_str().to_ascii_lowercase();
                        if matches!(
                            n.as_str(),
                            "connection" | "transfer-encoding" | "content-length"
                        ) {
                            continue;
                        }
                        out = out.header(k.as_str(), v.as_bytes());
                    }
                    let bytes = resp.bytes().await.unwrap_or_default();
                    if is_not_leader_refusal(status, &bytes) {
                        tracing::warn!(
                            candidate = %name,
                            path = %path_q,
                            remaining = last.saturating_sub(i),
                            "leader forward refused: candidate is not the control-plane leader"
                        );
                        refused = Some(
                            (axum::http::StatusCode::SERVICE_UNAVAILABLE, NOT_LEADER_BODY)
                                .into_response(),
                        );
                        continue 'candidates;
                    }
                    if is_stale_epoch_refusal(status, &bytes) {
                        if let Some(e) = disclosed_epoch {
                            cloud.cluster.adopt_epoch(e);
                        }
                        let now = cloud.cluster.epoch();
                        if now > cp_epoch && stale_retries < MAX_STALE_EPOCH_RETRIES {
                            stale_retries += 1;
                            cp_epoch = now;
                            tracing::warn!(
                                candidate = %name,
                                path = %path_q,
                                adopted_epoch = now,
                                attempt = stale_retries,
                                "leader forward fenced on a stale epoch; adopted receiver epoch, retrying"
                            );
                            continue;
                        }
                        refused = Some(
                            (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                STALE_EPOCH_BODY,
                            )
                                .into_response(),
                        );
                        continue 'candidates;
                    }
                    return out.body(axum::body::Body::from(bytes)).unwrap_or_else(|_| {
                        (axum::http::StatusCode::BAD_GATEWAY, "bad gateway").into_response()
                    });
                }
                Err(e) if e.is_connect() => {
                    tracing::warn!(candidate = %name, error = %e, "leader forward could not connect");
                    continue 'candidates;
                }
                Err(_) => {
                    return (
                        axum::http::StatusCode::BAD_GATEWAY,
                        "control-plane leader forward failed",
                    )
                        .into_response()
                }
            }
        }
    }
    refused.unwrap_or_else(|| {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "control-plane leader unreachable",
        )
            .into_response()
    })
}

/// Minimal streaming reverse proxy for the dashboard hosts: forwards method, path+query, headers
/// (sans hop-by-hop and Host, since the upstream needs ITS OWN Host to route, e.g. an ngrok origin)
/// and body, and streams the response back. REDIRECTS ARE NEVER FOLLOWED server-side (a dedicated
/// no-redirect client): auth flows such as the Clerk dev-browser handshake must bounce the BROWSER.
/// 3xx Location values (raw and percent-encoded inside query params like Clerk's redirect_url) that
/// reference the upstream origin are rewritten to the public host, so the user stays on shadw.cloud
/// through the auth loop.
async fn dashboard_proxy(
    http: &reqwest::Client,
    upstream: &str,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let _ = http;
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
    let upstream_host = upstream
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let url = format!("{upstream}{path_q}");
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (axum::http::StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response()
        }
    };
    let method = match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            return (axum::http::StatusCode::METHOD_NOT_ALLOWED, "bad method").into_response()
        }
    };
    let mut rb = http.request(method, &url).body(body.to_vec());
    for (k, v) in parts.headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "transfer-encoding"
                | "content-length"
                | "upgrade"
                | "keep-alive"
                | "x-forwarded-host"
                | "x-forwarded-proto"
        ) {
            continue;
        }
        rb = rb.header(k, v);
    }
    // x-forwarded-host/proto tell the app the PUBLIC origin; Host is left to the upstream's own
    // authority: an ngrok origin routes ON its Host header, and forcing the public domain 421'd
    // every dashboard-host request.
    rb = rb
        .header("x-forwarded-host", &public_host)
        .header("x-forwarded-proto", "https");
    match rb.send().await {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let mut builder = axum::http::Response::builder().status(status);
            for (k, v) in resp.headers().iter() {
                let name = k.as_str().to_ascii_lowercase();
                // content-encoding is NOT stripped and accept-encoding IS forwarded upstream: they
                // are one change. Stripping the request header served every dashboard byte
                // uncompressed while still emitting Vary: Accept-Encoding, and stripping only the
                // label would hand the browser brotli bytes labelled plaintext. Safe because the
                // workspace reqwest is built WITHOUT gzip/brotli, so it never decompresses behind
                // our back; the body streams verbatim and content-length/transfer-encoding are
                // still dropped.
                if matches!(
                    name.as_str(),
                    "connection" | "transfer-encoding" | "content-length"
                ) {
                    continue;
                }
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
                .unwrap_or_else(|_| {
                    (axum::http::StatusCode::BAD_GATEWAY, "proxy build failed").into_response()
                })
        }
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            format!("dashboard origin unreachable: {e}"),
        )
            .into_response(),
    }
}

/// Derives a stable, human-readable region id from a node's geolocation (e.g. "los-angeles"), so a
/// node's region reflects where it actually is; co-located nodes share the id, and it falls back to
/// "local" when geolocation is unavailable (offline).
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

/// Terminates HTTPS for the gateway: a real cert/key (PEM) from `HIVE_TLS_CERT` + `HIVE_TLS_KEY`
/// when both are set (production), else a self-signed cert for `localhost`/`*.localhost` (local
/// dev).
async fn serve_tls(app: axum::Router, addr: SocketAddr) -> anyhow::Result<()> {
    use axum_server::tls_rustls::RustlsConfig;
    install_ring_crypto_provider();

    let config = match (
        std::env::var("HIVE_TLS_CERT"),
        std::env::var("HIVE_TLS_KEY"),
    ) {
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
    match std::env::var("HIVE_PUBLIC_IP")
        .ok()
        .map(|s| s.trim().to_string())
    {
        Some(v) if v.eq_ignore_ascii_case("auto") => detected
            .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
            .filter(is_public_v4)
            .map(|ip| ip.to_string()),
        Some(v) if !v.is_empty() => v
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
            .map(|ip| ip.to_string()),
        _ => None,
    }
}

/// Resolves this node's Tencent VPC-private address for CCN inter-region traffic from
/// `HIVE_PRIVATE_ADDR` ONLY, never sniffed off an interface: an RFC1918 address existing on a host
/// says nothing about whether anyone else can dial it (a Docker/k8s/bridge/TUN interface produces
/// private addresses that are NOT the Tencent VPC NIC; the fleet learned this from `dht`'s
/// seed-address OOM incident), so an explicit operator value is the only source of truth, the same
/// posture `HIVE_PUBLIC_IP` takes. Accepts a bare IPv4 (paired with `HIVE_IROH_PORT`: the private
/// candidate must name a real port a peer can dial, so a missing `HIVE_IROH_PORT` makes a bare-IP
/// override inert rather than guessing) or a full `ip:port`. Unset gives `None`, the safe default:
/// this node never becomes a CCN-private dial target.
fn resolve_private_addr() -> Option<String> {
    let raw = std::env::var("HIVE_PRIVATE_ADDR").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(sa) = raw.parse::<std::net::SocketAddr>() {
        return hive_p2p::private_path::is_safe_private_candidate(sa.ip()).then(|| sa.to_string());
    }
    let ip = raw.parse::<std::net::IpAddr>().ok()?;
    if !hive_p2p::private_path::is_safe_private_candidate(ip) {
        return None;
    }
    let port = std::env::var("HIVE_IROH_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)?;
    Some(std::net::SocketAddr::new(ip, port).to_string())
}

/// Keeps this node's gossiped capacity figures current: a boot-time-only `disk_free_gb` goes stale
/// in exactly the direction that matters (a node fills up, keeps advertising the space it had at
/// boot and keeps winning deployments it cannot host), so each tick re-reads free disk (one
/// `statvfs`-class read, no CPU sampling) and free GPU memory, and re-observes the
/// runtime-capability tuple (wasm and bun runtime, runtime-artifact protocol, build toolchains:
/// rootfs/proof publication and runtime installation can move under a running process) from one
/// selected-backend source, publishing it under ONE registry write lock so gossip never sees a torn
/// verdict. The musqld binary can likewise be installed or swapped under a running process, so it
/// is probed only on nodes that opted into that engine (the fleet does not spawn a process every
/// tick for a capability nobody uses), and the restart audit is re-published too because its 24 h
/// window SLIDES (a node would keep advertising an OOM it had 25 h ago, or none while cycling).
/// `HIVE_DISK_REFRESH_SECS` (default 30) tunes it; 0 disables.
fn spawn_disk_refresh(
    registry: Arc<hive_edge::NodeRegistry>,
    runtime_capability_source: resources::RuntimeCapabilitySource,
) {
    let interval = Duration::from_secs(env_u64("HIVE_DISK_REFRESH_SECS", 30));
    if interval.is_zero() {
        return;
    }
    crate::supervise::spawn_supervised("disk-refresh", move || {
        let registry = registry.clone();
        let runtime_capability_source = runtime_capability_source.clone();
        async move {
            loop {
                tokio::time::sleep(interval).await;
                crate::supervise::beat("disk-refresh");
                registry.set_self_disk_free(crate::resources::disk_free_gb());
                registry.set_self_gpu_free(crate::resources::measured_gpu_free_mb());
                let runtime_capabilities = runtime_capability_source.detect().await;
                registry.set_self_runtime_capabilities(
                    runtime_capabilities.wasm_runtime,
                    runtime_capabilities.bun_runtime,
                    runtime_capabilities.runtime_artifact_protocol,
                    runtime_capabilities.build_toolchains.clone(),
                );
                if crate::musql::default_engine() == crate::musql::Engine::Musql {
                    registry.set_self_musql_engine(crate::musql::probe());
                }
                registry.set_self_restart_audit(
                    crate::restart_audit::started_ms(),
                    crate::restart_audit::oom_restarts_24h(),
                    crate::restart_audit::unclean_restarts_24h(),
                    crate::restart_audit::last_oom_ms(),
                );
            }
        }
    });
}

/// Re-geolocates every `HIVE_GEO_REFRESH_SECS` (default 3600, 0 disables) so a machine that MOVES
/// (a laptop node, or a cloud host whose ISP re-homes its IP) does not report a stale location
/// until the next restart. `HIVE_GEO` still wins unconditionally on every call (`geolocate()`'s
/// first check), so a manually pinned node is unaffected; this only helps the auto-detected case,
/// and it runs even with `--region` pinned because lat/lon still matter for DNS-nearest accuracy.
/// It never updates `region`, the node's stable identity (see `NodeRegistry::set_self_geo`), and
/// writes only when the position moved by at least `HIVE_GEO_REFRESH_MIN_KM` (default 50 km), so a
/// routine BGP reroute within the same city does not churn the registry every tick; a node with no
/// prior geo (boot geolocation failed, e.g. offline) accepts any successful lookup.
fn spawn_geo_refresh(registry: Arc<hive_edge::NodeRegistry>) {
    let interval = Duration::from_secs(env_u64("HIVE_GEO_REFRESH_SECS", 3600));
    if interval.is_zero() {
        return;
    }
    let min_km = std::env::var("HIVE_GEO_REFRESH_MIN_KM")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .unwrap_or(50.0);
    crate::supervise::spawn_supervised("geo-refresh", move || {
        let registry = registry.clone();
        async move {
            loop {
                tokio::time::sleep(interval).await;
                crate::supervise::beat("geo-refresh");
                let Some((lat, lon, city, country, _ip)) = geolocate().await else {
                    continue;
                };
                let (prev_lat, prev_lon) = {
                    let me = registry.me();
                    (me.lat, me.lon)
                };
                let moved = match (prev_lat, prev_lon) {
                    (Some(plat), Some(plon)) => {
                        hive_edge::haversine_km((plat, plon), (lat, lon)) >= min_km
                    }
                    _ => true,
                };
                if moved {
                    tracing::info!(city = %city, country = %country, lat, lon, "node re-geolocated — position moved materially, updating registry");
                    registry.set_self_geo(lat, lon, city, country);
                }
            }
        }
    });
}

/// Best-effort IP geolocation at startup, `(lat, lon, city, country, public_ip)`, through
/// ip-api.com with a short timeout; `None` on any failure, so a node always boots even offline.
/// `HIVE_GEO="lat,lon,city,country"` overrides it. The 5th element is the detected external IP,
/// used only when `HIVE_PUBLIC_IP=auto` (see `resolve_public_ip`).
async fn geolocate() -> Option<(f64, f64, String, String, Option<String>)> {
    if let Ok(manual) = std::env::var("HIVE_GEO") {
        let parts: Vec<&str> = manual.splitn(4, ',').collect();
        if parts.len() == 4 {
            if let (Ok(lat), Ok(lon)) = (parts[0].trim().parse(), parts[1].trim().parse()) {
                return Some((
                    lat,
                    lon,
                    parts[2].trim().to_string(),
                    parts[3].trim().to_string(),
                    None,
                ));
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
        v.get("query")
            .and_then(|q| q.as_str())
            .map(|s| s.to_string()),
    ))
}

async fn serve(router: axum::Router, addr: SocketAddr, label: &str) -> anyhow::Result<()> {
    let mut listeners = vec![tokio::net::TcpListener::bind(addr).await?];
    tracing::info!(%addr, "{label} listening");
    // For loopback, ALSO bind the other IP family: browsers resolve *.localhost to ::1 first, so a
    // v4-only bind makes deploys unreachable in the browser while curl on 127.0.0.1 works.
    if addr.ip().is_loopback() {
        let alt: SocketAddr = match addr.ip() {
            std::net::IpAddr::V4(_) => (std::net::Ipv6Addr::LOCALHOST, addr.port()).into(),
            std::net::IpAddr::V6(_) => (std::net::Ipv4Addr::LOCALHOST, addr.port()).into(),
        };
        match tokio::net::TcpListener::bind(alt).await {
            Ok(l) => {
                tracing::info!(%alt, "{label} also listening (dual-stack loopback)");
                listeners.push(l);
            }
            Err(e) => tracing::warn!(%alt, error=%e, "{label} could not bind alt loopback address"),
        }
    }
    let mut tasks = Vec::new();
    for l in listeners {
        let r = router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        tasks.push(tokio::spawn(async move { axum::serve(l, r).await }));
    }
    for t in tasks {
        t.await??;
    }
    Ok(())
}

/// Invoke a deployment route on this node's own public gateway (used by cron &
/// workflows). `deployment` is the Host subdomain (project or deployment id).
///
/// `path` is TENANT input (a cron job's path, a workflow step's path) joined onto the node's own
/// trusted `public_base`. A raw `format!("{base}{path}")` is vulnerable to the userinfo/authority
/// class of bug a URL parser later reinterprets: `path` beginning `@host:port/...` turns `base`
/// into userinfo and `host:port` into the REAL destination (confirmed live: a workflow step path
/// made the node GET an internal listener and returned its body in the run output), and one
/// beginning `//host/...` is a valid "network-path reference" that replaces the authority outright
/// under ordinary URL-relative-resolution rules. `Url::join` prevents the first (a plain relative
/// path's `@`/`#`/`:` are not re-parsed as authority syntax), and the explicit same-origin check
/// below closes the second.
async fn invoke(
    cloud: &Arc<CloudState>,
    deployment: &str,
    path: &str,
) -> anyhow::Result<(u16, String)> {
    let base = reqwest::Url::parse(&cloud.public_base)
        .map_err(|e| anyhow::anyhow!("invalid public_base: {e}"))?;
    let url = base
        .join(path)
        .map_err(|e| anyhow::anyhow!("invalid step/cron path: {e}"))?;
    anyhow::ensure!(
        url.scheme() == base.scheme()
            && url.host_str() == base.host_str()
            && url.port_or_known_default() == base.port_or_known_default(),
        "step/cron path must stay on this node's own gateway"
    );
    let resp = hive_outbound::same_origin_client()
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
        let fut: Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> =
            Box::pin(async move {
                let (status, body) = invoke(&cloud, &step.deployment, &step.path).await?;
                anyhow::ensure!(status < 500, "step {} -> HTTP {status}", step.name);
                Ok(format!(
                    "HTTP {status}: {}",
                    body.chars().take(200).collect::<String>()
                ))
            });
        fut
    })
}

/// Runs the sandbox idle-timeout reap every `HIVE_SANDBOX_REAP_SECS` (default 30, 0 disables) and
/// persists only on a tick that actually stopped something; without this loop a sandbox's
/// `timeout_ms` would be decorative. `reap_expired` carries the contract: only the OWNER reaps, and
/// a sandbox with a command in flight or an open shell is busy, not idle.
fn spawn_sandbox_timeout_reap_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_SANDBOX_REAP_SECS", 30));
    if interval.is_zero() {
        return;
    }
    crate::supervise::spawn_supervised("sandbox-timeout-reap", move || {
        let cloud = cloud.clone();
        async move {
            loop {
                tokio::time::sleep(interval).await;
                crate::supervise::beat("sandbox-timeout-reap");
                if cloud.sandboxes.reap_expired().await > 0 {
                    crate::persist::persist(&cloud);
                }
            }
        }
    });
}

/// A peer is a live container-lease holder when seen within this window; short on purpose, for fast
/// failover.
const LEASE_LIVE_WINDOW_MS: u64 = 12_000;

/// Container placement, every 3 s: for each container deployment this node holds, it computes the
/// preferred owner by rendezvous hash (`lease::hrw_owner`) over the LIVE holders (self plus the
/// peers that gossiped they hold it, seen within [`LEASE_LIVE_WINDOW_MS`]) and either
/// acquires/renews our fenced lease (when we are preferred) or releases it (so the preferred node
/// can take over). Every node sees the same gossiped holders and regions, so all of them compute
/// the same owner. A region-pinned container (`regions: ["virginia"]`) is elected only among
/// holders in an allowed region, so a holder outside it cannot win the hash and serve from the
/// wrong region; with NO holder in an allowed region the unconstrained set is used (availability
/// beats strict pinning when mis-placed). Each pass also drains superseded deployments' keep-warm
/// pools, so only a project's production deployment stays warm and the rest scale to zero.
fn spawn_lease_loop(cloud: Arc<CloudState>) {
    use std::collections::{HashMap, HashSet};
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            cloud.gw.reconcile_keepwarm();
            let self_id = cloud.node_name.clone();
            let region = cloud.region.clone();
            let now = now_ms();
            let nodes_now = cloud.registry.nodes();
            let live: HashSet<String> = nodes_now
                .iter()
                .filter(|n| n.is_self || now.saturating_sub(n.last_seen_ms) < LEASE_LIVE_WINDOW_MS)
                .map(|n| n.id.clone())
                .collect();
            let node_region: HashMap<String, String> = nodes_now
                .iter()
                .map(|n| (n.id.clone(), n.region.clone()))
                .collect();
            let holders = cloud.container_holders.read().clone();
            for key in cloud.gw.container_projects() {
                let mut live_holders: Vec<String> = holders.get(&key).cloned().unwrap_or_default();
                if !live_holders.contains(&self_id) {
                    live_holders.push(self_id.clone());
                }
                live_holders.retain(|n| live.contains(n));
                if live_holders.is_empty() {
                    live_holders.push(self_id.clone());
                }
                let regions = cloud.projects.get(&key).functions.regions;
                if !regions.is_empty() {
                    let allowed: Vec<String> = live_holders
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
                        live_holders = allowed;
                    }
                }
                match crate::lease::hrw_owner(&key, &live_holders) {
                    Some(pref) if pref == self_id => {
                        if let Some(l) = cloud
                            .leases
                            .acquire_or_renew(&key, &self_id, &region, 10_000)
                        {
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

/// How long boot waits for the cell-orphan reap's urgent half (listing +
/// SIGKILL of duplicate writers of a shared volume) before serving anyway.
const CELL_REAP_BOOT_WAIT: Duration = Duration::from_secs(20);

/// The boot reap's one journal line (`cell orphan reap: reaped=N per_volume={..}`): WARN whenever
/// anything was removed, skipped, left behind or left uncovered.
fn log_cell_reap(report: &hive_backend::ReapReport, elapsed: Duration) {
    if let Some(reason) = &report.skipped {
        tracing::warn!(
            reason = %reason,
            "cell orphan reap: SKIPPED -- nothing was removed and orphan_reap_ran() stays false; \
             every volume-mounting launch keeps running the per-volume guard"
        );
        return;
    }
    let quiet = report.stale == 0
        && report.confirmed
        && report.foreign == 0
        && report.legacy_kept == 0
        && report.gap.is_none();
    if quiet {
        tracing::info!(
            listed = report.listed,
            elapsed_ms = elapsed.as_millis() as u64,
            "cell orphan reap: reaped=0 per_volume={{}} (no cell of this node from another boot)"
        );
        return;
    }
    tracing::warn!(
        listed = report.listed,
        stale = report.stale,
        killed = report.killed,
        stopped = report.stopped,
        foreign_untouched = report.foreign,
        legacy_kept = report.legacy_kept,
        confirmed = report.confirmed,
        survivors = ?report.survivors,
        gap = report.gap.as_deref().unwrap_or(""),
        elapsed_ms = elapsed.as_millis() as u64,
        "cell orphan reap: reaped={} per_volume={:?}",
        report.reaped,
        report.per_volume
    );
}

/// Every 3 s re-resolves the control-plane owner from the live registry (`Cluster::observe_owner`
/// inside bumps the fencing epoch exactly on a real ownership change) and gossips our current
/// epoch, so the fleet's fencing tokens converge on the max witnessed; it also samples every
/// leader-only job's ownership continuity (`leadership::observe`), so the `may_act` tenure is
/// measured at this cadence and not only when a slow job happens to ask.
fn spawn_cluster_loop(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = cloud.control_plane_leader();
            cloud.registry.set_self_cp_epoch(cloud.cluster.epoch());
            leadership::observe(&cloud);
        }
    });
}

/// Periodic GuardianDB snapshot re-assert. `persist()` -> `guardian::replicate` only fires on a
/// STATE MUTATION, so a node quiet since its last restart never writes its own
/// `node/<name>/snapshot` key (the observed gap: nodes with no tenant activity since restart had no
/// snapshot anywhere in the shared KV). This loop captures and replicates on a fixed cadence
/// (`HIVE_GUARDIAN_SNAPSHOT_SECS`, 120), after a 30 s delay so first-boot restore settles, so every
/// node's snapshot is always PRESENT and FRESH; it is cheap at the blob layer (iroh-docs
/// content-addresses by BLAKE3, so identical bytes store no new blob). Capture clones and
/// serializes the whole platform state (16-30 MB on the leader: 3-5 s of CPU), so it runs on the
/// bulkhead runtime under `block_in_place`, never on a serving worker.
fn spawn_guardian_snapshot_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_GUARDIAN_SNAPSHOT_SECS", 120));
    crate::bulkhead::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let c = cloud.clone();
            tokio::task::block_in_place(move || {
                let snap = crate::persist::capture(&c);
                crate::guardian::replicate(&snap);
            });
            tokio::time::sleep(interval).await;
        }
    });
}

/// Periodic reap of departed nodes' `node/<name>/snapshot` keys. LEADER-ONLY, unlike the snapshot
/// loop (which writes only this node's OWN key, so every node runs it): this one DELETES other
/// nodes' keys from the replicated store, a fan-out mutation the single-writer discipline routes
/// through the control-plane leader (`may_act(GuardianReap)`, re-evaluated every pass, so a
/// leadership change mid-run hands the job over). It starts after a deliberately long delay (300 s:
/// at boot this node has not resynced the replicated doc, so an immediate pass would judge
/// departure from an incomplete view) and is inert until `HIVE_NODE_ROSTER` is configured (see
/// `guardian::reap_departed_node_snapshots`: an unset roster means "do nothing", never "everything
/// looks departed"); `HIVE_REAP_SECS=0` disables it.
fn spawn_guardian_reap_loop(cloud: Arc<CloudState>) {
    let secs = env_u64("HIVE_REAP_SECS", 6 * 60 * 60);
    if secs == 0 {
        return;
    }
    let interval = Duration::from_secs(secs);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            if leadership::may_act(&cloud, leadership::Job::GuardianReap) {
                let (reaped, withheld) = crate::guardian::reap_departed_node_snapshots().await;
                if reaped > 0 {
                    tracing::warn!(
                        reaped,
                        withheld,
                        "guardian reap: retired departed node snapshot keys"
                    );
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Periodic safety-net `persist()` for the metrics hour/day rollups (`metrics.rs`'s
/// `RollupSnapshot`, the only durable slice of `MetricsStore`): `state.rs`'s `record()` runs on
/// every HTTP request and never persists, so a tenant with real traffic but no OTHER admin mutation
/// since the last persist could lose its whole Weekly/Monthly usage history on an UNCLEAN shutdown
/// (crash, OOM kill, `kill -9`); the graceful SIGTERM flush covers only clean restarts. `persist()`
/// just marks a dirty generation and wakes the coalescing writer, so calling it on a timer
/// (`HIVE_METRICS_PERSIST_SECS`, 120) is cheap, like `spawn_guardian_snapshot_loop`'s periodic
/// flush.
fn spawn_metrics_persist_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_METRICS_PERSIST_SECS", 120));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            crate::persist::persist(&cloud);
        }
    });
}

/// Periodic podman lock-headroom sweep: containers AND volumes each consume one lock from a fixed
/// pool (default 2048), so a leak starves the whole HOST (witnessed: 2032 leaked volumes on the
/// leader, freeLocks 0, every deployment's cold start on that node failing as 503
/// CAPACITY_EXHAUSTED). The leak itself is fixed (`podman rm -v`) and the run path self-heals
/// reactively; this sweep restores headroom BEFORE a request pays for it and covers locks leaked by
/// anything outside that path (a crashed node, a manual `podman run`). It takes no `CloudState` (it
/// reads the HOST's podman state), runs on every node because the lock pool is per-host, and
/// `HIVE_LOCK_SWEEP_SECS=0` disables it.
fn spawn_container_lock_sweep() {
    let secs = env_u64("HIVE_LOCK_SWEEP_SECS", 300);
    if secs == 0 {
        return;
    }
    let interval = Duration::from_secs(secs);
    crate::supervise::spawn_supervised("container-lock-sweep", move || async move {
        loop {
            crate::supervise::beat("container-lock-sweep");
            let path_env = crate::git::podman_path_env();
            match hive_backend::sweep_container_locks(&path_env).await {
                Some((free, 0, 0)) => tracing::debug!(free_locks = free, "podman lock headroom ok"),
                Some((free, vols, cells)) => tracing::warn!(
                    free_locks_before = free,
                    volumes_reclaimed = vols,
                    cells_reclaimed = cells,
                    "reclaimed leaked podman locks"
                ),
                None => {}
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Warn at most once an hour per skipped job id: an orphan fires on every schedule, and one log
/// line per fire is the exact flood the skip exists to remove.
fn orphan_cron_warn_allowed(job_id: &str) -> bool {
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    let now = now_ms();
    let mut warned = LAST
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if now.saturating_sub(warned.get(job_id).copied().unwrap_or(0)) < 3_600_000 {
        return false;
    }
    warned.insert(job_id.to_string(), now);
    true
}

/// Ticks the cron scheduler every second and fires each due job as its own task. A project's cron
/// kill switch only stops the firing: the schedule keeps advancing, so jobs are still created,
/// updated and deleted on deploy, matching Vercel's semantics.
fn spawn_cron_loop(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let due = cloud.cron.tick(now_ms());
            if due.is_empty() {
                continue;
            }
            // Orphan guard: a job whose project has no deployment record in this node's
            // (replicated) deployment view can never be served, so `invoke` would resolve it to
            // 503 `no-deployment` / `deployment-not-ready` on every single fire, forever — the
            // `shoomoo` orphan. Skip it instead of recording that noise; the schedule still
            // advances, so a project that regains a deployment resumes firing untouched.
            //
            // Skipping is deliberately NOT deletion: a node that has just booted has not adopted
            // the deployment store yet, so its view is legitimately empty for a while and dropping
            // jobs here would wipe a valid set. Removal belongs to the project-delete path, which
            // is authoritative.
            let live: std::collections::HashSet<String> = cloud
                .gw
                .list()
                .iter()
                .map(|d| d.project.to_ascii_lowercase())
                .chain(
                    cloud
                        .peer_deployments
                        .read()
                        .values()
                        .flatten()
                        .map(|d| d.project.to_ascii_lowercase()),
                )
                .collect();
            for job in due {
                if !cloud.projects.cron_enabled(&job.deployment) {
                    continue;
                }
                let orphan = !live.contains(&job.deployment.to_ascii_lowercase());
                if orphan && orphan_cron_warn_allowed(&job.id) {
                    tracing::warn!(
                        job = %job.id,
                        project = %job.deployment,
                        path = %job.path,
                        "cron fire skipped: project has no deployment record, nothing can serve it"
                    );
                }
                if orphan {
                    continue;
                }
                let cloud = cloud.clone();
                tokio::spawn(async move {
                    let res = invoke(&cloud, &job.deployment, &job.path).await;
                    let (status, detail) = match res {
                        Ok((s, _)) => (s, format!("cron {} -> {s}", job.name)),
                        Err(e) => (0, format!("cron {} error: {e}", job.name)),
                    };
                    let ev = cloud.event(
                        &cloud.region,
                        "CRON",
                        &job.deployment,
                        &job.path,
                        status,
                        "cron",
                        &detail,
                    );
                    cloud.record(ev);
                });
            }
        }
    });
}

/// Loop-local partials returned by one peer's gossip sync, merged by whichever gossip round
/// receives it (the direct cloud.* store writes already happened inside the task, through
/// internally-synchronized stores). The loop keeps each target's last ANSWERED sync so it can stand
/// in while the next one is in flight; see [`round_contributions`].
#[derive(Default, Clone)]
struct PeerSync {
    /// (host, route) pairs learned from this peer's serve-hosts.
    routes: Vec<(String, crate::state::PeerRoute)>,
    /// `routes` is the peer's COMPLETE host list. False when the peer reported
    /// its route set unchanged and sent none — its previously published routes
    /// then stay in the table and only their liveness/transport is refreshed.
    routes_full: bool,
    /// The peer's route generation, present on both a full and an unchanged
    /// answer; remembered so the next round can ask for a delta.
    route_gen: Option<u64>,
    /// (region, gateway, latency_ms) of the peer — carried even on an unchanged
    /// answer so existing routes keep a current transport.
    meta: Option<(String, String, u64)>,
    /// This peer's fleet deployments (node_id, list) for the dashboard view.
    fleet: Option<(String, Vec<fluid_core::DeploymentInfo>)>,
    /// (container key, holder node_id) pairs for lease election.
    holders: Vec<(String, String)>,
    /// The peer's node id, if it was reached this round (drives the route TTL merge).
    seen: Option<String>,
    /// The node id the peer's own roster entry carried (its `NodeInfo.id`).
    identity: Option<String>,
    /// The peer's public zkauth roster export.
    #[cfg(feature = "zkauth")]
    zk_roster: Option<serde_json::Value>,
    /// The target answered at least one gossip request this round (drives the
    /// dead-target backoff in `gossip_round`).
    reached: bool,
}

impl PeerSync {
    /// The node this sync describes: its self-reported id, else the node its
    /// serve-hosts or fleet-deployments answer named.
    fn node(&self) -> Option<&str> {
        self.identity
            .as_deref()
            .or(self.seen.as_deref())
            .or(self.fleet.as_ref().map(|(n, _)| n.as_str()))
    }
}

/// The syncs one gossip round merges. `answers` holds each target's newest ANSWERED sync (with the
/// round it was dispatched in); a target contributes it when it landed this round, or when its next
/// sync is still in flight and its node is still in the registry (`alive`, the fleet-deployments
/// carry rule): a live peer whose sync outlasts the deadline is late, not absent, so its holders,
/// routes, deployments and zkauth roster never drop out of the per-round rebuilds on a slow round.
/// A target that failed contributes nothing. One sync per NODE, newest round first (ties: lowest
/// target key), because a seed is gossiped under both `seed:<eid>` and `<eid>` and the two answers
/// can come from different rounds.
fn round_contributions<'a>(
    answers: &'a std::collections::HashMap<String, (u64, PeerSync)>,
    landed: &std::collections::HashSet<String>,
    in_flight: &std::collections::HashSet<String>,
    alive: &std::collections::HashSet<String>,
) -> Vec<&'a PeerSync> {
    let mut by_node: std::collections::HashMap<&str, (u64, &str, &PeerSync)> =
        std::collections::HashMap::new();
    for (target, (round, sync)) in answers {
        let node = sync.node().unwrap_or(target.as_str());
        if !landed.contains(target) && !(in_flight.contains(target) && alive.contains(node)) {
            continue;
        }
        let newer = by_node
            .get(node)
            .is_none_or(|(r, t, _)| *round > *r || (*round == *r && target.as_str() < *t));
        if newer {
            by_node.insert(node, (*round, target.as_str(), sync));
        }
    }
    by_node.into_values().map(|(_, _, sync)| sync).collect()
}

/// Whether a gossip target string is a real HTTP(S) admin URL, the only thing `node_admins` may
/// hold: the deploy dispatcher (`git.rs`, `schedule.rs`) treats every entry as reachable via HTTP
/// and POSTs the fan-out to `{admin}` plus `RUNTIME_ARTIFACT_FANOUT_PATH`, while a peer reached
/// over iroh has a 64-hex endpoint id (or a `seed:` key) as its gossip key. Storing that poisons
/// `node_admins`, so every deploy placed on the node fails with a reqwest "builder error" (an
/// unparseable URL) instead of falling back to the working iroh dispatch route.
fn is_http_admin_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Syncs ONE peer: announces ourselves, learns its nodes/routes/deployments and converges
/// enterprise/lease state. All cloud.* writes go through internally-synchronized stores, so many of
/// these run concurrently safely; loop-local data (zkauth roster included) is returned as a
/// [`PeerSync`] merged by whichever gossip round receives it (the round does not wait past its
/// deadline).
///
/// HOT-JOIN (client side): a gossip failure may mean we are not yet in the peer's trust set (first
/// contact of a brand-new node, or a wiped trust roster), so with the fleet secret configured we
/// present a join proof, `HMAC(secret, OUR endpoint id)`, over the dedicated join stream, dialing
/// by KEY (the eid/seed mapping in `peer_iroh`; `fetch()` may have evicted it, so it is re-derived
/// from the target string, which IS the endpoint id or `seed:<id>`, and restored on admission). On
/// admission the reply is the peer's node roster, consumed like `/v1/nodes`. It is never tried when
/// the announce's own DIAL failed this round: the join dials the same key through the same
/// `acquire`, so against a dead seed it only doubled the round's cost (4 serial 5 s dial timeouts
/// per round for a powered-off node), while a trust refusal happens AFTER the connection is up and
/// so never reads as `Unreachable`. Its timeout MUST be at least
/// `hive_p2p::dial_fallback_ceiling()`: a first-contact peer's cached hint (private IPs from its
/// gossiped addr_json) is near-guaranteed to fail, so the join depends on `acquire`'s
/// fresh-discovery fallback running to completion instead of being cancelled mid-flight.
///
/// Roster handling: the answering peer's first entry is its OWN self-report, the only copy allowed
/// to rename it (`upsert_peer_self_report`); everything after it is a relayed third-party copy that
/// never renames. Each round also converges the control-plane fencing epoch on the max witnessed
/// anywhere in the fleet (monotonic, see `cluster.rs`) and registers or clears every peer's Tencent
/// CCN private-path candidate on the live `PeerPool`. That only informs the NEXT fresh dial (a
/// reused trunk is untouched, a live connection is never torn down) and is gated end to end by
/// `is_private_path_candidate` (both sides declare `provider = tencent` and the region pair is
/// configured), so without CCN configuration it is a no-op. The clear branch must run on the peer's
/// `eid` alone, not also on `private_addr` being present, or a peer that stops reporting one leaks
/// its entry forever; candidates of peers that left the registry are reaped each round for the same
/// reason (the per-peer loop only touches peers this round's answer mentioned).
///
/// `node_admins` takes only real HTTP(S) admin URLs ([`is_http_admin_url`]): an HTTP target is
/// inserted, while an iroh/seed-keyed target only REMOVES a stale non-http entry (a node that lost
/// its HTTP tunnel) and never clobbers a good HTTP admin URL the same node registered under another
/// gossip key, which would make the entry flap present/absent across rounds. A failed roster fetch
/// marks the target's health through `health::demote`, which keeps a peer that still announces TO
/// us (the fetch and announce directions fail independently) and marks it locally cold instead of
/// withdrawing it; a target reached by nothing (announce, roster, join) returns at once, since the
/// later reads would each burn their own 4 s HTTP timeout through the same evicted mapping.
///
/// Route sync is incremental: `target_node` remembers which node each target speaks for, so the
/// next round can ask `?since=<generation>` before the peer's identity is known, and the peer
/// answers the generation alone unless its host set moved. A peer on a pre-incremental build does
/// not understand it and answers nothing, so the plain path is tried once, otherwise a mixed fleet
/// would learn no routes mid-roll.
async fn sync_one_peer(cloud: Arc<CloudState>, peer: String, me_bytes: Vec<u8>) -> PeerSync {
    let mut out = PeerSync::default();
    let announce = gossip::fetch_outcome(
        &cloud,
        &peer,
        hive_p2p::GOSSIP_POST,
        "/v1/nodes/announce",
        &me_bytes,
    )
    .await;
    let announce_unreachable = matches!(announce, gossip::FetchOutcome::Unreachable);
    let announced = matches!(announce, gossip::FetchOutcome::Answered(_));
    let t0 = now_ms();
    let mut rtt = 0u64;
    let mut nodes_bytes =
        gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/nodes", &[]).await;
    if nodes_bytes.is_none() && !announce_unreachable {
        if let Ok(secret) = std::env::var("HIVE_JWT_SECRET") {
            if !secret.trim().is_empty() {
                let me_id = cloud.registry.me().peer_id;
                let target = {
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
                        hive_p2p::dial_fallback_ceiling(),
                        pool.join_request(&node_id, &addr, &me_bytes, &proof),
                    )
                    .await;
                    if let Ok(Ok(bytes)) = attempt {
                        if !bytes.is_empty() {
                            tracing::info!(peer = %node_id, "mesh join accepted — roster received");
                            cloud
                                .peer_iroh
                                .write()
                                .entry(peer.clone())
                                .or_insert((node_id, addr));
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
                    cloud
                        .peer_iroh
                        .write()
                        .insert(peer.clone(), (ps.id.clone(), addr));
                }
            }
            let peer_self_id = peer_self.as_ref().map(|n| n.id.clone());
            out.identity = peer_self_id.clone();
            let peer_endpoint_id = peer_self
                .as_ref()
                .and_then(|n| n.peer_id.clone())
                .filter(|id| !id.is_empty());
            let ccn_topology = hive_p2p::private_path::CcnTopology::from_env();
            let self_info = cloud.registry.me();
            for n in nodes {
                if n.id != cloud.node_name {
                    if cloud.relayed_trust_compat {
                        if let Some(addr) = n.iroh_addr.as_deref() {
                            if let Some(eid) = hive_p2p::endpoint_id_from_addr_json(addr) {
                                if let Ok(mut trust) = cloud.trusted_peer_ids.write() {
                                    trust.insert(eid);
                                }
                            }
                        }
                    }
                    cloud.cluster.adopt_epoch(n.cp_epoch);
                    if let Some(eid) = n.peer_id.as_deref() {
                        let private_ip = n
                            .private_addr
                            .as_deref()
                            .and_then(|a| a.parse::<std::net::SocketAddr>().ok());
                        let eligible = hive_p2p::private_path::is_private_path_candidate(
                            self_info.provider.as_deref(),
                            &self_info.region,
                            n.provider.as_deref(),
                            &n.region,
                            private_ip.map(|sa| sa.ip()),
                            &ccn_topology,
                        );
                        if let Some(pool) = cloud.mesh.read().clone() {
                            match (eligible, private_ip) {
                                (true, Some(sa)) => pool.set_private_candidate(eid, sa),
                                _ => pool.clear_private_candidate(eid),
                            }
                        }
                    }
                    if peer_self_id.as_deref() == Some(n.id.as_str()) {
                        cloud.registry.upsert_peer_self_report(n);
                    } else {
                        cloud.registry.upsert_peer(n);
                    }
                }
                if let Some(pool) = cloud.mesh.read().clone() {
                    let live: std::collections::HashSet<String> = cloud
                        .registry
                        .nodes()
                        .into_iter()
                        .filter_map(|n| n.peer_id)
                        .collect();
                    pool.retain_private_candidates(&live);
                }
            }
            if let Some(pid) = peer_self_id {
                if let Some(endpoint_id) = peer_endpoint_id {
                    if cloud
                        .registry
                        .set_health_if_endpoint(&pid, &endpoint_id, rtt, true)
                    {
                        crate::health::clear_cold_identity(&endpoint_id);
                    }
                } else {
                    cloud.registry.set_health(&pid, rtt, true);
                    crate::health::clear_cold(&cloud.registry, &pid);
                }
                if is_http_admin_url(&peer) {
                    cloud.node_admins.write().insert(pid, peer.clone());
                } else {
                    let mut m = cloud.node_admins.write();
                    let poisoned = m.get(&pid).map(|a| !is_http_admin_url(a)).unwrap_or(false);
                    if poisoned {
                        m.remove(&pid);
                    }
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
                let k = peer.strip_prefix("seed:").unwrap_or(&peer);
                (k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit())).then(|| k.to_string())
            });
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            crate::health::demote(&cloud.registry, &id, "gossip round: fetch failed", None);
        }
        if !announced {
            return out;
        }
    }
    out.reached = true;
    let since = cloud
        .target_node
        .read()
        .get(&peer)
        .and_then(|n| cloud.route_gens.read().get(n).copied());
    let hosts_path = match since {
        Some(g) => format!("/v1/serve-hosts?since={g}"),
        None => "/v1/serve-hosts".to_string(),
    };
    let mut host_bytes =
        gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, &hosts_path, &[]).await;
    if since.is_some()
        && host_bytes
            .as_deref()
            .map_or(true, |b| serde_json::from_slice::<serde_json::Value>(b).is_err())
    {
        host_bytes =
            gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/serve-hosts", &[]).await;
    }
    if let Some(bytes) = host_bytes {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let node_id = v
                .get("node")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let region = v
                .get("region")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let gateway = v
                .get("gateway")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !gateway.is_empty() && node_id != cloud.node_name {
                out.seen = Some(node_id.clone());
                out.meta = Some((region.clone(), gateway.clone(), rtt));
                out.route_gen = v.get("gen").and_then(|x| x.as_u64());
                cloud.target_node.write().insert(peer.clone(), node_id.clone());
                if let Some(hosts) = v.get("hosts").and_then(|x| x.as_array()) {
                    out.routes_full = true;
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
    if let Some(bytes) = gossip::fetch(
        &cloud,
        &peer,
        hive_p2p::GOSSIP_GET,
        "/v1/fleet-deployments",
        &[],
    )
    .await
    {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let node_id = v
                .get("node")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !node_id.is_empty() && node_id != cloud.node_name {
                if let Some(deps) = v.get("deployments") {
                    if let Ok(list) =
                        serde_json::from_value::<Vec<fluid_core::DeploymentInfo>>(deps.clone())
                    {
                        out.fleet = Some((node_id, list));
                    }
                }
            }
        }
    }
    #[cfg(feature = "zkauth")]
    if let Some(bytes) = gossip::fetch(
        &cloud,
        &peer,
        hive_p2p::GOSSIP_GET,
        "/v1/zkauth/roster-export",
        &[],
    )
    .await
    {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            out.zk_roster = Some(v);
        }
    }
    if let Some(bytes) = gossip::fetch(
        &cloud,
        &peer,
        hive_p2p::GOSSIP_GET,
        "/v1/enterprise/edge-export",
        &[],
    )
    .await
    {
        if let Ok(exp) = serde_json::from_slice::<crate::enterprise::EdgeExport>(&bytes) {
            cloud.enterprise.ingest_peer_edge(&peer, exp);
        }
    }
    if let Some(bytes) = gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/leases", &[]).await
    {
        if let Ok(leases) = serde_json::from_slice::<Vec<crate::lease::ContainerLease>>(&bytes) {
            for l in leases {
                cloud.leases.merge(l);
            }
        }
    }
    out
}

/// Upper bound on the targets one gossip round dials, however large the roster has grown.
const MAX_GOSSIP_TARGETS_PER_ROUND: usize = 64;

/// The gossip loop. Its STATIC targets are the configured `--peer` URLs (warm path, through the
/// persisted `peer_iroh` mapping or HTTP) plus the bootstrap seed keys, always-available iroh
/// rendezvous points that carry a cold or wiped node until its warm `peer_iroh` is repopulated;
/// each round re-adds a seed the timeout-and-evict path dropped (config-derived, so it must keep
/// being retried) without clobbering a fresher learned address. DYNAMIC (hot-join, key-addressed)
/// targets are added every round: every registry node with an iroh addr (learned transitively from
/// any peer's `/v1/nodes` or an inbound join/announce; the freshest addr wins, so a rejoined node's
/// new addr replaces a stale persisted one) and every persisted key-addressed roster entry not
/// covered above (continuity right after a restart, before the registry re-converges). They are
/// deduplicated by ENDPOINT ID, dialed by KEY (iroh resolves the address through
/// discovery/relay/holepunch, never by IP) and capped at [`MAX_GOSSIP_TARGETS_PER_ROUND`]. This is
/// what makes a new node visible fleet-wide with ZERO restarts: it joins one seed, the seed's
/// `/v1/nodes` carries it everywhere, and every node's next round dials it first-hand.
///
/// Targets sync concurrently as their own tasks ([`sync_one_peer`], under `bounded_round`): the
/// per-peer cloud.* writes (registry, `peer_iroh`, `node_admins`, trusted ids, enterprise, leases)
/// go through internally-synchronized stores, so only the loop-local maps are merged from the
/// returned [`PeerSync`] partials. The round waits only until `gossip_round::deadline()`; a target
/// still syncing then keeps running under its in-flight flag (never dispatched twice at once) and
/// its result lands in whichever round receives it: a result carrying this round's number reported
/// inside the round, an older one is a straggler of an earlier round and counts as a missed round.
/// `gossip_round::Rounds` owns the per-target leash (a target that keeps missing the deadline is
/// waited on only briefly, parked, never cancelled or withdrawn, and still dialed every round) and
/// the backoff for a target nobody has heard from. `answers` holds each target's newest ANSWERED
/// sync with the round it was dispatched in, standing in while the next sync is in flight (a later
/// result replaces an earlier one, a failed one drops the stand-in).
///
/// Merge: routes live in a PERSISTENT per-node set (`peer_routes_by_node`). A reached peer's full
/// snapshot REPLACES its node's set, an unchanged report only refreshes the transport fields and
/// liveness, and a node's routes drop solely when nothing about it has been seen within
/// `ROUTE_TTL_MS` (treating every reached answer as authoritative let an empty `unchanged` answer
/// wipe every route the node had published, and the deployment 404'd until the next full snapshot).
/// Route generations are forgotten for nodes that aged out entirely, so a returning node is
/// re-fetched in full instead of resumed from a stale generation, and `peer_routes` (host -> every
/// peer that serves it) is derived from the set each round. Fleet deployments are TTL-merged the
/// same way (`merge_deployments_ttl`: one missed fetch must not wipe a peer's projects from the
/// dashboard), the git-webhook index follows them ([`index_fleet_git_repos`]), and the replicated
/// zkauth rosters are rebuilt from scratch each cycle, so peer revocations converge, and swapped
/// whole.
///
/// Roster: the `peer_iroh` map IS the mesh roster, key-addressed by endpoint id. It is capped at
/// `ROSTER_CAP`, saved to disk every round so a restart bootstraps iroh gossip from disk without
/// any tunnel, and replicated through GuardianDB so a node whose local file is lost can re-adopt
/// it, written only when its content hash changed (`replicated_roster_hash`). Each round also
/// refreshes this node's own addresses ([`refresh_self_iroh_addrs`]) and seeds GuardianDB's client
/// with the peers' ([`seed_guardian_peers`]).
fn spawn_gossip_loop(
    cloud: Arc<CloudState>,
    peers: Vec<String>,
    seeds: Vec<(String, String, String)>,
) {
    use std::collections::HashMap;
    let mut targets = peers.clone();
    for (key, _, _) in &seeds {
        if !targets.contains(key) {
            targets.push(key.clone());
        }
    }
    tokio::spawn(async move {
        let mut replicated_roster_hash: u64 = 0;
        let mut fanout = crate::bounded_round::BoundedRound::<PeerSync>::new();
        let mut rounds = crate::gossip_round::Rounds::new(now_ms());
        let mut answers: HashMap<String, (u64, PeerSync)> = HashMap::new();
        loop {
            {
                let mut pi = cloud.peer_iroh.write();
                for (key, nid, addr) in &seeds {
                    pi.entry(key.clone())
                        .or_insert_with(|| (nid.clone(), addr.clone()));
                }
            }
            let targets: Vec<String> = {
                let mut round: Vec<String> = targets.clone();
                let mut covered: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
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
                let mut adds: Vec<(String, String)> = Vec::new();
                for n in cloud.registry.nodes() {
                    if n.is_self {
                        continue;
                    }
                    let Some(addr) = n.iroh_addr else { continue };
                    let Some(eid) = hive_p2p::endpoint_id_from_addr_json(&addr) else {
                        continue;
                    };
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
                round.truncate(MAX_GOSSIP_TARGETS_PER_ROUND);
                round
            };
            let mut routes: HashMap<String, Vec<crate::state::PeerRoute>> = HashMap::new();
            let mut seen_nodes: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut fleet: HashMap<String, Vec<fluid_core::DeploymentInfo>> = HashMap::new();
            let mut holders: HashMap<String, Vec<String>> = HashMap::new();
            for key in cloud.gw.container_projects() {
                holders
                    .entry(key)
                    .or_default()
                    .push(cloud.node_name.clone());
            }
            rounds.begin_round();
            let round_deadline = tokio::time::Instant::now() + crate::gossip_round::deadline();
            let mut landed = fanout.begin();
            let round_id = fanout.round();
            let me = cloud.registry.me();
            let me_bytes = serde_json::to_vec(&me).unwrap_or_default();
            let mut dispatched = 0usize;
            for peer in &targets {
                if fanout.in_flight(peer) {
                    rounds.note_in_flight();
                    continue;
                }
                let heard_ms = gossip_target_heard_ms(&cloud, peer, rounds.identity(peer));
                if !rounds.admit(peer, heard_ms, now_ms()) {
                    continue;
                }
                let (cloud, target, me_bytes) = (cloud.clone(), peer.clone(), me_bytes.clone());
                if fanout.spawn(peer, sync_one_peer(cloud, target, me_bytes)) {
                    dispatched += 1;
                }
            }
            let collect_started = std::time::Instant::now();
            let (more, stragglers) = fanout
                .collect_leashed(round_deadline, |t: &str| rounds.leash_ms(t, now_ms()))
                .await;
            let collect_ms = (dispatched > 0).then(|| collect_started.elapsed().as_millis() as u64);
            let now = now_ms();
            for t in fanout.outstanding() {
                rounds.note_straggler(&t, now);
            }
            landed.extend(more);
            let mut landed_now: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for (rid, t, pr) in landed {
                rounds.finished(
                    &t,
                    pr.reached,
                    pr.node().map(str::to_string),
                    now_ms(),
                    rid == round_id,
                );
                if pr.reached {
                    landed_now.insert(t.clone());
                    answers.insert(t, (rid, pr));
                } else {
                    landed_now.remove(&t);
                    answers.remove(&t);
                }
            }
            let resumed = rounds.round_done(
                &targets,
                fanout.in_flight_keys(),
                collect_ms,
                stragglers,
                now_ms(),
            );
            crate::health::note_gossip_round(&cloud.registry, resumed);
            {
                let in_flight = fanout.in_flight_keys();
                let dialed: std::collections::HashSet<&str> =
                    targets.iter().map(String::as_str).collect();
                answers.retain(|t, _| dialed.contains(t.as_str()) || in_flight.contains(t));
            }
            let alive: std::collections::HashSet<String> = cloud
                .registry
                .nodes()
                .into_iter()
                .flat_map(|n| [n.id, n.name])
                .collect();
            let contributions =
                round_contributions(&answers, &landed_now, fanout.in_flight_keys(), &alive);
            let now = now_ms();
            for pr in &contributions {
                if let Some(n) = &pr.seen {
                    seen_nodes.insert(n.clone());
                }
                if let Some((nid, list)) = &pr.fleet {
                    fleet.insert(nid.clone(), list.clone());
                }
                for (k, nid) in &pr.holders {
                    holders.entry(k.clone()).or_default().push(nid.clone());
                }
            }
            {
                let mut by_node = cloud.peer_routes_by_node.write();
                let mut gens = cloud.route_gens.write();
                for pr in &contributions {
                    let Some(n) = pr.seen.clone() else { continue };
                    if pr.routes_full {
                        let mut set = std::collections::HashMap::new();
                        for (h, route) in &pr.routes {
                            set.insert(h.clone(), route.clone());
                        }
                        by_node.insert(n.clone(), set);
                    } else if let Some(set) = by_node.get_mut(&n) {
                        if let Some((region, gateway, latency)) = &pr.meta {
                            for r in set.values_mut() {
                                r.region = region.clone();
                                r.gateway = gateway.clone();
                                r.latency_ms = *latency;
                                r.last_seen_ms = now;
                            }
                        }
                    }
                    if let Some(g) = pr.route_gen {
                        gens.insert(n.clone(), g);
                    }
                }
                by_node.retain(|n, set| {
                    if seen_nodes.contains(n) {
                        return true;
                    }
                    set.retain(|_, r| now.saturating_sub(r.last_seen_ms) < crate::state::ROUTE_TTL_MS);
                    !set.is_empty()
                });
                gens.retain(|n, _| by_node.contains_key(n));
            }
            #[cfg(feature = "zkauth")]
            crate::zkauth::set_peer_exports(
                contributions.iter().filter_map(|pr| pr.zk_roster.as_ref()),
            );
            let merged = {
                let by_node = cloud.peer_routes_by_node.read();
                let mut m: std::collections::HashMap<String, Vec<crate::state::PeerRoute>> =
                    std::collections::HashMap::new();
                for set in by_node.values() {
                    for (h, r) in set {
                        m.entry(h.clone()).or_default().push(r.clone());
                    }
                }
                m
            };
            *cloud.peer_routes.write() = merged;
            let merged_deps = {
                let prev = cloud.peer_deployments.read().clone();
                crate::state::merge_deployments_ttl(&prev, fleet, &alive)
            };
            index_fleet_git_repos(&cloud, &merged_deps);
            *cloud.peer_deployments.write() = merged_deps;
            *cloud.container_holders.write() = holders;
            /// Bound on the `peer_iroh` roster over a long uptime: past it, every entry the
            /// registry still vouches for (its own 30 s staleness already prunes dead nodes) and
            /// the static config-derived targets (CLI peers and bootstrap seeds, which must survive
            /// even while their node is briefly down, unlike a transitively learned entry) are kept
            /// and everything else is dropped. A dead or unreachable roster entry can never wedge
            /// the loop regardless: the per-phase iroh timeouts bound every individual dial.
            const ROSTER_CAP: usize = 256;
            {
                let mut pi = cloud.peer_iroh.write();
                if pi.len() > ROSTER_CAP {
                    let keep: std::collections::HashSet<String> = cloud
                        .registry
                        .nodes()
                        .into_iter()
                        .filter_map(|n| {
                            n.iroh_addr
                                .as_deref()
                                .and_then(hive_p2p::endpoint_id_from_addr_json)
                        })
                        .chain(targets.iter().cloned())
                        .collect();
                    pi.retain(|k, (nid, _)| keep.contains(k) || keep.contains(nid));
                }
            }
            let roster_json = {
                let pi = cloud.peer_iroh.read();
                crate::persist::save_peer_iroh(&pi);
                serde_json::to_vec(&*pi).unwrap_or_default()
            };
            let h = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                roster_json.hash(&mut hasher);
                hasher.finish()
            };
            if h != replicated_roster_hash && !roster_json.is_empty() {
                replicated_roster_hash = h;
                tokio::spawn(async move { crate::guardian::put("mesh/roster", roster_json).await });
            }
            refresh_self_iroh_addrs(&cloud);
            seed_guardian_peers(&cloud);
            tokio::time::sleep(crate::gossip_round::ROUND_SLEEP).await;
        }
    });
}

/// Keeps the git-webhook reverse index (`gitops::GitRepoIndex`, see `admin::git_webhook`) in sync
/// with freshly gossiped fleet deployments: a project first deployed via a DIFFERENT node than the
/// one that later receives a GitHub webhook delivery (the `webhook.<platform_domain>` DNS root
/// round-robins across every gateway node) would otherwise sit unindexed on this node until its own
/// next deploy. It is bounded to the projects visible in THIS round's peer data (not a project
/// rescan) and skips any project this node already has a LOCAL real-git record for, matching
/// `admin::git_for_project_fleet`'s local-wins precedence exactly.
fn index_fleet_git_repos(
    cloud: &CloudState,
    fleet_deployments: &std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>,
) {
    use std::collections::HashMap;
    let mut newest_by_project: HashMap<&str, &fluid_core::DeploymentInfo> = HashMap::new();
    for d in fleet_deployments.values().flatten() {
        if !d.git.as_ref().is_some_and(|g| g.is_real_git()) {
            continue;
        }
        newest_by_project
            .entry(d.project.as_str())
            .and_modify(|cur| {
                if d.created_at_ms > cur.created_at_ms {
                    *cur = d
                }
            })
            .or_insert(d);
    }
    for (project, d) in newest_by_project {
        if cloud.gw.git_for_project(project).is_some() {
            continue;
        }
        if let Some(g) = &d.git {
            cloud.git_index.set_project_repo(project, &g.repo_url);
        }
    }
}

/// Re-serializes this node's own MESH `iroh_addr` every gossip round: a one-time capture when
/// `bind_full` returned predates the endpoint's relay registration and hole-punch candidates, so it
/// shipped private addresses only in the very round every other node uses to dial this one. It then
/// publishes this node's GuardianDB-specific address, a SEPARATE iroh identity from the mesh's (see
/// `guardian::my_iroh_addr`: `None` until that client has bound, so it self-heals within a few
/// rounds after boot and never blocks).
fn refresh_self_iroh_addrs(cloud: &Arc<CloudState>) {
    if let Some(addr) = cloud.iroh.read().as_ref().and_then(hive_p2p::addr_json) {
        cloud.registry.set_self_iroh_addr(addr);
    }
    let cloud_for_self_addr = cloud.clone();
    tokio::spawn(async move {
        if let Some(addr) = crate::guardian::my_iroh_addr().await {
            cloud_for_self_addr
                .registry
                .set_self_guardian_addr(Some(addr));
        }
    });
}

/// Seeds GuardianDB's own iroh client with every known peer's GuardianDB-specific address
/// (`guardian::seed_known_peers`, never the mesh `iroh_addr`, which belongs to a DIFFERENT
/// identity) and persists them, mirroring `peer_iroh.json`, so a restart's boot-time seed has real
/// data once the mesh has had one round to gossip them. The seed is spawned so a slow or failed one
/// never blocks the gossip loop's cadence.
fn seed_guardian_peers(cloud: &CloudState) {
    let guardian_peer_map: std::collections::HashMap<String, String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self)
        .filter_map(|n| n.guardian_iroh_addr.clone().map(|a| (n.id.clone(), a)))
        .collect();
    if !guardian_peer_map.is_empty() {
        crate::persist::save_peer_guardian_addr(&guardian_peer_map);
        let guardian_addrs: Vec<String> = guardian_peer_map.into_values().collect();
        tokio::spawn(async move { crate::guardian::seed_known_peers(&guardian_addrs).await });
    }
}

/// Newest gossiped `last_seen_ms` the registry holds for a gossip target (0 = unknown), resolved
/// through the target's transport mapping, the endpoint id a key-addressed target IS, and `known`
/// (the node id the target answered as on its last sync, which is all a `--peer` URL target has
/// left once a failed fetch evicted its mapping). Stale records count: the question is "has anyone
/// heard it recently", for the dead-target backoff.
fn gossip_target_heard_ms(cloud: &CloudState, target: &str, known: Option<&str>) -> u64 {
    let mapped = cloud.peer_iroh.read().get(target).map(|(id, _)| id.clone());
    let key = target.strip_prefix("seed:").unwrap_or(target);
    mapped
        .iter()
        .map(String::as_str)
        .chain(known)
        .chain(std::iter::once(key))
        .filter_map(|id| cloud.registry.peer_last_seen_ms(id))
        .max()
        .unwrap_or(0)
}

/// Parse a positive-u64 env var with a default (clamped to >= 1).
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
        .max(1)
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
        if m >= threshold {
            (m, Some(false))
        } else {
            (m, None)
        }
    }
}

/// Eager full-mesh trunking (proactive): keeps a live iroh QUIC trunk to EVERY healthy peer with a
/// known iroh address, not just the ones this node directly gossips, so a cross-node request reuses
/// a warm trunk instead of paying a cold dial/holepunch on the critical path (the dial cost moves
/// here, off-request). It runs under the 15 s QUIC keepalive so an established trunk never lapses
/// between passes, and a missing or dead one (peer just restarted) is re-dialed within a tick. It
/// warms in PARALLEL, so one slow holepunch cannot serialize the rest, and `warm`'s connect is
/// bounded, so a dead peer cannot wedge the loop. The pause between passes is dithered by +-20% per
/// tick and phased per node (`meshwatch::node_stagger_ms`): after a fleet roll every node's warmer
/// would otherwise fire in lockstep and many nodes would holepunch the same restarted peer at the
/// same instant (a reconnect thundering herd), and wall time alone is SHARED entropy, so two nodes
/// computing in the same millisecond would draw the identical jitter; the dither breaks the
/// synchronization without changing the average cadence. Peers are labelled by endpoint id where
/// one is known (the pool keys trunks by the id in the address either way), so the warmer's logs
/// and alias map speak the identifier the control plane uses. `HIVE_TRUNK_WARM_INTERVAL` (s,
/// default 10).
fn spawn_trunk_warmer(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_TRUNK_WARM_INTERVAL", 10));
    tracing::info!(?interval, "eager mesh trunk warmer");
    tokio::spawn(async move {
        loop {
            let base = interval.as_millis() as u64;
            let dither = base / 5;
            let phase = crate::meshwatch::node_stagger_ms(&cloud.node_name);
            let jittered = base - dither + ((hive_core::now_ms() + phase) % (2 * dither + 1));
            tokio::time::sleep(Duration::from_millis(jittered)).await;
            let pool = match cloud.mesh.read().clone() {
                Some(p) => p,
                None => continue,
            };
            let peers: Vec<(String, String)> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self && n.healthy)
                .filter_map(|n| {
                    let label = n.peer_id.clone().unwrap_or_else(|| n.id.clone());
                    n.iroh_addr.map(|a| (label, a))
                })
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

/// Live relay-set refresh loop: keeps the bound `iroh` endpoint's relay map in sync with the
/// DESIRED set, namely this node's own `relay_url`, every healthy peer's `relay_url` (from the live
/// gossip-replicated `NodeRegistry`), the central `hive_edge::CENTRAL_RELAY_URL` backstop (always
/// included, so a node with no peer or own relay known yet still has a working relay path) and the
/// `HIVE_RELAY_URLS` manual overrides (read once at boot, the same source `relay_map_from_env`
/// reads at bind time, and merged into every refresh, not replaced), through
/// `Endpoint::insert_relay`/`remove_relay`, a genuine LIVE update of the already-bound endpoint
/// (see `RelaySet`). It is the ongoing source of truth for the mesh's relay set;
/// `relay_map_from_env` still seeds/merges at bind time. A pass is skipped while the iroh transport
/// is not bound. `HIVE_RELAY_SYNC_INTERVAL` (s, default 30).
fn spawn_relay_sync_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_RELAY_SYNC_INTERVAL", 30));
    let manual: Vec<String> = std::env::var("HIVE_RELAY_URLS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        ?interval,
        manual = manual.len(),
        "live relay-set refresh loop (dynamic relay list)"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let set = match cloud.relay_set.read().clone() {
                Some(s) => s,
                None => continue,
            };
            let me = cloud.registry.me();
            let mut desired: Vec<String> = Vec::new();
            if let Some(u) = &me.relay_url {
                desired.push(u.clone());
            }
            for n in cloud.registry.nodes() {
                if n.is_self || !n.healthy {
                    continue;
                }
                if let Some(u) = n.relay_url {
                    desired.push(u);
                }
            }
            desired.extend(manual.iter().cloned());
            desired.push(hive_edge::CENTRAL_RELAY_URL.to_string());
            desired.sort();
            desired.dedup();
            let n = desired.len();
            set.sync(desired).await;
            tracing::debug!(relays = n, "live relay set synced");
        }
    });
}

/// GuardianDB anti-entropy loop, the Dynamo-style read repair for the iroh-docs-backed replicated
/// store: each tick ([`anti_entropy_round`]) picks ONE peer from the `NodeRegistry`, asks it for
/// its per-namespace GuardianDB HEAD map (`GET /v1/guardian/heads`: key, content hash and timestamp
/// only, never value bytes), diffs it against this node's own heads (`guardian::namespace_heads`)
/// and, for every namespace with a REAL divergence, triggers a targeted `Doc::start_sync` against
/// exactly that peer, never a full-database refresh. It is the periodic mechanism that catches
/// entries a peer never picked up through the opportunistic live-sync path (this node was offline
/// or partitioned when the peer wrote, or the automatic DocTicket exchange never happened between
/// the pair). `HIVE_ANTI_ENTROPY_INTERVAL_SECS` (default 60).
fn spawn_anti_entropy_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_ANTI_ENTROPY_INTERVAL_SECS", 60));
    tracing::info!(
        ?interval,
        "guardian-db anti-entropy loop (head-CID exchange + targeted reconciliation)"
    );
    crate::supervise::spawn_supervised("anti-entropy", move || {
        let cloud = cloud.clone();
        async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                crate::supervise::beat("anti-entropy");
                anti_entropy_round(&cloud).await;
            }
        }
    });
}

/// One anti-entropy round (see [`spawn_anti_entropy_loop`]), split out so the loop body stays a
/// plain tick-and-call like the other periodic loops.
///
/// The peer rotates by a monotonic round counter, NEVER `now_ms() % len`: the loop ticks on a fixed
/// interval, so a clock-derived index advances by `interval mod len` each round, which is ZERO for
/// fleet sizes such as 12, 15 or 16 at a 60 s interval and pinned every round on the SAME peer, so
/// one unreachable candidate could consume the single per-round slot forever while every other peer
/// went un-reconciled; a counter cannot alias with the tick period. Candidates are deliberately NOT
/// health-filtered: a peer this node currently calls unhealthy is the one whose state is most
/// likely to have diverged, and filtering it out starved exactly the peers that needed reconciling
/// (a dead peer just costs one failed RPC per round, a live-but-unprobeable one gets its state
/// back).
///
/// Two kinds of divergence are counted per namespace: entry-level (a key's hash disagrees) and
/// CONTENT-level (the hashes agree, but one side cannot read the bytes: a doc entry replicates
/// independently of its blob, and iroh-docs stops retrying for good once the supplying peer
/// departs). The content gap is invisible to the entry comparison and is the divergence this fleet
/// actually suffers from, so counting it is what stops "heads already match" being reported over a
/// node that holds only hashes; a `content_local` of `None` is UNKNOWN (pre-upgrade peer), never
/// counted as missing, and a gap is worth reporting only when the PEER claims to hold what we lack,
/// since only that a reconcile can close.
async fn anti_entropy_round(cloud: &Arc<CloudState>) {
    let candidates: Vec<NodeInfo> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.peer_id.is_some() && n.iroh_addr.is_some())
        .collect();
    if candidates.is_empty() {
        return;
    }
    static ROUND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let idx = ROUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % candidates.len();
    let peer = &candidates[idx];
    let (peer_id, peer_addr) = (
        peer.peer_id.clone().unwrap(),
        peer.iroh_addr.clone().unwrap(),
    );

    let remote_bytes = match gossip::request_to(
        cloud,
        &peer_id,
        &peer_addr,
        hive_p2p::GOSSIP_GET,
        "/v1/guardian/heads",
        &[],
        10,
    )
    .await
    {
        Some(b) => b,
        None => {
            tracing::warn!(peer = %peer.name, error = "no response from peer", "anti-entropy: heads RPC failed");
            return;
        }
    };
    let remote: std::collections::HashMap<String, Vec<guardian_db::traits::EntryHead>> =
        match serde_json::from_slice(&remote_bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(peer = %peer.name, error = %e, "anti-entropy: unparsable heads response");
                return;
            }
        };
    let local = guardian::namespace_heads().await;

    for (ns, remote_heads) in &remote {
        let local_by_key: std::collections::HashMap<&str, &str> = local
            .get(ns)
            .map(|v| {
                v.iter()
                    .map(|h| (h.key.as_str(), h.hash.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let diverged = remote_heads
            .iter()
            .filter(|h| local_by_key.get(h.key.as_str()) != Some(&h.hash.as_str()))
            .count();
        let local_missing: std::collections::HashSet<&str> = local
            .get(ns)
            .map(|v| {
                v.iter()
                    .filter(|h| h.content_local == Some(false))
                    .map(|h| h.hash.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let recoverable = remote_heads
            .iter()
            .filter(|h| h.content_local == Some(true) && local_missing.contains(h.hash.as_str()))
            .count();
        if diverged == 0 && recoverable == 0 {
            tracing::debug!(namespace = %ns, peer = %peer.name, "anti-entropy: heads already match");
            continue;
        }
        if diverged == 0 {
            tracing::info!(
                namespace = %ns, peer = %peer.name, recoverable,
                "anti-entropy: entries match but CONTENT is missing locally; peer holds it — reconciling"
            );
        }
        let Some(guardian_addr) = peer.guardian_iroh_addr.clone() else {
            tracing::warn!(
                namespace = %ns, peer = %peer.name, count = diverged,
                error = "peer has no guardian_iroh_addr gossiped yet",
                "anti-entropy: cannot reconcile — peer's GuardianDB address unknown"
            );
            continue;
        };
        match guardian::sync_with_peer(&guardian_addr).await {
            Ok(outcome) => {
                tracing::info!(
                    namespace = %ns,
                    peer = %peer.name,
                    count = diverged,
                    entries_received = outcome.entries_received,
                    entries_sent = outcome.entries_sent,
                    "synced {diverged} missing entries from peer {}", peer.name
                );
            }
            Err(e) => {
                tracing::warn!(
                    namespace = %ns, peer = %peer.name, count = diverged, error = %e,
                    "anti-entropy: reconciliation sync failed"
                );
            }
        }
    }
}

/// Applies replicated project DELETION tombstones to this node's own deployment records. A
/// tombstone already replicates (`ProjectStore::snapshot_synced`) and removes the SETTINGS row on
/// every peer that receives it, but nothing applied it to the DEPLOYMENT records, a separate store:
/// a peer that was unreachable when the delete cascaded (or was down and came back) kept its
/// deployment rows, kept serving them and kept gossiping them into `peer_deployments`, which the
/// dashboard's fleet view reads, so the project reappeared fully alive and deleting it again hit
/// the same unreachable peer. No retry of the cascade at delete time can close that (the peer is by
/// definition not listening then), so the tombstone is the durable, idempotent instruction,
/// re-applied every tick by whoever holds it (the same shape as `tenancy_reconcile`: repair-only,
/// never creative).
///
/// A project RE-CREATED after its deletion must survive, so the tombstone is honoured only while
/// nothing newer exists: a settings row written after the tombstone, or ANY deployment created
/// after it, cancels the pass for that project entirely (never a partial reap, which would delete a
/// live deployment's siblings). With no local deployment it still re-applies the delete, driving
/// the generation-conditional relational delete every tick so a stale offline node's late boot
/// backfill cannot resurrect `project_teams`, and releases the project's durable PUBLIC PORT claim:
/// that release otherwise runs only in `purge_project_resources` on a node that tore a deployment
/// down, so a node that merely ALLOCATED the port (the allocator is fleet-coordinated) would keep
/// the claim in raw_ports.json forever, and a quarantined port is never re-granted, slowly losing
/// public ports to projects that no longer exist. With local deployments left, a newer one is a
/// causal recreation: the generation-aware `delete_project_local` removes only old-incarnation
/// records and preserves the recreated row and resources.
fn spawn_deletion_reconcile_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_DELETION_RECONCILE_SECS", 60));
    crate::supervise::spawn_supervised("deletion-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                crate::supervise::beat("deletion-reconcile");
                for (project, tomb_ms) in cloud.projects.tombstones_snapshot() {
                    let settings_recreated = cloud
                        .projects
                        .get_if_set(&project)
                        .is_some_and(|s| s.updated_ms > tomb_ms);
                    let peer_recreated = cloud
                        .peer_deployments
                        .read()
                        .values()
                        .flatten()
                        .any(|d| d.project == project && d.created_at_ms > tomb_ms);
                    let mine: Vec<fluid_core::DeploymentInfo> = cloud
                        .gw
                        .list()
                        .into_iter()
                        .filter(|d| d.project == project)
                        .collect();
                    if mine.is_empty() {
                        if settings_recreated || peer_recreated {
                            continue;
                        }
                        cloud.projects.apply_delete(&project, tomb_ms);
                        let retired = crate::raw_ports::release_raw_ports(&project);
                        if !retired.is_empty() {
                            tracing::warn!(
                                project,
                                ports = ?retired,
                                "deletion reconcile: retired public raw port(s) still claimed by \
                                 a project deleted fleet-wide — never re-granted"
                            );
                            crate::persist::persist(&cloud);
                        }
                        continue;
                    }
                    let team = crate::admin::norm(&cloud.projects.team_of(&project)).to_string();
                    let outcome =
                        crate::admin::delete_project_local(&cloud, &project, &team, tomb_ms).await;
                    tracing::warn!(
                        project,
                        deployments = outcome.removed.len(),
                        recreated = outcome.recreated,
                        tombstone_ms = tomb_ms,
                        "deletion reconcile: re-applied a replicated project deletion generation — \
                         stale old-incarnation records were removed while any causal recreation survived"
                    );
                }
            }
        }
    });
}

/// Runs `store_sync::reconcile_on_promotion` once per OWNERSHIP term, polled every
/// `HIVE_PROMOTION_RECONCILE_POLL_MS` (default 2000). It is the preamble to taking writes, so it
/// keys on the same authority `admin_ingress` serves mutations on (`is_control_plane_leader`:
/// authoritative owner and not isolated), never on `leadership::may_act`, whose tenure and quorum
/// terms would delay it past the first accepted write and flip on every quorum blip. It fires on
/// the first tick that authority holds, then again only after ownership ended or the authority
/// lapsed for `leadership::TERM_LAPSE` or longer (long enough for another node to have become the
/// owner and taken writes). A shorter isolation blip on a steady owner is not a promotion, and
/// re-running the reconcile then could adopt followers' stale wholesale copies over writes accepted
/// since.
fn spawn_promotion_reconcile_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_millis(env_u64("HIVE_PROMOTION_RECONCILE_POLL_MS", 2000));
    crate::supervise::spawn_supervised("promotion-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            let mut tick = tokio::time::interval(interval);
            let mut reconciled_this_term = false;
            let mut lapse_since: Option<std::time::Instant> = None;
            loop {
                tick.tick().await;
                crate::supervise::beat("promotion-reconcile");
                let owner = cloud
                    .control_plane_leader_with_source()
                    .is_some_and(|(owner, source)| {
                        source.is_authority() && owner == cloud.node_name
                    });
                if !owner {
                    reconciled_this_term = false;
                    lapse_since = None;
                    continue;
                }
                if cloud.mesh_health().isolated {
                    lapse_since.get_or_insert_with(std::time::Instant::now);
                    continue;
                }
                if lapse_since
                    .take()
                    .is_some_and(|since| since.elapsed() >= leadership::TERM_LAPSE)
                {
                    reconciled_this_term = false;
                }
                if reconciled_this_term {
                    continue;
                }
                reconciled_this_term = true;
                store_sync::reconcile_on_promotion(&cloud).await;
            }
        }
    });
}

/// Relational mirror loop: keeps the admin "view as PostgreSQL" browser's project_teams / teams /
/// team_members / deployments tables (and a FULL billing_accounts backfill) populated from live
/// store snapshots (`upsert_billing` fires only for tenants with active usage THIS tick, so an
/// account that exists but is not currently metered would otherwise never appear). It is a
/// WRITE-ONLY projection (the follower pull of the stores themselves is `store_follower`, its own
/// loops, never waiting on a SQL await here) and best-effort throughout, since these tables feed
/// only fleet visibility and read-only admin views.
///
/// Write discipline: the project/team backfill runs only under
/// `leadership::may_act(RelationalLeaderWrites)` and the billing backfill under
/// `leadership::may_act(BillingMeter)`, the SAME gate as the metering loop, so both billing writers
/// always sit on the same node (on a chain-less mesh that includes the
/// `HIVE_BILLING_COORDINATOR_NODE` pin); deployments differ: EVERY node syncs only its OWN
/// `gw.list()` rows (single writer per row by construction, see `relational::sync_deployments`).
///
/// A content hash per ordinary section skips ticks with no change. Billing is hashed per tenant
/// over the whole (account, ledger, invoices, checkouts) tuple, so a checkout or ledger entry
/// landing with no account-field change still flips it and only tenants whose own rows changed are
/// re-upserted (the hash map is replaced wholesale each pass, so a tenant that disappeared stops
/// being tracked). Project rows are deliberately re-asserted every ten leader ticks. The loop runs
/// on the BULKHEAD runtime, its supervisor included (see `bulkhead`): the guardian SQL engine
/// reloads a whole table per statement, and the billing section on the serving runtime's workers
/// once froze the leader. `HIVE_RELATIONAL_MIRROR_SECS` (s, default 60).
fn spawn_relational_mirror_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_RELATIONAL_MIRROR_SECS", 60));
    tracing::info!(
        ?interval,
        "relational mirror loop (projects/teams/members/deployments + billing backfill → SQL view)"
    );
    crate::bulkhead::spawn(async move {
        crate::supervise::spawn_supervised("relational-mirror", move || {
            let cloud = cloud.clone();
            async move {
                let hash_of = |s: &str| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    s.hash(&mut h);
                    h.finish()
                };
                let mut tick = tokio::time::interval(interval);
                let (mut teams_hash, mut deps_hash) = (0u64, 0u64);
                /// The ledger is append-only per tenant, so after the first full mirror only entries
                /// newer than the last mirrored `ts_ms` are sent each tick (the engine's cost is rows
                /// SENT x rows STORED, so sending the whole ledger every tick was the quadratic hog;
                /// `>=` is on purpose, entries sharing the mark's millisecond are re-sent and converge
                /// through ON CONFLICT). A mark advances to the newest entry of a snapshot taken BEFORE
                /// the write, so nothing that arrived meanwhile is skipped: it is newer than the mark
                /// and goes out next tick. A full re-send every this many ticks is the convergence
                /// safety net for anything a prune or a leader change left behind.
                const LEDGER_FULL_RESEND_ROUNDS: u32 = 60;
                let mut ledger_marks: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                let mut ledger_round: u32 = 0;
                /// Authoritative project rows are re-asserted every this many leader ticks: a stale
                /// node can boot and backfill an old ProjectStore snapshot after the real
                /// deletion/recreation write; version-conditional SQL rejects it when visible, and this
                /// bounded re-assertion repairs the distributed LWW race if the stale replica had not
                /// received the newer row yet. The round counter starts one short, so a newly elected
                /// leader runs it at once.
                const PROJECT_REASSERT_TICKS: u8 = 10;
                let mut project_reconcile_round = PROJECT_REASSERT_TICKS - 1;
                let mut billing_hashes: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                loop {
                    tick.tick().await;
                    crate::supervise::beat("relational-mirror");
                    let deps = cloud.gw.list();
                    if let Ok(json) = serde_json::to_string(&deps) {
                        let h = hash_of(&json);
                        if h != deps_hash {
                            relational::sync_deployments(&cloud.node_name, &deps).await;
                            deps_hash = h;
                        }
                    }
                    if leadership::may_act(&cloud, leadership::Job::RelationalLeaderWrites) {
                        project_reconcile_round =
                            (project_reconcile_round + 1) % PROJECT_REASSERT_TICKS;
                        if project_reconcile_round == 0 {
                            let projects: Vec<(String, String, String, u64)> = cloud
                                .projects
                                .snapshot()
                                .into_iter()
                                .map(|(project, s)| {
                                    (project, s.team, s.build.root_dir, s.updated_ms)
                                })
                                .collect();
                            relational::backfill_projects(projects).await;
                        }
                        let teams = cloud.teams.list_authoritative();
                        if let Ok(json) = serde_json::to_string(&teams) {
                            let h = hash_of(&json);
                            if h != teams_hash {
                                relational::sync_teams(&teams).await;
                                teams_hash = h;
                            }
                        }
                    }
                    let billing_authority =
                        leadership::may_act(&cloud, leadership::Job::BillingMeter);
                    if billing_authority {
                        let (accounts, _) = cloud.billing.snapshot();
                        ledger_round = (ledger_round + 1) % LEDGER_FULL_RESEND_ROUNDS;
                        let full_resend = ledger_round == 0;
                        let per_tenant: Vec<_> = accounts
                            .iter()
                            .map(|acc| {
                                let mark = ledger_marks.get(&acc.tenant).copied().unwrap_or(0);
                                let mut ledger = cloud.billing.ledger(&acc.tenant);
                                if !full_resend && mark > 0 {
                                    ledger.retain(|e| e.ts_ms >= mark);
                                }
                                let invoices = cloud.billing.finalized_invoices(&acc.tenant);
                                let checkouts = cloud.billing.checkouts_for_tenant(&acc.tenant);
                                (acc, ledger, invoices, checkouts)
                            })
                            .collect();
                        let mut dirty: Vec<relational::BillingRows<'_>> = Vec::new();
                        let mut next_hashes: std::collections::HashMap<String, u64> =
                            std::collections::HashMap::with_capacity(per_tenant.len());
                        for (acc, ledger, invoices, checkouts) in &per_tenant {
                            let Ok(json) =
                                serde_json::to_string(&(acc, ledger, invoices, checkouts))
                            else {
                                continue;
                            };
                            let h = hash_of(&json);
                            next_hashes.insert(acc.tenant.clone(), h);
                            if billing_hashes.get(&acc.tenant) != Some(&h) {
                                dirty.push((
                                    *acc,
                                    ledger.as_slice(),
                                    invoices.as_slice(),
                                    checkouts.as_slice(),
                                ));
                            }
                        }
                        if !dirty.is_empty() {
                            let started = std::time::Instant::now();
                            let rows: usize = dirty.iter().map(|(_, l, _, _)| l.len()).sum();
                            relational::upsert_billing_many(&dirty).await;
                            let elapsed_ms = started.elapsed().as_millis() as u64;
                            if elapsed_ms > 5_000 {
                                tracing::warn!(
                                    tenants = dirty.len(),
                                    ledger_rows = rows,
                                    elapsed_ms,
                                    full_resend,
                                    "relational: billing mirror pass was slow -- on the bulkhead runtime, so serving is unaffected, but the row volume per tick should be bounded"
                                );
                            } else {
                                tracing::debug!(
                                    tenants = dirty.len(),
                                    ledger_rows = rows,
                                    elapsed_ms,
                                    full_resend,
                                    "relational: billing mirror pass"
                                );
                            }
                        }
                        for (acc, ledger, _, _) in &per_tenant {
                            if let Some(newest) = ledger.iter().map(|e| e.ts_ms).max() {
                                let m = ledger_marks.entry(acc.tenant.clone()).or_insert(0);
                                *m = (*m).max(newest);
                            }
                        }
                        ledger_marks.retain(|t, _| accounts.iter().any(|a| &a.tenant == t));
                        billing_hashes = next_hashes;
                    }
                }
            }
        });
    });
}

/// Prunes ledger entries covered by a durable, grace-elapsed checkpoint, from both the relational
/// mirror and the in-memory ledger (never the account itself: `remove_account` is untouched,
/// pruning and account deletion are orthogonal). A checkpoint is a tamper-evident hash chain
/// computed once at period close in `BillingStore::account()`'s rollover branch (see
/// `billing::LedgerCheckpoint`). The gates, in order: the checkpoint's grace period
/// (`HIVE_LEDGER_PRUNE_GRACE_MS`, default 7 days) has elapsed; the DURABILITY gate, the relational
/// mirror holds exactly the checkpoint's `entry_count` rows for the range, else the tenant is
/// skipped and retried next tick (the in-memory copy is never pruned ahead of the mirror); the
/// BLAST-RADIUS guard, the commitment is recomputed fresh from the CURRENT in-memory rows for that
/// exact range and a mismatch refuses loudly instead of deleting data that no longer matches what
/// was proven. The relational prune always runs before the in-memory prune, so a crash between the
/// two retries cleanly next tick (the DELETE and the retain are both idempotent on an already-empty
/// range).
///
/// It runs on every metering tick, before the usage-stats early return, so a tenant that stops
/// generating usage still gets pruned. The in-memory `pruned=true` flag a pruned tenant now carries
/// must be mirrored even on a quiet tick, so its account/invoice/checkout rows are re-upserted
/// here; its ledger rows were just DELETEd in SQL and the surviving entries are already mirrored
/// (and re-asserted by the mirror loop's periodic full pass), so re-sending the whole remaining
/// ledger would cost one full table reload per row.
async fn prune_checkpointed_ledgers(cloud: &CloudState) {
    let mut pruned_tenants: Vec<String> = Vec::new();
    for acc in cloud.billing.all_accounts() {
        let tenant = acc.tenant.as_str();
        let invs = cloud.billing.finalized_invoices(tenant);
        for inv in &invs {
            let Some(cp) = inv.ledger_checkpoint.as_ref() else {
                continue;
            };
            if cp.pruned {
                continue;
            }
            let grace = env_u64("HIVE_LEDGER_PRUNE_GRACE_MS", 7 * 24 * 60 * 60 * 1000);
            if now_ms().saturating_sub(inv.created_ms) < grace {
                continue;
            }
            let mirrored = match relational::count_billing_ledger_rows(
                tenant,
                cp.period_start_ms,
                cp.period_end_ms,
            )
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(tenant, error = %e, "ledger checkpoint prune: relational count failed, retrying next tick");
                    continue;
                }
            };
            if mirrored != cp.entry_count {
                tracing::debug!(
                    tenant,
                    mirrored,
                    expected = cp.entry_count,
                    "ledger checkpoint prune: relational mirror not yet caught up, retrying next tick"
                );
                continue;
            }
            let fresh = cloud.billing.compute_ledger_checkpoint(
                tenant,
                cp.period_start_ms,
                cp.period_end_ms,
            );
            if fresh.commitment != cp.commitment || fresh.entry_count != cp.entry_count {
                tracing::warn!(
                    tenant,
                    expected_commitment = %cp.commitment,
                    fresh_commitment = %fresh.commitment,
                    expected_entry_count = cp.entry_count,
                    fresh_entry_count = fresh.entry_count,
                    "ledger checkpoint prune REFUSED: commitment mismatch (data changed since checkpoint was proven)"
                );
                continue;
            }
            if let Err(e) =
                relational::prune_billing_ledger(tenant, cp.period_start_ms, cp.period_end_ms).await
            {
                tracing::warn!(tenant, error = %e, "ledger checkpoint prune: relational delete failed, retrying next tick");
                continue;
            }
            let removed =
                cloud
                    .billing
                    .prune_ledger_range(tenant, cp.period_start_ms, cp.period_end_ms);
            cloud
                .billing
                .mark_checkpoint_pruned(tenant, cp.period_start_ms);
            tracing::info!(
                tenant,
                removed,
                period_start_ms = cp.period_start_ms,
                period_end_ms = cp.period_end_ms,
                commitment = %cp.commitment,
                "ledger checkpoint: raw entries pruned, commitment + invoice retained"
            );
            if !pruned_tenants.iter().any(|t| t == tenant) {
                pruned_tenants.push(tenant.to_string());
            }
        }
    }
    if !pruned_tenants.is_empty() {
        let owned: Vec<_> = pruned_tenants
            .iter()
            .map(|tenant| {
                (
                    cloud.billing.account(tenant),
                    Vec::<billing::LedgerEntry>::new(),
                    cloud.billing.finalized_invoices(tenant),
                    cloud.billing.checkouts_for_tenant(tenant),
                )
            })
            .collect();
        let batch: Vec<relational::BillingRows<'_>> = owned
            .iter()
            .map(|(a, l, i, c)| (a, l.as_slice(), i.as_slice(), c.as_slice()))
            .collect();
        relational::upsert_billing_many(&batch).await;
    }
}

/// Billing meter loop, the metering-to-billing pipeline. On a fixed interval it pulls the
/// fleet-wide per-function usage stats (local plus every peer over the mesh), aggregates them per
/// tenant and feeds the cumulative totals to the billing store, which charges only the DELTA since
/// the last reading (delta times rate card, into the ledger, into the invoice); counter resets
/// (pool recycle, node restart) are handled by the store's meter. It meters only where
/// `leadership::may_act(BillingMeter)` holds (the strict chain owner, or the election or the
/// `HIVE_BILLING_COORDINATOR_NODE` pin only on a chain-less mesh; not isolated; 120 s of continuous
/// ownership; a direct voter majority): charging twice is worse than charging a tick late, and a
/// missed tick is caught up by the next delta.
///
/// [`prune_checkpointed_ledgers`] runs first on every metered tick, before the early return for a
/// fleet with no usage stats. Browser requests are counted SEPARATELY from `requests` and never
/// folded in (no per-invocation charge for owner-served browser traffic, see
/// `RateCard::browser_per_million_cents`), and GPU is metered by instance wall time (`fluid_ms`),
/// not just active CPU, because it is held for the instance's entire life. Metering is strictly per
/// tenant; the whole tick's tenants are then mirrored into the fleet-replicated relational layer
/// (see `relational.rs`'s module doc) in ONE batch (per-tenant mirroring re-read the ENTIRE
/// relational index before every write), ONLY from this node, the elected billing authority, so
/// every other node's local replica converges to the same account state within seconds instead of
/// staying empty or stale; the mirror is best-effort, and the HTTP proxy-to-leader read stays
/// correct whatever its freshness. Only the ledger entries THIS tick's metering appended go out:
/// the whole ledger once went out every tick and the engine reloads the table once per row (the
/// quadratic hog behind the leader freezes); older rows are already mirrored and the mirror loop's
/// periodic full pass is the convergence net. Runs mock or real (Stripe only affects top-up
/// checkout, not metering). `HIVE_BILLING_METER_INTERVAL` (s, default 60).
fn spawn_billing_meter_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_BILLING_METER_INTERVAL", 60));
    tracing::info!(
        ?interval,
        "billing meter loop (usage → charges → invoices; leader-elected)"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if !leadership::may_act(&cloud, leadership::Job::BillingMeter) {
                continue;
            }
            prune_checkpointed_ledgers(&cloud).await;
            let stats = admin::fleet_function_stats(&cloud).await;
            if stats.is_empty() {
                continue;
            }
            let mut totals: std::collections::HashMap<String, billing::UsageTotals> =
                std::collections::HashMap::new();
            for s in &stats {
                let t = totals.entry(s.tenant.clone()).or_default();
                t.active_cpu_ms = t.active_cpu_ms.saturating_add(s.active_cpu_ms);
                t.mem_gb_hr_milli = t
                    .mem_gb_hr_milli
                    .saturating_add((s.memory_gb_hrs * 1000.0) as u64);
                t.requests = t.requests.saturating_add(s.requests);
                t.browser_requests = t.browser_requests.saturating_add(s.browser_requests);
                if s.gpu {
                    t.gpu_ms = t.gpu_ms.saturating_add(s.fluid_ms);
                }
            }
            let mut charged_any = 0u64;
            let tick_started_ms = now_ms();
            let mut metered: Vec<String> = Vec::new();
            for (tenant, tot) in totals {
                charged_any += cloud.billing.meter_usage(&tenant, tot);
                metered.push(tenant);
            }
            let owned: Vec<_> = metered
                .iter()
                .map(|tenant| {
                    let mut ledger = cloud.billing.ledger(tenant);
                    ledger.retain(|e| e.ts_ms >= tick_started_ms);
                    (
                        cloud.billing.account(tenant),
                        ledger,
                        cloud.billing.finalized_invoices(tenant),
                        cloud.billing.checkouts_for_tenant(tenant),
                    )
                })
                .collect();
            let batch: Vec<relational::BillingRows<'_>> = owned
                .iter()
                .map(|(a, l, i, c)| (a, l.as_slice(), i.as_slice(), c.as_slice()))
                .collect();
            relational::upsert_billing_many(&batch).await;
            if charged_any > 0 {
                tracing::debug!(cents = charged_any, "billing meter charged usage");
            }
        }
    });
}

/// Loud, cheap, self-observed early warning for a process climbing toward its cgroup memory ceiling
/// while every EXTERNAL signal (systemd is-active, /healthz 200, even :443 customer traffic) stays
/// green (the fc-sanjose-2 shape). It is a SELF-check for exactly the failure class no peer probe
/// can see, distinct from [`spawn_health_loop`] (PEER-observed reachability, the right check for
/// routing decisions), because the process keeps answering everything a probe would ask right up
/// until it does not. `HIVE_MEMORY_ALARM_PCT` (default 80.0): once RSS crosses that fraction of the
/// cgroup limit it warns on every tick until it drops back below, noisy on purpose, since the
/// incident this guards against was invisible for however long the climb took.
fn spawn_memory_pressure_alarm() {
    let alarm_pct = std::env::var("HIVE_MEMORY_ALARM_PCT")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(80.0);
    crate::supervise::spawn_supervised("memory-pressure-alarm", move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            crate::supervise::beat("memory-pressure-alarm");
            let m = crate::supervise::memory_pressure();
            if let Some(pct) = m.pct_of_limit {
                if pct >= alarm_pct {
                    tracing::warn!(
                        rss_mb = m.rss_mb,
                        cgroup_limit_mb = m.cgroup_limit_mb,
                        pct_of_limit = pct,
                        "memory pressure high — this node may be approaching the fc-sanjose-2 wedge shape (climbing RSS while every external health signal stays green)"
                    );
                }
            }
        }
    });
}

/// One probe's outcome. `Default` (empty name) is what a probe task that
/// panicked or was dropped reports: no verdict either way.
#[derive(Default)]
struct ProbeOutcome {
    name: String,
    endpoint_id: String,
    observed_last_seen_ms: u64,
    rtt: Option<u64>,
}

/// Active full-mesh health probing, the fast path for down-detection. Every node directly probes
/// every OTHER public-IP node with a resolvable iroh address in PARALLEL on a short interval, so
/// health, up AND down, is owned by a direct probe (sub-`interval` flips) instead of transitive
/// gossip plus the ~30 s staleness drain. Public-IP nodes only: NAT'd nodes are reachable solely
/// via relay, are already excluded from client DNS (the `public_ip` gate in `lb_records`), and stay
/// on the staleness model so relay-probe jitter cannot churn their health or spam logs; `nodes()`'s
/// 30 s staleness drop stays the backstop for a peer that is both unprobeable and gone. A round is
/// skipped while no mesh transport is bound (a peer is never false-flagged) and when there is
/// nothing to probe and no probe still running.
///
/// Each probe runs in its own task and a round waits for them only until `health::probe_deadline()`
/// (`bounded_round`): under `join_all` one failing peer's probe (two samples at the dial-fallback
/// ceiling) held every round, so every other peer's verdict, the restore hysteresis and the miss
/// counters all ran at ~33 s instead of the interval. A probe still running at the deadline stays
/// in flight (never probed twice at once) and its verdict lands in whichever round receives it.
/// Each result retains the immutable endpoint id and liveness timestamp captured with the attempted
/// address, and the miss counters of identities no longer probed are forgotten, never one whose
/// probe is still running.
///
/// The budget is per target. The fast `timeout` is the right steady-state check (is the warm trunk
/// still alive) but is shorter than the connect budget alone, so a target needing a fresh dial
/// would be cancelled before it could finish connecting, and `probe`'s trunk eviction (meant to let
/// the next dial resolve the peer's CURRENT addr via discovery) could never deliver that: a peer
/// whose cached address went stale (restart with new socket addrs, NAT rebind) stayed unhealthy
/// PERMANENTLY, and unhealthy nodes drop out of client DNS and placement, silently shrinking the
/// fleet. Once a target is already failing (misses at or past the threshold) it gets the full
/// `dial_fallback_ceiling()` so discovery has a real chance to recover it. Each probe takes TWO
/// samples and passes on EITHER (a multi-ping liveness check): a single lost datagram train on a
/// lossy cross-continent path used to count as a full round miss, and at threshold 2 two unlucky
/// rounds withdrew a healthy peer. The samples run sequentially so the second only spends budget
/// when the first genuinely failed (which also gives `probe`'s trunk eviction from the first
/// failure a fresh-dial chance within the same round).
///
/// Config: `HIVE_HEALTH_INTERVAL` (s, default 5), `HIVE_HEALTH_TIMEOUT` (s, default 2),
/// `HIVE_HEALTH_FAIL_THRESHOLD` (consecutive misses, default 2).
fn spawn_health_loop(cloud: Arc<CloudState>) {
    let interval = crate::health::probe_interval();
    let timeout = crate::health::probe_timeout();
    let deadline = crate::health::probe_deadline();
    let threshold = env_u64("HIVE_HEALTH_FAIL_THRESHOLD", 2) as u32;
    tracing::info!(
        ?interval,
        ?timeout,
        ?deadline,
        threshold,
        "active health probing (public nodes)"
    );
    tokio::spawn(async move {
        let mut misses: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut fanout = crate::bounded_round::BoundedRound::<ProbeOutcome>::new();
        loop {
            tokio::time::sleep(interval).await;
            if cloud.mesh.read().is_none() {
                continue;
            }
            let targets: Vec<(String, String, String, u64)> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self && n.public_ip.is_some())
                .filter_map(|n| {
                    Some((
                        n.id,
                        n.peer_id.filter(|id| !id.is_empty())?,
                        n.iroh_addr?,
                        n.last_seen_ms,
                    ))
                })
                .collect();
            if targets.is_empty() && fanout.in_flight_keys().is_empty() {
                continue;
            }
            let round_deadline = tokio::time::Instant::now() + deadline;
            let mut landed = fanout.begin();
            let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (name, endpoint_id, addr, observed_last_seen_ms) in targets {
                live.insert(endpoint_id.clone());
                let failing = *misses.get(&endpoint_id).unwrap_or(&0) >= threshold;
                let budget = if failing {
                    hive_p2p::dial_fallback_ceiling()
                } else {
                    timeout
                };
                let cloud = cloud.clone();
                let key = endpoint_id.clone();
                fanout.spawn(&key, async move {
                    let first = gossip::probe(&cloud, &endpoint_id, &addr, budget).await;
                    let rtt = match first {
                        Some(ms) => Some(ms),
                        None => gossip::probe(&cloud, &endpoint_id, &addr, budget).await,
                    };
                    ProbeOutcome {
                        name,
                        endpoint_id,
                        observed_last_seen_ms,
                        rtt,
                    }
                });
            }
            let (more, _) = fanout.collect(round_deadline).await;
            landed.extend(more);
            for (_, _, outcome) in landed {
                let ProbeOutcome {
                    name,
                    endpoint_id,
                    observed_last_seen_ms,
                    rtt,
                } = outcome;
                if name.is_empty() {
                    continue;
                }
                let prev = *misses.get(&endpoint_id).unwrap_or(&0);
                let (next, write) = health_decision(prev, rtt.is_some(), threshold);
                misses.insert(endpoint_id.clone(), next);
                match write {
                    Some(true) => {
                        if cloud.registry.set_health_if_endpoint(
                            &name,
                            &endpoint_id,
                            rtt.unwrap_or(0),
                            true,
                        ) {
                            crate::health::clear_cold_identity(&endpoint_id);
                        }
                    }
                    Some(false) => {
                        crate::health::demote_exact(
                            &cloud.registry,
                            &name,
                            &endpoint_id,
                            &format!("mesh probe failed ({next} consecutive)"),
                            Some(observed_last_seen_ms),
                        );
                    }
                    None => {}
                }
            }
            let restored = crate::health::restore_gossip_alive(&cloud.registry);
            if !restored.is_empty() {
                let pool = { cloud.mesh.read().clone() };
                if let Some(pool) = pool {
                    for endpoint_id in &restored {
                        pool.close_peer(endpoint_id).await;
                    }
                }
                tracing::warn!(
                    restored = restored.len(),
                    "health: restored peers that are gossiping but unprobeable — \
                     this node's mesh transport is failing against live peers"
                );
            }
            misses.retain(|endpoint_id, _| {
                live.contains(endpoint_id) || fanout.in_flight(endpoint_id)
            });
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
