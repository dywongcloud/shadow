//! `hive-cloud` — one node of a unified, multi-MacBook cloud.
//!
//! A single binary that runs, behind one public gateway + one admin API:
//! builds & sandbox (Hive control plane), serving with Fluid compute, an edge
//! pipeline (WAF, bot management, CDN), cron, workflows, previews, and a
//! region-aware node registry that meshes with peer nodes over HTTP gossip.

mod acme;
mod admin;
mod apikeys;
mod app_discovery;
mod audit;
mod auth;
mod billing;
mod browser_admission;
mod browser_artifacts;
mod browser_db;
mod browser_db_rest;
// bn-impl-relay-byte-metering (module declaration; sibling-owned file, flagged)
mod browser_metering;
mod browser_presence;
mod build_coordinates;
mod build_executor;
mod cluster;
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
mod geoip;
mod git;
mod github_app_auth;
mod gitops;
mod gossip;
mod gpu_pool;
mod guardian;
mod health;
mod hrana;
mod hrana_proto;
mod identity;
mod incidents;
mod inference;
mod integrations;
mod lease;
mod memwatch;
mod mesh_raw;
mod meshwatch;
mod metrics;
mod microfrontends;
mod microfrontends_api;
mod notifications;
mod persist;
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

/// The embedded relay's `AccessControl` (bn-p2p-revocation-latency) — see the
/// doc comment at its construction site (in `main`, right before the relay is
/// spawned) for why it's denylist-shaped and why `cloud` is a deferred cell.
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
            // Either CloudState isn't constructed yet (a connection landing in
            // the narrow startup window) or it has already been dropped
            // (shutdown) — fail open, matching this relay's existing
            // best-effort convention.
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

// Heap profiling, Linux only. jemalloc replaces the system allocator so a heap
// profile can be taken from a LIVE node, which is the capability whose absence
// left the 2026-07 fc-sanjose OOM (RSS ~12.9GB anon before the kernel killed
// it) permanently un-root-caused: nothing on the node could answer "which
// allocation site is growing?".
//
// `prof:true` compiles the machinery in; `prof_active:false` leaves it OFF, so
// the steady-state cost is jemalloc-vs-system-malloc and nothing more — no
// sampling, no per-allocation bookkeeping. Sampling is enabled at runtime via
// the admin endpoint (see `admin::heap_profile`), against the one node actually
// misbehaving, with no rebuild and no restart. lg_prof_sample:19 = sample every
// ~512KiB, the usual production-safe default.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(target_os = "linux")]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:false,lg_prof_sample:19\0";

/// The public HTTPS listener's `axum_server::Handle`, set once at listener
/// startup so the SIGTERM handler (defined earlier in `main`, run before the
/// listener spawns in source order but racing it at runtime) can reach it
/// without threading an extra parameter through every intervening call. A
/// `OnceLock` rather than a plain global `Handle` because the listener is only
/// created in the `cloud.ingress != "ngrok"` branch — ngrok ingress has no
/// public listener here to drain, and the shutdown handler already treats
/// "not set" as that case, not an error.
static SHUTDOWN_HTTPS_HANDLE: std::sync::OnceLock<axum_server::Handle> = std::sync::OnceLock::new();

#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum RestartReason {
    MemoryPressure = 1,
    MeshIsolation = 2,
    MeshDegradation = 3,
}

impl RestartReason {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::MemoryPressure),
            2 => Some(Self::MeshIsolation),
            3 => Some(Self::MeshDegradation),
            _ => None,
        }
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

    pub(crate) fn request(&self, reason: RestartReason) -> bool {
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
        self.inner.notify.notify_one();
        true
    }

    fn reason(&self) -> Option<RestartReason> {
        let state = self.inner.reason.load(std::sync::atomic::Ordering::Acquire);
        RestartReason::from_u8(state & Self::REASON_MASK)
    }

    /// Atomically close the request latch and consume the winning reason. A
    /// watchdog racing this boundary either wins before the seal and changes
    /// the exit code, or observes the sealed state and reports `false`; it can
    /// never report a successful request after main committed to exit zero.
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

// Explicit runtime instead of #[tokio::main] for ONE reason: worker stack
// size. run_build's async pipeline compiles to poll frames large enough to
// blow tokio's default 2 MiB worker stack — witnessed live: every deploy on a
// debug binary died `thread 'tokio-rt-worker' has overflowed its stack` the
// moment the build task started (the CI acceptance node crashed on its first
// deploy, red since f75aa2c5), and release binaries sit close enough to the
// edge that the same class killed nodes under real load. 16 MiB is virtual
// address space per worker, not resident memory — pages are only committed
// when touched — so the cost is nil and the whole failure class is gone.
fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .expect("build tokio runtime")
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // Install the process-level rustls CryptoProvider FIRST (later installs
    // are idempotent no-ops). The dep tree links both `ring` and `aws-lc-rs`
    // rustls features, so any rustls user that runs before one of the lazy
    // installs in spawned tasks panics "Could not automatically determine the
    // process-level CryptoProvider" — witnessed at boot on the ngrok-ingress
    // path, where the panicking task leaked a redb open handle and wedged
    // guardian init ("Database already open") until the next restart.
    let _ = rustls::crypto::ring::default_provider().install_default();
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

    // Operator diagnostic — answered and exited BEFORE any node state exists,
    // so it can be run safely from a laptop, a bastion or a live fleet node
    // without joining the mesh or touching a port.
    if !args.dns_probe.is_empty() {
        return dns_probe::run_cli(&args.dns_probe).await;
    }
    if !args.dht_probe.is_empty() {
        return dht_probe::run_cli(&args.dht_probe).await;
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

    // Restart audit — FIRST thing after the diagnostics, before any subsystem
    // can wedge or overwrite state. A node killed by the cgroup OOM killer is
    // restarted by systemd within a second and the platform records NOTHING:
    // the killed process cannot log its own death and the new one starts
    // clean, so the only evidence is a kernel line in `dmesg` on that host.
    // Measured cost of that silence: a node cycling every 2-3h presented as
    // "random unhealthy nodes" for a whole session, with every downstream
    // symptom investigated separately. This reads the previous run's marker,
    // reaches a verdict, and says so loudly. Pure local file I/O — it cannot
    // fail the boot. See restart_audit.rs.
    restart_audit::audit_boot(&args.name);

    // Shared isolation backend: a ranked chain, Firecracker -> Litebox -> Mock.
    // `HIVE_FORCE_MOCK=1` suppresses Firecracker specifically (its ORIGINAL,
    // still-preserved purpose: fc-frankfurt sets this because its `/dev/kvm` +
    // firecracker binary both pass the existence check below while a real
    // microVM *boot* has hard-reset that host three times — see AGENTS.md's
    // "PVM kernels" section). It does NOT mean "force mock no matter what":
    // when a verified Litebox backend is available it is a strictly better
    // fallback than the mock's zero isolation, so it is tried first. A node
    // with no Litebox binary/verification falls through to mock exactly as
    // before — zero behavior change for every node except one deliberately
    // opted in via `HIVE_LITEBOX_VERIFIED=1`.
    let force_mock = std::env::var("HIVE_FORCE_MOCK")
        .map(|v| v == "1")
        .unwrap_or(false);
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
    let firecracker = Arc::new(FirecrackerBackend::new(fc_cfg.clone()));
    let firecracker_runtime_capabilities =
        resources::RuntimeCapabilitySource::firecracker(firecracker.clone(), &fc_cfg);
    // Backend kind ("firecracker"|"litebox"|"mock") captured alongside the
    // backend — gossiped so the placement scheduler only auto-targets
    // production isolation backends (never the local/mock Mac nodes).
    let sandbox_fc_supported = firecracker.is_supported() && !force_mock;
    // Litebox Tier 2: NEVER auto-detected live (see `LiteboxBackend::smoke_test`'s
    // doc comment and AGENTS.md's PVM two-tier precedent) — an operator runs
    // `--litebox-probe` once during bring-up on an idle node and only then
    // sets `HIVE_LITEBOX_VERIFIED=1`. Tier 1 (`is_supported`, existence-only)
    // still gates it so a verified-elsewhere env var copied onto a node that
    // never got the runner binary staged doesn't silently no-op into mock.
    let litebox = Arc::new(LiteboxBackend::default());
    let litebox_verified = std::env::var("HIVE_LITEBOX_VERIFIED")
        .map(|v| v == "1")
        .unwrap_or(false);
    let litebox_supported = litebox.is_supported() && litebox_verified && !sandbox_fc_supported;
    // Sandboxes' own backend selection mirrors the main isolation-backend
    // ranking one step behind (Firecracker -> Litebox -> none, never Mock —
    // an unsandboxed dev host reports EngineUnavailable instead of a fake
    // "sandbox" that doesn't isolate anything). `exec_command`/`exec_pty` are
    // now generic `CellBackend` trait methods (previously Firecracker-only
    // inherent methods), so this can hold a trait object like every other
    // subsystem instead of the concrete Firecracker type.
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
        // See `hive_backend::litebox`'s module doc for the full, honest
        // security posture — this beats mock's zero isolation but is NOT
        // Firecracker/gVisor-grade; never silently substituted for either.
        tracing::warn!(
            "isolation backend: Litebox (unprivileged syscall sandbox, HIVE_LITEBOX_VERIFIED=1) \
             — real microVMs unavailable/suppressed on this host. NOT a hardware isolation \
             boundary; see hive_backend::litebox module doc for the full security posture."
        );
        (litebox, "litebox", litebox_runtime_capabilities)
    } else {
        if force_mock && !litebox_verified {
            tracing::warn!("isolation backend: MockBackend (HIVE_FORCE_MOCK=1, no verified Litebox) — runtime is mocked for local development");
        } else if force_mock {
            tracing::warn!("isolation backend: MockBackend (HIVE_FORCE_MOCK=1) — Litebox verified but its runner binary is missing on this host (Tier 1 check failed)");
        } else {
            tracing::warn!("isolation backend: MockBackend (sandboxed child process) — real microVMs need Linux + /dev/kvm; this is expected for local dev. ALL OTHER subsystems run for real.");
        }
        (
            Arc::new(MockBackend::new(MockConfig {
                root: std::env::temp_dir().join("hive-cloud-cells"),
                provision_latency: Duration::from_millis(200),
                cache_root: std::env::temp_dir().join("hive-cloud-cache"),
                // Durable: must survive a hive-node restart, unlike root/cache_root
                // above. Lives next to the sealed artifacts it describes.
                receipts_dir: crate::persist::data_dir().join("runtime-artifacts-v1"),
            })),
            "mock",
            resources::RuntimeCapabilitySource::mock(),
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
    // public_base is gossiped as `gateway` in serve_hosts and used for HTTP mesh
    // routing between nodes.  Using the `--listen` bind address (usually 0.0.0.0)
    // makes every cross-node proxy connect to localhost instead of the remote peer,
    // silently breaking the mesh.  Prefer HIVE_PUBLIC_IP when it is set so the
    // gateway URL carries a real address other nodes can actually reach.
    let public_base = {
        let port = args.listen.port();
        match std::env::var("HIVE_PUBLIC_IP")
            .ok()
            .map(|s| s.trim().to_string())
        {
            Some(v) if !v.is_empty() && v != "0.0.0.0" => format!("http://{}:{}", v, port),
            _ => format!("http://{}", args.listen),
        }
    };
    let cap = resources::capacity();
    // GPU probe (nvidia-smi, once at boot; HIVE_GPUS override) — advertised in
    // gossip so placement can target GPU hosts for gpu-requesting functions.
    let gpus = resources::detect_gpus();
    // Observe all three runtime capabilities from the backend that was actually
    // selected above. Firecracker reuses its provision-time exact-rootfs proof;
    // Litebox answers only after its selected instance remains supported (and
    // NEVER advertises Bun regardless — its syscall shim panics on Bun's own
    // boot probe); Mock never advertises runtime-artifact isolation.
    let runtime_capabilities = runtime_capability_source.detect().await;
    let wasm_rt = runtime_capabilities.wasm_runtime;
    let bun_rt = runtime_capabilities.bun_runtime;
    let runtime_artifact_protocol = runtime_capabilities.runtime_artifact_protocol;
    // A declaration is never enough: initialization re-hashes runsc and the
    // nft policy, checks the exact network/image/runtime, then executes a real
    // runsc + quota + nested-Buildah probe. Any fault advertises no capability.
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
    // Self-hosted discovery (Seer): pkarr relay URLs the node publishes to + resolves
    // from, instead of depending on n0's public pkarr/DNS. Added alongside n0 (the
    // mesh keeps working if Seer is down). Run Seer itself with HIVE_SEER_ADDR.
    let discovery_urls: Vec<String> = std::env::var("HIVE_DISCOVERY_DNS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // HIVE_DISCOVERY_N0=0 drops n0's public pkarr/DNS (Seer-only discovery, n0 relay
    // kept). Default keeps n0 (Seer additive).
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
    // Reachable PUBLIC IP for the client-facing DNS (Seer). Authoritative source is
    // HIVE_PUBLIC_IP (set on nodes with a real inbound-reachable address). These cloud
    // nodes sit behind 1:1 NAT (private NIC IP), so the public IP can't be sniffed off
    // the interface — it MUST be configured. `HIVE_PUBLIC_IP=auto` opts into ip-api
    // detection (correct for 1:1-NAT cloud nodes; do NOT use on home-NAT'd nodes, where
    // the detected IP is the ISP gateway, not reachable inbound). Unset → None (NAT-safe).
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

    // Embedded iroh-relay server: every hived instance runs its OWN relay listener
    // in-process (a real HTTP relay/NAT-traversal fallback for the mesh — see
    // `iroh_relay::server`'s module docs: it's a genuine in-process server API,
    // not just the CLI binary, via `Server::spawn`). This is additive alongside
    // the standalone `iroh-relay` binaries already running on bkk/va/sj (:3340)
    // during the transition — a different default port (3341) means an embedded
    // listener never collides with a standalone one on the same host. Started
    // PLAIN HTTP (no TLS/QUIC address-discovery yet): the CLI binary's
    // self-signed-cert generation for that isn't exposed as a public library call
    // in this iroh-relay version, so wiring it up is a deliberate, separate
    // follow-up. Best-effort + fail-open: a bind failure here must NEVER block
    // this node's own admin/gateway serving.
    let relay_port: u16 = std::env::var("HIVE_OWN_RELAY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(args.relay_port);
    let relay_bind: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        relay_port,
    );
    // Deny revoked browser endpoints AT THE RELAY (bn-p2p-revocation-latency):
    // routing-layer revocation (`fluid_gateway`'s browser target removal)
    // stops NEW invocations from picking a revoked target, but a revoked
    // endpoint could still reconnect and sit on the relay indefinitely. This
    // is a real `AccessControl` (iroh-relay 1.0.2's `RelayConfig.access`,
    // `Arc<dyn DynAccessControl>`, default `AllowAll`) keyed on the
    // handshake-PROVEN `EndpointId`, denylist-shaped so it never affects
    // fleet-mesh peers relaying through this SAME server (only endpoint_ids
    // this node's OWN `browser_admissions` store has explicitly revoked are
    // ever denied — everyone else, including every fleet node, is Allow).
    // `CloudState` doesn't exist yet at this point in boot, so the impl reads
    // through a `Weak` cell filled in once it does (`browser_relay_access_cell`
    // below) — until then it fails OPEN (Allow), matching this whole relay
    // block's existing best-effort/fail-open convention.
    let browser_relay_access_cell: Arc<std::sync::OnceLock<std::sync::Weak<CloudState>>> =
        Arc::new(std::sync::OnceLock::new());
    let mut embedded_relay_cfg = iroh_relay::server::ServerConfig::default();
    let mut relay_cfg = iroh_relay::server::RelayConfig::new(relay_bind);
    relay_cfg.access = Arc::new(BrowserRelayAccess {
        cloud: browser_relay_access_cell.clone(),
    });
    // PER-CLIENT INGRESS RATE LIMIT. This relay is PUBLIC-FACING on :3340 on
    // every node and previously ran with `Limits::default()`, i.e. none at all.
    // n0's own guidance is blunt that a relay has finite bandwidth and finite
    // connection slots, and the exposure is larger than it looks: browser/WASM
    // iroh clients are relay-ONLY by compile-time construction (the IP transport
    // is `#[cfg(not(wasm_browser))]`), so any browser-side traffic lands here
    // rather than going direct.
    //
    // Sized as a per-client BACKSTOP, not a working limit: 16 MiB/s is far above
    // what a relayed control-plane peer or tunnel actually pulls, so legitimate
    // fleet traffic never touches it, while one abusive client can no longer
    // saturate a node's relay. `HIVE_RELAY_CLIENT_BPS=0` disables it and restores
    // the previous unlimited behaviour.
    //
    // Deliberately NOT setting `accept_conn_limit`/`accept_conn_burst`: iroh-relay
    // 1.0.2 documents both as "Not currently implemented, setting this has no
    // effect". Setting them would look like a connection cap while enforcing
    // nothing — worse than leaving them alone, because a future reader would
    // believe the cap exists.
    let relay_bps = env_u64("HIVE_RELAY_CLIENT_BPS", 16 * 1024 * 1024);
    if let Some(bps) = std::num::NonZeroU32::new(relay_bps.min(u32::MAX as u64) as u32) {
        let mut rl = iroh_relay::server::ClientRateLimit::new(bps);
        // Burst allowance so a legitimate short spike (a deploy artifact moving
        // over a relayed trunk) is not clipped by the steady-state rate.
        rl.max_burst_bytes = std::num::NonZeroU32::new(bps.get().saturating_mul(2));
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
    // Advertise our OWN relay only when we're actually listening AND have a
    // routable public address to put in the URL — a NAT'd node can't usefully
    // advertise a relay peers can't reach (same rule `public_ip`/`public_url`
    // already follow). Kept alive for the whole process (`relay_server` isn't
    // dropped until `main` returns, which — barring a fatal listener error below
    // — is never, so the server outlives the boot function).
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
        // Not ready yet — GuardianDB's own iroh client hasn't bound at this
        // point in boot. Filled in by the gossip loop via
        // registry.set_self_guardian_addr() once guardian::my_iroh_addr()
        // resolves (best-effort, usually within the first couple of rounds).
        guardian_iroh_addr: None,
        // Not set here either — mirrors `guardian_iroh_addr`: populated right
        // after `registry` exists, via `registry.set_self_relay_url(..)` below,
        // so `relay_url`'s own doc comment stays the single source of truth for
        // when/why it's `None` (still filled in this same boot, just after the
        // registry the setter needs is constructed).
        relay_url: None,
        // Nameserver INTENT: only a REAL public `:53` bind counts. A
        // loopback/dev bind (the default 127.0.0.1:5354) is not reachable by
        // any resolver, so advertising it would put a black hole in the
        // delegated zone's NS set. This is a necessary condition, never a
        // sufficient one — it says nothing about whether the internet can
        // reach the listener. Peers prove that separately and gossip the
        // result (`dns_attest`, `dns_probe::spawn_ns_prober`), and the DNS
        // reconciler publishes an NS only for a node that is currently proven.
        dns_ns: std::env::var("HIVE_DNS_ADDR").ok().and_then(|a| {
            let port_is_53 = a.rsplit(':').next() == Some("53");
            let host = a.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
            let public_bind = host == "0.0.0.0" || host == "[::]" || host == "::";
            (port_is_53 && public_bind).then(|| a.clone())
        }),
        // API-zone capability: this binary's Seer answers `api.{platform}`
        // (see dnsserver::api_zone), so a public-`:53` node may appear in that
        // zone's NS set. Older binaries never set this, which is what gates the
        // api delegation until enough of the fleet can actually answer it.
        dns_api: {
            let ns_ok = std::env::var("HIVE_DNS_ADDR")
                .ok()
                .map(|a| {
                    let port_is_53 = a.rsplit(':').next() == Some("53");
                    let host = a.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
                    port_is_53 && (host == "0.0.0.0" || host == "[::]" || host == "::")
                })
                .unwrap_or(false);
            ns_ok
                && std::env::var("HIVE_PLATFORM_DOMAIN")
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
        },
        // Filled in by `dns_probe::spawn_ns_prober` from this node's own
        // off-host probes of its peers, refreshed every round (same post-boot
        // fill-in pattern as `relay_url`/`guardian_iroh_addr`).
        dns_attest: Vec::new(),
        // A MEASUREMENT, not intent: `false` until the dashboard probe loop
        // (spawned below, post-registry) sees the local upstream answer within
        // budget. Same fill-in pattern as `dns_attest`.
        dashboard: false,
        // Boot value; the gossip loop refreshes this every round from the
        // cluster's observed-owner epoch (registry.set_self_cp_epoch).
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
        // Seeded here so the very first gossip round already carries a real
        // figure; `spawn_disk_refresh` keeps it current from then on. Without a
        // boot seed a node advertises 0 ("unknown") for its first interval,
        // which placement must not mistake for "full".
        disk_free_gb: crate::resources::disk_free_gb(),
        gpu_free_mb: crate::resources::measured_gpu_free_mb(),
        // Seeded from the boot audit that already ran above, so the very first
        // gossip round already carries the verdict; `spawn_disk_refresh` slides
        // the 24h window from then on.
        started_ms: crate::restart_audit::started_ms(),
        oom_restarts_24h: crate::restart_audit::oom_restarts_24h(),
        last_oom_ms: crate::restart_audit::last_oom_ms(),
        backend: backend_name.clone(),
        gpu_count: gpus.0,
        wasm_runtime: wasm_rt,
        bun_runtime: bun_rt,
        runtime_artifact_protocol,
        // The executor may be initialized above, but this stays fail-closed until
        // every git build surface consumes it in this binary.
        build_isolation_protocol: None,
        // Published after CloudState::new proves the transfer receiver actually
        // initialized (its worker, durable store and recovery all succeeded) —
        // never asserted from this literal.
        artifact_transfer_protocol: None,
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
    let registry = NodeRegistry::new(me);
    // Populate this node's own relay_url now that `registry` exists (mirrors
    // `set_self_guardian_addr`'s post-boot fill-in pattern) — it rides along in
    // the very next `/v1/nodes/announce` gossip broadcast, no new RPC surface.
    registry.set_self_relay_url(own_relay_url.clone());

    // Report any at-rest secret this node can no longer open. A key change
    // orphans previously-sealed values silently (decrypt hands back the raw
    // ciphertext rather than failing), so without this the breakage is
    // invisible until an app misbehaves for unrelated-looking reasons.
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
    // Fill the embedded relay's deferred AccessControl cell now that
    // CloudState finally exists (it was constructed and wired into the relay
    // config well before this point — see the relay-spawn block above).
    // `Weak` deliberately: the AccessControl impl must never be the thing
    // keeping CloudState alive.
    let _ = browser_relay_access_cell.set(Arc::downgrade(&cloud));
    // Advertise the sealed-artifact transfer receiver only after CloudState
    // construction PROVED it initialized (durable store opened, worker
    // spawned, interrupted transactions recovered). A receiver that failed
    // closed keeps advertising `None`, so no coordinator ever selects this
    // node as an immutable-generation transfer target.
    if cloud.runtime_artifact_transfer.enabled() {
        cloud.registry.set_self_artifact_transfer_protocol(Some(
            crate::runtime_artifact_transfer_wire::PROTOCOL_VERSION,
        ));
    }

    // Tell the gateway the PUBLIC domain user deployments are reachable on, so
    // the URLs it reports (`DeploymentInfo::alias` and friends, which the
    // dashboard shows and the build log prints as "Aliased to …") name the host
    // that actually serves the deployment. Without this they fall back to the
    // local-dev `<project>.localhost`, which is what made a production deploy
    // report "Aliased to shoomoo.localhost". Reporting only — routing keys on
    // the host's first label and is unaffected either way.
    fluid_gateway::set_public_apps_domain(&cloud.apps_domain);

    // `--acme-txt-selftest` seed (see the arg's doc comment).
    if let Some((fqdn, value)) = args
        .acme_txt_selftest
        .as_deref()
        .and_then(|s| s.split_once('='))
    {
        cloud.acme_challenges.insert(fqdn, value);
        tracing::info!(%fqdn, "ACME DNS-01 selftest TXT seeded into the challenge store");
    }

    // Restore persisted platform state from disk (deployments, settings, WAF…).
    persist::restore(&cloud, persist::load());
    // Start the coalescing background persister: after this, persist() marks dirty
    // + wakes the writer instead of fsync-ing the whole state on the request thread.
    persist::spawn_persister(cloud.clone());
    deployment_ledger::spawn_outbox(cloud.clone());
    // Metrics hour/day rollups (metrics.rs's RollupSnapshot, the only durable slice
    // of MetricsStore) are the sole exception to "persist() runs after every
    // mutation": state.rs's record() — called on every single HTTP request — never
    // calls persist(), so a tenant with real traffic but no OTHER admin mutation
    // (deploy/db-create/team-edit etc.) since the last persist can lose its entire
    // Weekly/Monthly usage history on an UNCLEAN shutdown (crash, OOM-kill, `kill
    // -9` — the graceful SIGTERM flush below covers a clean `systemctl restart`,
    // but not those). persist() is cheap to call even under heavy traffic (it only
    // marks a dirty generation + wakes the coalescing background writer, which
    // folds every mutation since its last drain into ONE capture+write) — a
    // periodic safety-net call closes the gap, matching spawn_guardian_snapshot_
    // loop's identical "periodic flush independent of mutation timing" fix for the
    // same underlying bug class.
    spawn_metrics_persist_loop(cloud.clone());
    // Graceful-shutdown flush: on SIGTERM/SIGINT (e.g. `systemctl restart`) drain
    // the public listener's in-flight connections (bounded), THEN write the
    // latest state synchronously so a restart loses nothing from the coalescing
    // window. Previously this exited immediately on signal, severing every
    // in-flight request AND every gateway tunnel proxying to a placed app's cell
    // the instant SIGTERM arrived — the literal mechanism behind "a hive-node
    // restart breaks placed apps' cell tunnels". Bounded by
    // HIVE_SHUTDOWN_GRACE_SECS (default 15s) so a stuck connection cannot hang a
    // restart forever; systemd's own TimeoutStopSec is the final backstop if this
    // grace window is somehow exceeded.
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
            // HARD DEADLINE on the whole graceful sequence. Several steps below
            // await work that is not individually bounded (the runtime-artifact
            // transfer drain, the platform-state flush, guardian shutdown), and a
            // wedge in any of them leaves the process alive but dark — witnessed
            // live on fc-sanjose (2026-08-26): memwatch requested a MemoryPressure
            // restart at rss 12GB, "beginning graceful shutdown" logged, and the
            // process then sat frozen for 5+ minutes serving nothing while the
            // fleet treated the dark leader as current. The watchdog turns any
            // wedged step into the bounded restart the caller asked for; the exit
            // code matches what the normal tail would have used.
            {
                let deadline = Duration::from_secs(env_u64("HIVE_SHUTDOWN_DEADLINE_SECS", 90));
                let code = if requested_reason.is_some() { 17 } else { 0 };
                tokio::spawn(async move {
                    tokio::time::sleep(deadline).await;
                    tracing::error!(
                        ?deadline,
                        exit_code = code,
                        "graceful shutdown exceeded its hard deadline — forcing exit now"
                    );
                    std::process::exit(code);
                });
            }
            let grace = Duration::from_secs(env_u64("HIVE_SHUTDOWN_GRACE_SECS", 15));
            if let Some(handle) = SHUTDOWN_HTTPS_HANDLE.get() {
                tracing::info!(?grace, "shutdown requested → draining public listener (in-flight requests + cell tunnels)");
                handle.graceful_shutdown(Some(grace));
                // graceful_shutdown stops new connections and gives existing ones
                // the grace window; wait for it here since exit() below would
                // otherwise kill them the instant this task returns regardless.
                tokio::time::sleep(grace).await;
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
            // Stamp the run marker as a GRACEFUL exit. Without this every
            // deploy restart and every `systemctl restart` would be classified
            // `unclean_exit` on the way back up, and a signal that fires on
            // ordinary operations is a signal nobody reads — which is how the
            // real OOM kills stayed invisible in the first place.
            restart_audit::mark_clean_exit(&flush_node_name);
            // Same reason, different file: the geo cache's saver is debounced,
            // so a clean restart would otherwise drop whatever was learned
            // inside the last window and de-tailor those prefixes on the way
            // back up. Cheap (one small sidecar) and best-effort.
            flush_cloud.dns_geo.flush_blocking();
            // Tell every peer we are going away, instead of letting them find out.
            //
            // Without this, `exit(0)` below tears the process down with no QUIC
            // CONNECTION_CLOSE on the wire, so each peer keeps a trunk it believes
            // is live until its own idle timeout expires — and on restart this node
            // usually comes back with different socket addrs, so those trunks are
            // not merely idle but WRONG. That is the already-documented "after a
            // peer restarts with new socket addrs, the stale QUIC trunk lingers
            // until idle-timeout (~tens of seconds)" behaviour in gossip.rs: a
            // symptom that was recorded without its cause. `close()` collapses that
            // window to one round trip. iroh's own guidance is explicit that
            // `Endpoint::close()` must be awaited to completion rather than left to
            // process teardown; it is bounded here so a wedged relay can never turn
            // a clean restart into a hung one.
            // Drain the exact final admitted Guardian generation and durably shut
            // the backend down (Docs/Router/Store) before the endpoint closes and
            // the process exits — an acknowledged write must not merely reach the
            // watch channel and then be abandoned mid-flight by process::exit.
            if let Err(error) = crate::guardian::shutdown(final_guardian_generation).await {
                tracing::error!(%error, "Guardian shutdown did not complete cleanly; exiting anyway");
            }
            // Bound to its own statement so the lock guard is dropped BEFORE the
            // await below — holding it across an await makes this future non-Send.
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
        // Team-scoped browser targets (bn-team-scoped-browser-targets-never-served):
        // resolve the CALLER's authenticated tenant from the same headers
        // auth::require_auth already accepts (Bearer / hive_jwt cookie / a
        // dashboard API key) so try_browser can gate a Team-scoped target to
        // members of its own owning tenant, never a public/anonymous caller.
        // Public-scoped targets never call this at all.
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
        // Live relay-set tracker (dynamic-hive-relay-urls-list): kept alongside
        // `mesh`, synced on an interval by `spawn_relay_sync_loop` below.
        *cloud.relay_set.write() = Some(hive_p2p::RelaySet::new(ep.clone()));
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
                        cloud.registry.upsert_peer_self_report(node);
                        cloud.audit.record("_global", "mesh", "join", "node", &name, &format!("endpoint {remote_id} admitted via join proof"));
                        tracing::info!(peer = %remote_id, node = %name, "mesh join ADMITTED (hot-join, key-addressed)");
                        serde_json::to_vec(&cloud.registry.nodes()).unwrap_or_default()
                    })
                });
                h
            });
        // Generic raw TCP/UDP mesh forwarding (owner-node accept side): resolve
        // inbound `STREAM_RAW_TARGET` handshakes to this node's local container
        // legs — the cross-node hop behind the raw-port proxy / UDP relay.
        let raw_resolver = crate::mesh_raw::resolver(cloud.clone());
        let admission_cloud = cloud.clone();
        let browser_admission: hive_p2p::BrowserAdmissionHandler =
            std::sync::Arc::new(move |endpoint_id: String| {
                let cloud = admission_cloud.clone();
                Box::pin(async move {
                    crate::browser_admission::endpoint_admitted(&cloud, &endpoint_id).await
                })
            });
        // Browser-replicated database exchange (bn-browser-fleet-crr-exchange):
        // the fleet half of `Op::CrrSync` — per-request grant re-check against
        // this node's own replicated admission view, capped apply/export
        // against the per-project replica file. See browser_db.rs.
        // HIVE_BROWSER_DB_LISTEN=0 disables the serve arm (rollout/ops knob):
        // `Op::CrrSync` then gets NO_HANDLER — exactly how a pre-change
        // binary refuses it (never a fake grant, never a crash).
        let browser_crr = std::env::var("HIVE_BROWSER_DB_LISTEN")
            .ok()
            .filter(|v| v.trim() == "0")
            .map_or_else(
                || Some(crate::browser_db::crr_sync_handler(&cloud)),
                |_| None,
            );
        // bn-impl-relay-byte-metering (registration line; sibling-owned file, flagged)
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
        ));
        tracing::info!(gateway = %args.listen, "iroh P2P tunnel server accepting peer connections (join + raw-target surfaces on)");
    }

    // Initial owner resolution (single-node: this node is owner) + seed the
    // gossiped fencing epoch.
    let _ = cloud.control_plane_leader();
    cloud.registry.set_self_cp_epoch(cloud.cluster.epoch());

    // Background loops: cron scheduler + peer gossip.
    spawn_cron_loop(cloud.clone());
    spawn_cluster_loop(cloud.clone());
    spawn_guardian_snapshot_loop(cloud.clone());
    spawn_guardian_reap_loop(cloud.clone());
    spawn_lease_loop(cloud.clone());

    // Self-management GC: reap stale clone/build working dirs under the deploy
    // roots ($HIVE_DATA/deploys, plus the legacy /tmp/hive-deploys) every 10 min
    // (dirs untouched >30 min are dead builds), so build scratch never exhausts
    // host disk. Pairs with the firecracker orphan-overlay GC. Skips dirs that
    // still back a live deployment.
    let gc_cloud = cloud.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let n = crate::git::gc_build_dirs(&gc_cloud, Duration::from_secs(1800)).await;
            if n > 0 {
                tracing::info!(removed = n, "gc: cleaned stale build dirs");
            }
            // Same tick, different leak: a `Building…` placeholder whose build
            // task never reached its removal line (user cancel aborts the task
            // handle; a panic ends it) owns the project's host alias forever and
            // makes this node answer for a deployment it does not have. Reaped
            // here so a running node self-heals instead of waiting for a restart.
            let p = crate::git::reap_orphan_placeholders(&gc_cloud).await;
            if p > 0 {
                tracing::info!(removed = p, "gc: reaped orphaned Building… placeholders");
            }
        }
    });

    // Warm the always-on GuardianDB (durable, iroh-replicated state store) so it
    // is live before the first snapshot. Best-effort; never blocks boot.
    guardian::set_node_name(&args.name);
    // Seed known peers (GuardianDB-specific addresses — persist::
    // save_peer_guardian_addr in the gossip loop, mirroring peer_iroh.json)
    // BEFORE the KV store's one-time open. Empty on this node's first-ever
    // boot with this feature (nothing has persisted a guardian address yet);
    // has real data starting the restart after the gossip loop has had a
    // chance to populate and persist it. See set_boot_seed_peers's doc
    // comment for why this specific window matters.
    guardian::set_boot_seed_peers(
        crate::persist::load_peer_guardian_addr()
            .into_values()
            .collect(),
    );
    guardian::init_background();
    // Fleet-consistent relational tables (project_teams, billing_*) — see
    // relational.rs's module doc. Idempotent (CREATE TABLE IF NOT EXISTS);
    // best-effort like every other GuardianDB-backed call, never blocks boot.
    // Backfill existing projects (created before this migration shipped, so
    // `set_project_team` never fired for them) — see backfill_projects's doc
    // comment for why this is safe on every node but billing is deliberately
    // excluded (self-heals via the metering loop instead).
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
    // Active full-mesh health probing: direct, parallel probes of every public peer so
    // down-detection is fast (sub-interval) rather than transitive-gossip + staleness.
    spawn_health_loop(cloud.clone());

    // Eager full-mesh trunking: proactively keep a live iroh trunk to EVERY healthy
    // peer (not just the ones we directly gossip), so cross-node requests reuse a
    // warm trunk instead of paying a cold dial/holepunch on the critical path.
    spawn_trunk_warmer(cloud.clone());

    // Live relay-set refresh (dynamic-hive-relay-urls-list): keeps the bound iroh
    // endpoint's relay map in sync with [own relay_url, every healthy peer's
    // relay_url, the central relay.shadw.cloud backstop] as the registry changes,
    // via live `insert_relay`/`remove_relay` (no rebind) — see `RelaySet`.
    spawn_relay_sync_loop(cloud.clone());

    // GuardianDB anti-entropy loop (implement-anti-entropy-loop): periodic
    // Dynamo-style read-repair over the iroh-docs-backed replicated store —
    // catches divergences the opportunistic live-sync path missed (this node
    // offline/partitioned during a peer's write, ticket exchange never ran
    // between this pair, etc). See `spawn_anti_entropy_loop`'s doc comment.
    spawn_anti_entropy_loop(cloud.clone());
    spawn_geo_refresh(cloud.registry.clone());
    spawn_disk_refresh(cloud.registry.clone(), runtime_capability_source.clone());
    spawn_memory_pressure_alarm();

    // Billing meter loop: periodically converts measured fleet compute usage into
    // charges (usage → rate card → ledger → invoice). Runs whether Stripe is
    // configured or not (mock or real). Web3 decentralization: the loop is spawned
    // on EVERY node, and each tick the acting meter is ELECTED from live membership
    // (lowest healthy cryptographic iroh identity — see `Cluster::billing_leader`)
    // with automatic failover; no hardcoded privileged node. A 2-tick stability
    // window keeps a flapping health view from double-charging during transitions.
    // `HIVE_BILLING_COORDINATOR_NODE` remains as an explicit manual PIN override.
    spawn_billing_meter_loop(cloud.clone());

    spawn_promotion_reconcile_loop(cloud.clone());

    // Relational mirror: teams/members/deployments + full billing backfill into
    // the fleet-replicated SQL view (see spawn_relational_mirror_loop's doc).
    spawn_relational_mirror_loop(cloud.clone());

    // Web-push + SMS delivery for the notification inbox — leader-only inside
    // the loop, tenant-scoped by construction (see `push::spawn_push_dispatcher`).
    crate::push::spawn_push_dispatcher(cloud.clone());

    // Managed World Queue delivery loop (hive-native Queue for the Vercel WDK
    // World interface -- no external queue dependency).
    tokio::spawn(crate::world_queue::run_delivery_loop(
        cloud.clone(),
        cloud.world_queue.clone(),
    ));

    // Cloudflare Queues parity: Worker-consumer push delivery + retention
    // sweep + GuardianDB recovery (crate::queues).
    tokio::spawn(crate::queues::spawn_delivery_loop(cloud.clone()));

    // Nameserver prover: EVERY node (not leader-only — the whole value is
    // independent vantages) queries every peer that claims a public `:53` and
    // gossips the ones that actually answer, so the reconciler below can
    // publish an NS only for a node proven reachable from off its own host.
    dns_probe::spawn_ns_prober(cloud.clone());

    // Dashboard capability prober: measure THIS node's own dashboard upstream
    // and gossip the verdict (`NodeInfo::dashboard`), so the DNS reconciler can
    // keep slow-SSR nodes out of the apex/`www` A-set (see
    // `vercel_dns::desired_platform`). The budget bounds the first-paint SSR a
    // published node may impose on a first visit; a node with no upstream
    // configured simply never claims the capability.
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
                    _ => false, // slow (over budget), connection refused, or error
                };
                cloud.registry.set_self_dashboard(ok);
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    // Vercel DNS reconciler (ngrok retirement): leader-elected loop publishing
    // healthy node IPs to api.{platform}/*.{apps} via the Vercel API. No-op in
    // ngrok ingress mode (unless HIVE_DNS_RECONCILE=1 forces it for testing).
    vercel_dns::spawn_reconciler(cloud.clone());

    // ACME (Let's Encrypt DNS-01 via Vercel): leader issues/renews the wildcard
    // bundles; every node syncs them from the replicated store and hot-swaps the
    // SNI resolver. No-ops entirely in ngrok ingress mode.
    acme::spawn_acme(cloud.clone());
    acme::spawn_cert_sync(cloud.clone());
    // Custom tenant domains (verified attaches) get per-domain HTTP-01
    // bundles: leader issues/renews, every node syncs via the same
    // guardian/mesh/http distribution as the platform bundles.
    acme::spawn_custom_cert_loop(cloud.clone());
    // Self-heal provisioned DB backings (restart-killed / machine-reset
    // containers) — see spawn_db_reconcile's doc for the witnessed loss classes.
    databases::spawn_db_reconcile(cloud.clone());
    // Custom-domain ownership verification: the leader watches pending
    // challenges and asks each attach's owning node to activate (the owner
    // arm re-proves the TXT itself) — see activate_domain_alias in admin.rs.
    crate::admin::spawn_domain_verify_loop(cloud.clone());
    // Managed serverless-GPU inference endpoints (llama.cpp): coordinator
    // nodes run/converge llama-server children, the leader injects
    // HIVE_INFERENCE_URL — see inference.rs's module doc.
    inference::spawn_reconcile(cloud.clone());
    // Reschedule orphaned workflow-world queue jobs (claimed-then-dropped
    // deliveries) so a delivery-path failure can never strand runs in
    // `pending` forever — see world::reconcile_orphan_jobs.
    world::spawn_world_reconcile(cloud.clone());

    // Auto-deploy git-sourced projects that have NO installed webhook by polling
    // their tracked branch's HEAD (leader-only) — the credential-free path that
    // makes `git push` deploy even when the owner's GitHub connection is dead or
    // the project was imported as a plain public URL. See git::spawn_git_poll_reconcile.
    git::spawn_git_poll_reconcile(cloud.clone());

    // Keep podman's shared lock pool from filling up. Containers AND volumes each
    // consume one lock out of a fixed pool (default 2048), so a leak starves the
    // whole HOST: witnessed 2032 leaked volumes on the leader, freeLocks 0, and
    // every deployment's cold start on that node failing as 503
    // CAPACITY_EXHAUSTED. The leak itself is fixed (`podman rm -v`) and the run
    // path self-heals reactively, but this sweep restores headroom BEFORE a
    // request pays for it — and covers locks leaked by anything outside that
    // path (a crashed node, a manual podman run).
    spawn_container_lock_sweep();

    // Memory watchdog. Every node (RSS is per-host, like the lock pool above).
    // The fleet's OOM kills are episodic bursts that idle-state profiling never
    // catches, so this arms jemalloc sampling and dumps a profile DURING the
    // burst, and logs the allocator/RSS/bound gauges every tick above the arm
    // threshold — the record that survives the kill. See `memwatch`.
    memwatch::spawn(cloud.hive.clone(), controlled_restart.clone());

    // Mesh-isolation watchdog. Every node. A node whose iroh transport wedges
    // keeps its process, unit and HTTP surfaces healthy while seeing ZERO of
    // its peers — and never recovers on its own (measured: 2.17M iroh
    // transport events, gossip completely dead, `systemctl is-active` still
    // `active`). On the control-plane leader that also fails every admin
    // mutation fleet-wide, because the leader-forward candidate list is built
    // from the local registry and comes out empty. See `meshwatch`.
    meshwatch::spawn(cloud.clone(), controlled_restart.clone());
    tenancy_reconcile::spawn(cloud.clone());
    spawn_deletion_reconcile_loop(cloud.clone());

    // Restart-audit heartbeat. Writes the marker the NEXT boot classifies
    // against (a SIGKILLed process cannot write it on the way out, which is
    // exactly why the kill was invisible), and re-states an OOM-cycling
    // verdict periodically so it is loud in a log tail taken at any moment —
    // not only in the seconds right after the restart. See restart_audit.rs.
    restart_audit::spawn(args.name.clone());

    // Reap browser-function artifacts no live deployment references anymore.
    // Every node (the store is per-host), guarded against empty/mostly-orphaned
    // keep-sets — see browser_artifacts::gc.
    browser_artifacts::spawn_gc_loop(cloud.clone());
    // Browser-replicated databases (bn-browser-fleet-crr-exchange): per project
    // with a `browser_db` opt-in, keep the fleet replica file (+ its spec-
    // derived schema) present, and reap inert replicas only past the 30-day
    // grace window with the blast-radius guards — see browser_db.rs.
    browser_db::spawn_reconcile(cloud.clone());
    // Fleet<->fleet anti-entropy for those replicas. Without it, node-to-node
    // convergence flows ONLY through browser carriers, so a project whose tabs
    // are all closed stops converging and a freshly-joined node keeps answering
    // from the empty replica `reconcile_replicas` created for it. Pull-only,
    // every node, bounded peers per tick — see browser_db::spawn_fleet_sync.
    browser_db::spawn_fleet_sync(cloud.clone());
    // Protocol-wide browser ROLL CALL: a periodic read-only inventory + drift
    // audit over the admitted browser population (who is present, what each
    // serves, which db grant each holds) against this node's own gateway
    // routing. Every node, because the routing half is node-local — see
    // browser_admission::spawn_roll_call. Cadence: HIVE_BROWSER_ROLL_CALL_SECS
    // (default 420s, `0` disables).
    browser_admission::spawn_roll_call(cloud.clone());

    // Public gateway, wrapped in the edge pipeline.
    let public = fluid_gateway::public_router(gw.clone()).layer(
        axum::middleware::from_fn_with_state(cloud.clone(), edge::edge_pipeline),
    );
    // Connection-level DoS bounds on the control plane (the admin router has no
    // streaming/SSE endpoints and deploys enqueue-then-return, so a bounded
    // per-request timeout and body cap are safe — unlike the public gateway,
    // which streams tenant responses and already caps request bodies at 16 MiB
    // + per-IP rate-limits in the edge pipeline). Env-tunable.
    let admin_max_body: usize = std::env::var("HIVE_ADMIN_MAX_BODY_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
        * 1024
        * 1024;
    let admin_req_timeout = Duration::from_secs(
        std::env::var("HIVE_ADMIN_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
    );
    let admin_router = admin::router(cloud.clone())
        // guardian-growth-and-gc-observability: guardian.rs owns this route's
        // handler/state end-to-end (single-writer scope), so it merges here
        // rather than adding a line inside admin::router() itself. Same
        // auth/rate-limit/body-limit/timeout layers as every other admin
        // route below.
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
            // Incoming GitOps/OpenEdge build-notification receiver
            // (OPENEDGE_WEBHOOK_URL, /v1/git/webhook) — same admin router, its own
            // host so webhook traffic is distinguishable from the api./admin.
            // developer/operator surfaces.
            let webhook_host = format!("webhook.{}", cloud.platform_domain);
            // Dashboard hosts (apex + www): reverse-proxied to `HIVE_DASHBOARD_UPSTREAM`
            // — each node's own self-hosted dashboard on loopback
            // (http://127.0.0.1:3002), never an external tunnel. Empty upstream =
            // no dashboard hosts.
            let dash_upstream = std::env::var("HIVE_DASHBOARD_UPSTREAM")
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| !v.is_empty());
            let dash_hosts = vec![
                cloud.platform_domain.clone(),
                format!("www.{}", cloud.platform_domain),
            ];
            tracing::info!(%api_host, %admin_host, %webhook_host, dashboard = ?dash_upstream, "host-based dispatch active (api/admin/webhook hosts → admin router; apex/www → dashboard proxy)");
            host_switch_router(
                cloud.clone(),
                api_host,
                admin_host,
                webhook_host,
                dash_hosts,
                dash_upstream,
                cloud.http.clone(),
                admin_router.clone(),
                public,
            )
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

    // Generic raw-TCP ingress (the db_gateway pattern generalized to ALLOCATED
    // ports): one public listener per raw-protocol deployment's stamped
    // public_port (`raw_ports`), on every node — local connections splice into
    // the leased instance, remote ones ride the iroh mesh's raw-target streams
    // to the owner. Reconciles listeners against local + gossiped allocations.
    raw_proxy::spawn(cloud.clone());

    // UDP relay — the DATAGRAM half of the raw-port space (Minecraft Bedrock
    // 19132/udp, any UDP service). Its own mechanism, NOT the TCP splice:
    // NAT-style per-client session table on each allocated public UDP port,
    // forwarding to the container's published `/udp` loopback port locally or
    // over `[u32 len]`-framed raw-target mesh streams to the owner node, with
    // idle-timeout session eviction. Shares raw_proxy's allocations + mesh
    // primitive; see `udp_relay.rs`.
    udp_relay::spawn(cloud.clone());

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
        // Graceful-shutdown handle for the PUBLIC listener specifically — this is
        // the one carrying real customer traffic (including active gateway tunnel
        // proxying to placed apps' cells), so it is the highest-value target for
        // "a restart must not break placed apps' cell tunnels". Cloned into the
        // SIGTERM handler below; `graceful_shutdown` stops accepting NEW
        // connections and gives already-open ones up to the grace window to
        // finish instead of the previous behavior (immediate `process::exit`
        // severing every in-flight request/tunnel the instant SIGTERM arrived).
        let https_shutdown_handle = axum_server::Handle::new();
        SHUTDOWN_HTTPS_HANDLE
            .set(https_shutdown_handle.clone())
            .ok();
        tokio::spawn(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
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
        // Port 80: redirect-only (no content ever served in cleartext) — with
        // ONE deliberate exception: ACME HTTP-01 challenges, which Let's
        // Encrypt fetches over plain http by definition. Answering them
        // directly (instead of 301ing to an https host that has no cert yet —
        // the deadlock that made first issuance impossible) is the entire
        // point of the challenge type. Everything else still 301s.
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
                                    // Miss on this node: the issuer is the
                                    // leader and Let's Encrypt's
                                    // multi-perspective fetches land on RANDOM
                                    // nodes within ~1s of set_ready — long
                                    // before the 60s store_sync pull can
                                    // replicate the leader-local token. Proxy
                                    // the lookup to the leader (whose own
                                    // port-80 arm answers from the store)
                                    // instead of fail-validating every
                                    // issuance ~13/14 of the time — the proxy
                                    // previously existed only on the 443
                                    // pipeline LE never reaches (adversarial
                                    // finding, both re-reviews). Unknown token
                                    // everywhere = flat 404, no existence leak.
                                    let leader = cloud.control_plane_leader();
                                    if leader != cloud.node_name {
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

        // Dedicated public IPv4 addon (paid): one additional HTTPS listener
        // per address this node owns, reusing this SAME `public` router (edge
        // pipeline + SNI resolver included) — never a bare TCP splice, which
        // would bypass WAF/bot defense entirely. See
        // `dedicated_ipv4_listener.rs`'s module doc for the full reasoning.
        dedicated_ipv4_listener::spawn(cloud.clone(), public.clone());
    }

    tracing::info!(region=%region, node=%args.name, public=%args.listen, admin=%args.admin, tls=%tls_addr, "hive-cloud node up");

    // The loopback admin listener serves the raw admin router, and the
    // dashboard's /cloud proxy targets it (HIVE_ADMIN=127.0.0.1:8786) — so a
    // dashboard-driven mutation was handled by WHICHEVER node hosted that
    // dashboard and never reached the control-plane leader (only the public
    // admin_ingress forwarded). Witnessed 2026-08-04: an admission granted on
    // a follower never appeared on the leader, and browser serving never
    // engaged through the public ingress. Mirror admin_ingress's discipline
    // here: mutations on a non-leader forward to the leader; reads stay local.
    let admin_router = admin_router.layer(axum::middleware::from_fn_with_state(
        (cloud.clone(), format!("api.{}", cloud.platform_domain)),
        admin_loopback_forward,
    ));

    let pub_srv = serve(public, args.listen, "public");
    let adm_srv = serve(admin_router, args.admin, "admin");
    tokio::try_join!(pub_srv, adm_srv)?;
    Ok(())
}

/// Mutations that must never be forwarded to the control-plane leader by
/// either leader gate — two DISTINCT rationales share this one bypass.
///
/// A managed SQLite database is a FILE on `Database::host_node`, and
/// `hrana::serve` proxies to that node itself. Sending its pipeline POSTs to
/// the leader first would (a) add a hop that still has to proxy, (b) pin an
/// interactive transaction's baton to whichever node was leader when the
/// stream opened — so a leadership change mid-transaction strands it — and (c)
/// deadlock the owner-proxy hop outright, because the mesh envelope
/// (`/v1/databases/<id>/hrana-mesh`) is itself a POST and would be bounced
/// straight back to the leader instead of being served by the owner it was
/// deliberately addressed to.
///
/// shadw drive (`/v1/drive/*`, both the REST surface and the WebDAV mount)
/// needs no owner at all: every write lands in `relational::drive_*`, which
/// is GuardianDB's CRDT last-write-wins relational store — safe to apply from
/// ANY node, by the store's own design (see `relational::drive_put_file`'s
/// doc comment). Leader-forwarding it bought zero correctness and cost a
/// real round trip on every single PUT/DELETE — measured as the direct cause
/// of "copying/moving files is slow" over WebDAV, since a Windows Explorer
/// drag-drop of many small files serializes one leader-forward hop per file.
/// WebDAV's own non-CRUD methods (MKCOL/COPY/MOVE/PROPFIND/LOCK/UNLOCK)
/// already bypassed this gate by accident (the `is_mutation` classifier below
/// only recognizes POST/PUT/DELETE/PATCH) — this makes PUT/DELETE consistent
/// with them instead of the other way around.
fn owner_routed(path: &str) -> bool {
    path.starts_with("/v1/sqlite/")
        || (path.starts_with("/v1/databases/") && path.ends_with("/hrana-mesh"))
        || (path.starts_with("/v1/databases/") && path.ends_with("/studio-mesh"))
        || path.starts_with("/v1/drive/")
        // browser_db's libsql/Hrana + Upstash REST surface (bn-browser-db-rest):
        // owner-ROUTED to the elected REST owner (`browser_db::rest_owner_for_project`),
        // never leader-forwarded — the `/v1/sqlite/` precedent, computed rather
        // than stored since there is no `Database` record to carry a host_node.
        || path.starts_with("/v1/browser-db/")
        || (path.starts_with("/v1/projects/") && path.ends_with("/browser-db/rest-mesh"))
        // Runtime artifact transfers are addressed to the immutable request's
        // exact target node. Sending a chunk to the control-plane leader would
        // mutate a different host's transaction journal (or fail WrongTarget)
        // and make resumable delivery impossible across leader changes.
        || path.starts_with("/v1/runtime-artifact-transfer/v1/")
}

/// Loopback-admin mutation forwarding (the admin_ingress leader rule, applied
/// to the raw admin listener). Reads serve locally; mutations on a non-leader
/// forward to the current leader over the SNI-pinned client. Chain
/// termination + internal hops: a request marked x-hive-admin-forwarded
/// (forwarded by a peer) or x-hive-internal (service hop) is always served
/// locally; /v1/token is stateless HS256 minting and must never depend on
/// leader reachability.
async fn admin_loopback_forward(
    axum::extract::State((cloud, api_host)): axum::extract::State<(
        std::sync::Arc<state::CloudState>,
        String,
    )>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_mutation = matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    // Chain termination with the same anti-spoof rule admin_ingress applies: a
    // forwarded-marked mutation is only ever applied on the CURRENT leader —
    // landing anywhere else means a forged marker or a mid-flight leadership
    // change, both refused with 503 (the client retries and re-resolves).
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
    // Forward to the leader over the public SNI-pinned HTTPS ingress — the
    // exact helper admin_ingress uses (production-proven transport), never a
    // second bespoke path.
    admin_forward_to_leader(&cloud, &api_host, req).await
}

/// One listener, split by Host (real-DNS ingress): `api.{platform_domain}` (the
/// PLATFORM API + API-key surface), `admin.{platform_domain}` (the ops/admin
/// console surface) AND `webhook.{platform_domain}` (incoming GitOps/OpenEdge
/// build-notification receiver, `OPENEDGE_WEBHOOK_URL`) → the admin router; the
/// dashboard hosts → the dashboard proxy; anything else → the deployment edge
/// pipeline. Implemented as a fallback handler that oneshots into the matching
/// inner router, so the x-hive-proxied loop guard, WS upgrade path and
/// everything else inside each router are untouched. Host matching is
/// case-insensitive and strips `:port`. api/admin/webhook share one router
/// today (same auth); the split is by HOSTNAME so each reads as its own
/// surface, and they can diverge (separate auth/route sets) without touching
/// this dispatch.
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
            // `api-<region>.<platform>` is the region-pinned API surface: same
            // dispatch as `api.`, but its DNS record names ONE region's nodes,
            // so a client that wants a specific region uses the hostname alone.
            // Pattern-matched rather than enumerated — the region list lives in
            // the DNS reconciler/ACME SANs, and an api-<x> host that was never
            // published simply doesn't resolve, so accepting the shape is safe.
            let region_api = api_host
                .strip_prefix("api.")
                .map(|domain| {
                    // `api-<region>.<domain>` exactly: the suffix must be a REAL
                    // label boundary (`.domain`), or `api-evil-shadw.cloud`-style
                    // hosts would match a bare ends_with(domain).
                    let suffix = format!(".{domain}");
                    host.starts_with("api-")
                        && host.ends_with(&suffix)
                        && host.len() > "api-".len() + suffix.len()
                        && !host["api-".len()..host.len() - suffix.len()].contains('.')
                })
                .unwrap_or(false);
            if host == api_host || host == admin_host || host == webhook_host || region_api {
                // Pass the MATCHED host so a leader-forward pins to the right SNI
                // (the platform cert covers api./admin./webhook. and, once the
                // SAN-coverage reissue lands, every api-<region>.).
                return admin_ingress(cloud, admin, host, req).await;
            }
            // Dashboard hosts: reverse-proxy to the configured origin — each
            // node's own self-hosted dashboard on loopback, never an external
            // tunnel (HIVE_DASHBOARD_UPSTREAM=http://127.0.0.1:3002).
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

/// Regional AdminAPI ingress for `api.{platform_domain}`. Runs on EVERY healthy
/// node — clients reach the nearest via health-aware DNS. It authenticates the
/// request, then serves locally IF this node is the control-plane leader, else
/// forwards to the leader over HTTPS (pinned to the leader's IP, SNI = api host).
/// A loop-guard header prevents re-forwarding. First-slice policy: after auth,
/// forward ALL requests (reads + writes) to the leader. Entirely dormant unless a
/// node runs with `HIVE_INGRESS!=ngrok` AND `HIVE_JWT_SECRET` set (see caller).
/// Explicit cache policy on every admin GET response (previously NO `/v1/*`
/// JSON carried any Cache-Control at all — every intermediary guessed).
/// TENANT-SAFE BY CONSTRUCTION, two classes only:
///
/// - **Global, non-tenant catalogs** (`/v1/regions*`, `/v1/frameworks`):
///   `public, s-maxage=3600, stale-while-revalidate=86400` — shared caches/CDNs
///   may serve these to everyone (they contain zero tenant data) and keep
///   serving stale while revalidating in the background; browsers get a short
///   `max-age` so a catalog change still lands quickly.
/// - **Everything else** (tenant-scoped data): `private, no-store` — NO shared
///   cache (CDN/proxy) may store it (structurally eliminating the
///   cross-tenant-cache-bleed class: there is nothing a tenant-blind cache key
///   could ever mis-serve), and browsers don't persist it locally either. The
///   tenant-keyed stale-while-revalidate layer for this data lives in the
///   dashboard client (`ui/lib/api.ts`: cache keys are `${tenant}|${path}`,
///   TTL+SWR-ish reuse, purged on every mutation and team switch) — the layer
///   that can actually key on the VERIFIED tenant.
///
/// Handlers that set their own Cache-Control (project thumbnails) keep it.
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
        // Epoch fence (proposal step 5): a forwarded mutation carries the
        // sender's control-plane epoch. If it is BEHIND ours, the sender's view
        // of ownership is stale (it missed at least one promotion/failover) —
        // refuse rather than apply a write routed under superseded ownership.
        // Absent/unparsable header (pre-upgrade peers) fences nothing: the
        // owner-recheck below still gates.
        let is_mutation = matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
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
                    // Disclose OUR current epoch so a well-behaved forwarder can
                    // max-merge it and re-stamp instead of failing the write on
                    // a pure race (an epoch bump landing while the forward was
                    // in flight). This refusal is provably-not-applied — it
                    // fires before the router — so that retry is safe; a
                    // genuinely superseded sender never converges to a current
                    // epoch and is still refused after the bounded retries.
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
    // Auth-first: with enforcement on, reject a mutation lacking a valid token
    // BEFORE forwarding (fail fast). Reads pass (the leader re-verifies anyway).
    if crate::auth::enforced() {
        let is_mutation = matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
        let path = req.uri().path();
        // Keep in sync with auth::require_auth's `open` list — the zkauth
        // preview-unlock endpoints authenticate via a shared x-hive-internal
        // secret (zkauth::internal_ok), never a platform JWT.
        let open = path == "/healthz"
            || path == "/v1/token"
            || path == "/v1/git/webhook"
            || path == "/v1/zkauth/register"
            || path == "/v1/zkauth/preview-proof"
            // Per-database bearer, not a platform JWT — see auth::require_auth.
            || path.starts_with("/v1/sqlite/")
            // Per-project browser_db REST bearer, not a platform JWT — see
            // auth::require_auth.
            || path.starts_with("/v1/browser-db/")
            // WebDAV Basic-auth, not a platform JWT — see auth::require_auth.
            || path.starts_with("/v1/drive/webdav/");
        if is_mutation && !open {
            // Accept a platform JWT or a dashboard API key (`hive_…`) — the
            // leader's `require_auth` re-verifies either; this gate only
            // fails fast. JWT-only here made API keys silently read-only.
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
    // libsql/Hrana pipelines are OWNER-routed, never leader-routed — see
    // `owner_routed`.
    if !is_mutation || owner_routed(req.uri().path()) || req.uri().path() == "/v1/token" {
        return serve_local(admin, req).await;
    }
    // Leader serves locally.
    if cloud.is_control_plane_leader() {
        return serve_local(admin, req).await;
    }
    // Forward to the leader. Every candidate is SNI-pinned to a registry IP: a
    // leader with no reachable IP — OR a malformed registry IP that won't parse to
    // a socket addr — must FAIL CLOSED (never fall back to plain DNS, which could
    // send the write anywhere), so with nothing dialable the mutation is refused
    // with 503 rather than applied here (split-brain). Reads never reach this
    // point (they returned locally above).
    admin_forward_to_leader(&cloud, &api_host, req).await
}

/// The EXACT body a node returns when a forwarded mutation lands on it and it is
/// not the control-plane leader. One constant, three users — the two refusal
/// sites and `is_not_leader_refusal` — because a forwarder now has to RECOGNISE
/// this answer to route around it, and a drift between emitter and matcher would
/// silently reinstate the dead end this constant exists to close.
const NOT_LEADER_BODY: &str = "not control-plane leader";

/// The refusal itself. Load-bearing property: it is emitted BEFORE `next.run` /
/// `serve_local`, so a caller that receives it knows the request was PROVABLY
/// not applied — which is the only reason re-sending it to another candidate is
/// safe (an ambiguous failure, e.g. a mid-flight transport error, is never
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

/// The EXACT body the epoch fence in `admin_ingress`'s forwarded-marker branch
/// returns when a forwarded mutation's sender epoch is behind the receiver's.
/// One constant, two users — emitter and [`is_stale_epoch_refusal`] — the same
/// emitter/matcher drift discipline as [`NOT_LEADER_BODY`].
const STALE_EPOCH_BODY: &str = "stale control-plane epoch (ownership changed); retry";

/// True for exactly the response the `admin_ingress` epoch fence produces.
/// Like [`not_leader_refusal`], that refusal is emitted BEFORE the receiver's
/// router ever runs, so the mutation was provably not applied and re-sending
/// it with a converged epoch is safe.
fn is_stale_epoch_refusal(status: u16, body: &[u8]) -> bool {
    status == axum::http::StatusCode::SERVICE_UNAVAILABLE.as_u16()
        && std::str::from_utf8(body).is_ok_and(|s| s.trim() == STALE_EPOCH_BODY)
}

/// Bounded count of same-candidate re-stamps after a receiver discloses a
/// newer epoch (a bump that landed mid-forward, or one gossip delivered during
/// the round trip). A bump storm faster than the forward RTT, or a sender that
/// can never converge, still ends in the refusal — the fence's protection is a
/// bound, not a single shot.
const MAX_STALE_EPOCH_RETRIES: u32 = 3;

/// A no-redirect reqwest client that resolves `api_host` to a specific leader
/// IP:443 — so the forward deterministically hits the leader with a valid SNI +
/// cert (the wildcard/api bundle covers `api_host`). Cached per (ip, host).
/// Returns `None` when `ip` doesn't parse to an address or the client can't be
/// built, so the caller FAILS CLOSED rather than falling back to plain DNS (which
/// would resolve `api_host` to an arbitrary node and mis-route the forward).
///
/// The connect timeout is separate from (and far tighter than) the overall
/// request timeout: a candidate whose address black-holes must not burn the
/// whole 30s budget before the next candidate is tried.
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

/// Ordered (node name, dialable IP) candidates for a leader forward.
///
/// WHY THIS IS A LIST AND NOT ONE ADDRESS. `control_plane_leader()` resolves the
/// owner from the CALLING node's own registry health view, and a health verdict
/// is per-observer (AGENTS.md, "a health verdict is per-OBSERVER"). One missed
/// probe against the real owner makes this node fall through
/// `HIVE_CP_OWNER_CHAIN` to the next entry — which does NOT agree it is the
/// owner and answers the forwarded write with `not control-plane leader`. That
/// dead-ends a write the fleet was perfectly able to accept: measured live
/// 2026-08-05, every follower logged 6–9 owner changes in 3h (cp epoch 86,591)
/// while the leader itself logged ZERO, i.e. the transitions were the observers'
/// blips, not real failovers — surfacing to browser-node donors as a raw
/// "not control-plane leader" on `POST /v1/browser/admissions`.
///
/// Order: this node's believed leader first (unchanged behaviour and, in the
/// common case, correct), then the remaining curated chain entries in chain
/// order — the operator-controlled candidate set that is the ONLY place
/// ownership can legitimately sit. Addresses come from the LOCAL registry, so a
/// candidate is still only ever a node this fleet already knows; deliberately
/// NOT health-filtered, because the stale health verdict is precisely the input
/// that just misled us. Self is skipped (we already know we are not the leader,
/// so a round trip to our own public address can only refuse again).
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

/// Forward an admin MUTATION to the control-plane leader over HTTPS (pinned to a
/// registry IP, SNI/Host = api host). Adds the loop-guard header + the epoch
/// fencing token; preserves method, path+query, headers (incl.
/// Authorization/Cookie) and body verbatim, so the receiving leader re-runs the
/// FULL auth path and re-derives the tenant from the caller's own token — this
/// hop grants nothing and asserts no identity of its own.
///
/// Walks [`leader_forward_candidates`] and moves to the next one on exactly
/// three provably-not-applied outcomes: the `not control-plane leader` refusal
/// (issued before the receiver's router ever runs), the stale-epoch fence
/// refusal (also issued before the router ever runs — retried in place after
/// adopting the receiver's disclosed epoch, bounded by
/// [`MAX_STALE_EPOCH_RETRIES`], then walked like any other refusal), and a
/// CONNECT failure (nothing was sent). Any other answer — including a
/// mid-flight transport error, whose effect is unknowable — is returned as-is,
/// because re-sending a mutation that may already have been applied is worse
/// than surfacing the failure.
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
            continue; // unparsable registry address: nothing was sent, try the next
        };
        loop {
            // The forwarder's control-plane epoch rides along as the fencing
            // token — the receiver refuses the write if this is behind ITS
            // epoch (the sender's view of ownership is stale). See
            // admin_ingress's forwarded branch.
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
                    // The fence discloses the epoch it fenced against, so a
                    // mid-flight bump is converged deterministically instead of
                    // racing gossip for it.
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
                        // Keep the refusal as the answer of last resort, then
                        // try the next candidate — this node's own view is the
                        // thing in doubt.
                        refused = Some(
                            (axum::http::StatusCode::SERVICE_UNAVAILABLE, NOT_LEADER_BODY)
                                .into_response(),
                        );
                        continue 'candidates;
                    }
                    if is_stale_epoch_refusal(status, &bytes) {
                        // Provably not applied (the fence fires before the
                        // receiver's router), so re-sending is safe — same
                        // argument as the not-leader refusal. The disclosed
                        // epoch is authoritative: max-merge it and re-stamp,
                        // turning a mid-flight bump race into one retried round
                        // trip instead of a user-visible 503.
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
                        // Nothing newer to converge to (pre-upgrade receiver and
                        // gossip has not caught up), or the bound is spent.
                        // Another candidate may still be at our epoch — epoch
                        // views are per-observer, like health — so walk on; the
                        // refusal stays the answer of last resort.
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
                    continue 'candidates; // connection never established: provably not applied
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
    // Every candidate refused (or none was dialable at all). Fail CLOSED: never
    // apply the mutation locally, which is exactly the split-brain the
    // single-writer rule exists to prevent.
    refused.unwrap_or_else(|| {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "control-plane leader unreachable",
        )
            .into_response()
    })
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
    // Tell the app what the PUBLIC origin is (Next/Clerk build absolute URLs).
    // Host itself is left to default to the upstream's own authority (NOT
    // overridden to public_host): an ngrok tunnel origin routes ON its Host
    // header — forcing it to the platform's public domain here broke ngrok
    // routing outright (live-reproduced: every dashboard-host request 421'd,
    // "Misdirected Request", the moment this override shipped). A same-origin
    // loopback upstream doesn't need Host forwarding at all (there's only one
    // app listening there); x-forwarded-host/proto remain the mechanism the
    // app itself is expected to read for its own absolute-URL construction.
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
                // `content-encoding` is deliberately NOT stripped, and
                // `accept-encoding` is deliberately forwarded upstream (see the
                // request loop above). The two are ONE change: the upstream
                // Next server (`compress: true`) only compresses when it sees
                // the client's Accept-Encoding, and its compressed bytes are
                // only decodable by the browser if the Content-Encoding label
                // survives this hop. Stripping the request header made the
                // dashboard serve every HTML/JS/CSS byte uncompressed while
                // still emitting `Vary: Accept-Encoding` — the exact live
                // signature measured on shadw.cloud. Stripping only ONE of the
                // two would be worse than the bug: forwarding the header while
                // deleting the label hands the browser brotli bytes labelled
                // as plaintext. This is safe to pass through because the
                // workspace `reqwest` is built WITHOUT the gzip/brotli
                // features, so it never transparently decompresses the body
                // behind our back; the body is streamed verbatim below, and
                // `content-length`/`transfer-encoding` are still dropped so the
                // re-framing stays consistent.
                if matches!(
                    name.as_str(),
                    "connection" | "transfer-encoding" | "content-length"
                ) {
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

/// Resolve this node's Tencent VPC-private address for CCN inter-region
/// traffic, from `HIVE_PRIVATE_ADDR` ONLY — deliberately never sniffed off an
/// interface (spec: "do not hardcode `eth0`", and this fleet already learned
/// the hard way, via `dht`'s seed-address OOM incident, that an RFC1918
/// address existing on a host says nothing about whether it is dialable by
/// anyone else — a Docker/k8s/bridge/TUN interface can produce a private
/// address that is NOT the Tencent VPC NIC). An explicit operator value is
/// the only source of truth this repo already has for "this address is the
/// one meant for CCN", the same posture `HIVE_PUBLIC_IP` takes for the public
/// side.
///
/// Accepts either a bare IPv4 (`10.20.0.15`, paired with `HIVE_IROH_PORT` —
/// falling back to iroh's default ephemeral-port convention is NOT safe here
/// since the private candidate must name a real port a peer can dial, so a
/// missing `HIVE_IROH_PORT` makes a bare-IP override inert rather than
/// guessing) or a full `ip:port` pair. Unset ⇒ `None` — the safe default:
/// this node never becomes a CCN-private dial target, byte-identical to
/// pre-feature behavior.
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

/// Periodic re-geolocation so a machine that MOVES (a laptop node, or a cloud
/// host whose ISP re-homes its IP) doesn't report a stale location until the
/// next restart — the exact drift observed live on the LA-boot/San-Jose-now
/// laptop nodes. `HIVE_GEO` still wins unconditionally on every call
/// (`geolocate()`'s own first check), so a manually-pinned node is unaffected;
/// this only helps the auto-detected case. Deliberately does NOT update
/// `region` — see `NodeRegistry::set_self_geo`'s doc for why re-deriving a
/// node's stable identity from a drifted geolocation would be the disruptive
/// half of a "fix", not this row's ask. Only writes when the position moved
/// MATERIALLY (`HIVE_GEO_REFRESH_MIN_KM`, default 50km — a routine BGP reroute
/// within the same city must not spuriously churn the registry every tick).
/// Keep this node's gossiped free-disk figure current.
///
/// Placement reads `NodeInfo::disk_free_gb` to refuse a node with no headroom.
/// A boot-time-only value would be worse than useless there: it goes stale in
/// exactly the direction that matters (a node fills up, keeps advertising the
/// space it had at boot, and keeps winning deployments it cannot host). That is
/// the shape that took fc-sanjose to 0 bytes free and 9 dead deployments on
/// 2026-07-31.
///
/// Cheap by construction: one `statvfs`-class read per tick, no CPU sampling.
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
                // Rootfs/proof publication and runtime installation can move
                // underneath this process. Re-observe all three fields from one
                // selected-backend source, then publish the tuple under one
                // registry write lock so gossip can never see a torn verdict.
                let runtime_capabilities = runtime_capability_source.detect().await;
                registry.set_self_runtime_capabilities(
                    runtime_capabilities.wasm_runtime,
                    runtime_capabilities.bun_runtime,
                    runtime_capabilities.runtime_artifact_protocol,
                );
                // Same tick, same reason as the disk figure: the restart
                // audit's 24h window SLIDES, so a boot-time-only value goes
                // stale in the direction that matters (a node keeps
                // advertising an OOM it had 25h ago, or — worse — advertises
                // none while cycling).
                registry.set_self_restart_audit(
                    crate::restart_audit::started_ms(),
                    crate::restart_audit::oom_restarts_24h(),
                    crate::restart_audit::last_oom_ms(),
                );
            }
        }
    });
}

fn spawn_geo_refresh(registry: Arc<hive_edge::NodeRegistry>) {
    // `--region` pinned explicitly (not "auto") means the operator already
    // decided the identity; still worth refreshing lat/lon for DNS-nearest
    // accuracy, so this loop runs unconditionally rather than gating on that.
    let interval = Duration::from_secs(env_u64("HIVE_GEO_REFRESH_SECS", 3600));
    if interval.is_zero() {
        return; // HIVE_GEO_REFRESH_SECS=0 opts out entirely
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
                    // No prior geo at all (boot geolocation failed, e.g. offline
                    // at start) — any successful lookup now is real information.
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
    // For loopback, ALSO bind the other IP family on the same port. Browsers
    // resolve `*.localhost` to ::1 (IPv6) first, so a v4-only bind makes deploys
    // unreachable in the browser even though 127.0.0.1 works for curl/CLI.
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
            let node_region: HashMap<String, String> = nodes_now
                .iter()
                .map(|n| (n.id.clone(), n.region.clone()))
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

fn spawn_cluster_loop(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            // Re-resolve the control-plane owner from the live registry
            // (observe_owner inside bumps the fencing epoch exactly on real
            // ownership transitions) and gossip our current epoch so the
            // fleet's fencing tokens converge on the max witnessed.
            let _ = cloud.control_plane_leader();
            cloud.registry.set_self_cp_epoch(cloud.cluster.epoch());
        }
    });
}

/// Periodic GuardianDB snapshot re-assert. `persist()` -> `guardian::replicate`
/// only fires on a STATE MUTATION, so a node quiet since its last restart never
/// writes its own `node/<name>/snapshot` key into the replicated doc — leaving
/// the fleet's snapshot set permanently short of "every peer contributes one"
/// (the observed gap: nodes with no tenant activity since restart had no
/// snapshot anywhere in the shared KV). This loop captures + replicates on a
/// fixed cadence so every node's snapshot is always PRESENT and FRESH,
/// independent of mutation timing. Cheap at the blob layer when unchanged:
/// iroh-docs content-addresses by BLAKE3, so re-putting identical bytes yields
/// the same hash and stores no new blob — only a changed snapshot creates one.
fn spawn_guardian_snapshot_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_GUARDIAN_SNAPSHOT_SECS", 120));
    tokio::spawn(async move {
        // Small initial delay so first-boot restore/seed settles before the
        // first assert; then steady cadence.
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let snap = crate::persist::capture(&cloud);
            crate::guardian::replicate(&snap);
            tokio::time::sleep(interval).await;
        }
    });
}

/// Periodic reap of departed nodes' `node/<name>/snapshot` keys.
///
/// LEADER-ONLY, unlike the snapshot loop above: that one writes only this
/// node's OWN key (every node must, hence every node runs it), while this one
/// DELETES other nodes' keys from the replicated store — a fan-out mutation,
/// which the platform's single-writer discipline routes through the
/// control-plane leader. The leader check is re-evaluated every pass, not
/// captured once, so leadership changing mid-run hands the job over correctly.
///
/// Fully inert until `HIVE_NODE_ROSTER` is configured — see
/// `guardian::reap_departed_node_snapshots` for why an unset roster must mean
/// "do nothing" and never "everything looks departed". `HIVE_REAP_SECS=0`
/// disables the loop outright.
fn spawn_guardian_reap_loop(cloud: Arc<CloudState>) {
    let secs = env_u64("HIVE_REAP_SECS", 6 * 60 * 60);
    if secs == 0 {
        return;
    }
    let interval = Duration::from_secs(secs);
    tokio::spawn(async move {
        // Deliberately long initial delay: at boot this node has not yet
        // resynced the replicated doc, so an immediate pass would judge
        // departure from an incomplete local view.
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            if cloud.is_control_plane_leader() {
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

/// Periodic safety-net flush for metrics hour/day rollups — see the call site's
/// comment in `main()` for the full bug this closes.
fn spawn_metrics_persist_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_METRICS_PERSIST_SECS", 120));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            crate::persist::persist(&cloud);
        }
    });
}

/// Periodic podman lock-headroom sweep — see the call site's comment in `main()`.
///
/// Takes no `CloudState`: it reads the HOST's podman state, nothing of ours.
/// Runs on every node (not leader-only) because the lock pool it protects is
/// per-host. `HIVE_LOCK_SWEEP_SECS=0` disables it.
fn spawn_container_lock_sweep() {
    let secs = env_u64("HIVE_LOCK_SWEEP_SECS", 300);
    if secs == 0 {
        return;
    }
    let interval = Duration::from_secs(secs);
    crate::supervise::spawn_supervised("container-lock-sweep", move || async move {
        // One immediate pass at boot: a node coming back from a crash or a
        // podman-machine reset is exactly when leaked locks are already there.
        loop {
            crate::supervise::beat("container-lock-sweep");
            let path_env = crate::git::podman_path_env();
            match hive_backend::sweep_container_locks(&path_env).await {
                // Healthy — recorded at debug so a normal node stays quiet.
                Some((free, 0, 0)) => tracing::debug!(free_locks = free, "podman lock headroom ok"),
                Some((free, vols, cells)) => tracing::warn!(
                    free_locks_before = free,
                    volumes_reclaimed = vols,
                    cells_reclaimed = cells,
                    "reclaimed leaked podman locks"
                ),
                // No podman on this host (or it doesn't report FreeLocks) — nothing
                // to protect; keep looping cheaply rather than killing the task, so
                // a podman installed later is still covered.
                None => {}
            }
            tokio::time::sleep(interval).await;
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
    let _ = gossip::fetch(
        &cloud,
        &peer,
        hive_p2p::GOSSIP_POST,
        "/v1/nodes/announce",
        &me_bytes,
    )
    .await;
    let t0 = now_ms();
    let mut rtt = 0u64;
    let mut nodes_bytes =
        gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/nodes", &[]).await;
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
                    // MUST be >= hive_p2p::dial_fallback_ceiling(): a first-contact
                    // peer's cached hint (private IPs from its gossiped addr_json) is
                    // near-guaranteed to fail, so this join depends entirely on
                    // `acquire`'s fresh-discovery fallback getting to run to
                    // completion instead of being cancelled mid-flight.
                    let attempt = tokio::time::timeout(
                        hive_p2p::dial_fallback_ceiling(),
                        pool.join_request(&node_id, &addr, &me_bytes, &proof),
                    )
                    .await;
                    if let Ok(Ok(bytes)) = attempt {
                        if !bytes.is_empty() {
                            tracing::info!(peer = %node_id, "mesh join accepted — roster received");
                            // Restore the transport mapping fetch() evicted.
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
            let peer_endpoint_id = peer_self
                .as_ref()
                .and_then(|n| n.peer_id.clone())
                .filter(|id| !id.is_empty());
            // Tencent CCN private-path eligibility: computed once per gossip
            // round (not per peer) since the topology/self-info are stable
            // within it. `ccn_topology`/`self_provider`/`self_region` cost
            // nothing when `HIVE_CCN_REGIONS`/`HIVE_CLOUD_PROVIDER` are unset
            // — every check below then trivially returns `false` and this
            // whole block is a no-op, byte-identical to pre-feature behavior.
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
                    // Converge the control-plane fencing epoch on the max
                    // witnessed anywhere in the fleet (monotonic; see cluster.rs).
                    cloud.cluster.adopt_epoch(n.cp_epoch);
                    // Tencent CCN private-path: register/clear this peer's
                    // private-VPC candidate on the live `PeerPool` so the
                    // NEXT `acquire()` for it (reused trunks are untouched —
                    // this never tears down a live connection, only informs
                    // the next fresh dial) tries the private address
                    // alongside its normal public/relay candidates. Gated
                    // end-to-end by `is_private_path_candidate`: both sides
                    // must declare `provider = tencent` and the region pair
                    // must be configured, so a peer with no provider/region
                    // match is a no-op here, exactly as before this feature
                    // existed.
                    // Unconditional on `eid` alone (not also gated on
                    // `private_addr` being present) — a peer that stays
                    // gossiped but stops reporting a private address (e.g.
                    // `provider` flipped away from tencent) must still reach
                    // the `else` branch and get cleared; gating on both
                    // `Some`s left that case leaked forever (adversarial
                    // review finding).
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
                    // The response's first entry is the answering peer's OWN
                    // self-report — the only copy allowed to rename it (see
                    // upsert_peer_self_report); everything after it is a
                    // relayed third-party copy that must never rename.
                    if peer_self_id.as_deref() == Some(n.id.as_str()) {
                        cloud.registry.upsert_peer_self_report(n);
                    } else {
                        cloud.registry.upsert_peer(n);
                    }
                }
                // Reap private-candidate entries for any peer no longer in
                // the registry's own live set (decommissioned/evicted) — the
                // per-peer loop above only ever touches peers THIS round's
                // gossip response actually mentioned, so a peer that stops
                // being gossiped entirely (rather than merely losing its
                // private_addr) would otherwise leak its entry forever
                // (adversarial review finding). Cheap: the registry is the
                // whole fleet, at most low hundreds of entries, and this is
                // per-gossip-round, not per-peer.
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
                // `node_admins` MUST hold only real HTTP(S) admin URLs — the
                // deploy dispatcher (see git.rs / schedule.rs) treats any entry
                // here as "reachable via HTTP" and does `http.post(format!(
                // "{admin}/v1/git/deploy"))`. A peer reached over IROH has a
                // 64-char-hex node-id (or a `seed:` key) as its gossip `peer`
                // key, NOT a URL — storing that poisons node_admins so every
                // deploy placed on that node fails with a reqwest "builder
                // error" (unparseable URL) instead of falling back to the
                // working iroh dispatch route. So: insert ONLY for http(s)
                // peers, and REMOVE any stale entry when the same node is now
                // only iroh-reachable (a node that lost its HTTP tunnel).
                if peer.starts_with("http://") || peer.starts_with("https://") {
                    cloud.node_admins.write().insert(pid, peer.clone());
                } else {
                    // Iroh/seed key, not a URL. Clean a POISONED (non-http)
                    // stored value, but never clobber a good HTTP admin URL the
                    // same node may have registered via a different (http-
                    // tunnel) gossip key this or a prior round — else the entry
                    // would flap present/absent across rounds.
                    let mut m = cloud.node_admins.write();
                    let poisoned = m
                        .get(&pid)
                        .map(|a| !(a.starts_with("http://") || a.starts_with("https://")))
                        .unwrap_or(false);
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
                // Key-addressed targets: the target string IS the endpoint id (or a
                // seed:<id>), so a failed round still marks health even after
                // fetch() evicted the transport mapping.
                let k = peer.strip_prefix("seed:").unwrap_or(&peer);
                (k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit())).then(|| k.to_string())
            });
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            // ONE failed gossip fetch used to withdraw the peer outright here —
            // no threshold, no counter-evidence, on every node, every round.
            // On the leader that is a fleet-wide removal from DNS and
            // placement caused by a single transient fetch. The chokepoint
            // keeps a peer that is still announcing TO us (the fetch direction
            // and the announce direction are different paths and fail
            // independently) and marks it locally cold instead. See health.rs.
            crate::health::demote(&cloud.registry, &id, "gossip round: fetch failed", None);
        }
    }
    if let Some(bytes) =
        gossip::fetch(&cloud, &peer, hive_p2p::GOSSIP_GET, "/v1/serve-hosts", &[]).await
    {
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
            crate::zkauth::ingest_peer_export(&v);
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

fn spawn_gossip_loop(
    cloud: Arc<CloudState>,
    peers: Vec<String>,
    seeds: Vec<(String, String, String)>,
) {
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
                    pi.entry(key.clone())
                        .or_insert_with(|| (nid.clone(), addr.clone()));
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
                // Registry-derived (freshest addr wins — a rejoined node's new addr
                // must replace a stale persisted one).
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
            let mut seen_nodes: std::collections::HashSet<String> =
                std::collections::HashSet::new();
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
                holders
                    .entry(key)
                    .or_default()
                    .push(cloud.node_name.clone());
            }
            // Announce + learn each peer's view CONCURRENTLY. The per-peer cloud.* writes
            // (registry, peer_iroh, node_admins, trusted ids, enterprise, leases, zkauth)
            // all go through internally-synchronized stores, so they're race-free across
            // tasks; only the loop-local maps are merged from the returned partials. This
            // overlaps the network waits so one slow peer no longer serializes the rest.
            let me = cloud.registry.me();
            let me_bytes = serde_json::to_vec(&me).unwrap_or_default();
            let partials = futures::future::join_all(
                targets
                    .iter()
                    .map(|peer| sync_one_peer(cloud.clone(), peer.clone(), me_bytes.clone())),
            )
            .await;
            for pr in partials {
                if let Some(n) = pr.seen {
                    seen_nodes.insert(n);
                }
                for (h, route) in pr.routes {
                    routes.entry(h).or_default().push(route);
                }
                if let Some((nid, list)) = pr.fleet {
                    fleet.insert(nid, list);
                }
                for (k, nid) in pr.holders {
                    holders.entry(k).or_default().push(nid);
                }
            }
            // #24: TTL-merge routes so a route from a peer we briefly couldn't reach
            // this round survives (up to ROUTE_TTL_MS) instead of vanishing and
            // 404-ing the deployment; reached peers' routes are still authoritative.
            let merged = {
                let prev = cloud.peer_routes.read().clone();
                crate::state::merge_routes_ttl(
                    &prev,
                    routes,
                    &seen_nodes,
                    now_ms(),
                    crate::state::ROUTE_TTL_MS,
                )
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
            // Keep the git-webhook reverse index (`gitops::GitRepoIndex`, see
            // `admin::git_webhook`) in sync with freshly-gossiped fleet
            // deployments: a project first deployed via a DIFFERENT node than the
            // one that later receives a GitHub webhook delivery (the
            // `webhook.<platform_domain>` DNS root round-robins across every
            // gateway node) would otherwise sit unindexed on this node until its
            // own next deploy. Bounded to the projects visible in THIS round's
            // peer data (not a project rescan), and skips any project this node
            // already has a LOCAL real-git record for — matching
            // `admin::git_for_project_fleet`'s local-wins precedence exactly.
            {
                let mut newest_by_project: HashMap<&str, &fluid_core::DeploymentInfo> =
                    HashMap::new();
                for d in merged_deps.values().flatten() {
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
                        continue; // local record wins, unconditionally
                    }
                    if let Some(g) = &d.git {
                        cloud.git_index.set_project_repo(project, &g.repo_url);
                    }
                }
            }
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
            // Refresh THIS node's own MESH iroh_addr (re-serialize `ep.addr()`
            // every round, same pattern as guardian_iroh_addr/relay_url below).
            // `iroh_addr` was previously captured EXACTLY ONCE, at the instant
            // `bind_full` returned — before the endpoint has had any time to
            // register with its relay or learn hole-punch candidates, so it
            // shipped private-addrs-only in the gossip round every OTHER node
            // uses to dial this one. Root-caused live: fc-sanjose-gpu-2's `iroh_addr`
            // still read `{"id":...,"addrs":[{"Ip":"10.0.2.2:48670"}]}` (no
            // relay, no public IP) an hour after boot with its security group
            // open the whole time, while its SEPARATE `guardian_iroh_addr` (which
            // DOES refresh every round, see below) correctly carried the relay
            // hint and the real public IP — proving the gap was staleness, not
            // reachability. This is exactly the class of bug
            // `dnsserver-nearest-by-geo`'s sibling row (`geo-refresh-not-only-at-
            // boot`) fixed for lat/lon; same shape here for the mesh address.
            if let Some(addr) = cloud.iroh.read().as_ref().and_then(hive_p2p::addr_json) {
                cloud.registry.set_self_iroh_addr(addr);
            }
            // Publish THIS node's own GuardianDB-specific address (a SEPARATE
            // iroh identity from the mesh's iroh_addr above — GuardianDB runs
            // its own independent client) once it's ready, so peers can gossip
            // it and seed it correctly. `None` until GuardianDB's client has
            // bound; self-heals within a few rounds after boot, never blocks.
            let cloud_for_self_addr = cloud.clone();
            tokio::spawn(async move {
                if let Some(addr) = crate::guardian::my_iroh_addr().await {
                    cloud_for_self_addr
                        .registry
                        .set_self_guardian_addr(Some(addr));
                }
            });
            // Seed GuardianDB's OWN iroh client with every currently-known
            // peer's GuardianDB-specific address (re-asserted every round,
            // same rationale as the bootstrap-seed re-assertion above) — NOT
            // the mesh iroh_addr above, which belongs to a DIFFERENT identity
            // (see guardian::seed_peer's doc comment: feeding the wrong one
            // in previously caused a live retry-storm, reverted). Best-effort,
            // spawned so a slow/failed seed round never blocks the gossip
            // loop's own cadence. Persisted (mirroring peer_iroh.json) so a
            // restart's boot-time seed has real data once the mesh has had at
            // least one round to gossip these addresses around.
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
                tokio::spawn(
                    async move { crate::guardian::seed_known_peers(&guardian_addrs).await },
                );
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
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
        loop {
            // ±20% dither per tick (now-derived): after a fleet roll every
            // node's warmer otherwise fires in lockstep, and 14 nodes
            // holepunching the same restarted peer in the same instant is a
            // reconnect thundering herd — the tau-style jitter breaks the
            // synchronization without changing the average cadence.
            let base = interval.as_millis() as u64;
            let dither = base / 5;
            // Mix per-node identity into the phase: wall time alone is SHARED
            // entropy — two nodes computing in the same millisecond draw the
            // identical "jitter" and stay in lockstep.
            let phase = crate::meshwatch::node_stagger_ms(&cloud.node_name);
            let jittered = base - dither + ((hive_core::now_ms() + phase) % (2 * dither + 1));
            tokio::time::sleep(Duration::from_millis(jittered)).await;
            let pool = match cloud.mesh.read().clone() {
                Some(p) => p,
                None => continue, // iroh transport not bound yet
            };
            // Every healthy peer with a known iroh address → ensure a live trunk.
            // Label peers by their ENDPOINT ID where one is known, falling back to
            // the name. The pool keys trunks canonically by the id parsed out of
            // the address, so this no longer decides WHICH trunk gets warmed — but
            // it keeps the warmer's logs and the alias map speaking the same
            // identifier the control plane uses, rather than the name-only view
            // that previously left the control-plane trunk permanently cold.
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

/// Live relay-set refresh loop (dynamic-hive-relay-urls-list): keeps the bound
/// `iroh` endpoint's relay map in sync with the DESIRED set —
/// `[this node's own relay_url] + [every healthy peer's relay_url, from the live
/// gossip-replicated NodeRegistry] + [the central relay.shadw.cloud backstop] +
/// [HIVE_RELAY_URLS manual overrides — merged in, not replaced]` — via
/// `Endpoint::insert_relay`/`remove_relay` (a genuine LIVE update on the
/// already-bound endpoint; see `RelaySet`). Replaces the OLD bind-time-only
/// `relay_map_from_env` as the ongoing source of truth for the mesh's relay set
/// (that function still seeds/merges at bind time — kept, not removed).
/// Config: `HIVE_RELAY_SYNC_INTERVAL` (s, default 30).
fn spawn_relay_sync_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_RELAY_SYNC_INTERVAL", 30));
    // Manual overrides, read once at boot (same source `relay_map_from_env`
    // reads at bind time) — merged into every refresh, not just the seed.
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
                None => continue, // iroh transport not bound (or not yet) — nothing to sync
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
            // Central backstop: always included, so a node with no peer/own relay
            // known yet still has a working relay path.
            desired.push(hive_edge::CENTRAL_RELAY_URL.to_string());
            desired.sort();
            desired.dedup();
            let n = desired.len();
            set.sync(desired).await;
            tracing::debug!(relays = n, "live relay set synced");
        }
    });
}

/// GuardianDB anti-entropy loop (implement-anti-entropy-loop): each tick, picks
/// ONE peer from the live, healthy `NodeRegistry` and asks it for its
/// per-namespace GuardianDB HEAD map (`GET /v1/guardian/heads` —
/// design-head-cid-exchange-rpc, key+content-hash+timestamp only, never value
/// bytes), diffs it against this node's own local heads
/// (`guardian::namespace_heads`), and for every namespace with a REAL
/// divergence triggers a targeted `Doc::start_sync` reconciliation against
/// exactly that peer (implement-reconciliation-trigger — never a
/// full-database refresh). This is the Dynamo-style read-repair loop for the
/// iroh-docs-backed replicated store: the periodic mechanism that catches
/// entries a peer never picked up via the opportunistic live-sync path (e.g.
/// this node was offline/partitioned when the peer wrote, or automatic
/// DocTicket exchange never happened between this pair). Peer selection uses
/// `now_ms()` modulo the healthy-peer count — good enough churn for eventual
/// full-mesh coverage without a new RNG dependency; this is convergence
/// machinery, not a security-sensitive selection. Config:
/// `HIVE_ANTI_ENTROPY_INTERVAL_SECS` (s, default 60).
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

/// One anti-entropy round — see `spawn_anti_entropy_loop`. Split out so the
/// loop body itself stays a plain tick-and-call, matching this file's other
/// periodic loops (`spawn_trunk_warmer`, `spawn_relay_sync_loop`).
async fn anti_entropy_round(cloud: &Arc<CloudState>) {
    // Deliberately NOT health-filtered. A peer this node currently calls
    // unhealthy is precisely the one whose state is most likely to have
    // diverged, and filtering it out starved exactly the peers that needed
    // reconciling — the convergence machinery avoided its own repair targets.
    // A dead peer just costs one failed RPC per round; a live-but-unprobeable
    // one gets its state back.
    let candidates: Vec<NodeInfo> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.peer_id.is_some() && n.iroh_addr.is_some())
        .collect();
    if candidates.is_empty() {
        return; // no reachable peer this round — nothing to compare against
    }
    // Rotate with a monotonic counter, NEVER `now_ms() % len`. The loop ticks on
    // a fixed 60s interval, so a clock-derived index advances by exactly
    // `60000 mod len` each round — which is ZERO for len 12, 15 or 16, the exact
    // band this ~17-node fleet sits in. That pinned every round on the SAME peer
    // indefinitely: one unreachable candidate could consume the single per-round
    // slot forever while every other peer went un-reconciled. A counter cannot
    // alias with the tick period.
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
        // Entries whose (key -> hash) disagree: classic entry-level divergence.
        let diverged = remote_heads
            .iter()
            .filter(|h| local_by_key.get(h.key.as_str()) != Some(&h.hash.as_str()))
            .count();
        // CONTENT-level divergence: the hashes agree, but one of us cannot
        // actually read the bytes. This is invisible to the entry comparison
        // above, and it is the divergence this fleet actually suffers from —
        // a doc entry replicates independently of its blob, and iroh-docs stops
        // retrying for good once the supplying peer departs. Counting it is what
        // stops "heads already match" from being reported over a node that holds
        // nothing but hashes.
        let local_missing: std::collections::HashSet<&str> = local
            .get(ns)
            .map(|v| {
                v.iter()
                    // `None` = UNKNOWN (pre-upgrade peer), never counted as missing.
                    .filter(|h| h.content_local == Some(false))
                    .map(|h| h.hash.as_str())
                    .collect()
            })
            .unwrap_or_default();
        // Only worth reporting when the PEER claims to hold what we lack — that
        // is a gap a reconcile can actually close.
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

/// Apply replicated project DELETION tombstones to this node's own deployment
/// records.
///
/// A project tombstone already replicates (`ProjectStore::snapshot_synced`) and
/// already removes the SETTINGS row on every peer that receives it — but nothing
/// ever applied it to the DEPLOYMENT records, which are a separate store. So a
/// peer that was unreachable when the delete cascaded (or that was down and came
/// back) kept its deployment rows, kept serving them, and kept gossiping them
/// into `peer_deployments` — which is what the dashboard's fleet view reads. The
/// project reappeared, fully alive, and deleting it again hit the same
/// unreachable peer. That is the "deleting a project does nothing" report, and
/// no amount of retrying the cascade at delete time can close it: the peer is by
/// definition not listening then.
///
/// Making the tombstone the durable, idempotent instruction — re-applied on
/// every tick by whoever holds it — is what converges the fleet without an
/// operator. Same shape as `tenancy_reconcile`: repair-only, never creative.
///
/// A project RE-CREATED after its deletion must survive, so the tombstone is
/// only honoured while nothing newer than it exists: a settings row written
/// after the tombstone, or ANY deployment created after it, cancels the pass for
/// that project entirely (never a partial reap, which would delete a live
/// deployment's siblings).
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
                        // Re-apply even with no local deployments: this drives
                        // the generation-conditional relational delete every
                        // tick, so a stale offline node's late boot backfill
                        // cannot resurrect project_teams indefinitely.
                        cloud.projects.apply_delete(&project, tomb_ms);
                        // No deployment records left, but a node can still hold
                        // the project's durable PUBLIC PORT claim: the release
                        // runs inside `purge_project_resources`, which only runs
                        // on a node that actually tore a deployment down. A node
                        // that merely ALLOCATED the port (the allocator is
                        // fleet-coordinated) keeps the claim in raw_ports.json
                        // forever, and a quarantined port is never re-granted —
                        // so the fleet slowly loses public ports to projects that
                        // no longer exist. Witnessed: 9000/9001 still claimed for
                        // a project deleted fleet-wide. The tombstone is the
                        // authority here too.
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
                    // A newer local deployment is a causal recreation. The
                    // generation-aware helper removes only old-incarnation
                    // records and preserves the recreated row/resources.
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

fn spawn_promotion_reconcile_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_millis(env_u64("HIVE_PROMOTION_RECONCILE_POLL_MS", 2000));
    crate::supervise::spawn_supervised("promotion-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            let mut tick = tokio::time::interval(interval);
            let mut was_leader = false;
            loop {
                tick.tick().await;
                crate::supervise::beat("promotion-reconcile");
                let cp_leader = cloud.control_plane_leader() == cloud.node_name
                    && !cloud.mesh_health().isolated;
                let just_promoted = cp_leader && !was_leader;
                was_leader = cp_leader;
                if just_promoted {
                    store_sync::reconcile_on_promotion(&cloud).await;
                }
            }
        }
    });
}

/// Relational mirror loop — keeps the admin "view as PostgreSQL" browser's
/// project_teams / teams / team_members / deployments tables (and a FULL
/// billing_accounts backfill) populated from live store snapshots.
/// `upsert_billing` only fires for tenants with active usage THIS tick, so an
/// account that exists but isn't currently metered would otherwise never
/// appear in the mirror (the live-witnessed missing-billing-accounts gap:
/// simpfi/thoth-division had project_teams rows but no billing_accounts row).
///
/// Write discipline, per section:
/// - teams/members + billing backfill: control-plane-leader ONLY (the node
///   where every admin mutation lands, so its stores are authoritative) —
///   same single-writer rule as `upsert_billing`'s metering site. The billing
///   manual pin (`HIVE_BILLING_COORDINATOR_NODE`) is honored for the billing
///   section so both billing writers always sit on the same node.
/// - deployments: EVERY node syncs only its OWN `gw.list()` rows (single
///   writer per row by construction — see `relational::sync_deployments`).
///
/// A content hash per ordinary section skips ticks with no change. Project rows
/// are deliberately re-asserted every ten leader ticks: that bounded write is
/// the repair path for a stale offline node's late LWW backfill. Best-effort
/// throughout: these tables feed only fleet visibility/read-only admin views.
/// Config: `HIVE_RELATIONAL_MIRROR_SECS` (s, def 60).
fn spawn_relational_mirror_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_RELATIONAL_MIRROR_SECS", 60));
    tracing::info!(
        ?interval,
        "relational mirror loop (projects/teams/members/deployments + billing backfill → SQL view)"
    );
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
            // Re-assert authoritative project rows every ten leader ticks. A
            // stale node can boot and backfill an old ProjectStore snapshot
            // after the real deletion/recreation write; version-conditional SQL
            // rejects it when visible, and this bounded re-assertion repairs the
            // distributed LWW race if the stale replica had not received the
            // newer row yet. Start at 9 so a newly elected leader runs it now.
            let mut project_reconcile_round = 9u8;
            // Per-tenant billing hashes: only tenants whose own rows changed are
            // re-upserted (an aggregate hash re-wrote all of them for one
            // tenant's change).
            let mut billing_hashes: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            let billing_pin = std::env::var("HIVE_BILLING_COORDINATOR_NODE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let mut last_peer_lookup_warn = std::time::Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(std::time::Instant::now);
            loop {
                tick.tick().await;
                crate::supervise::beat("relational-mirror");
                // Deployments: every node, own rows only.
                let deps = cloud.gw.list();
                if let Ok(json) = serde_json::to_string(&deps) {
                    let h = hash_of(&json);
                    if h != deps_hash {
                        relational::sync_deployments(&cloud.node_name, &deps).await;
                        deps_hash = h;
                    }
                }
                let isolated = cloud.mesh_health().isolated;
                let cp_leader = cloud.control_plane_leader() == cloud.node_name && !isolated;
                if cp_leader {
                    project_reconcile_round = (project_reconcile_round + 1) % 10;
                    if project_reconcile_round == 0 {
                        let projects: Vec<(String, String, String, u64)> = cloud
                            .projects
                            .snapshot()
                            .into_iter()
                            .map(|(project, s)| (project, s.team, s.build.root_dir, s.updated_ms))
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
                } else if !isolated {
                    // Follower: adopt the leader's node-local stores wholesale. A
                    // whole CLASS of stores (teams, incidents, apikeys, webhooks,
                    // databases, domains, integrations, gitops, docs, notifications,
                    // identity, enterprise) take mutations only on the leader
                    // (admin_ingress forward) but serve GETs from the local store,
                    // so a follower's copy otherwise diverges forever -- live-
                    // witnessed as sj=5 / bkk=4 / va=2 teams (a stale failover
                    // stand-in then corrupted the teams mirror) and the admin
                    // incidents page showing nothing on non-leader nodes. Wholesale
                    // replace (not merge) is correct for that class under the
                    // single-writer model: the leader IS the authority.
                    //
                    // TWO stores are deliberate exceptions and MERGE per key
                    // instead, because they are written on whichever node the
                    // browser actually reached rather than only on the leader —
                    // `browser_presence` and `browser_admissions` (see each one's
                    // own `adopt`). For those, a wholesale replace silently drops
                    // every record admitted through another node, which is exactly
                    // the bug both were fixed for; do not "restore consistency" by
                    // making them replace again.
                    //
                    // `store_sync::REGISTRY` drives every one through the same
                    // generic path; each entry's `adopt` declines an
                    // empty/unparsable payload so an unreachable/booting leader can
                    // never wipe a follower. See `crate::store_sync`.
                    let leader = cloud.control_plane_leader();
                    let peer = cloud.registry.nodes().into_iter().find(|n| {
                        n.name == leader
                            && !n.is_self
                            && n.healthy
                            && n.peer_id.is_some()
                            && n.iroh_addr.is_some()
                    });
                    if let Some(peer) = peer {
                        let (peer_id, peer_addr) = (
                            peer.peer_id.clone().unwrap(),
                            peer.iroh_addr.clone().unwrap(),
                        );
                        // FETCH CONCURRENTLY, adopt after. This was a serial
                        // `for` over the whole registry — ~24 stores today —
                        // each a mesh round trip with its own 10s budget, so
                        // one slow store stalled every store behind it and the
                        // worst case (24 x 10s) overran the loop's own tick
                        // interval outright, leaving followers silently stale.
                        // A healthy cross-continent probe on this fleet has
                        // measured 7462ms (AGENTS.md), so this is not a
                        // hypothetical tail. `reconcile_on_promotion` already
                        // fetches this exact way; matching it here.
                        //
                        // `adopt` stays OFF the concurrent half deliberately:
                        // it is synchronous and takes store locks, so it runs
                        // in a plain loop after the join, preserving the
                        // existing one-at-a-time apply semantics exactly.
                        use futures::StreamExt as _;
                        let futs: Vec<_> = store_sync::REGISTRY
                            .iter()
                            .map(|store| {
                                let (peer_id, peer_addr) = (peer_id.clone(), peer_addr.clone());
                                let cloud = cloud.clone();
                                async move {
                                    let local = (store.snapshot)(&cloud);
                                    let path = format!("/v1/store-snapshot/{}", store.name);
                                    let bytes = gossip::request_to(
                                        &cloud,
                                        &peer_id,
                                        &peer_addr,
                                        hive_p2p::GOSSIP_GET,
                                        &path,
                                        &[],
                                        10,
                                    )
                                    .await;
                                    (store, local, bytes)
                                }
                            })
                            .collect();
                        let bounded = futures::stream::iter(futs)
                            .buffer_unordered(
                                std::env::var("HIVE_STORE_SYNC_CONCURRENCY")
                                    .ok()
                                    .and_then(|v| v.parse().ok())
                                    .filter(|v| *v > 0)
                                    .unwrap_or(8),
                            )
                            .collect::<Vec<_>>()
                            .await;
                        let mut adopted_store = false;
                        for (store, local, bytes) in bounded {
                            let Some(bytes) = bytes else { continue };
                            // Raw byte-compare change-gate: `snapshot` is
                            // deterministic, so equal bytes = no change. Skip
                            // empties (an old leader without this arm returns []).
                            if !bytes.is_empty() && bytes != local {
                                if let Some(n) = (store.adopt)(&cloud, &bytes) {
                                    adopted_store = true;
                                    tracing::info!(
                                        leader = %leader,
                                        store = store.name,
                                        count = n,
                                        "store follower-sync: adopted the leader's snapshot"
                                    );
                                }
                            }
                        }
                        if adopted_store {
                            // A follower merge is a real mutation, including
                            // recovered permanent tombstones. Queue persistence
                            // now instead of leaving a crash-loss window until
                            // the unrelated periodic capture.
                            persist::persist(&cloud);
                        }
                    } else if last_peer_lookup_warn.elapsed() >= Duration::from_secs(300) {
                        tracing::warn!(
                            leader = %leader,
                            "store follower-sync: no healthy, addressable registry entry for \
                             the control-plane leader -- this node cannot pull \
                             store_sync::REGISTRY snapshots and its local copies \
                             (projects/teams/billing/etc.) will silently drift stale until \
                             this resolves"
                        );
                        last_peer_lookup_warn = std::time::Instant::now();
                    }
                }
                let billing_authority = match &billing_pin {
                    Some(pin) => pin == &cloud.node_name && !isolated,
                    None => cp_leader,
                };
                if billing_authority {
                    let (accounts, _) = cloud.billing.snapshot();
                    // Per-tenant (ledger, invoices, checkouts) fetched ONCE up
                    // front and reused for both the dirty-check hash below and
                    // (if dirty) the actual upsert loop.
                    //
                    // DIRTY-CHECK MUST COVER MORE THAN JUST ACCOUNTS: previously
                    // `h` hashed `accounts` alone, so a checkout/ledger-entry/
                    // invoice landing with no accompanying account-field change
                    // never flipped `billing_hash` and the whole billing mirror
                    // section stayed gated shut — live-witnessed: a checkout
                    // opened against an otherwise-unchanged account stayed absent
                    // from `billing_checkouts` for multiple ticks, only appearing
                    // once an unrelated account field happened to change on the
                    // same tick. Hashing the full per-tenant (account, ledger,
                    // invoices, checkouts) tuple set closes that gap: ANY
                    // billing-related change on ANY tenant flips the hash and
                    // triggers a re-sync on the next tick.
                    let per_tenant: Vec<_> = accounts
                        .iter()
                        .map(|acc| {
                            let ledger = cloud.billing.ledger(&acc.tenant);
                            let invoices = cloud.billing.finalized_invoices(&acc.tenant);
                            let checkouts = cloud.billing.checkouts_for_tenant(&acc.tenant);
                            (acc, ledger, invoices, checkouts)
                        })
                        .collect();
                    // PER-TENANT dirty-check. This used to serialize the whole
                    // fleet's billing dataset to one JSON string and hash that,
                    // which meant (a) an unconditional full clone + serialize of
                    // every tenant's entire unbounded ledger on EVERY tick even
                    // when nothing changed, and (b) any single tenant's change
                    // flipping the aggregate hash and re-upserting EVERY tenant.
                    // Hashing each tenant's own tuple independently keeps the
                    // exact same "any billing-related change triggers a re-sync"
                    // guarantee while writing only the tenants that actually
                    // moved, and drops the whole-dataset JSON round trip.
                    let mut dirty: Vec<relational::BillingRows<'_>> = Vec::new();
                    let mut next_hashes: std::collections::HashMap<String, u64> =
                        std::collections::HashMap::with_capacity(per_tenant.len());
                    for (acc, ledger, invoices, checkouts) in &per_tenant {
                        let Ok(json) = serde_json::to_string(&(acc, ledger, invoices, checkouts))
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
                        // ONE session (one full index refresh) for the whole
                        // batch instead of one per tenant — see
                        // `relational::upsert_billing_many`. Per-tenant
                        // transactions are unchanged.
                        relational::upsert_billing_many(&dirty).await;
                    }
                    // Replace wholesale so a tenant that disappeared from the
                    // snapshot stops being tracked (no unbounded growth).
                    billing_hashes = next_hashes;
                }
            }
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
    tracing::info!(
        ?interval,
        "billing meter loop (usage → charges → invoices; leader-elected)"
    );
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
            // CONTROL-PLANE OWNER — same resolution as admin mutations, ACME and
            // DNS (owner chain first, identity election fallback), so all four
            // single-writer roles sit on exactly one designation and cannot
            // drift apart (proposal step 6).
            let elected = match &manual_pin {
                Some(pin) => Some(pin.clone()),
                None => Some(cloud.control_plane_leader()),
            };
            // Isolation gate (same rationale as is_control_plane_leader): a node
            // that can't see its expected peers must never self-elect as the
            // metering coordinator from that blind view — another node with a
            // live mesh view is (or will be) charging; charging twice is worse
            // than charging one tick late.
            let am_leader = elected.as_deref() == Some(cloud.node_name.as_str())
                && !cloud.mesh_health().isolated;
            leader_ticks = if am_leader {
                leader_ticks.saturating_add(1)
            } else {
                0
            };
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
                t.mem_gb_hr_milli = t
                    .mem_gb_hr_milli
                    .saturating_add((s.memory_gb_hrs * 1000.0) as u64);
                t.requests = t.requests.saturating_add(s.requests);
                // Counted SEPARATELY from `t.requests`, never folded in: the
                // bn-impl-billing-metering design decision is NO per-invocation
                // charge for owner-served browser traffic (Cloudflare/Salad
                // precedent -- bill only platform-consumed resources). This
                // counter exists so quota/UI have real numbers ahead of any
                // future distinct rate; RateCard.browser_per_million_cents is
                // 0.0 today, not omitted, so pricing it later needs no schema
                // migration.
                t.browser_requests = t.browser_requests.saturating_add(s.browser_requests);
                if s.gpu {
                    // GPU is held for the instance's entire life — meter its
                    // wall-time (fluid_ms), not just active CPU.
                    t.gpu_ms = t.gpu_ms.saturating_add(s.fluid_ms);
                }
            }
            let mut charged_any = 0u64;
            // Meter first, then mirror the whole tick's tenants in ONE batch.
            // Mirroring inside this loop called `relational::upsert_billing`
            // per tenant, and each of those re-read the ENTIRE relational
            // index before writing (see `upsert_billing_many`). Metering
            // itself is unchanged and still strictly per tenant.
            let mut metered: Vec<String> = Vec::new();
            for (tenant, tot) in totals {
                charged_any += cloud.billing.meter_usage(&tenant, tot);
                metered.push(tenant);
            }
            // Mirror into the fleet-replicated relational layer (see
            // relational.rs's module doc) right after metering — ONLY this
            // node (the elected billing authority) ever writes it, so
            // every OTHER node's local replica converges to this SAME
            // account state within seconds instead of staying empty/stale
            // (the confirmed 5-way billing-divergence bug). Best-effort:
            // the existing HTTP proxy-to-leader read remains correct and
            // available regardless of this mirror's freshness.
            let owned: Vec<_> = metered
                .iter()
                .map(|tenant| {
                    (
                        cloud.billing.account(tenant),
                        cloud.billing.ledger(tenant),
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
/// Loud, cheap, self-observed early warning for the fc-sanjose-2 shape: a
/// process climbing toward its cgroup memory ceiling while every EXTERNAL
/// signal (systemd is-active, /healthz 200, even :443 customer traffic) stays
/// green. Distinct from `spawn_health_loop` (which is PEER-observed reachability,
/// the right check for routing decisions) — this is a SELF-check for exactly
/// the failure class no peer probe can see, because the process is still
/// answering everything a probe would ask, right up until it isn't.
/// `HIVE_MEMORY_ALARM_PCT` (default 80.0): once RSS crosses this fraction of
/// the cgroup limit, warn on every tick until it drops back below — noisy on
/// purpose, since the incident this guards against was invisible for however
/// long the climb took.
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

fn spawn_health_loop(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(env_u64("HIVE_HEALTH_INTERVAL", 5));
    let timeout = Duration::from_secs(env_u64("HIVE_HEALTH_TIMEOUT", 2));
    let threshold = env_u64("HIVE_HEALTH_FAIL_THRESHOLD", 2) as u32;
    tracing::info!(
        ?interval,
        ?timeout,
        threshold,
        "active health probing (public nodes)"
    );
    tokio::spawn(async move {
        let mut misses: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        loop {
            tokio::time::sleep(interval).await;
            // No mesh transport bound yet → skip the round (never false-flag a peer).
            if cloud.mesh.read().is_none() {
                continue;
            }
            // Probe set: every OTHER node with a public IP + a resolvable iroh address.
            // Each result retains the immutable endpoint id and liveness timestamp
            // captured with the attempted address.
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
            if targets.is_empty() {
                continue;
            }
            // Probe ALL targets concurrently — a dead/slow peer must not delay the rest.
            //
            // Budget is per-target, not fleet-wide. The fast `timeout` (2s) is the
            // right STEADY-STATE check — "is the warm trunk still alive" — but it is
            // shorter than `connect_budget` alone, so a target needing a fresh dial
            // gets cancelled before it can even finish connecting, and `probe`'s own
            // trunk eviction (whose stated purpose is "let the next dial resolve the
            // peer's CURRENT addr via discovery") could never actually deliver that:
            // every subsequent attempt was cancelled at the same 2s. A peer whose
            // cached address went stale (restart with new socket addrs, NAT rebind)
            // therefore stayed unhealthy PERMANENTLY — and unhealthy nodes are
            // dropped from client DNS and placement, so this silently shrank the
            // fleet. Once a target is already failing, give it the full
            // dial_fallback_ceiling so discovery gets a genuine chance to recover it.
            let results = futures::future::join_all(targets.into_iter().map(
                |(name, endpoint_id, addr, observed_last_seen_ms)| {
                    let cloud = cloud.clone();
                    let failing = *misses.get(&endpoint_id).unwrap_or(&0) >= threshold;
                    let budget = if failing {
                        hive_p2p::dial_fallback_ceiling()
                    } else {
                        timeout
                    };
                    async move {
                        // TWO samples per round, pass on EITHER (tau's multi-ping
                        // liveness): a single lost datagram train on a lossy
                        // cross-continent path counted as a full round miss, and
                        // at threshold=2 two unlucky rounds withdrew a healthy
                        // peer. The samples run sequentially so the second only
                        // spends budget when the first genuinely failed (which
                        // also gives `probe`'s trunk-eviction from the first
                        // failure a fresh-dial chance within the same round).
                        let first = gossip::probe(&cloud, &endpoint_id, &addr, budget).await;
                        let rtt = match first {
                            Some(ms) => Some(ms),
                            None => gossip::probe(&cloud, &endpoint_id, &addr, budget).await,
                        };
                        (name, endpoint_id, observed_last_seen_ms, rtt)
                    }
                },
            ))
            .await;
            let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (name, endpoint_id, observed_last_seen_ms, rtt) in results {
                live.insert(endpoint_id.clone());
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
            // Forget miss-counters for endpoint identities no longer in the probe set.
            misses.retain(|endpoint_id, _| live.contains(endpoint_id));
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
