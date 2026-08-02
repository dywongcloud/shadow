//! `fluid-compute` — the Fluid pool: "serverless servers".
//!
//! Per function it keeps a pool of long-lived instances (cells running the
//! function server). The routing/scaling rules follow Vercel's Fluid writeups:
//!
//! * **In-function concurrency** — one instance handles up to
//!   `max_concurrency` simultaneous requests.
//! * **Reuse before provision** — a request is routed to an already-running
//!   instance with a free slot, choosing the one with the *fewest* in-flight
//!   requests (least-loaded), which Vercel found beats round-robin.
//! * **Cold-start on saturation** — only when every instance is full do we
//!   provision a new one, up to `max_instances`.
//! * **Keep-warm** — `min_instances` are kept ready.
//! * **Scale-to-zero** — idle instances beyond `min_instances` are drained
//!   after `idle_ttl`.
//! * **`waitUntil`** — an instance's lease is held for a declared post-response
//!   window (`x-fluid-wait-until-ms`) so background work runs after the response
//!   (see `fluid-gateway`).
//! * **Active CPU pricing** — metering reports active CPU time (ms) + provisioned
//!   memory (GB-hrs), not idle wall-time (`FunctionStats`), matching Fluid's
//!   billing convention where I/O-idle time isn't CPU-billed.
//!
//! Concurrency discipline matches the control plane: one `parking_lot::Mutex`
//! over the registry, never held across an `.await`; backend provisioning and
//! teardown happen outside the lock.

use fluid_core::FunctionConfig;
use hive_backend::{connect_endpoint, CellBackend, CellEndpoint, CellSpec, FunctionLaunch};
use hive_core::{now_ms, CellId, ResourceSpec};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// A single running function instance (one cell).
struct Instance {
    cell_id: CellId,
    handle: hive_backend::CellHandle,
    endpoint: CellEndpoint,
    /// Loopback UDP publishes of this instance's container (container_port →
    /// host_port), chosen at cold start. This is the "instance registry" surface
    /// hive-cloud's UDP relay resolves the LOCAL datagram leg from (via
    /// [`Lease::udp_host_port`]) — datagrams cannot ride the TCP tunnel
    /// `endpoint`, so they go straight at `127.0.0.1:<host_port>`. Empty for
    /// non-container functions and containers with no declared UDP specs.
    udp_ports: Vec<hive_core::UdpPublish>,
    inflight: u32,
    started_at_ms: u64,
    last_active_ms: u64,
    draining: bool,
    /// Lifetime request count — drives request-based recycling (no runtime heap
    /// signal needed). A long-lived warm instance that has served many requests is
    /// recycled to bound memory drift / leaks (#13).
    requests_served: u64,
}

/// Is this instance due for recycling? Pure (testable) policy over the signal-free
/// inputs we DO have: lifetime request count and wall-clock age. (Heap-growth /
/// GC-pause recycling needs a runtime-emitted signal — left as a hook.)
fn instance_recyclable(
    requests_served: u64,
    age_ms: u64,
    max_requests: u64,
    max_age_ms: u64,
) -> bool {
    (max_requests > 0 && requests_served >= max_requests)
        || (max_age_ms > 0 && requests_served > 0 && age_ms >= max_age_ms)
}

/// Everything needed to (re)launch instances of one function.
struct FunctionPool {
    cfg: FunctionConfig,
    /// Owning team/tenant (normalized; empty => "personal"). Every cell this pool
    /// spawns is tagged with it, and per-tenant instance quotas sum across all
    /// pools sharing this tenant.
    tenant: String,
    /// Image / rootfs name for cells of this function.
    image: String,
    /// Working dir for the function process (host path for mock backend).
    workdir: String,
    instances: Vec<Instance>,
    /// Cold starts currently in flight (counts toward max_instances).
    provisioning: u32,
    // ---- cost metering ----
    /// Total requests served.
    requests: u64,
    /// Sum of per-request active durations. This is what *traditional* 1:1
    /// serverless would bill (one dedicated instance per concurrent request).
    served_ms_sum: u64,
    /// Alive-time of instances already scaled down. Combined with live
    /// instances' alive-time, this is what *Fluid* bills (instance-time,
    /// shared across concurrent requests).
    instance_ms_retired: u64,
    /// Instances reaped because they became unreachable (health/nack).
    dead_reaped: u64,
    /// Requests served by a donated browser node instead of fleet compute
    /// (`fluid-gateway::try_browser`'s success path never calls `lease()`, so
    /// without this counter browser-served traffic is invisible to billing).
    browser_requests: u64,
    /// Sum of per-request active durations for browser-served requests —
    /// tracked alongside `browser_requests` for a future distinct rate, kept
    /// separate from `served_ms_sum` since it isn't fleet instance-time.
    browser_served_ms: u64,
    /// Keep-warm failure backoff: the autoscaler won't attempt to warm this
    /// pool again until `now_ms()` reaches this. Prevents a pool whose cold
    /// starts keep failing (e.g. host out of locks/processes) from retrying
    /// every autoscaler tick and pinning the host.
    warm_backoff_until_ms: u64,
    /// Consecutive keep-warm failures; drives the exponential backoff above.
    warm_fail_streak: u32,
    /// `now_ms()` of the last keep-warm cold start that RETURNED Ok. A successful
    /// `cold_start` only means the backend accepted the launch — it says nothing
    /// about whether the instance stayed up. Paired with `crash_streak` below to
    /// tell "warmed and healthy" apart from "warmed and died seconds later".
    last_warm_ok_ms: u64,
    /// Consecutive times an instance this pool successfully warmed VANISHED
    /// inside `CRASH_LOOP_WINDOW_MS`. This is the crash-loop signal the plain
    /// `warm_fail_streak` cannot see: a container that starts fine and then
    /// exits on its own (bad entrypoint, unaccepted licence prompt, missing env)
    /// takes the `Ok` branch, so without this the streak reset every tick and
    /// keep-warm relaunched it forever — one misconfigured app churning the host
    /// and, before the volume leak was fixed, draining podman's shared lock pool
    /// until EVERY other deployment on the node failed with CAPACITY_EXHAUSTED.
    crash_streak: u32,
    /// Circuit half-open gate: while `now_ms()` is under this, a pool whose
    /// failure streak has crossed the threshold fails leases FAST; once past it,
    /// exactly one request is allowed through as a probe (which re-arms this on
    /// failure, or clears the streak on success).
    ///
    /// Deliberately SEPARATE from `warm_backoff_until_ms`. That one is tuned for
    /// how often to re-warm (seconds), and reusing it made the circuit useless:
    /// a failing cold start takes far longer than its own 2-8s backoff, so every
    /// arriving request found the window already expired and paid the full
    /// startup cost again. The gate has to outlast the failure it is guarding.
    circuit_probe_after_ms: u64,
    /// Dynamic per-instance safe-concurrency ceiling. `None` = use
    /// `cfg.max_concurrency`. A pressure monitor LOWERS this (CPU-heavy / event-loop
    /// lag / memory pressure) so the scheduler scales out earlier instead of piling
    /// onto a hot instance; an I/O-heavy function keeps the full ceiling. The
    /// effective reuse limit is `min(cfg.max_concurrency, safe_concurrency)`.
    safe_concurrency: Option<u32>,
    // ---- scheduler observability ----
    /// NACKs because all instances were at the safe-concurrency ceiling.
    nack_concurrency: u64,
    /// NACKs because the tenant hit its cross-pool instance quota.
    nack_quota: u64,
    /// Cold starts triggered by saturation (scale-out decisions).
    scale_out_total: u64,
    /// Requests that coalesced onto an in-flight cold start (singleflight) instead of
    /// launching their own — each one is a cold start avoided.
    coldstart_deduped: u64,
    /// Instances recycled (request-count / age based) to bound memory drift (#13).
    recycled: u64,
    // ---- cold-start phase decomposition (#12) ----
    /// Last cold start's backend `provision` time (boot/microVM/container) in ms.
    last_provision_ms: u64,
    /// Last cold start's `start_function` time (runtime init + tunnel attach) in ms.
    last_runtime_init_ms: u64,
    /// Most recent CPU sample (% of allocated vCPU, max across live instances) that
    /// drove the adaptive-concurrency controller (#2). 0 when never sampled / the
    /// backend can't report CPU.
    last_cpu_pct: f32,
}

impl FunctionPool {
    fn live_count(&self) -> u32 {
        self.instances.iter().filter(|i| !i.draining).count() as u32 + self.provisioning
    }
    fn total_inflight(&self) -> u32 {
        self.instances.iter().map(|i| i.inflight).sum()
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FunctionStats {
    pub key: String,
    pub instances: usize,
    pub inflight: u32,
    /// This pool's function requests a serverless GPU — its instance-time is
    /// metered on the GPU dimension in addition to CPU/memory.
    #[serde(default)]
    pub gpu: bool,
    pub provisioning: u32,
    pub max_concurrency: u32,
    pub min_instances: u32,
    pub max_instances: u32,
    /// Owning team/tenant (normalized; "personal" when unset).
    pub tenant: String,
    // ---- cost metering ----
    pub requests: u64,
    /// What traditional 1:1 serverless would bill (instance-seconds).
    pub traditional_ms: u64,
    /// What Fluid bills (shared instance-time).
    pub fluid_ms: u64,
    /// Savings vs traditional, 0.0–1.0 (the "up to 85%" story).
    pub savings_pct: f64,
    /// Instances reaped as unhealthy/unreachable and replaced.
    pub dead_reaped: u64,
    /// Requests served by a donated browser node instead of fleet compute.
    #[serde(default)]
    pub browser_requests: u64,
    /// Sum of active durations for browser-served requests.
    #[serde(default)]
    pub browser_ms: u64,
    // ---- Active CPU pricing (Vercel Fluid convention) -------------------------
    // Fluid bills the ACTIVE CPU time used (ms) plus the PROVISIONED MEMORY
    // (GB-hrs) — NOT idle wall-time. While an instance waits on I/O (DB, API, LLM)
    // it isn't billed for CPU, which is where the "90%+ savings" come from.
    /// Active CPU time across all requests (ms) — the time instances were actually
    /// processing, excluding idle keep-warm time.
    pub active_cpu_ms: u64,
    /// Provisioned memory consumed, in GB-hours: Σ (instance memory GB × alive hrs).
    pub memory_gb_hrs: f64,
    /// Additional savings of Active-CPU billing vs paying for full instance-time
    /// (fluid_ms), 0.0–1.0 — the "Active CPU pricing" story on top of multiplexing.
    pub active_cpu_savings_pct: f64,
    // ---- scheduler observability ----
    /// Effective per-instance safe-concurrency ceiling currently in force
    /// (min(max_concurrency, pressure-hook value)).
    pub safe_concurrency: u32,
    /// Total NACKs (backpressure events) — sum of the reason counters below.
    pub nack_total: u64,
    /// NACKs because all instances were at the safe-concurrency ceiling.
    pub nack_concurrency: u64,
    /// NACKs because the tenant hit its cross-pool instance quota.
    pub nack_quota: u64,
    /// Cold starts triggered by saturation (scale-out decisions).
    pub scale_out_total: u64,
    /// Cold starts avoided by singleflight coalescing onto an in-flight startup.
    pub coldstart_deduped: u64,
    /// Instances recycled (request-count / age based) to bound memory drift.
    pub recycled: u64,
    /// Last cold start's provision (boot) time in ms — cold-start phase decomposition.
    pub last_provision_ms: u64,
    /// Last cold start's runtime-init + tunnel-attach time in ms.
    pub last_runtime_init_ms: u64,
    /// Most recent CPU sample (% of allocated vCPU) driving adaptive concurrency (#2).
    pub last_cpu_pct: f32,
    /// Whether adaptive concurrency has currently backed the ceiling BELOW max (#2).
    pub concurrency_throttled: bool,
}

pub struct FluidConfig {
    pub autoscaler_interval: Duration,
    /// How long `lease` waits for a slot before giving up (backpressure).
    pub lease_timeout: Duration,
    /// Max total live instances ONE tenant may hold across all of its function
    /// pools (multi-tenant fairness — stops a single team monopolizing a node).
    /// 0 = unlimited. Over-quota leases get backpressure (Saturated), never a
    /// cross-tenant eviction.
    pub max_instances_per_tenant: u32,
    /// Recycle a warm instance after it has served this many requests (0 = never).
    /// Bounds memory drift / leaks for long-lived hot instances WITHOUT needing a
    /// runtime heap signal — recycling is graceful (drain, then keep-warm replaces).
    pub max_requests_per_instance: u64,
    /// Recycle a warm instance once it reaches this age in seconds (0 = never).
    pub max_instance_age_secs: u64,
}

impl Default for FluidConfig {
    fn default() -> Self {
        FluidConfig {
            autoscaler_interval: Duration::from_millis(500),
            lease_timeout: Duration::from_secs(20),
            max_instances_per_tenant: 0,
            // Conservative recycle ceilings: a continuously-hot instance is replaced
            // after ~10k requests or ~1h, so leaks/drift can't accumulate unbounded.
            max_requests_per_instance: 10_000,
            max_instance_age_secs: 3_600,
        }
    }
}

pub struct Fluid {
    backend: Arc<dyn CellBackend>,
    cfg: FluidConfig,
    registry: Mutex<HashMap<String, FunctionPool>>,
    /// Bounds how many cold starts run concurrently across ALL pools. A burst of
    /// keep-warm reconciles (e.g. every deployment warming at once on boot) or a
    /// traffic spike would otherwise spawn unbounded backend containers in
    /// parallel and saturate the host's process table / container lock pool.
    cold_start_sem: Arc<tokio::sync::Semaphore>,
}

/// Max cold starts in flight at once across the whole node (ALL pools share this
/// one ceiling — it exists to bound process-table/container-lock pressure on the
/// HOST, not to throttle any single function). Was a flat `4` regardless of the
/// node's actual capacity, which head-of-line-blocks a multi-tenant burst (many
/// DIFFERENT functions cold-starting at once, e.g. on a fleet node serving dozens
/// of projects) to 4-at-a-time even on a 96-core bare-metal box. Scales with the
/// host's core count instead, floored at the original conservative `4` (small/mock
/// nodes keep today's exact behavior) and capped at `32` (still a real ceiling —
/// this bounds HOST pressure, not per-function throughput, so it must not grow
/// unbounded on very large boxes). `HIVE_MAX_COLD_STARTS` overrides both ends when
/// a node operator has measured a different safe number for their hardware.
fn max_concurrent_cold_starts() -> usize {
    if let Ok(v) = std::env::var("HIVE_MAX_COLD_STARTS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).clamp(4, 32)
}

/// Compose the per-function key.
pub fn func_key(deployment: &str, function: &str) -> String {
    format!("{deployment}/{function}")
}

/// Runtime-specific concurrency policy (#5). Different runtimes tolerate very
/// different in-instance concurrency before a new instance is warranted, so the
/// initial safe-concurrency BASELINE is runtime-aware (the adaptive hook can still
/// move it within bounds later):
///   * node / bun / edge isolate — single-threaded but I/O-friendly event loop:
///     keep the full configured concurrency (multiplexing I/O waits is the whole
///     point; Bun's own event loop has the identical characteristic).
///   * python — the GIL serializes CPU work, so cap concurrency lower (scale out).
///   * container / microVM — process/cgroup-level pressure varies; moderate cap.
///   * wasm / unknown — trust the configured value (sandbox limits already apply).
/// Only ever LOWERS toward a runtime-appropriate ceiling; never raises above the
/// deployment's configured `max_concurrency`.
pub fn recommended_safe_concurrency(runtime: &str, configured_max: u32) -> u32 {
    let cap = match runtime.trim().to_ascii_lowercase().as_str() {
        "node" | "nodejs" | "js" | "bun" | "edge" | "isolate" | "edge-isolate" => configured_max,
        "python" | "py" => configured_max.min(8),
        "container" | "docker" | "microvm" | "firecracker" => configured_max.min(16),
        "wasm" | "wasi" => configured_max,
        _ => configured_max,
    };
    cap.max(1)
}

/// Releases a cold start's `provisioning` reservation and records the failure if
/// the cold start did not produce an instance — including when the future is
/// simply DROPPED because the caller gave up. See [`Fluid::cold_start`].
struct ColdStartGuard {
    fluid: Arc<Fluid>,
    key: String,
    /// Cleared on success; while armed, Drop treats this as a failed cold start.
    armed: bool,
    /// `None` while armed means the future was cancelled rather than returning an
    /// error — worth distinguishing in the log, since an abandoned cold start
    /// points at a caller/proxy deadline rather than a broken deployment.
    error: Option<String>,
}

impl Drop for ColdStartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut streak = 0u32;
        if let Some(p) = self.fluid.registry.lock().get_mut(&self.key) {
            p.provisioning = p.provisioning.saturating_sub(1);
            p.warm_fail_streak = p.warm_fail_streak.saturating_add(1);
            p.warm_backoff_until_ms = now_ms() + (1000u64 << p.warm_fail_streak.min(6));
            p.circuit_probe_after_ms = now_ms() + CIRCUIT_PROBE_INTERVAL_MS;
            streak = p.warm_fail_streak;
        }
        // The line to read first when a deployment will not start: it names the
        // streak driving the circuit and whether the caller waited for the answer.
        warn!(
            func = %self.key,
            fail_streak = streak,
            abandoned = self.error.is_none(),
            error = self.error.as_deref().unwrap_or("caller gave up before the cold start finished"),
            "cold start failed"
        );
    }
}

/// CPU thresholds (% of a cell's allocated vCPU budget) for the adaptive
/// concurrency controller (#2). Above HIGH we back off; below LOW we recover; the
/// band between is hysteresis so we don't oscillate.
pub const AIMD_CPU_HIGH: f32 = 85.0;
pub const AIMD_CPU_LOW: f32 = 60.0;

/// An instance that disappears within this long of a SUCCESSFUL warm did not get
/// scaled down — it exited on its own. Wide enough to catch a container that dies
/// on its entrypoint (the observed case died in ~2s) without mistaking a genuine
/// idle-drain for a crash.
pub const CRASH_LOOP_WINDOW_MS: u64 = 30_000;
/// Consecutive crash-loop deaths after which the pool's circuit OPENS and leases
/// fail fast instead of cold-starting into the same immediate death.
pub const CRASH_CIRCUIT_THRESHOLD: u32 = 3;
/// How long an open circuit stays open before letting ONE probe request through.
/// Must comfortably exceed how long a failing cold start takes (observed ~20s for
/// a container that exits before listening), or arriving requests keep finding the
/// gate expired and pay the full failure cost every time.
pub const CIRCUIT_PROBE_INTERVAL_MS: u64 = 30_000;

/// One AIMD step for a function's per-instance safe-concurrency ceiling (#2), given
/// the most-pressured instance's CPU utilization (% of allocated vCPU). Classic
/// additive-increase / multiplicative-decrease driven by a REAL CPU signal (not
/// latency, which would penalize I/O-bound functions):
///   * CPU at/above HIGH  → halve the ceiling (multiplicative decrease) so new
///     requests scale OUT to another instance instead of piling onto a hot one.
///   * CPU at/below LOW    → +1 (additive increase) to reclaim headroom, so an
///     I/O-bound function (low CPU even when busy) climbs back to its full max.
///   * in between          → hold (hysteresis).
/// Result is clamped to `[1, max]`. Pure + deterministic for unit testing.
pub fn aimd_step(cpu_pct: f32, current: u32, max: u32) -> u32 {
    let next = if cpu_pct >= AIMD_CPU_HIGH {
        (current / 2).max(1)
    } else if cpu_pct <= AIMD_CPU_LOW {
        current.saturating_add(1)
    } else {
        current
    };
    next.clamp(1, max.max(1))
}

/// Normalize an owner slug: empty => "personal" (matches the control layer).
fn norm_tenant(t: String) -> String {
    if t.trim().is_empty() {
        "personal".into()
    } else {
        t
    }
}

impl Fluid {
    pub fn start(backend: Arc<dyn CellBackend>, cfg: FluidConfig) -> Arc<Fluid> {
        let fluid = Arc::new(Fluid {
            backend,
            cfg,
            registry: Mutex::new(HashMap::new()),
            cold_start_sem: Arc::new(tokio::sync::Semaphore::new(max_concurrent_cold_starts())),
        });
        let f = fluid.clone();
        tokio::spawn(async move { f.autoscaler_loop().await });
        let h = fluid.clone();
        tokio::spawn(async move { h.health_loop().await });
        fluid
    }

    /// Register (or replace) a function pool owned by `tenant` (empty =>
    /// "personal"). Does not provision yet; the autoscaler will bring up
    /// `min_instances`.
    pub fn register(
        &self,
        key: String,
        cfg: FunctionConfig,
        image: String,
        workdir: String,
        tenant: String,
    ) {
        let tenant = norm_tenant(tenant);
        // Runtime-aware initial safe-concurrency baseline (#5). `None` when it equals
        // the configured max (i.e. no reduction) so the common case is unchanged.
        let rec = recommended_safe_concurrency(&cfg.runtime, cfg.max_concurrency);
        let safe_concurrency = if rec < cfg.max_concurrency {
            Some(rec)
        } else {
            None
        };
        let mut reg = self.registry.lock();
        // Re-registering a key (a redeploy, or a boot restore over a live pool)
        // installs a fresh pool whose `instances` is empty. Anything the OLD pool
        // was running would then be orphaned: still alive on the host, still
        // holding a lock/process slot/memory/published port, but unknown to the
        // pool — so never reused, never drained, never terminated. Mark them
        // draining instead and let the reconcile loop terminate them properly.
        let orphans: Vec<Instance> = reg
            .get_mut(&key)
            .map(|old| {
                old.instances
                    .drain(..)
                    .map(|mut i| {
                        i.draining = true;
                        i
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !orphans.is_empty() {
            warn!(
                func = %key,
                count = orphans.len(),
                "re-registering a pool with live instances — draining them instead of orphaning"
            );
        }
        reg.insert(
            key,
            FunctionPool {
                cfg,
                tenant,
                image,
                workdir,
                instances: orphans,
                provisioning: 0,
                requests: 0,
                served_ms_sum: 0,
                instance_ms_retired: 0,
                dead_reaped: 0,
                browser_requests: 0,
                browser_served_ms: 0,
                warm_backoff_until_ms: 0,
                warm_fail_streak: 0,
                last_warm_ok_ms: 0,
                crash_streak: 0,
                circuit_probe_after_ms: 0,
                safe_concurrency,
                nack_concurrency: 0,
                nack_quota: 0,
                scale_out_total: 0,
                coldstart_deduped: 0,
                recycled: 0,
                last_provision_ms: 0,
                last_runtime_init_ms: 0,
                last_cpu_pct: 0.0,
            },
        );
    }

    /// Set a pool's dynamic safe-concurrency ceiling (the CPU/IO-pressure hook). A
    /// pressure monitor calls this to LOWER reuse for CPU-heavy / lagging functions
    /// (scale out earlier) or raise it back for I/O-bound ones. Clamped to ≥1 and to
    /// `cfg.max_concurrency`; `None` restores the static ceiling. No-op if unknown.
    pub fn set_safe_concurrency(&self, key: &str, n: Option<u32>) {
        if let Some(p) = self.registry.lock().get_mut(key) {
            p.safe_concurrency = n.map(|v| v.clamp(1, p.cfg.max_concurrency));
        }
    }

    /// Replace an existing pool's function config IN PLACE, keeping its live
    /// instances — the no-redeploy settings-edit hook (ports/protocol changes
    /// from the dashboard's Network page, via `Gateway::update_manifest`).
    /// [`Fluid::register`] is NOT usable for this: it replaces the whole
    /// [`FunctionPool`] (fresh `instances: Vec::new()`), which would orphan
    /// every running cell untracked. The new config applies to FUTURE instance
    /// launches (e.g. `cold_start`'s per-launch UDP port publishes); instances
    /// already running keep their launch-time shape until recycled.
    /// Recomputes the runtime-aware safe-concurrency baseline the same way
    /// `register` does. No-op if the pool isn't registered.
    pub fn update_config(&self, key: &str, cfg: FunctionConfig) {
        let rec = recommended_safe_concurrency(&cfg.runtime, cfg.max_concurrency);
        let safe_concurrency = if rec < cfg.max_concurrency {
            Some(rec)
        } else {
            None
        };
        if let Some(p) = self.registry.lock().get_mut(key) {
            p.cfg = cfg;
            p.safe_concurrency = safe_concurrency;
        }
    }

    /// Set a pool's keep-warm floor. Setting it to 0 lets the autoscaler drain the
    /// pool's idle instances to zero (scale-to-zero) — used to stop keeping
    /// superseded (non-production) deployments warm while still allowing a cold
    /// start if their immutable URL is hit. No-op if the pool isn't registered.
    pub fn set_min_instances(&self, key: &str, n: u32) {
        if let Some(p) = self.registry.lock().get_mut(key) {
            p.cfg.min_instances = n;
        }
    }

    /// The owning tenant of a function pool (normalized), if registered.
    pub fn tenant_of(&self, key: &str) -> Option<String> {
        self.registry.lock().get(key).map(|p| p.tenant.clone())
    }

    /// Name of the active isolation backend ("mock" | "firecracker").
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Make a built deployment available to the cells that will serve it (see
    /// [`hive_backend::CellBackend::deliver_build`]). No-op for same-host backends.
    pub async fn deliver_build(
        &self,
        image: &str,
        build_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        self.backend.deliver_build(image, build_dir).await
    }

    /// Total live instances (running + provisioning) a tenant currently holds
    /// across all of its function pools.
    pub fn tenant_live_instances(&self, tenant: &str) -> u32 {
        let t = norm_tenant(tenant.to_string());
        self.registry
            .lock()
            .values()
            .filter(|p| p.tenant == t)
            .map(|p| p.live_count())
            .sum()
    }

    /// Max invocation duration configured for a function (seconds).
    pub fn max_duration_secs(&self, key: &str) -> Option<u64> {
        self.registry
            .lock()
            .get(key)
            .map(|p| p.cfg.max_duration_secs)
    }

    pub fn stats(&self) -> Vec<FunctionStats> {
        let reg = self.registry.lock();
        let now = now_ms();
        let mut out: Vec<FunctionStats> = reg
            .iter()
            .map(|(k, p)| {
                // Fluid bill = retired instance-time + live instances' alive-time.
                let live_alive: u64 = p
                    .instances
                    .iter()
                    .map(|i| now.saturating_sub(i.started_at_ms))
                    .sum();
                let fluid_ms = p.instance_ms_retired + live_alive;
                let traditional_ms = p.served_ms_sum;
                let savings_pct = if traditional_ms > 0 {
                    (1.0 - (fluid_ms as f64 / traditional_ms as f64)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                // Active CPU pricing (Vercel convention): bill active processing
                // time + provisioned memory GB-hrs, not idle instance wall-time.
                let active_cpu_ms = p.served_ms_sum;
                let memory_gb_hrs =
                    (p.cfg.memory_mib as f64 / 1024.0) * (fluid_ms as f64 / 3_600_000.0);
                // Active-CPU billing only charges processing time, so on top of the
                // multiplexing win it saves the idle keep-warm portion of fluid_ms.
                let active_cpu_savings_pct = if fluid_ms > 0 {
                    (1.0 - (active_cpu_ms as f64 / fluid_ms as f64)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                FunctionStats {
                    key: k.clone(),
                    instances: p.instances.iter().filter(|i| !i.draining).count(),
                    inflight: p.total_inflight(),
                    gpu: p.cfg.gpu,
                    provisioning: p.provisioning,
                    max_concurrency: p.cfg.max_concurrency,
                    min_instances: p.cfg.min_instances,
                    max_instances: p.cfg.max_instances,
                    tenant: p.tenant.clone(),
                    requests: p.requests,
                    browser_requests: p.browser_requests,
                    browser_ms: p.browser_served_ms,
                    traditional_ms,
                    fluid_ms,
                    savings_pct,
                    dead_reaped: p.dead_reaped,
                    active_cpu_ms,
                    memory_gb_hrs,
                    active_cpu_savings_pct,
                    safe_concurrency: p
                        .cfg
                        .max_concurrency
                        .min(p.safe_concurrency.unwrap_or(u32::MAX))
                        .max(1),
                    nack_total: p.nack_concurrency + p.nack_quota,
                    nack_concurrency: p.nack_concurrency,
                    nack_quota: p.nack_quota,
                    scale_out_total: p.scale_out_total,
                    coldstart_deduped: p.coldstart_deduped,
                    recycled: p.recycled,
                    last_provision_ms: p.last_provision_ms,
                    last_runtime_init_ms: p.last_runtime_init_ms,
                    last_cpu_pct: p.last_cpu_pct,
                    concurrency_throttled: p.safe_concurrency.is_some(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// Acquire a slot on an instance for one request. Reuses a running instance
    /// (least-loaded with a free slot) or cold-starts when saturated.
    pub async fn lease(self: &Arc<Self>, key: &str) -> anyhow::Result<Lease> {
        // Open circuit: this deployment's instances keep dying seconds after they
        // start, so cold-starting another one just dies the same way while the
        // caller waits out the whole lease timeout (the observed symptom was a
        // request hanging ~90s). Fail fast and name the REAL cause —
        // `classify_lease_error` maps this to DEPLOYMENT_CIRCUIT_OPEN so a broken
        // app stops masquerading as host CAPACITY_EXHAUSTED. Reconcile keeps
        // retrying on its backoff curve, so a fixed deployment recovers on its own.
        if let Some(p) = self.registry.lock().get(key) {
            // Why a struggling pool did or did not circuit. Gated on a non-zero
            // streak so a healthy pool logs nothing, which also makes this the
            // first thing to read when a deployment is failing in production.
            if p.warm_fail_streak > 0 || p.crash_streak > 0 {
                warn!(
                    func = %key,
                    instances = p.instances.len(),
                    warm_fail_streak = p.warm_fail_streak,
                    crash_streak = p.crash_streak,
                    probe_gate_ms_left = p.circuit_probe_after_ms.saturating_sub(now_ms()),
                    "lease against a failing pool"
                );
            }
            // Only with NO live instance: a pool serving traffic is never circuited.
            if p.instances.is_empty() {
                // (a) Instances come up and then die on their own.
                if p.crash_streak >= CRASH_CIRCUIT_THRESHOLD {
                    let n = p.crash_streak;
                    anyhow::bail!(
                        "function '{key}' is crash-looping — {n} consecutive instances exited \
                         within {}s of starting; check the deployment's logs, entrypoint and \
                         required env (DeploymentCircuitOpen)",
                        CRASH_LOOP_WINDOW_MS / 1000
                    );
                }
                // (b) Instances never reach a listening state at all (the witnessed
                // "exited before listening on 127.0.0.1:PORT" shape). Keep-warm
                // already backs off on this, but the REQUEST path did not: every
                // incoming request re-ran the doomed cold start and then burned the
                // reroute budget on top, so the caller waited ~45s to be told
                // CAPACITY_EXHAUSTED. Fail fast for the duration of the backoff the
                // failures already armed — when it expires the next request probes
                // again (half-open), so a fixed deployment recovers with no operator
                // action.
                if p.warm_fail_streak >= CRASH_CIRCUIT_THRESHOLD
                    && now_ms() < p.circuit_probe_after_ms
                {
                    let n = p.warm_fail_streak;
                    anyhow::bail!(
                        "function '{key}' cannot start — {n} consecutive cold starts failed \
                         (the container exits before it listens on its port); check the \
                         deployment's logs, entrypoint and required env (DeploymentCircuitOpen)"
                    );
                }
            }
        }
        let deadline = tokio::time::Instant::now() + self.cfg.lease_timeout;
        // Coalesce window: how long a request waits for an in-flight cold start
        // (singleflight) before it's allowed to launch its own. Bounded so a slow
        // startup never blocks scale-out beyond this; capped by the lease timeout.
        let coalesce_until =
            tokio::time::Instant::now() + Duration::from_millis(800).min(self.cfg.lease_timeout);
        let mut counted_dedupe = false;
        loop {
            let coalesce = tokio::time::Instant::now() < coalesce_until;
            let decision = self.decide_lease(key, coalesce)?;
            match decision {
                LeaseDecision::Ready { cell_id, endpoint } => {
                    return Ok(Lease {
                        fluid: self.clone(),
                        key: key.to_string(),
                        cell_id,
                        endpoint,
                        started: tokio::time::Instant::now(),
                        released: false,
                    });
                }
                LeaseDecision::WaitForWarm => {
                    // Singleflight: a cold start is already coming online — wait for it
                    // and reuse it rather than booting a duplicate. Counts as one cold
                    // start avoided. Bails at the overall lease deadline.
                    if !counted_dedupe {
                        if let Some(p) = self.registry.lock().get_mut(key) {
                            p.coldstart_deduped += 1;
                        }
                        counted_dedupe = true;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!("function '{key}' cold-start coalesce timed out");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                LeaseDecision::ColdStart => match self.cold_start(key).await {
                    Ok((cell_id, endpoint)) => {
                        // Started fine — close the circuit. A pool with
                        // min_instances=0 is never warmed by the autoscaler, so this
                        // is the ONLY place its failure streak can be cleared.
                        if let Some(p) = self.registry.lock().get_mut(key) {
                            p.warm_fail_streak = 0;
                            p.warm_backoff_until_ms = 0;
                            p.circuit_probe_after_ms = 0;
                        }
                        return Ok(Lease {
                            fluid: self.clone(),
                            key: key.to_string(),
                            cell_id,
                            endpoint,
                            started: tokio::time::Instant::now(),
                            released: false,
                        });
                    }
                    Err(e) => {
                        // Release the provisioning reservation, and record the failure
                        // against the SAME streak/backoff keep-warm uses. Without this
                        // a request-driven pool (min_instances=0, so keep-warm never
                        // runs) could fail its cold start on every single request
                        // forever, each caller paying the full startup timeout plus the
                        // reroute budget, with nothing anywhere counting the failures.
                        // The reservation release AND the failure count both
                        // happen in `cold_start`'s Drop guard, so that a client
                        // who gives up mid-cold-start is handled identically to
                        // an explicit error. Doing it here too would double-count.
                        return Err(e);
                    }
                },
                LeaseDecision::Saturated(reason) => {
                    // Backpressure: briefly wait for a slot to free, then NACK so the
                    // caller (router) can fail over to another peer/region. The reason
                    // is recorded in the pool's counters for observability.
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!("function '{key}' saturated ({reason:?})");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                LeaseDecision::NotFound => anyhow::bail!("no such function '{key}'"),
            }
        }
    }

    fn decide_lease(&self, key: &str, coalesce: bool) -> anyhow::Result<LeaseDecision> {
        let mut reg = self.registry.lock();
        // Existence + per-function knobs. The EFFECTIVE reuse ceiling is the lower of
        // the static `max_concurrency` and the dynamic `safe_concurrency` (lowered by
        // the pressure hook for CPU-heavy / lagging functions so we scale out sooner).
        let (max_c, max_instances) = match reg.get(key) {
            Some(p) => (
                p.cfg
                    .max_concurrency
                    .min(p.safe_concurrency.unwrap_or(u32::MAX))
                    .max(1),
                p.cfg.max_instances,
            ),
            None => return Ok(LeaseDecision::NotFound),
        };
        // Reuse the least-loaded healthy instance with a free slot (beats round-robin
        // — Vercel's finding). Draining instances are never reused.
        {
            let pool = reg.get_mut(key).expect("key present under held lock");
            if let Some(inst) = pool
                .instances
                .iter_mut()
                .filter(|i| !i.draining && i.inflight < max_c)
                .min_by_key(|i| i.inflight)
            {
                inst.inflight += 1;
                inst.last_active_ms = now_ms();
                return Ok(LeaseDecision::Ready {
                    cell_id: inst.cell_id.clone(),
                    endpoint: inst.endpoint.clone(),
                });
            }
        }
        // Singleflight: if a cold start is ALREADY in flight for this pool (demand or
        // keep-warm), coalesce onto it — wait briefly + reuse it — instead of booting
        // a duplicate instance. Self-correcting: once it's up (provisioning→0) and
        // still full, the next pass scales out. `coalesce` is false once the caller's
        // short wait window elapses, so a slow startup never blocks scale-out forever.
        if coalesce && reg.get(key).expect("present").provisioning > 0 {
            return Ok(LeaseDecision::WaitForWarm);
        }
        // All full: this pool must be under its OWN ceiling to cold-start (scale out).
        if reg.get(key).expect("present").live_count() >= max_instances {
            let pool = reg.get_mut(key).expect("present");
            pool.nack_concurrency += 1;
            return Ok(LeaseDecision::Saturated(NackReason::ConcurrencyLimit));
        }
        // ...and the owning tenant must be under its cross-pool instance quota.
        // Backpressure (Saturated), never a cross-tenant eviction, so one team
        // can't starve another off a shared node.
        if self.cfg.max_instances_per_tenant > 0 {
            let tenant = reg.get(key).expect("present").tenant.clone();
            let tenant_live: u32 = reg
                .values()
                .filter(|p| p.tenant == tenant)
                .map(|p| p.live_count())
                .sum();
            if tenant_live >= self.cfg.max_instances_per_tenant {
                reg.get_mut(key).expect("present").nack_quota += 1;
                return Ok(LeaseDecision::Saturated(NackReason::TenantQuota));
            }
        }
        let pool = reg.get_mut(key).expect("present");
        pool.provisioning += 1;
        pool.scale_out_total += 1;
        Ok(LeaseDecision::ColdStart)
    }

    /// Provision a cell and start the function in it. `provisioning` was already
    /// incremented by the caller; on success we add the instance with inflight=1.
    /// Count EVERY failing cold start here, CANCELLATION-SAFELY — the one point
    /// all callers funnel through (keep-warm, a request-path lease, any future
    /// caller).
    ///
    /// Two distinct bugs made the earlier per-caller counting useless, both
    /// witnessed live:
    ///
    /// 1. Only the caller that actually RUNS the cold start reaches its own error
    ///    branch. Every request that coalesced behind an in-flight one bailed with
    ///    "cold-start coalesce timed out" and counted nothing — and those waiters
    ///    are the majority under load.
    /// 2. Worse, when the CLIENT gives up (curl timeout, browser navigation away,
    ///    an upstream proxy deadline), axum drops the request future mid-await. A
    ///    dropped future never returns `Err` at all, so no error branch anywhere
    ///    runs: the failure went uncounted AND the `provisioning` reservation
    ///    taken in `decide_lease` was never released. A leaked reservation is the
    ///    expensive half — `live_count()` includes `provisioning`, so it both makes
    ///    every later request coalesce until the lease deadline instead of trying,
    ///    and convinces keep-warm the pool is already at `min_instances` so it
    ///    stops warming. The pool wedges permanently with nothing running.
    ///
    /// A Drop guard is the only correct shape here: it fires on success-with-error,
    /// on early return, AND on cancellation, which is exactly the set of ways a
    /// cold start can fail to produce an instance.
    async fn cold_start(self: &Arc<Self>, key: &str) -> anyhow::Result<(CellId, CellEndpoint)> {
        let mut guard = ColdStartGuard {
            fluid: self.clone(),
            key: key.to_string(),
            armed: true,
            error: None,
        };
        let r = self.cold_start_inner(key).await;
        match &r {
            // `cold_start_inner` already released the reservation and pushed the
            // instance on this path, so there is nothing for the guard to undo.
            Ok(_) => guard.armed = false,
            Err(e) => guard.error = Some(e.to_string()),
        }
        r
    }

    async fn cold_start_inner(
        self: &Arc<Self>,
        key: &str,
    ) -> anyhow::Result<(CellId, CellEndpoint)> {
        // Bound concurrent provisioning so a burst can't saturate the host. Held
        // for the whole provision+start; dropped when this fn returns.
        let _permit = self.cold_start_sem.clone().acquire_owned().await;
        let (image, launch, mem, vcpus, tenant) = {
            let reg = self.registry.lock();
            let pool = reg
                .get(key)
                .ok_or_else(|| anyhow::anyhow!("no such function '{key}'"))?;
            let port = free_port()?;
            // CONTAINER functions publish every declared UDP port spec on its own
            // loopback host port (`-p 127.0.0.1:<host>:<container>/udp`) so the
            // UDP relay has a local datagram leg — datagrams cannot ride the TCP
            // tunnel. Host ports are probed as real UDP binds; a failed probe
            // skips that spec LOUDLY (the app's other ports still serve) rather
            // than failing the whole cold start over an auxiliary publish.
            let udp_ports: Vec<hive_core::UdpPublish> = if pool
                .cfg
                .start_cmd
                .first()
                .map(String::as_str)
                == Some("__container__")
            {
                let mut seen = std::collections::BTreeSet::new();
                pool.cfg
                        .ports
                        .iter()
                        .filter(|s| s.protocol == fluid_core::ServiceProtocol::Udp && seen.insert(s.container_port))
                        .filter_map(|s| match free_udp_port() {
                            Ok(hp) => Some(hive_core::UdpPublish { container_port: s.container_port, host_port: hp }),
                            Err(e) => {
                                warn!(func = %key, container_port = s.container_port, error = %e, "no free loopback UDP host port; skipping this UDP publish");
                                None
                            }
                        })
                        .collect()
            } else {
                Vec::new()
            };
            let launch = FunctionLaunch {
                start_cmd: pool.cfg.start_cmd.clone(),
                env: pool.cfg.env.clone(),
                workdir: Some(pool.workdir.clone()),
                port,
                max_concurrency: pool.cfg.max_concurrency,
                // Carry the container memory/cpus/pids ceilings to the backend's
                // podman path (cpus/pids ignored for microVM/process functions).
                memory_mib: pool.cfg.memory_mib,
                cpus: pool.cfg.cpus,
                pids: pool.cfg.pids,
                // The single resolved runtime signal for this launch — explicit
                // config wins, else inferred from argv. Every backend/guest agent
                // reads THIS instead of re-sniffing start_cmd independently.
                runtime: hive_core::Runtime::resolve(&pool.cfg.runtime, &pool.cfg.start_cmd),
                // Non-HTTP protocol (gRPC/TCP/UDP): the backend must front this
                // function's local tunnel hop with a RAW byte splice, never the
                // HTTP-framed tunnel path (which would corrupt e.g. Postgres /
                // Minecraft wire bytes by HTTP-parsing them).
                raw_proxy: pool.cfg.needs_raw_proxy(),
                udp_ports,
                // Serverless GPU: the backend's podman path passes the host
                // GPUs through (CDI) when set.
                gpu: pool.cfg.gpu,
            };
            (
                pool.image.clone(),
                launch,
                pool.cfg.memory_mib,
                pool.cfg.vcpus,
                pool.tenant.clone(),
            )
        };

        // A CONTAINER deployment (`["__container__", image, port]`) is run via host
        // podman by the backend (mock OR firecracker), not as a microVM/process —
        // surface that on the CellSpec so the backend skips booting a microVM.
        let container =
            if launch.start_cmd.first().map(String::as_str) == Some("__container__") {
                Some(hive_backend::ContainerSpec {
                    image: launch.start_cmd.get(1).cloned().unwrap_or_default(),
                    // Single TCP entry (the common shape): container port from
                    // start_cmd[2], host port = the launch's assigned free port —
                    // same (container, host) pairing the backends build from
                    // `func.start_cmd`/`func.port` in their podman paths.
                    ports: {
                        let mut ports = vec![hive_backend::ContainerPort::tcp(
                            launch
                                .start_cmd
                                .get(2)
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(8080),
                            launch.port,
                        )];
                        // Declared UDP specs publish alongside the primary TCP port
                        // (same mapping the backends emit from `launch.udp_ports`).
                        ports.extend(launch.udp_ports.iter().map(|u| {
                            hive_backend::ContainerPort {
                                container_port: u.container_port,
                                host_port: u.host_port,
                                protocol: hive_backend::ContainerProtocol::Udp,
                            }
                        }));
                        ports
                    },
                })
            } else {
                None
            };

        let spec = CellSpec {
            id: CellId::new(),
            image,
            resources: ResourceSpec {
                vcpus: vcpus.max(1),
                mem_mib: mem,
                disk_mib: 1024,
                timeout_secs: 0,
            },
            tenant,
            container,
        };
        debug!(func = %key, cell = %spec.id, "cold-starting function instance");
        // Cold-start phase decomposition (#12): time provision (boot/microVM/container)
        // and start_function (runtime init + tunnel attach) separately so a slow cold
        // start is explainable, not a single opaque number.
        let t_prov = now_ms();
        let handle = self.backend.provision(&spec).await?;
        let provision_ms = now_ms().saturating_sub(t_prov);
        // If starting the function fails, the cell was already provisioned —
        // tear it back down so it doesn't leak (a leaked "Created" container
        // still holds a lock + process slot on the host).
        let t_run = now_ms();
        let endpoint = match self.backend.start_function(&handle, &launch).await {
            Ok(ep) => ep,
            Err(e) => {
                let _ = self.backend.terminate(&handle).await;
                return Err(e);
            }
        };

        let runtime_init_ms = now_ms().saturating_sub(t_run);
        let cell_id = spec.id.clone();
        let added = {
            let mut reg = self.registry.lock();
            if let Some(pool) = reg.get_mut(key) {
                pool.provisioning = pool.provisioning.saturating_sub(1);
                pool.last_provision_ms = provision_ms;
                pool.last_runtime_init_ms = runtime_init_ms;
                pool.instances.push(Instance {
                    cell_id: cell_id.clone(),
                    handle: handle.clone(),
                    endpoint: endpoint.clone(),
                    udp_ports: launch.udp_ports.clone(),
                    inflight: 1, // this lease
                    started_at_ms: now_ms(),
                    last_active_ms: now_ms(),
                    draining: false,
                    requests_served: 0,
                });
                true
            } else {
                false
            }
        };
        if !added {
            // Function was unregistered mid-start; tear the cell back down.
            let _ = self.backend.terminate(&handle).await;
            anyhow::bail!("function '{key}' unregistered during cold start");
        }
        Ok((cell_id, endpoint))
    }

    /// Called by `Lease::drop` to release a slot and record its active time.
    fn release(&self, key: &str, cell_id: &CellId, active_ms: u64) {
        let mut reg = self.registry.lock();
        if let Some(pool) = reg.get_mut(key) {
            pool.requests += 1;
            pool.served_ms_sum += active_ms;
            if let Some(inst) = pool.instances.iter_mut().find(|i| &i.cell_id == cell_id) {
                inst.inflight = inst.inflight.saturating_sub(1);
                inst.last_active_ms = now_ms();
                inst.requests_served += 1;
            }
        }
    }

    /// Called by `fluid-gateway::try_browser` on a successful browser-served
    /// response — the ONLY metering chokepoint for that path, since it never
    /// calls `lease()` and so never reaches `release()` above. No-op if the
    /// pool is unknown (a browser target can outlive the pool that would
    /// otherwise back it, e.g. before any fleet cold start has ever run).
    pub fn record_browser_request(&self, key: &str, active_ms: u64) {
        if let Some(pool) = self.registry.lock().get_mut(key) {
            pool.browser_requests += 1;
            pool.browser_served_ms += active_ms;
        }
    }

    /// Remove an instance the router/health-check found unreachable, terminate
    /// it, and account its time. Keep-warm/cold-start will replace it. This is
    /// our analogue of an instance `nack` → router drops it.
    pub async fn mark_dead(self: &Arc<Self>, key: &str, cell_id: &CellId) {
        let handle = {
            let mut reg = self.registry.lock();
            let Some(pool) = reg.get_mut(key) else { return };
            let Some(pos) = pool.instances.iter().position(|i| &i.cell_id == cell_id) else {
                return; // already removed (idempotent)
            };
            let inst = pool.instances.remove(pos);
            pool.dead_reaped += 1;
            pool.instance_ms_retired += now_ms().saturating_sub(inst.started_at_ms);
            inst.handle
        };
        let _ = self.backend.terminate(&handle).await;
    }

    /// Remove a function pool entirely and terminate all its instances. Used
    /// when a deployment is deleted.
    pub async fn unregister(self: &Arc<Self>, key: &str) {
        let handles: Vec<hive_backend::CellHandle> = {
            let mut reg = self.registry.lock();
            match reg.remove(key) {
                Some(pool) => pool.instances.into_iter().map(|i| i.handle).collect(),
                None => return,
            }
        };
        for h in handles {
            let _ = self.backend.terminate(&h).await;
        }
    }

    // ---- health checks ------------------------------------------------

    async fn health_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            // Probe idle instances only (don't disturb in-flight requests).
            let candidates: Vec<(String, CellId, CellEndpoint)> = {
                let reg = self.registry.lock();
                reg.iter()
                    .flat_map(|(k, p)| {
                        p.instances
                            .iter()
                            .filter(|i| !i.draining && i.inflight == 0)
                            .map(|i| (k.clone(), i.cell_id.clone(), i.endpoint.clone()))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            for (key, cell_id, endpoint) in candidates {
                if !probe(&endpoint).await {
                    warn!(func = %key, cell = %cell_id, "instance failed health check; reaping");
                    self.mark_dead(&key, &cell_id).await;
                }
            }
        }
    }

    // ---- autoscaler: keep-warm + scale-to-zero ------------------------

    async fn autoscaler_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.cfg.autoscaler_interval).await;
            self.clone().reconcile().await;
        }
    }

    async fn reconcile(self: Arc<Self>) {
        // Decide scale-up (keep-warm) and scale-down (idle) under the lock.
        let mut to_warm: Vec<String> = Vec::new();
        let mut to_drain: Vec<(String, CellId, hive_backend::CellHandle)> = Vec::new();
        let now = now_ms();
        {
            let mut reg = self.registry.lock();
            for (key, pool) in reg.iter_mut() {
                // Keep-warm: bring live count up to min_instances — unless this
                // pool is in failure backoff (its cold starts keep failing, so
                // don't hammer the host every tick).
                let live = pool.live_count();
                // Crash-loop detection, BEFORE the keep-warm decision below so an
                // armed backoff suppresses this very tick. Nothing in this loop
                // drains a pool below `min_instances` (scale-to-zero explicitly
                // keeps min; recycling needs an age/request count a seconds-old
                // instance cannot have), so an instance missing this soon after a
                // successful warm exited on its own.
                if live < pool.cfg.min_instances
                    && pool.last_warm_ok_ms != 0
                    && now.saturating_sub(pool.last_warm_ok_ms) < CRASH_LOOP_WINDOW_MS
                {
                    pool.crash_streak = pool.crash_streak.saturating_add(1);
                    pool.last_warm_ok_ms = 0; // count each death exactly once
                    let backoff_ms = 1000u64 << pool.crash_streak.min(6);
                    pool.warm_backoff_until_ms = now + backoff_ms;
                    warn!(
                        func = %key,
                        crash_streak = pool.crash_streak,
                        backoff_ms,
                        "instance exited seconds after a successful warm — backing off (crash loop)"
                    );
                } else if live >= pool.cfg.min_instances
                    && pool.last_warm_ok_ms != 0
                    && now.saturating_sub(pool.last_warm_ok_ms) >= CRASH_LOOP_WINDOW_MS
                {
                    // Stayed up past the window — genuinely healthy, close the circuit.
                    pool.crash_streak = 0;
                }
                let live = pool.live_count();
                if live < pool.cfg.min_instances && now >= pool.warm_backoff_until_ms {
                    for _ in 0..(pool.cfg.min_instances - live) {
                        pool.provisioning += 1;
                        to_warm.push(key.clone());
                    }
                }
                // Scale-to-zero: drain idle instances above min_instances.
                // Plus instance recycling (#13): retire instances that have served
                // too many requests / lived too long, GRACEFULLY — mark draining (no
                // new requests via decide_lease), terminate once idle; keep-warm
                // brings up a replacement next tick. Safe even for hot instances.
                let ttl_ms = pool.cfg.idle_ttl_secs * 1000;
                let max_req = self.cfg.max_requests_per_instance;
                let max_age_ms = self.cfg.max_instance_age_secs.saturating_mul(1000);
                let mut live_after = pool.instances.iter().filter(|i| !i.draining).count() as u32;
                let mut retired_add = 0u64;
                let mut recycled_add = 0u64;
                for inst in pool.instances.iter_mut() {
                    if inst.draining {
                        // Already draining (e.g. recycled while busy) — terminate once
                        // its in-flight requests have all completed.
                        if inst.inflight == 0 {
                            to_drain.push((key.clone(), inst.cell_id.clone(), inst.handle.clone()));
                        }
                        continue;
                    }
                    let age_ms = now.saturating_sub(inst.started_at_ms);
                    if instance_recyclable(inst.requests_served, age_ms, max_req, max_age_ms) {
                        inst.draining = true; // stop new requests now; terminate when idle
                        recycled_add += 1;
                        retired_add += age_ms;
                        if inst.inflight == 0 {
                            to_drain.push((key.clone(), inst.cell_id.clone(), inst.handle.clone()));
                        }
                        continue;
                    }
                    let idle =
                        inst.inflight == 0 && now.saturating_sub(inst.last_active_ms) > ttl_ms;
                    if idle && live_after > pool.cfg.min_instances {
                        inst.draining = true;
                        live_after -= 1;
                        retired_add += age_ms;
                        to_drain.push((key.clone(), inst.cell_id.clone(), inst.handle.clone()));
                    }
                }
                pool.instance_ms_retired += retired_add;
                pool.recycled += recycled_add;
            }
        }

        // Execute outside the lock.
        for key in to_warm {
            let f = self.clone();
            tokio::spawn(async move {
                match f.cold_start(&key).await {
                    Ok((cell_id, _)) => {
                        // Warm instance: it was created with inflight=1; reset to 0.
                        if let Some(pool) = f.registry.lock().get_mut(&key) {
                            if let Some(inst) =
                                pool.instances.iter_mut().find(|i| i.cell_id == cell_id)
                            {
                                inst.inflight = 0;
                                inst.last_active_ms = now_ms();
                            }
                            // The backend accepted the launch — clear the hard-failure
                            // backoff. `crash_streak` is deliberately NOT cleared here:
                            // Ok only means it STARTED, and clearing it on start is
                            // exactly what let a start-then-die container relaunch
                            // forever. Reconcile clears it once the instance has
                            // actually survived CRASH_LOOP_WINDOW_MS.
                            pool.warm_fail_streak = 0;
                            pool.warm_backoff_until_ms = 0;
                            pool.circuit_probe_after_ms = 0;
                            pool.last_warm_ok_ms = now_ms();
                        }
                        debug!(func = %key, "warm instance ready");
                    }
                    Err(e) => {
                        // Reservation release, backoff (2s, 4s, 8s … capped ~64s)
                        // and the circuit gate are all armed by `cold_start`'s Drop
                        // guard, so a pool that keeps failing stops storming no
                        // matter which caller drove the attempt.
                        debug!(func = %key, error = %e, "keep-warm cold start failed");
                    }
                }
            });
        }
        for (key, cell_id, handle) in to_drain {
            let f = self.clone();
            tokio::spawn(async move {
                let _ = f.backend.terminate(&handle).await;
                if let Some(pool) = f.registry.lock().get_mut(&key) {
                    pool.instances.retain(|i| i.cell_id != cell_id);
                }
                debug!(func = %key, cell = %cell_id, "instance scaled to zero");
            });
        }

        // ---- #2 adaptive concurrency (CPU-driven AIMD) --------------------
        // Sample the REAL CPU of each pool's live instances and AIMD-adjust the
        // per-instance safe-concurrency ceiling. Snapshot the handles under the
        // lock, then sample + apply OUTSIDE it (cpu_percent is async and must not
        // be awaited while holding the parking_lot registry lock).
        self.adapt_concurrency().await;
    }

    /// CPU-driven adaptive concurrency controller (#2). For every pool with live
    /// instances, take the MAX CPU across its instances (the most-pressured one)
    /// and step the safe-concurrency ceiling via [`aimd_step`]. A pool whose
    /// backend can't report CPU (all samples `None`) is left untouched — we never
    /// fall back to a latency proxy.
    async fn adapt_concurrency(&self) {
        // Snapshot (key, handles, current-ceiling, max) without holding the lock
        // across the async CPU sampling.
        let targets: Vec<(String, Vec<hive_backend::CellHandle>, u32, u32)> = {
            let reg = self.registry.lock();
            reg.iter()
                .filter_map(|(key, pool)| {
                    let handles: Vec<_> = pool
                        .instances
                        .iter()
                        .filter(|i| !i.draining)
                        .map(|i| i.handle.clone())
                        .collect();
                    if handles.is_empty() {
                        return None;
                    }
                    let current = pool.safe_concurrency.unwrap_or(pool.cfg.max_concurrency);
                    Some((key.clone(), handles, current, pool.cfg.max_concurrency))
                })
                .collect()
        };

        for (key, handles, current, max) in targets {
            let mut max_cpu = 0.0f32;
            let mut sampled = false;
            for h in &handles {
                if let Some(c) = self.backend.cpu_percent(h).await {
                    sampled = true;
                    if c > max_cpu {
                        max_cpu = c;
                    }
                }
            }
            if !sampled {
                continue; // backend gives no CPU signal → hold (never guess from latency)
            }
            let next = aimd_step(max_cpu, current, max);
            if let Some(pool) = self.registry.lock().get_mut(&key) {
                pool.last_cpu_pct = max_cpu;
                // `None` means "no dynamic cap" (full max); only store Some when we've
                // actually backed off below max.
                pool.safe_concurrency = if next >= pool.cfg.max_concurrency {
                    None
                } else {
                    Some(next)
                };
                if next < current {
                    debug!(func = %key, cpu = max_cpu, ceiling = next, "AIMD backoff (CPU pressure)");
                }
            }
        }
    }
}

/// Why a warm instance / pool could not immediately accept a request (a NACK).
/// Surfaced so the router can react (retry → scale-out → peer → region) and so the
/// reason is observable. (Per-instance CPU / memory / event-loop-lag / tunnel
/// reasons require runtime-emitted signals — see `set_safe_concurrency`, the hook
/// a pressure monitor lowers to express those as a reduced safe concurrency.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NackReason {
    /// Every live instance is at its safe concurrency ceiling.
    ConcurrencyLimit,
    /// The owning tenant is at its cross-pool instance quota.
    TenantQuota,
}

enum LeaseDecision {
    Ready {
        cell_id: CellId,
        endpoint: CellEndpoint,
    },
    ColdStart,
    /// Singleflight: a cold start is already in flight for this pool, so this request
    /// should briefly WAIT for it (and reuse it) instead of launching a duplicate —
    /// avoids a cold-start storm where N concurrent requests each boot an instance.
    WaitForWarm,
    Saturated(NackReason),
    NotFound,
}

/// Held for the duration of one request; releases the instance slot on drop.
pub struct Lease {
    fluid: Arc<Fluid>,
    key: String,
    cell_id: CellId,
    pub endpoint: CellEndpoint,
    started: tokio::time::Instant,
    released: bool,
}

impl Lease {
    pub fn cell_id(&self) -> &CellId {
        &self.cell_id
    }

    /// The loopback host port THIS lease's instance publishes `container_port`
    /// over UDP on (`127.0.0.1:<host_port>`, podman `-p …/udp`) — the local
    /// datagram leg the UDP relay sends to while holding the lease (so inflight
    /// accounting keeps the instance alive for the session). `None` when the
    /// instance publishes no such UDP port (non-container function, or the spec
    /// wasn't declared/published).
    pub fn udp_host_port(&self, container_port: u16) -> Option<u16> {
        let reg = self.fluid.registry.lock();
        reg.get(&self.key)?
            .instances
            .iter()
            .find(|i| i.cell_id == self.cell_id)?
            .udp_ports
            .iter()
            .find(|u| u.container_port == container_port)
            .map(|u| u.host_port)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            let active_ms = self.started.elapsed().as_millis() as u64;
            self.fluid.release(&self.key, &self.cell_id, active_ms);
        }
    }
}

/// True if we can open a connection to the instance endpoint within 1s.
async fn probe(endpoint: &CellEndpoint) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(1), connect_endpoint(endpoint)).await,
        Ok(Ok(_))
    )
}

/// Grab a free TCP port by binding to :0 and reading the assigned port.
fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Grab a free UDP port by binding a UDP socket to :0 — a port free for TCP is
/// not necessarily free for UDP, so UDP publishes get their own probe. (Same
/// probe-then-release TOCTOU window as [`free_port`]; podman's own bind fails
/// loudly in the rare race and keep-warm relaunches.)
fn free_udp_port() -> anyhow::Result<u16> {
    let s = std::net::UdpSocket::bind("127.0.0.1:0")?;
    Ok(s.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_backend::{CellHandle, CellSpec};
    use hive_core::{BuildJob, BuildResult, LogLine, ResourceSpec};

    // A no-op backend: the scheduler-logic tests exercise `decide_lease` /
    // `set_safe_concurrency` / counters, none of which invoke the backend, so these
    // methods are never actually called — they only satisfy the trait.
    struct StubBackend;
    #[async_trait::async_trait]
    impl hive_backend::CellBackend for StubBackend {
        fn name(&self) -> &'static str {
            "stub"
        }
        async fn provision(&self, _: &CellSpec) -> anyhow::Result<CellHandle> {
            anyhow::bail!("stub")
        }
        async fn run_build(
            &self,
            _: &CellHandle,
            _: &BuildJob,
            _: tokio::sync::mpsc::UnboundedSender<LogLine>,
        ) -> anyhow::Result<BuildResult> {
            anyhow::bail!("stub")
        }
        async fn start_function(
            &self,
            _: &CellHandle,
            _: &hive_backend::FunctionLaunch,
        ) -> anyhow::Result<CellEndpoint> {
            anyhow::bail!("stub")
        }
        async fn terminate(&self, _: &CellHandle) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn aimd_backs_off_under_cpu_and_recovers_with_headroom() {
        let max = 16;
        // CPU-bound: above HIGH halves the ceiling each step (multiplicative decrease).
        assert_eq!(aimd_step(95.0, 16, max), 8);
        assert_eq!(aimd_step(95.0, 8, max), 4);
        assert_eq!(aimd_step(95.0, 1, max), 1, "never below 1");
        // Headroom (low CPU) adds 1 each step (additive increase), capped at max.
        assert_eq!(aimd_step(10.0, 4, max), 5);
        assert_eq!(aimd_step(10.0, 16, max), 16, "clamped to max");
        // Hysteresis band: hold steady, no oscillation.
        assert_eq!(aimd_step(70.0, 8, max), 8);
        // Boundary semantics: exactly LOW recovers, exactly HIGH backs off.
        assert_eq!(aimd_step(AIMD_CPU_LOW, 8, max), 9);
        assert_eq!(aimd_step(AIMD_CPU_HIGH, 8, max), 4);
    }

    #[test]
    fn max_concurrent_cold_starts_env_override_and_clamping() {
        // One test, not two: HIVE_MAX_COLD_STARTS is process-global, and cargo
        // runs tests on multiple threads within the same process by default —
        // two separate #[test]s mutating the same var would race each other.
        std::env::set_var("HIVE_MAX_COLD_STARTS", "7");
        assert_eq!(max_concurrent_cold_starts(), 7);

        std::env::set_var("HIVE_MAX_COLD_STARTS", "not-a-number");
        let n = max_concurrent_cold_starts();
        assert!(
            (4..=32).contains(&n),
            "must stay within the [4,32] host-scaled bounds, got {n}"
        );

        std::env::set_var("HIVE_MAX_COLD_STARTS", "0");
        let n = max_concurrent_cold_starts();
        assert!(
            (4..=32).contains(&n),
            "zero override must be rejected (would deadlock every cold start), got {n}"
        );

        std::env::remove_var("HIVE_MAX_COLD_STARTS");
        let n = max_concurrent_cold_starts();
        assert!(
            (4..=32).contains(&n),
            "no override must also stay within the host-scaled bounds, got {n}"
        );
    }

    // A backend that reports a fixed CPU% — drives the adapt_concurrency() loop
    // end-to-end without spinning a real process.
    struct CpuBackend {
        cpu: f32,
    }
    #[async_trait::async_trait]
    impl hive_backend::CellBackend for CpuBackend {
        fn name(&self) -> &'static str {
            "cpu-stub"
        }
        async fn provision(&self, _: &CellSpec) -> anyhow::Result<CellHandle> {
            anyhow::bail!("unused")
        }
        async fn run_build(
            &self,
            _: &CellHandle,
            _: &BuildJob,
            _: tokio::sync::mpsc::UnboundedSender<LogLine>,
        ) -> anyhow::Result<BuildResult> {
            anyhow::bail!("unused")
        }
        async fn start_function(
            &self,
            _: &CellHandle,
            _: &hive_backend::FunctionLaunch,
        ) -> anyhow::Result<CellEndpoint> {
            anyhow::bail!("unused")
        }
        async fn terminate(&self, _: &CellHandle) -> anyhow::Result<()> {
            Ok(())
        }
        async fn cpu_percent(&self, _: &CellHandle) -> Option<f32> {
            Some(self.cpu)
        }
    }

    fn fake_instance(id: &str) -> Instance {
        Instance {
            cell_id: CellId::from(id.to_string()),
            handle: CellHandle {
                id: CellId::from(id.to_string()),
                image: "img".into(),
                resources: ResourceSpec {
                    vcpus: 1,
                    mem_mib: 64,
                    disk_mib: 64,
                    timeout_secs: 0,
                },
                root: std::path::PathBuf::from("/tmp"),
                endpoint: None,
            },
            endpoint: CellEndpoint::Tcp("127.0.0.1:1".into()),
            udp_ports: Vec::new(),
            inflight: 0,
            started_at_ms: now_ms(),
            last_active_ms: now_ms(),
            draining: false,
            requests_served: 0,
        }
    }

    #[tokio::test]
    async fn adapt_concurrency_throttles_under_real_cpu_and_recovers() {
        // High-CPU backend → adapt_concurrency must back the ceiling off below max.
        let f = Fluid::start(
            Arc::new(CpuBackend { cpu: 95.0 }),
            FluidConfig {
                autoscaler_interval: Duration::from_secs(3600), // we drive reconcile manually
                ..Default::default()
            },
        );
        let key = "dpl/api";
        f.register(
            key.into(),
            FunctionConfig {
                max_concurrency: 16,
                ..Default::default()
            },
            "img".into(),
            ".".into(),
            "t".into(),
        );
        // Place a live instance so there's something to sample.
        f.registry
            .lock()
            .get_mut(key)
            .unwrap()
            .instances
            .push(fake_instance("c1"));

        // One AIMD step at 95% CPU: 16 -> 8.
        f.adapt_concurrency().await;
        {
            let reg = f.registry.lock();
            let p = reg.get(key).unwrap();
            assert_eq!(
                p.safe_concurrency,
                Some(8),
                "CPU pressure halves the ceiling"
            );
            assert!((p.last_cpu_pct - 95.0).abs() < 0.01, "cpu sample recorded");
        }
        // A second step keeps backing off: 8 -> 4.
        f.adapt_concurrency().await;
        assert_eq!(
            f.registry.lock().get(key).unwrap().safe_concurrency,
            Some(4)
        );

        // Now swap to an idle (low-CPU) backend and confirm recovery climbs back to
        // None (full max). Build a fresh Fluid since the backend is fixed at start.
        let f2 = Fluid::start(
            Arc::new(CpuBackend { cpu: 5.0 }),
            FluidConfig {
                autoscaler_interval: Duration::from_secs(3600),
                ..Default::default()
            },
        );
        f2.register(
            key.into(),
            FunctionConfig {
                max_concurrency: 4,
                ..Default::default()
            },
            "img".into(),
            ".".into(),
            "t".into(),
        );
        f2.registry
            .lock()
            .get_mut(key)
            .unwrap()
            .instances
            .push(fake_instance("c1"));
        // Seed a throttled ceiling, then let low CPU recover it.
        f2.set_safe_concurrency(key, Some(1));
        for _ in 0..6 {
            f2.adapt_concurrency().await;
        }
        assert_eq!(
            f2.registry.lock().get(key).unwrap().safe_concurrency,
            None,
            "low CPU recovers to full max"
        );
    }

    #[test]
    fn aimd_io_bound_function_keeps_full_concurrency() {
        // An I/O-bound function is busy (high inflight) but low CPU — it must KEEP
        // its full ceiling, which a latency-based controller would wrongly cut.
        let max = 32;
        let mut c = 1u32;
        for _ in 0..40 {
            c = aimd_step(5.0, c, max); // low CPU every tick
        }
        assert_eq!(
            c, max,
            "I/O-bound (low CPU) climbs to and holds full concurrency"
        );
    }

    fn fluid(max_per_tenant: u32) -> Arc<Fluid> {
        // autoscaler_interval huge so reconcile never fires mid-test.
        Fluid::start(
            Arc::new(StubBackend),
            FluidConfig {
                autoscaler_interval: Duration::from_secs(3600),
                lease_timeout: Duration::from_millis(40),
                max_instances_per_tenant: max_per_tenant,
                ..Default::default()
            },
        )
    }

    fn fcfg(max_concurrency: u32, max_instances: u32) -> FunctionConfig {
        FunctionConfig {
            max_concurrency,
            max_instances,
            min_instances: 0,
            ..Default::default()
        }
    }

    fn add_instance(f: &Fluid, key: &str, inflight: u32, draining: bool) -> CellId {
        let id = CellId::new();
        let mut reg = f.registry.lock();
        let p = reg.get_mut(key).unwrap();
        p.instances.push(Instance {
            cell_id: id.clone(),
            handle: CellHandle {
                id: id.clone(),
                image: "i".into(),
                resources: ResourceSpec {
                    vcpus: 1,
                    mem_mib: 64,
                    disk_mib: 64,
                    timeout_secs: 0,
                },
                root: std::path::PathBuf::from("/tmp"),
                endpoint: None,
            },
            endpoint: CellEndpoint::Tcp("127.0.0.1:1".into()),
            udp_ports: Vec::new(),
            inflight,
            started_at_ms: now_ms(),
            last_active_ms: now_ms(),
            draining,
            requests_served: 0,
        });
        id
    }

    fn decision(f: &Fluid, key: &str) -> LeaseDecision {
        // coalesce=false: exercise the raw scale-out decision (singleflight tested separately).
        f.decide_lease(key, false).unwrap()
    }

    #[tokio::test]
    async fn reuses_least_loaded_until_safe_ceiling_then_scales_out() {
        let f = fluid(0);
        f.register("k".into(), fcfg(10, 5), "i".into(), "/w".into(), "t".into());
        // one warm instance, 0 inflight → reuse
        add_instance(&f, "k", 0, false);
        assert!(matches!(decision(&f, "k"), LeaseDecision::Ready { .. }));
        // fill it to the concurrency ceiling → next lease must scale out (cold start)
        {
            let mut reg = f.registry.lock();
            reg.get_mut("k").unwrap().instances[0].inflight = 10;
        }
        assert!(matches!(decision(&f, "k"), LeaseDecision::ColdStart));
        assert_eq!(f.registry.lock().get("k").unwrap().scale_out_total, 1);
    }

    #[tokio::test]
    async fn safe_concurrency_hook_scales_out_earlier_for_cpu_pressure() {
        let f = fluid(0);
        f.register("k".into(), fcfg(20, 5), "i".into(), "/w".into(), "t".into());
        add_instance(&f, "k", 3, false); // 3 inflight, static ceiling 20 → would reuse
                                         // Pressure monitor lowers the safe ceiling to 3 (CPU-heavy / event-loop lag):
        f.set_safe_concurrency("k", Some(3));
        // Now the instance is AT the safe ceiling → scale out instead of piling on.
        assert!(matches!(decision(&f, "k"), LeaseDecision::ColdStart));
        // Raising it back to I/O-friendly concurrency reuses the warm instance.
        f.set_safe_concurrency("k", None);
        assert!(matches!(decision(&f, "k"), LeaseDecision::Ready { .. }));
    }

    #[tokio::test]
    async fn nack_concurrency_when_at_max_instances() {
        let f = fluid(0);
        f.register("k".into(), fcfg(1, 1), "i".into(), "/w".into(), "t".into());
        add_instance(&f, "k", 1, false); // the one allowed instance is full
        match decision(&f, "k") {
            LeaseDecision::Saturated(r) => assert_eq!(r, NackReason::ConcurrencyLimit),
            d => panic!("expected Saturated(ConcurrencyLimit), got {d:?}"),
        }
        assert_eq!(f.registry.lock().get("k").unwrap().nack_concurrency, 1);
    }

    #[tokio::test]
    async fn nack_tenant_quota() {
        let f = fluid(1); // tenant may hold 1 live instance total
        f.register(
            "a".into(),
            fcfg(1, 5),
            "i".into(),
            "/w".into(),
            "team".into(),
        );
        f.register(
            "b".into(),
            fcfg(1, 5),
            "i".into(),
            "/w".into(),
            "team".into(),
        );
        add_instance(&f, "a", 1, false); // team now has 1 live instance (its quota)
                                         // b wants to cold-start a 2nd instance for the same tenant → quota NACK
        match decision(&f, "b") {
            LeaseDecision::Saturated(r) => assert_eq!(r, NackReason::TenantQuota),
            d => panic!("expected Saturated(TenantQuota), got {d:?}"),
        }
        assert_eq!(f.registry.lock().get("b").unwrap().nack_quota, 1);
    }

    #[test]
    fn runtime_concurrency_policy() {
        // node / edge keep full concurrency (I/O-friendly event loop)
        assert_eq!(recommended_safe_concurrency("node", 50), 50);
        assert_eq!(recommended_safe_concurrency("edge", 50), 50);
        // python capped (GIL serializes CPU work) — but never above configured
        assert_eq!(recommended_safe_concurrency("python", 50), 8);
        assert_eq!(recommended_safe_concurrency("python", 4), 4);
        // container/microVM moderate cap
        assert_eq!(recommended_safe_concurrency("container", 50), 16);
        assert_eq!(recommended_safe_concurrency("microvm", 10), 10);
        // unknown trusts configured; never returns 0
        assert_eq!(recommended_safe_concurrency("brainfuck", 7), 7);
        assert_eq!(recommended_safe_concurrency("python", 0), 1);
    }

    #[tokio::test]
    async fn register_applies_runtime_baseline() {
        let f = fluid(0);
        let mut c = fcfg(50, 5);
        c.runtime = "python".into();
        f.register("py".into(), c, "i".into(), "/w".into(), "t".into());
        // python's safe baseline is capped to 8 even though max_concurrency=50
        add_instance(&f, "py", 8, false); // at the safe ceiling
        assert!(matches!(
            f.decide_lease("py", false).unwrap(),
            LeaseDecision::ColdStart
        ));
        // a node function with the same config keeps full concurrency (no cold start at 8)
        let mut c2 = fcfg(50, 5);
        c2.runtime = "node".into();
        f.register("nd".into(), c2, "i".into(), "/w".into(), "t".into());
        add_instance(&f, "nd", 8, false);
        assert!(matches!(
            f.decide_lease("nd", false).unwrap(),
            LeaseDecision::Ready { .. }
        ));
    }

    #[test]
    fn instance_recycling_policy() {
        // request-count based
        assert!(instance_recyclable(10_000, 0, 10_000, 0));
        assert!(!instance_recyclable(9_999, 0, 10_000, 0));
        // age based (only once it has served ≥1 request)
        assert!(instance_recyclable(1, 3_600_000, 0, 3_600_000));
        assert!(!instance_recyclable(0, 3_600_000, 0, 3_600_000)); // fresh keep-warm, never served
                                                                   // disabled (0 = unlimited)
        assert!(!instance_recyclable(1_000_000, 9_999_999, 0, 0));
    }

    #[tokio::test]
    async fn singleflight_coalesces_onto_inflight_cold_start() {
        let f = fluid(0);
        f.register("k".into(), fcfg(10, 5), "i".into(), "/w".into(), "t".into());
        // A cold start is already in flight (e.g. the first request, or keep-warm).
        f.registry.lock().get_mut("k").unwrap().provisioning = 1;
        // With coalescing, a concurrent request WAITS for it instead of booting another.
        assert!(matches!(
            f.decide_lease("k", true).unwrap(),
            LeaseDecision::WaitForWarm
        ));
        // Once the coalesce window elapses (coalesce=false), it may scale out its own.
        assert!(matches!(
            f.decide_lease("k", false).unwrap(),
            LeaseDecision::ColdStart
        ));
    }

    #[tokio::test]
    async fn draining_instances_are_never_reused() {
        let f = fluid(0);
        f.register("k".into(), fcfg(10, 1), "i".into(), "/w".into(), "t".into());
        add_instance(&f, "k", 0, true); // draining, idle — must NOT be reused
                                        // at max_instances (1, the draining one counts as live? live_count excludes
                                        // draining) → it can cold-start a replacement rather than reuse the drainer.
        assert!(matches!(decision(&f, "k"), LeaseDecision::ColdStart));
    }

    // Add a Debug impl for LeaseDecision so the panic messages above compile.
    impl std::fmt::Debug for LeaseDecision {
        fn fmt(&self, w: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                LeaseDecision::Ready { .. } => write!(w, "Ready"),
                LeaseDecision::ColdStart => write!(w, "ColdStart"),
                LeaseDecision::WaitForWarm => write!(w, "WaitForWarm"),
                LeaseDecision::Saturated(r) => write!(w, "Saturated({r:?})"),
                LeaseDecision::NotFound => write!(w, "NotFound"),
            }
        }
    }
}
