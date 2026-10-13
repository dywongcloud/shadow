//! The Fluid pool ("serverless servers"): per function, a pool of long-lived instances with
//! in-function concurrency (`max_concurrency`), least-loaded reuse before provisioning, cold start
//! only on saturation (up to `max_instances`), keep-warm (`min_instances`), scale-to-zero after
//! `idle_ttl`, `waitUntil` lease holds, and active-CPU + provisioned-memory metering. One
//! `parking_lot::Mutex` over the registry, never held across an `.await`.

use fluid_core::FunctionConfig;
use hive_backend::{
    connect_endpoint, CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch,
    RuntimeArtifactPaths, SealedRuntimeArtifact,
};
use hive_core::{now_ms, CellId, ResourceSpec};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// A single running function instance (one cell).
struct Instance {
    cell_id: CellId,
    handle: hive_backend::CellHandle,
    endpoint: CellEndpoint,
    /// Loopback UDP publishes of this instance's container (container_port → host_port), chosen at
    /// cold start — the surface the UDP relay resolves its LOCAL datagram leg from ([`Lease::udp_host_port`]).
    udp_ports: Vec<hive_core::UdpPublish>,
    /// TCP twin of `udp_ports` (includes the primary; see `TcpPublish`).
    tcp_ports: Vec<hive_core::TcpPublish>,
    inflight: u32,
    started_at_ms: u64,
    last_active_ms: u64,
    draining: bool,
    /// Lifetime request count — drives request-based recycling, so a long-lived warm instance is
    /// recycled to bound memory drift without a runtime heap signal.
    requests_served: u64,
}

/// Is this instance due for recycling? Pure policy over lifetime request count and wall-clock age.
fn instance_recyclable(
    requests_served: u64,
    age_ms: u64,
    max_requests: u64,
    max_age_ms: u64,
) -> bool {
    (max_requests > 0 && requests_served >= max_requests)
        || (max_age_ms > 0 && requests_served > 0 && age_ms >= max_age_ms)
}

/// Complete launch authority for one function pool, built before [`Fluid::register_many`] takes the
/// registry lock so a malformed batch cannot leave a partially registered deployment.
#[derive(Clone, Debug)]
pub struct PoolSpec {
    pub key: String,
    pub cfg: FunctionConfig,
    pub image: String,
    pub workdir: String,
    pub tenant: String,
}

/// Everything needed to (re)launch instances of one function.
struct FunctionPool {
    cfg: FunctionConfig,
    /// Owning team/tenant (normalized; empty => "personal"); every cell this pool spawns is tagged with
    /// it, and per-tenant instance quotas sum across pools sharing it.
    tenant: String,
    /// Image / rootfs name for cells of this function.
    image: String,
    /// Working dir for the function process (host path for mock backend).
    workdir: String,
    instances: Vec<Instance>,
    /// Cold starts currently in flight (counts toward max_instances).
    provisioning: u32,
    /// Total requests served.
    requests: u64,
    /// Sum of per-request active durations. This is what *traditional* 1:1
    /// serverless would bill (one dedicated instance per concurrent request).
    served_ms_sum: u64,
    /// Alive-time of instances already scaled down; with live instances' alive-time, what Fluid bills
    /// (instance-time shared across concurrent requests).
    instance_ms_retired: u64,
    /// Instances reaped because they became unreachable (health/nack).
    dead_reaped: u64,
    /// Requests served by a donated browser node instead of fleet compute (`try_browser`'s success
    /// path never calls `lease()`, so without this counter browser traffic is invisible to billing).
    browser_requests: u64,
    /// Sum of per-request active durations for browser-served requests, kept apart from `served_ms_sum`.
    browser_served_ms: u64,
    /// Keep-warm failure backoff: the autoscaler will not warm this pool again before this, so failing
    /// cold starts cannot retry every tick and pin the host.
    warm_backoff_until_ms: u64,
    /// Consecutive keep-warm failures; drives the exponential backoff above.
    warm_fail_streak: u32,
    /// `now_ms()` of the last keep-warm cold start that RETURNED Ok — the backend accepted the launch,
    /// nothing more; paired with `crash_streak` to tell "warmed and healthy" from "died seconds later".
    last_warm_ok_ms: u64,
    /// Consecutive times an instance this pool successfully warmed VANISHED inside
    /// `CRASH_LOOP_WINDOW_MS` — the crash-loop signal `warm_fail_streak` cannot see (a container that
    /// starts fine and exits takes the `Ok` branch).
    crash_streak: u32,
    /// Circuit half-open gate: under this, a pool past its failure threshold fails leases FAST; past it,
    /// exactly one request is let through as a probe. SEPARATE from `warm_backoff_until_ms`, whose
    /// seconds-scale backoff is shorter than a failing cold start — the gate must outlast the failure.
    circuit_probe_after_ms: u64,
    /// The `hive_core::fault` marker of the most recent cold-start failure when it was a NODE fault
    /// (missing rootfs/kernel, no hypervisor, exhausted lock pool), carried through the circuit bail so
    /// the user-visible code names the node, not their app. Cleared by any successful start.
    last_node_fault: Option<&'static str>,
    /// Instances removed because the request path proved they STOPPED ANSWERING during the current
    /// sustained-failure run — the health signal that opens the circuit while instances are still
    /// PRESENT ([`UNHEALTHY_CIRCUIT_THRESHOLD`]).
    unhealthy_reaps: u32,
    /// `now_ms()` of the most recent such removal; the run's window anchor
    /// (see [`UNHEALTHY_REAP_WINDOW_MS`]).
    unhealthy_reap_ms: u64,
    /// Dynamic per-instance safe-concurrency ceiling (`None` = `cfg.max_concurrency`), LOWERED by the
    /// pressure monitor so the scheduler scales out earlier; effective reuse limit is the min of the two.
    safe_concurrency: Option<u32>,
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
    /// Last cold start's backend `provision` time (boot/microVM/container) in ms.
    last_provision_ms: u64,
    /// Last cold start's `start_function` time (runtime init + tunnel attach) in ms.
    last_runtime_init_ms: u64,
    /// Most recent CPU sample (% of allocated vCPU, max across live instances) that drove the adaptive
    /// controller; 0 when never sampled.
    last_cpu_pct: f32,
}

impl FunctionPool {
    fn from_spec(spec: PoolSpec) -> Self {
        let recommended = recommended_safe_concurrency(&spec.cfg.runtime, spec.cfg.max_concurrency);
        let safe_concurrency = (recommended < spec.cfg.max_concurrency).then_some(recommended);
        Self {
            cfg: spec.cfg,
            tenant: norm_tenant(spec.tenant),
            image: spec.image,
            workdir: spec.workdir,
            instances: Vec::new(),
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
            unhealthy_reaps: 0,
            unhealthy_reap_ms: 0,
            last_node_fault: None,
            safe_concurrency,
            nack_concurrency: 0,
            nack_quota: 0,
            scale_out_total: 0,
            coldstart_deduped: 0,
            recycled: 0,
            last_provision_ms: 0,
            last_runtime_init_ms: 0,
            last_cpu_pct: 0.0,
        }
    }

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
    pub requests: u64,
    /// What traditional 1:1 serverless would bill (instance-seconds).
    pub traditional_ms: u64,
    /// What Fluid bills (shared instance-time).
    pub fluid_ms: u64,
    /// Savings vs traditional, 0.0–1.0 (the "up to 85%" story).
    pub savings_pct: f64,
    /// Instances reaped as unhealthy/unreachable and replaced.
    pub dead_reaped: u64,
    /// Of those, how many were reaped because the instance was REGISTERED but STOPPED ANSWERING in the
    /// current run; at or above `UNHEALTHY_CIRCUIT_THRESHOLD` the circuit is open with instances present.
    #[serde(default)]
    pub unhealthy_reaps: u32,
    /// Requests served by a donated browser node instead of fleet compute.
    #[serde(default)]
    pub browser_requests: u64,
    /// Sum of active durations for browser-served requests.
    #[serde(default)]
    pub browser_ms: u64,
    /// Active CPU time across all requests (ms) — the time instances were actually
    /// processing, excluding idle keep-warm time.
    pub active_cpu_ms: u64,
    /// Provisioned memory consumed, in GB-hours: Σ (instance memory GB × alive hrs).
    pub memory_gb_hrs: f64,
    /// Additional savings of Active-CPU billing vs paying for full instance-time
    /// (fluid_ms), 0.0–1.0 — the "Active CPU pricing" story on top of multiplexing.
    pub active_cpu_savings_pct: f64,
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
    /// Max total live instances ONE tenant may hold across all of its pools (0 = unlimited); over-quota
    /// leases get backpressure (Saturated), never a cross-tenant eviction.
    pub max_instances_per_tenant: u32,
    /// Recycle a warm instance after this many requests (0 = never), gracefully (drain, keep-warm replaces).
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
            max_requests_per_instance: 10_000,
            max_instance_age_secs: 3_600,
        }
    }
}

const SHUTDOWN_RUNNING: u8 = 0;
const SHUTDOWN_CLOSING: u8 = 1;
const SHUTDOWN_COMPLETE: u8 = 2;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

type TerminationResult = Result<(), String>;

struct PendingTermination {
    handle: CellHandle,
    /// Integrity-chain identity for this cell when the caller had it; `None` only means no
    /// `ExecutionEnded` entry is recorded for that one termination.
    execution: Option<ExecutionEndContext>,
    in_progress: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<TerminationResult>>,
}

/// What `ExecutionObserver::on_ended` needs, carried with a `CellHandle` from the call site that
/// knows it to the one owned termination worker.
#[derive(Clone)]
struct ExecutionEndContext {
    deployment_id: String,
    requests_served: u64,
    /// "drained" | "crashed" | "killed" — the caller's own classification, decided at the scheduling
    /// call site, never re-derived from the termination outcome.
    outcome: String,
}

pub struct Fluid {
    backend: Arc<dyn CellBackend>,
    /// Records cell started/ended into the tamper-evidence chain (`hive_core::integrity`); injected
    /// because `fluid-compute` must never depend on `hive-cloud`, and a `OnceLock` because hive-cloud
    /// constructs `Fluid` BEFORE the `deployment_ledger` that backs the real observer.
    execution_observer: std::sync::OnceLock<Arc<dyn hive_core::ExecutionObserver>>,
    cfg: FluidConfig,
    registry: Mutex<HashMap<String, FunctionPool>>,
    /// Bounds how many cold starts run concurrently across ALL pools, so a keep-warm burst or traffic
    /// spike cannot saturate the host's process table / container lock pool.
    cold_start_sem: Arc<tokio::sync::Semaphore>,
    cold_start_capacity: u32,
    shutdown_state: AtomicU8,
    shutdown_result: Mutex<Option<Result<usize, String>>>,
    shutdown_notify: tokio::sync::Notify,
    background_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// A cell removed from the registry stays here until the one owned termination worker finishes
    /// backend teardown; the authoritative owner shutdown drains after joining that worker.
    terminating: Mutex<HashMap<CellId, PendingTermination>>,
    termination_notify: tokio::sync::Notify,
    termination_closing: AtomicBool,
    termination_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Max cold starts in flight across the whole node (a HOST-pressure bound, not per-function
/// throughput): scales with core count, floored at 4 and capped at 32; `HIVE_MAX_COLD_STARTS`
/// overrides both ends.
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

/// Runtime-aware initial safe-concurrency baseline: node/bun/edge keep the configured value (I/O
/// event loops), python is capped (the GIL), container/microVM moderate, wasm/unknown trusted. Only
/// ever LOWERS toward a runtime ceiling, never above the configured `max_concurrency`.
pub fn recommended_safe_concurrency(runtime: &str, configured_max: u32) -> u32 {
    let cap = match runtime.trim().to_ascii_lowercase().as_str() {
        "node" | "nodejs" | "js" | "bun" | "edge" | "isolate" | "edge-isolate" => configured_max,
        "python" | "py" => configured_max.min(8),
        "container" | "docker" | "microvm" | "firecracker" => configured_max.min(16),
        "wasm" | "wasi" | "wasmer" | "wasix" => configured_max,
        _ => configured_max,
    };
    cap.max(1)
}

struct UnpublishedCell {
    fluid: Arc<Fluid>,
    handle: Option<CellHandle>,
}

impl UnpublishedCell {
    fn new(fluid: Arc<Fluid>, handle: CellHandle) -> Self {
        Self {
            fluid,
            handle: Some(handle),
        }
    }

    fn publish(&mut self) {
        self.handle = None;
    }
}

impl Drop for UnpublishedCell {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        self.fluid.schedule_termination(handle);
    }
}

/// Releases a cold start's `provisioning` reservation and records the failure if no instance was
/// produced — including when the future is DROPPED because the caller gave up.
struct ColdStartGuard {
    fluid: Arc<Fluid>,
    key: String,
    /// Cleared on success; while armed, Drop treats this as a failed cold start.
    armed: bool,
    /// `None` while armed means the future was cancelled rather than returning an error — an abandoned
    /// cold start points at a caller deadline, not a broken deployment.
    error: Option<String>,
}

impl Drop for ColdStartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let node_fault = self.error.as_deref().and_then(node_fault_marker);
        let abandoned = self.error.is_none();
        let mut streak = 0u32;
        if let Some(p) = self.fluid.registry.lock().get_mut(&self.key) {
            p.provisioning = p.provisioning.saturating_sub(1);
            if !abandoned {
                p.warm_fail_streak = p.warm_fail_streak.saturating_add(1);
                p.circuit_probe_after_ms = now_ms() + CIRCUIT_PROBE_INTERVAL_MS;
            }
            p.warm_backoff_until_ms = now_ms() + (1000u64 << p.warm_fail_streak.min(6));
            streak = p.warm_fail_streak;
            if let Some(marker) = node_fault {
                p.last_node_fault = Some(marker);
            } else if self.error.is_some() {
                p.last_node_fault = None;
            }
        }
        warn!(
            func = %self.key,
            fail_streak = streak,
            abandoned = self.error.is_none(),
            node_fault = node_fault.unwrap_or(""),
            error = self.error.as_deref().unwrap_or("caller gave up before the cold start finished"),
            "cold start failed"
        );
    }
}

/// The `hive_core::fault` marker a backend error carries, if any — how a NODE fault survives the
/// lease path's `anyhow` string; kept in step with `fluid-gateway::classify_lease_error`.
fn node_fault_marker(es: &str) -> Option<&'static str> {
    for marker in [
        hive_core::fault::NODE_IMAGE_MISSING,
        hive_core::fault::NODE_BACKEND_UNAVAILABLE,
        hive_core::fault::NODE_LOCK_POOL_EXHAUSTED,
        hive_core::fault::NODE_RUNTIME_MISSING,
    ] {
        if es.contains(marker) {
            return Some(marker);
        }
    }
    None
}

/// CPU thresholds (% of a cell's vCPU budget) for the adaptive concurrency controller: above HIGH
/// back off, below LOW recover, the band between is hysteresis.
pub const AIMD_CPU_HIGH: f32 = 85.0;
pub const AIMD_CPU_LOW: f32 = 60.0;

/// An instance that disappears within this long of a SUCCESSFUL warm exited on its own; wide enough
/// for a container dying on its entrypoint without mistaking an idle-drain for a crash.
pub const CRASH_LOOP_WINDOW_MS: u64 = 30_000;
/// Consecutive crash-loop deaths after which the pool's circuit OPENS and leases
/// fail fast instead of cold-starting into the same immediate death.
pub const CRASH_CIRCUIT_THRESHOLD: u32 = 3;
/// How long an open circuit stays open before ONE probe request is let through; must comfortably
/// exceed a failing cold start (~20 s observed).
pub const CIRCUIT_PROBE_INTERVAL_MS: u64 = 30_000;

/// Consecutive INSTANCE failures within [`UNHEALTHY_REAP_WINDOW_MS`] after which the circuit OPENS
/// although instances are PRESENT — health, not count, decides this arm.
pub const UNHEALTHY_CIRCUIT_THRESHOLD: u32 = 3;
/// How far apart two instance failures may be and still count as ONE sustained run.
pub const UNHEALTHY_REAP_WINDOW_MS: u64 = 300_000;
/// How long the unhealthy-instance circuit stays open: ~300 s worst case for one probe (the tunnel's
/// `LOCAL_RESPONSE_DEADLINE` × up to 5 attempts), and re-armed when the probe lease is handed out.
pub const UNHEALTHY_CIRCUIT_PROBE_MS: u64 = 300_000;

/// One AIMD step for a pool's per-instance safe-concurrency ceiling from the most-pressured
/// instance's CPU: at/above HIGH halve, at/below LOW +1, between hold; clamped to `[1, max]`.
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

/// Outcome of a speculative (request-less) warm attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmVerdict {
    /// A cold start was admitted and completed; one instance is up.
    Started,
    /// Nothing to do: an instance already exists or a start is in flight.
    AlreadyWarm,
    /// Refused without side effects (no reservation taken).
    Refused,
}

impl Fluid {
    pub fn start(backend: Arc<dyn CellBackend>, cfg: FluidConfig) -> Arc<Fluid> {
        let cold_start_capacity = max_concurrent_cold_starts();
        let fluid = Arc::new(Fluid {
            backend,
            execution_observer: std::sync::OnceLock::new(),
            cfg,
            registry: Mutex::new(HashMap::new()),
            cold_start_sem: Arc::new(tokio::sync::Semaphore::new(cold_start_capacity)),
            cold_start_capacity: cold_start_capacity as u32,
            shutdown_state: AtomicU8::new(SHUTDOWN_RUNNING),
            shutdown_result: Mutex::new(None),
            shutdown_notify: tokio::sync::Notify::new(),
            background_tasks: Mutex::new(Vec::with_capacity(2)),
            terminating: Mutex::new(HashMap::new()),
            termination_notify: tokio::sync::Notify::new(),
            termination_closing: AtomicBool::new(false),
            termination_task: Mutex::new(None),
        });
        let f = fluid.clone();
        let autoscaler = tokio::spawn(async move { f.autoscaler_loop().await });
        let h = fluid.clone();
        let health = tokio::spawn(async move { h.health_loop().await });
        fluid.background_tasks.lock().extend([autoscaler, health]);
        let t = fluid.clone();
        *fluid.termination_task.lock() = Some(tokio::spawn(async move {
            t.termination_loop().await;
        }));
        fluid
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown_state.load(Ordering::Acquire) != SHUTDOWN_RUNNING
    }

    /// Late-bind the integrity-chain observer once hive-cloud's `deployment_ledger` exists; idempotent.
    pub fn set_execution_observer(&self, observer: Arc<dyn hive_core::ExecutionObserver>) {
        let _ = self.execution_observer.set(observer);
    }

    fn enqueue_termination(
        &self,
        handle: CellHandle,
        execution: Option<ExecutionEndContext>,
        wait: bool,
    ) -> Option<tokio::sync::oneshot::Receiver<TerminationResult>> {
        let id = handle.id.clone();
        let (waiter, receiver) = if wait {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let mut pending = self.terminating.lock();
        match pending.entry(id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if let Some(waiter) = waiter {
                    entry.get_mut().waiters.push(waiter);
                }
                if entry.get().execution.is_none() {
                    entry.get_mut().execution = execution;
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(PendingTermination {
                    handle,
                    execution,
                    in_progress: false,
                    waiters: waiter.into_iter().collect(),
                });
            }
        }
        drop(pending);
        self.termination_notify.notify_one();
        receiver
    }

    fn schedule_termination(&self, handle: CellHandle) {
        self.enqueue_termination(handle, None, false);
    }

    async fn termination_loop(self: Arc<Self>) {
        loop {
            self.termination_notify.notified().await;
            if self.termination_closing.load(Ordering::Acquire) {
                return;
            }
            loop {
                let next = {
                    let mut pending = self.terminating.lock();
                    let id = pending
                        .iter()
                        .filter(|(_, entry)| !entry.in_progress)
                        .min_by_key(|(id, _)| id.to_string())
                        .map(|(id, _)| id.clone());
                    id.and_then(|id| {
                        let entry = pending.get_mut(&id)?;
                        entry.in_progress = true;
                        Some((id, entry.handle.clone()))
                    })
                };
                let Some((id, handle)) = next else {
                    break;
                };

                let outcome = self
                    .backend
                    .terminate(&handle)
                    .await
                    .map_err(|error| format!("{error:#}"));
                let (waiters, execution) = {
                    let mut pending = self.terminating.lock();
                    let waiters = pending
                        .get_mut(&id)
                        .map(|entry| {
                            entry.in_progress = false;
                            std::mem::take(&mut entry.waiters)
                        })
                        .unwrap_or_default();
                    let execution = if outcome.is_ok() {
                        pending.remove(&id).and_then(|entry| entry.execution)
                    } else {
                        None
                    };
                    (waiters, execution)
                };
                for waiter in waiters {
                    let _ = waiter.send(outcome.clone());
                }
                if let (true, Some(observer), Some(ctx)) =
                    (outcome.is_ok(), self.execution_observer.get(), &execution)
                {
                    observer.on_ended(
                        &ctx.deployment_id,
                        id.as_str(),
                        ctx.requests_served,
                        &ctx.outcome,
                    );
                }
                if let Err(error) = outcome {
                    warn!(cell = %id, %error, "backend cell termination failed; shutdown retains it for retry");
                    break;
                }
                if self.termination_closing.load(Ordering::Acquire) {
                    return;
                }
            }
        }
    }

    async fn terminate_tracked(
        self: &Arc<Self>,
        handle: CellHandle,
        execution: Option<ExecutionEndContext>,
    ) -> anyhow::Result<()> {
        let receiver = self
            .enqueue_termination(handle, execution, true)
            .expect("tracked termination creates one waiter");
        match receiver.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow::anyhow!(error)),
            Err(_) => {
                anyhow::bail!("Fluid termination worker stopped before acknowledging an owned cell")
            }
        }
    }

    async fn stop_termination_worker(&self, deadline: tokio::time::Instant) -> anyhow::Result<()> {
        self.termination_closing.store(true, Ordering::Release);
        self.termination_notify.notify_waiters();
        let Some(mut task) = self.termination_task.lock().take() else {
            return Ok(());
        };
        match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow::anyhow!(
                "Fluid termination worker failed before shutdown drain: {error}"
            )),
            Err(_) => {
                task.abort();
                let _ = task.await;
                anyhow::bail!("Fluid termination worker did not stop before shutdown deadline")
            }
        }
    }

    async fn shutdown_inner(self: &Arc<Self>) -> anyhow::Result<usize> {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;

        let tasks = std::mem::take(&mut *self.background_tasks.lock());
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }

        let cold_start_barrier = tokio::time::timeout_at(
            deadline,
            self.cold_start_sem
                .clone()
                .acquire_many_owned(self.cold_start_capacity),
        )
        .await;
        let _cold_start_barrier = match cold_start_barrier {
            Ok(Ok(barrier)) => barrier,
            Ok(Err(_)) => {
                let _ = self.stop_termination_worker(deadline).await;
                anyhow::bail!("Fluid cold-start barrier closed during shutdown");
            }
            Err(_) => {
                let _ = self.stop_termination_worker(deadline).await;
                anyhow::bail!("Fluid shutdown timed out waiting for in-flight cold starts");
            }
        };

        let detached: Vec<CellHandle> = {
            let mut registry = self.registry.lock();
            std::mem::take(&mut *registry)
                .into_values()
                .flat_map(|pool| pool.instances.into_iter().map(|instance| instance.handle))
                .collect()
        };
        let owned = {
            let mut terminating = self.terminating.lock();
            let mut ids: HashSet<CellId> = terminating.keys().cloned().collect();
            for handle in detached {
                ids.insert(handle.id.clone());
                terminating
                    .entry(handle.id.clone())
                    .or_insert_with(|| PendingTermination {
                        handle,
                        execution: None,
                        in_progress: false,
                        waiters: Vec::new(),
                    });
            }
            ids.len()
        };

        self.stop_termination_worker(deadline).await?;

        let mut last_errors = Vec::new();
        for attempt in 0..2 {
            let handles: Vec<CellHandle> = self
                .terminating
                .lock()
                .values()
                .map(|entry| entry.handle.clone())
                .collect();
            if handles.is_empty() {
                return Ok(owned);
            }
            last_errors.clear();
            for handle in handles {
                let id = handle.id.clone();
                let outcome = tokio::time::timeout_at(deadline, self.backend.terminate(&handle))
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "Fluid shutdown timed out terminating {} owned cell(s)",
                            self.terminating.lock().len()
                        )
                    })?
                    .map_err(|error| format!("{error:#}"));
                let waiters = {
                    let mut terminating = self.terminating.lock();
                    let waiters = terminating
                        .get_mut(&id)
                        .map(|entry| std::mem::take(&mut entry.waiters))
                        .unwrap_or_default();
                    if outcome.is_ok() {
                        terminating.remove(&id);
                    }
                    waiters
                };
                for waiter in waiters {
                    let _ = waiter.send(outcome.clone());
                }
                if let Err(error) = outcome {
                    last_errors.push(format!("{id}: {error}"));
                }
            }
            if self.terminating.lock().is_empty() {
                return Ok(owned);
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        anyhow::bail!(
            "Fluid shutdown could not terminate {} owned cell(s): {}",
            self.terminating.lock().len(),
            last_errors.join("; ")
        )
    }

    /// Stop admission and background work, wait for every cold start to leave the provisioning
    /// boundary, then terminate every backend handle; concurrent and repeated callers observe the
    /// same terminal result.
    pub async fn shutdown(self: &Arc<Self>) -> anyhow::Result<usize> {
        if self
            .shutdown_state
            .compare_exchange(
                SHUTDOWN_RUNNING,
                SHUTDOWN_CLOSING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let fluid = self.clone();
            tokio::spawn(async move {
                let result = fluid
                    .shutdown_inner()
                    .await
                    .map_err(|error| format!("{error:#}"));
                *fluid.shutdown_result.lock() = Some(result);
                fluid
                    .shutdown_state
                    .store(SHUTDOWN_COMPLETE, Ordering::Release);
                fluid.shutdown_notify.notify_waiters();
            });
        }

        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT + Duration::from_secs(5);
        loop {
            let notified = self.shutdown_notify.notified();
            if self.shutdown_state.load(Ordering::Acquire) == SHUTDOWN_COMPLETE {
                break;
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .map_err(|_| anyhow::anyhow!("Fluid shutdown did not reach a terminal result"))?;
        }
        match self
            .shutdown_result
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Fluid shutdown completed without a result"))?
        {
            Ok(terminated) => Ok(terminated),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    }

    /// Register (or replace) one function pool. This compatibility wrapper uses
    /// the same atomic batch path as deployment registration.
    pub fn register(
        &self,
        key: String,
        cfg: FunctionConfig,
        image: String,
        workdir: String,
        tenant: String,
    ) {
        self.register_many(vec![PoolSpec {
            key,
            cfg,
            image,
            workdir,
            tenant,
        }])
        .expect("one function pool always has a unique non-empty key");
    }

    /// Validate and install a deployment's complete function-pool set under one
    /// registry lock. No pool becomes leasable until every key is known valid.
    pub fn register_many(&self, specs: Vec<PoolSpec>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.is_shutting_down(),
            "Fluid is shutting down; function-pool registration is closed"
        );
        let mut keys = HashSet::with_capacity(specs.len());
        for spec in &specs {
            anyhow::ensure!(!spec.key.is_empty(), "function pool key is empty");
            anyhow::ensure!(
                keys.insert(spec.key.clone()),
                "duplicate function pool key {:?}",
                spec.key
            );
        }
        let mut pools: Vec<(String, FunctionPool)> = specs
            .into_iter()
            .map(|spec| {
                let key = spec.key.clone();
                (key, FunctionPool::from_spec(spec))
            })
            .collect();

        let mut reg = self.registry.lock();
        anyhow::ensure!(
            !self.is_shutting_down(),
            "Fluid shutdown began before function-pool registration committed"
        );
        for (key, pool) in &mut pools {
            let (orphans, kept_warm) = match reg.get_mut(key) {
                Some(old)
                    if old.cfg == pool.cfg
                        && old.image == pool.image
                        && old.workdir == pool.workdir
                        && old.tenant == pool.tenant =>
                {
                    let n = old.instances.len();
                    (old.instances.drain(..).collect::<Vec<_>>(), n)
                }
                Some(old) => (
                    old.instances
                        .drain(..)
                        .map(|mut instance| {
                            instance.draining = true;
                            instance
                        })
                        .collect::<Vec<_>>(),
                    0,
                ),
                None => (Vec::new(), 0),
            };
            if kept_warm > 0 {
                tracing::debug!(
                    func = %key,
                    count = kept_warm,
                    "identical pool re-registered — keeping warm instances"
                );
            } else if !orphans.is_empty() {
                warn!(
                    func = %key,
                    count = orphans.len(),
                    "re-registering a pool with live instances — draining them instead of orphaning"
                );
            }
            pool.instances = orphans;
        }
        for (key, pool) in pools {
            reg.insert(key, pool);
        }
        Ok(())
    }

    /// Set a pool's dynamic safe-concurrency ceiling (the pressure hook); clamped to `[1,
    /// cfg.max_concurrency]`, `None` restores the static ceiling, no-op if unknown.
    pub fn set_safe_concurrency(&self, key: &str, n: Option<u32>) {
        if let Some(p) = self.registry.lock().get_mut(key) {
            p.safe_concurrency = n.map(|v| v.clamp(1, p.cfg.max_concurrency));
        }
    }

    /// Replace an existing pool's function config IN PLACE, keeping its live instances (the
    /// no-redeploy settings edit via `Gateway::update_manifest`); `register` would replace the whole
    /// pool and orphan every running cell. Applies to FUTURE launches; recomputes the runtime baseline.
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

    /// Set a pool's keep-warm floor; 0 lets the autoscaler drain it to zero (superseded deployments)
    /// while a cold start stays possible. No-op if unknown.
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

    /// Derive both serving locations from one sealed descriptor: static files use the verified host
    /// application root, function launch the backend-specific cwd.
    pub fn runtime_artifact_paths(
        &self,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<RuntimeArtifactPaths> {
        let host_static_root = artifact.host_app_root()?;
        let guest_workdir = self
            .backend
            .delivered_workdir(artifact)?
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| host_static_root.clone());
        Ok(RuntimeArtifactPaths {
            host_static_root,
            guest_workdir,
            delivery_required: true,
        })
    }

    pub fn delivered_workdir(
        &self,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<Option<String>> {
        self.backend.delivered_workdir(artifact)
    }

    /// Make the exact sealed artifact available to serving cells.
    pub async fn deliver_build(
        &self,
        image: &str,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<()> {
        self.backend.deliver_build(image, artifact).await
    }

    /// Re-read the backend's committed artifact receipt. Delivery callers use
    /// this to bind the launch record to the exact bytes the backend accepted.
    pub async fn runtime_artifact_identity(
        &self,
        image: &str,
    ) -> anyhow::Result<Option<hive_core::RuntimeArtifactIdentity>> {
        self.backend.runtime_artifact_identity(image).await
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
                let active_cpu_ms = p.served_ms_sum;
                let memory_gb_hrs =
                    (p.cfg.memory_mib as f64 / 1024.0) * (fluid_ms as f64 / 3_600_000.0);
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
                    unhealthy_reaps: p.unhealthy_reaps,
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
        anyhow::ensure!(
            !self.is_shutting_down(),
            "Fluid is shutting down; new function leases are closed"
        );
        if let Some(p) = self.registry.lock().get_mut(key) {
            if p.warm_fail_streak > 0 || p.crash_streak > 0 || p.unhealthy_reaps > 0 {
                warn!(
                    func = %key,
                    instances = p.instances.len(),
                    warm_fail_streak = p.warm_fail_streak,
                    crash_streak = p.crash_streak,
                    unhealthy_reaps = p.unhealthy_reaps,
                    probe_gate_ms_left = p.circuit_probe_after_ms.saturating_sub(now_ms()),
                    "lease against a failing pool"
                );
            }
            if p.instances.is_empty() {
                if let Some(marker) = p.last_node_fault {
                    if p.warm_fail_streak >= CRASH_CIRCUIT_THRESHOLD
                        && now_ms() < p.circuit_probe_after_ms
                    {
                        let n = p.warm_fail_streak;
                        anyhow::bail!(
                            "function '{key}' cannot start because THIS NODE is broken — {n} \
                             consecutive cold starts failed with a node provisioning fault \
                             ({marker}); the node's cell artifacts / hypervisor / container lock \
                             pool need an operator, and no change to this deployment will help. \
                             See the node's `cold start failed` logs for the exact missing path"
                        );
                    }
                }
                if p.crash_streak >= CRASH_CIRCUIT_THRESHOLD {
                    let n = p.crash_streak;
                    anyhow::bail!(
                        "function '{key}' is crash-looping — {n} consecutive instances exited \
                         within {}s of starting; check the deployment's logs, entrypoint and \
                         required env (DeploymentCircuitOpen)",
                        CRASH_LOOP_WINDOW_MS / 1000
                    );
                }
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
            let now = now_ms();
            if now.saturating_sub(p.unhealthy_reap_ms) > UNHEALTHY_REAP_WINDOW_MS {
                p.unhealthy_reaps = 0;
            }
            if p.unhealthy_reaps >= UNHEALTHY_CIRCUIT_THRESHOLD {
                if now < p.circuit_probe_after_ms {
                    let n = p.unhealthy_reaps;
                    anyhow::bail!(
                        "function '{key}' is not answering — {n} of its instances were reaped \
                         because requests to them got no response at all (the function starts \
                         and stays registered but does not reply on its port); check the \
                         deployment's logs, entrypoint and required env (DeploymentCircuitOpen)"
                    );
                }
                p.circuit_probe_after_ms = now + UNHEALTHY_CIRCUIT_PROBE_MS;
                warn!(
                    func = %key,
                    unhealthy_reaps = p.unhealthy_reaps,
                    probe_ms = UNHEALTHY_CIRCUIT_PROBE_MS,
                    "unhealthy-instance circuit half-open: one probe lease"
                );
            }
        }
        let deadline = tokio::time::Instant::now() + self.cfg.lease_timeout;
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
                        if let Some(p) = self.registry.lock().get_mut(key) {
                            p.warm_fail_streak = 0;
                            p.warm_backoff_until_ms = 0;
                            p.circuit_probe_after_ms = 0;
                            p.last_node_fault = None;
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
                        return Err(e);
                    }
                },
                LeaseDecision::Saturated(reason) => {
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
        anyhow::ensure!(
            !self.is_shutting_down(),
            "Fluid is shutting down; cold-start admission is closed"
        );
        let mut reg = self.registry.lock();
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
        if coalesce && reg.get(key).expect("present").provisioning > 0 {
            return Ok(LeaseDecision::WaitForWarm);
        }
        if reg.get(key).expect("present").live_count() >= max_instances {
            let pool = reg.get_mut(key).expect("present");
            pool.nack_concurrency += 1;
            return Ok(LeaseDecision::Saturated(NackReason::ConcurrencyLimit));
        }
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

    /// Speculatively bring one instance of `key` up AHEAD of any request — the ONE warming entry point
    /// shared by keep-warm and prewarm hints, so they share the crash-loop circuit, backoff and
    /// reservation semantics. Refuses without a reservation when the pool is warm, starting, saturated,
    /// in backoff, circuited or shutting down; a wrong guess costs one idle instance for `idle_ttl`.
    pub async fn warm(self: &Arc<Self>, key: &str) -> anyhow::Result<WarmVerdict> {
        let now = now_ms();
        {
            let mut reg = self.registry.lock();
            let Some(pool) = reg.get_mut(key) else {
                return Ok(WarmVerdict::Refused);
            };
            if self.is_shutting_down() {
                return Ok(WarmVerdict::Refused);
            }
            if pool.crash_streak >= CRASH_CIRCUIT_THRESHOLD {
                return Ok(WarmVerdict::Refused);
            }
            if now < pool.warm_backoff_until_ms || now < pool.circuit_probe_after_ms {
                return Ok(WarmVerdict::Refused);
            }
            if !pool.instances.iter().any(|i| !i.draining) && pool.provisioning == 0 {
            } else {
                return Ok(WarmVerdict::AlreadyWarm);
            }
            if pool.live_count() >= pool.cfg.max_instances {
                return Ok(WarmVerdict::Refused);
            }
            pool.provisioning += 1;
        }
        match self.warm_instance(key).await {
            Ok(()) => Ok(WarmVerdict::Started),
            Err(e) => Err(e),
        }
    }

    /// Run one cold start for an already-reserved pool and apply the shared success/failure bookkeeping;
    /// used by keep-warm and by [`Self::warm`].
    pub async fn warm_instance(self: &Arc<Self>, key: &str) -> anyhow::Result<()> {
        match self.cold_start(key).await {
            Ok((cell_id, _)) => {
                if let Some(pool) = self.registry.lock().get_mut(key) {
                    if let Some(inst) = pool.instances.iter_mut().find(|i| i.cell_id == cell_id) {
                        inst.inflight = 0;
                        inst.last_active_ms = now_ms();
                    }
                    pool.warm_fail_streak = 0;
                    pool.warm_backoff_until_ms = 0;
                    pool.circuit_probe_after_ms = 0;
                    pool.last_node_fault = None;
                    pool.last_warm_ok_ms = now_ms();
                }
                debug!(func = %key, "warm instance ready");
                Ok(())
            }
            Err(e) => {
                debug!(func = %key, error = %e, "keep-warm cold start failed");
                Err(e)
            }
        }
    }

    /// Provision a cell and start the function in it (`provisioning` already incremented by the
    /// caller; on success the instance is added with inflight=1). EVERY failing cold start is counted
    /// here, cancellation-safely, by a Drop guard: a request whose client gave up is dropped mid-await
    /// and never returns `Err`, so an error branch would leak the reservation and wedge the pool.
    async fn cold_start(self: &Arc<Self>, key: &str) -> anyhow::Result<(CellId, CellEndpoint)> {
        let mut guard = ColdStartGuard {
            fluid: self.clone(),
            key: key.to_string(),
            armed: true,
            error: None,
        };
        let r = self.cold_start_inner(key).await;
        match &r {
            Ok(_) => guard.armed = false,
            Err(e) => guard.error = Some(e.to_string()),
        }
        r
    }

    async fn cold_start_inner(
        self: &Arc<Self>,
        key: &str,
    ) -> anyhow::Result<(CellId, CellEndpoint)> {
        anyhow::ensure!(
            !self.is_shutting_down(),
            "Fluid shutdown began before cold-start admission"
        );
        let _permit = self
            .cold_start_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("Fluid cold-start admission is closed"))?;
        anyhow::ensure!(
            !self.is_shutting_down(),
            "Fluid shutdown began while the cold start waited for host capacity"
        );
        let (image, mut launch, mem, vcpus, tenant) = {
            let reg = self.registry.lock();
            let pool = reg
                .get(key)
                .ok_or_else(|| anyhow::anyhow!("no such function '{key}'"))?;
            let port = free_port()?;
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
            let tcp_ports: Vec<hive_core::TcpPublish> = if pool
                .cfg
                .start_cmd
                .first()
                .map(String::as_str)
                == Some("__container__")
            {
                let primary: Option<u16> = pool.cfg.start_cmd.get(2).and_then(|s| s.parse().ok());
                let mut seen = std::collections::BTreeSet::new();
                pool.cfg
                    .ports
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.protocol,
                            fluid_core::ServiceProtocol::Tcp | fluid_core::ServiceProtocol::Grpc
                        ) || (s.protocol == fluid_core::ServiceProtocol::Http
                            && s.preferred_public_port.is_some())
                    })
                    .filter(|s| seen.insert(s.container_port))
                    .filter_map(|s| {
                        if Some(s.container_port) == primary {
                            return Some(hive_core::TcpPublish {
                                container_port: s.container_port,
                                host_port: port,
                            });
                        }
                        match free_port() {
                            Ok(hp) if hp == port => {
                                warn!(func = %key, container_port = s.container_port, "loopback TCP probe returned the primary host port; skipping this publish for this instance");
                                None
                            }
                            Ok(hp) => Some(hive_core::TcpPublish {
                                container_port: s.container_port,
                                host_port: hp,
                            }),
                            Err(e) => {
                                warn!(func = %key, container_port = s.container_port, error = %e, "no free loopback TCP host port; skipping this TCP publish");
                                None
                            }
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
                runtime_artifact: None,
                port,
                max_concurrency: pool.cfg.max_concurrency,
                memory_mib: pool.cfg.memory_mib,
                cpus: pool.cfg.cpus,
                pids: pool.cfg.pids,
                runtime: hive_core::Runtime::resolve(&pool.cfg.runtime, &pool.cfg.start_cmd),
                raw_proxy: pool.cfg.needs_raw_proxy(),
                udp_ports,
                tcp_ports,
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

        let is_container = launch.start_cmd.first().map(String::as_str) == Some("__container__");
        let needs_runtime_artifact = image.starts_with("dpl-")
            && !is_container
            && self.backend.requires_runtime_artifact_authorization();
        if needs_runtime_artifact {
            let identity = self
                .backend
                .runtime_artifact_identity(&image)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "isolated runtime artifact {image:?} has no committed identity; refusing to provision or execute it ({})",
                        hive_core::fault::NODE_IMAGE_MISSING
                    )
                })?;
            anyhow::ensure!(
                launch
                    .workdir
                    .as_deref()
                    .is_some_and(|workdir| !workdir.trim().is_empty()),
                "isolated runtime artifact {image:?} has no validated runtime workdir ({})",
                hive_core::fault::NODE_IMAGE_MISSING
            );
            anyhow::ensure!(
                identity.id == image
                    && identity.protocol == hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION
                    && identity.content_sha256.len() == 64
                    && identity.content_sha256.bytes().all(|byte| {
                        byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                    }),
                "isolated runtime artifact {image:?} returned a mismatched identity/protocol/digest ({})",
                hive_core::fault::NODE_IMAGE_MISSING
            );
            launch.runtime_artifact = Some(identity);
        } else {
            launch.runtime_artifact = None;
        }

        let container =
            if launch.start_cmd.first().map(String::as_str) == Some("__container__") {
                Some(hive_backend::ContainerSpec {
                    image: launch.start_cmd.get(1).cloned().unwrap_or_default(),
                    ports: {
                        let mut ports = vec![hive_backend::ContainerPort::tcp(
                            launch
                                .start_cmd
                                .get(2)
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(8080),
                            launch.port,
                        )];
                        ports.extend(launch.udp_ports.iter().map(|u| {
                            hive_backend::ContainerPort {
                                container_port: u.container_port,
                                host_port: u.host_port,
                                protocol: hive_backend::ContainerProtocol::Udp,
                            }
                        }));
                        ports.extend(launch.tcp_ports.iter().filter_map(|t| {
                            if t.host_port == launch.port {
                                return None;
                            }
                            Some(hive_backend::ContainerPort {
                                container_port: t.container_port,
                                host_port: t.host_port,
                                protocol: hive_backend::ContainerProtocol::Tcp,
                            })
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
        let t_prov = now_ms();
        let handle = self
            .backend
            .provision_runtime(&spec, launch.runtime_artifact.as_ref())
            .await?;
        let mut unpublished = UnpublishedCell::new(self.clone(), handle.clone());
        let provision_ms = now_ms().saturating_sub(t_prov);
        let t_run = now_ms();
        let endpoint = self.backend.start_function(&handle, &launch).await?;

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
                    tcp_ports: launch.tcp_ports.clone(),
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
            anyhow::bail!("function '{key}' unregistered during cold start");
        }
        unpublished.publish();
        if let Some(observer) = self.execution_observer.get() {
            let deployment_id = key.split_once('/').map(|(d, _)| d).unwrap_or(key);
            observer.on_started(deployment_id, cell_id.as_str(), self.backend.name());
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

    /// Called by `fluid-gateway::try_browser` on a successful browser-served response — the ONLY
    /// metering chokepoint for that path, which never calls `lease()`. No-op for an unknown pool.
    pub fn record_browser_request(&self, key: &str, active_ms: u64) {
        if let Some(pool) = self.registry.lock().get_mut(key) {
            pool.browser_requests += 1;
            pool.browser_served_ms += active_ms;
        }
    }

    /// Remove an instance the router/health check found unreachable, terminate it and account its
    /// time; keep-warm/cold-start replaces it.
    pub async fn mark_dead(self: &Arc<Self>, key: &str, cell_id: &CellId) {
        let (handle, requests_served) = {
            let mut reg = self.registry.lock();
            let Some(pool) = reg.get_mut(key) else { return };
            let Some(pos) = pool.instances.iter().position(|i| &i.cell_id == cell_id) else {
                return; // already removed (idempotent)
            };
            let inst = pool.instances.remove(pos);
            pool.dead_reaped += 1;
            let now = now_ms();
            if now.saturating_sub(pool.unhealthy_reap_ms) > UNHEALTHY_REAP_WINDOW_MS {
                pool.unhealthy_reaps = 0; // previous failure aged out of the run
            }
            pool.unhealthy_reaps = pool.unhealthy_reaps.saturating_add(1);
            pool.unhealthy_reap_ms = now;
            pool.instance_ms_retired += now.saturating_sub(inst.started_at_ms);
            if pool.unhealthy_reaps >= UNHEALTHY_CIRCUIT_THRESHOLD {
                pool.circuit_probe_after_ms = now + UNHEALTHY_CIRCUIT_PROBE_MS;
                warn!(
                    func = %key,
                    cell = %cell_id,
                    unhealthy_reaps = pool.unhealthy_reaps,
                    probe_ms = UNHEALTHY_CIRCUIT_PROBE_MS,
                    "instances stopped answering while still registered; circuit open"
                );
            }
            (inst.handle, inst.requests_served)
        };
        let deployment_id = key
            .split_once('/')
            .map(|(d, _)| d)
            .unwrap_or(key)
            .to_string();
        let execution = Some(ExecutionEndContext {
            deployment_id,
            requests_served,
            outcome: "crashed".to_string(),
        });
        let _ = self.terminate_tracked(handle, execution).await;
    }

    /// Remove a function pool entirely and terminate all its instances.
    pub async fn unregister(self: &Arc<Self>, key: &str) {
        self.unregister_many(&[key.to_string()]).await;
    }

    /// Remove a deployment's complete pool set under one lock, then terminate every detached cell
    /// outside it, so an in-flight cold start cannot publish into part of the removed set.
    pub async fn unregister_many(self: &Arc<Self>, keys: &[String]) -> usize {
        let mut unique: Vec<&str> = keys.iter().map(String::as_str).collect();
        unique.sort_unstable();
        unique.dedup();
        let (removed, handles): (usize, Vec<(hive_backend::CellHandle, ExecutionEndContext)>) = {
            let mut reg = self.registry.lock();
            let mut removed = 0usize;
            let mut handles = Vec::new();
            for key in unique {
                if let Some(pool) = reg.remove(key) {
                    removed += 1;
                    let deployment_id = key
                        .split_once('/')
                        .map(|(d, _)| d)
                        .unwrap_or(key)
                        .to_string();
                    handles.extend(pool.instances.into_iter().map(|instance| {
                        (
                            instance.handle,
                            ExecutionEndContext {
                                deployment_id: deployment_id.clone(),
                                requests_served: instance.requests_served,
                                outcome: "killed".to_string(),
                            },
                        )
                    }));
                }
            }
            (removed, handles)
        };
        for (handle, execution) in handles {
            let _ = self.terminate_tracked(handle, Some(execution)).await;
        }
        removed
    }

    async fn health_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
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

    async fn autoscaler_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.cfg.autoscaler_interval).await;
            self.clone().reconcile().await;
        }
    }

    async fn reconcile(self: Arc<Self>) {
        let mut to_warm: Vec<String> = Vec::new();
        let mut to_drain: Vec<(String, CellId, hive_backend::CellHandle, u64)> = Vec::new();
        let now = now_ms();
        {
            let mut reg = self.registry.lock();
            for (key, pool) in reg.iter_mut() {
                let live = pool.live_count();
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
                    pool.crash_streak = 0;
                }
                let live = pool.live_count();
                if live < pool.cfg.min_instances && now >= pool.warm_backoff_until_ms {
                    for _ in 0..(pool.cfg.min_instances - live) {
                        pool.provisioning += 1;
                        to_warm.push(key.clone());
                    }
                }
                let ttl_ms = pool.cfg.idle_ttl_secs * 1000;
                let max_req = self.cfg.max_requests_per_instance;
                let max_age_ms = self.cfg.max_instance_age_secs.saturating_mul(1000);
                let mut live_after = pool.instances.iter().filter(|i| !i.draining).count() as u32;
                let mut retired_add = 0u64;
                let mut recycled_add = 0u64;
                for inst in pool.instances.iter_mut() {
                    if inst.draining {
                        if inst.inflight == 0 {
                            to_drain.push((
                                key.clone(),
                                inst.cell_id.clone(),
                                inst.handle.clone(),
                                inst.requests_served,
                            ));
                        }
                        continue;
                    }
                    let age_ms = now.saturating_sub(inst.started_at_ms);
                    if instance_recyclable(inst.requests_served, age_ms, max_req, max_age_ms) {
                        inst.draining = true; // stop new requests now; terminate when idle
                        recycled_add += 1;
                        retired_add += age_ms;
                        if inst.inflight == 0 {
                            to_drain.push((
                                key.clone(),
                                inst.cell_id.clone(),
                                inst.handle.clone(),
                                inst.requests_served,
                            ));
                        }
                        continue;
                    }
                    let idle =
                        inst.inflight == 0 && now.saturating_sub(inst.last_active_ms) > ttl_ms;
                    if idle && live_after > pool.cfg.min_instances {
                        inst.draining = true;
                        live_after -= 1;
                        retired_add += age_ms;
                        to_drain.push((
                            key.clone(),
                            inst.cell_id.clone(),
                            inst.handle.clone(),
                            inst.requests_served,
                        ));
                    }
                }
                pool.instance_ms_retired += retired_add;
                pool.recycled += recycled_add;
            }
        }

        let warm = futures::future::join_all(to_warm.into_iter().map(|key| {
            let f = self.clone();
            async move {
                let _ = f.warm_instance(&key).await;
            }
        }));
        let drain = futures::future::join_all(to_drain.into_iter().map(
            |(key, cell_id, handle, requests_served)| {
                let f = self.clone();
                async move {
                    let deployment_id = key
                        .split_once('/')
                        .map(|(d, _)| d)
                        .unwrap_or(&key)
                        .to_string();
                    let execution = ExecutionEndContext {
                        deployment_id,
                        requests_served,
                        outcome: "drained".to_string(),
                    };
                    let _ = f.terminate_tracked(handle, Some(execution)).await;
                    if let Some(pool) = f.registry.lock().get_mut(&key) {
                        pool.instances.retain(|i| i.cell_id != cell_id);
                    }
                    debug!(func = %key, cell = %cell_id, "instance scaled to zero");
                }
            },
        ));
        tokio::join!(warm, drain);

        self.adapt_concurrency().await;
    }

    /// CPU-driven adaptive concurrency: per pool, the MAX CPU across live instances steps the
    /// safe-concurrency ceiling via [`aimd_step`]; a backend that cannot report CPU is left untouched.
    async fn adapt_concurrency(&self) {
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

        let sampled_pools = futures::future::join_all(targets.into_iter().map(
            |(key, handles, current, max)| async move {
                let samples =
                    futures::future::join_all(handles.iter().map(|h| self.backend.cpu_percent(h)))
                        .await;
                (key, samples, current, max)
            },
        ))
        .await;

        for (key, samples, current, max) in sampled_pools {
            let mut max_cpu = 0.0f32;
            let mut sampled = false;
            for c in samples.into_iter().flatten() {
                sampled = true;
                if c > max_cpu {
                    max_cpu = c;
                }
            }
            if !sampled {
                continue; // backend gives no CPU signal → hold (never guess from latency)
            }
            let next = aimd_step(max_cpu, current, max);
            if let Some(pool) = self.registry.lock().get_mut(&key) {
                pool.last_cpu_pct = max_cpu;
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

/// Why a warm instance / pool could not immediately accept a request (a NACK), so the router can
/// react (retry → scale-out → peer → region) and the reason is observable.
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
    /// Singleflight: a cold start is already in flight for this pool, so this request briefly WAITS for
    /// it instead of launching a duplicate.
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

    /// The loopback host port THIS lease's instance publishes `container_port` over UDP on — the local
    /// datagram leg the UDP relay sends to while holding the lease; `None` when not published.
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

    /// The loopback host port THIS lease's instance publishes `container_port` over TCP on — the direct
    /// local splice leg for raw/published TCP ports; same contract as [`Lease::udp_host_port`].
    pub fn tcp_host_port(&self, container_port: u16) -> Option<u16> {
        let reg = self.fluid.registry.lock();
        reg.get(&self.key)?
            .instances
            .iter()
            .find(|i| i.cell_id == self.cell_id)?
            .tcp_ports
            .iter()
            .find(|t| t.container_port == container_port)
            .map(|t| t.host_port)
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

/// Grab a free UDP port by binding to :0 — a port free for TCP is not necessarily free for UDP
/// (same probe-then-release window as [`free_port`]; podman's own bind fails loudly in the race).
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
            tcp_ports: Vec::new(),
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
            tcp_ports: Vec::new(),
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
