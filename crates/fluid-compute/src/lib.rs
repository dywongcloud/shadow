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
    inflight: u32,
    started_at_ms: u64,
    last_active_ms: u64,
    draining: bool,
}

/// Everything needed to (re)launch instances of one function.
struct FunctionPool {
    cfg: FunctionConfig,
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
}

impl FunctionPool {
    fn live_count(&self) -> u32 {
        self.instances.iter().filter(|i| !i.draining).count() as u32 + self.provisioning
    }
    fn total_inflight(&self) -> u32 {
        self.instances.iter().map(|i| i.inflight).sum()
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct FunctionStats {
    pub key: String,
    pub instances: usize,
    pub inflight: u32,
    pub provisioning: u32,
    pub max_concurrency: u32,
    pub min_instances: u32,
    pub max_instances: u32,
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
}

pub struct FluidConfig {
    pub autoscaler_interval: Duration,
    /// How long `lease` waits for a slot before giving up (backpressure).
    pub lease_timeout: Duration,
}

impl Default for FluidConfig {
    fn default() -> Self {
        FluidConfig {
            autoscaler_interval: Duration::from_millis(500),
            lease_timeout: Duration::from_secs(20),
        }
    }
}

pub struct Fluid {
    backend: Arc<dyn CellBackend>,
    cfg: FluidConfig,
    registry: Mutex<HashMap<String, FunctionPool>>,
}

/// Compose the per-function key.
pub fn func_key(deployment: &str, function: &str) -> String {
    format!("{deployment}/{function}")
}

impl Fluid {
    pub fn start(backend: Arc<dyn CellBackend>, cfg: FluidConfig) -> Arc<Fluid> {
        let fluid = Arc::new(Fluid {
            backend,
            cfg,
            registry: Mutex::new(HashMap::new()),
        });
        let f = fluid.clone();
        tokio::spawn(async move { f.autoscaler_loop().await });
        let h = fluid.clone();
        tokio::spawn(async move { h.health_loop().await });
        fluid
    }

    /// Register (or replace) a function pool. Does not provision yet; the
    /// autoscaler will bring up `min_instances`.
    pub fn register(&self, key: String, cfg: FunctionConfig, image: String, workdir: String) {
        let mut reg = self.registry.lock();
        reg.insert(
            key,
            FunctionPool {
                cfg,
                image,
                workdir,
                instances: Vec::new(),
                provisioning: 0,
                requests: 0,
                served_ms_sum: 0,
                instance_ms_retired: 0,
                dead_reaped: 0,
            },
        );
    }

    /// Max invocation duration configured for a function (seconds).
    pub fn max_duration_secs(&self, key: &str) -> Option<u64> {
        self.registry.lock().get(key).map(|p| p.cfg.max_duration_secs)
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
                FunctionStats {
                    key: k.clone(),
                    instances: p.instances.iter().filter(|i| !i.draining).count(),
                    inflight: p.total_inflight(),
                    provisioning: p.provisioning,
                    max_concurrency: p.cfg.max_concurrency,
                    min_instances: p.cfg.min_instances,
                    max_instances: p.cfg.max_instances,
                    requests: p.requests,
                    traditional_ms,
                    fluid_ms,
                    savings_pct,
                    dead_reaped: p.dead_reaped,
                }
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// Acquire a slot on an instance for one request. Reuses a running instance
    /// (least-loaded with a free slot) or cold-starts when saturated.
    pub async fn lease(self: &Arc<Self>, key: &str) -> anyhow::Result<Lease> {
        let deadline = tokio::time::Instant::now() + self.cfg.lease_timeout;
        loop {
            let decision = self.decide_lease(key)?;
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
                LeaseDecision::ColdStart => match self.cold_start(key).await {
                    Ok((cell_id, endpoint)) => {
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
                        // Release the provisioning reservation and surface the error.
                        if let Some(p) = self.registry.lock().get_mut(key) {
                            p.provisioning = p.provisioning.saturating_sub(1);
                        }
                        return Err(e);
                    }
                },
                LeaseDecision::Saturated => {
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!("function '{key}' saturated (all instances at max concurrency)");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                LeaseDecision::NotFound => anyhow::bail!("no such function '{key}'"),
            }
        }
    }

    fn decide_lease(&self, key: &str) -> anyhow::Result<LeaseDecision> {
        let mut reg = self.registry.lock();
        let pool = match reg.get_mut(key) {
            Some(p) => p,
            None => return Ok(LeaseDecision::NotFound),
        };
        let max_c = pool.cfg.max_concurrency;
        // Least-loaded instance with a free slot.
        let pick = pool
            .instances
            .iter_mut()
            .filter(|i| !i.draining && i.inflight < max_c)
            .min_by_key(|i| i.inflight);
        if let Some(inst) = pick {
            inst.inflight += 1;
            inst.last_active_ms = now_ms();
            return Ok(LeaseDecision::Ready {
                cell_id: inst.cell_id.clone(),
                endpoint: inst.endpoint.clone(),
            });
        }
        // All full: cold-start if under the ceiling.
        if pool.live_count() < pool.cfg.max_instances {
            pool.provisioning += 1;
            Ok(LeaseDecision::ColdStart)
        } else {
            Ok(LeaseDecision::Saturated)
        }
    }

    /// Provision a cell and start the function in it. `provisioning` was already
    /// incremented by the caller; on success we add the instance with inflight=1.
    async fn cold_start(self: &Arc<Self>, key: &str) -> anyhow::Result<(CellId, CellEndpoint)> {
        let (image, launch, mem) = {
            let reg = self.registry.lock();
            let pool = reg.get(key).ok_or_else(|| anyhow::anyhow!("no such function '{key}'"))?;
            let port = free_port()?;
            let launch = FunctionLaunch {
                start_cmd: pool.cfg.start_cmd.clone(),
                env: pool.cfg.env.clone(),
                workdir: Some(pool.workdir.clone()),
                port,
                max_concurrency: pool.cfg.max_concurrency,
            };
            (pool.image.clone(), launch, pool.cfg.memory_mib)
        };

        let spec = CellSpec {
            id: CellId::new(),
            image,
            resources: ResourceSpec {
                vcpus: 1,
                mem_mib: mem,
                disk_mib: 1024,
                timeout_secs: 0,
            },
        };
        debug!(func = %key, cell = %spec.id, "cold-starting function instance");
        let handle = self.backend.provision(&spec).await?;
        let endpoint = self.backend.start_function(&handle, &launch).await?;

        let cell_id = spec.id.clone();
        let added = {
            let mut reg = self.registry.lock();
            if let Some(pool) = reg.get_mut(key) {
                pool.provisioning = pool.provisioning.saturating_sub(1);
                pool.instances.push(Instance {
                    cell_id: cell_id.clone(),
                    handle: handle.clone(),
                    endpoint: endpoint.clone(),
                    inflight: 1, // this lease
                    started_at_ms: now_ms(),
                    last_active_ms: now_ms(),
                    draining: false,
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
            }
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
                // Keep-warm: bring live count up to min_instances.
                let live = pool.live_count();
                if live < pool.cfg.min_instances {
                    for _ in 0..(pool.cfg.min_instances - live) {
                        pool.provisioning += 1;
                        to_warm.push(key.clone());
                    }
                }
                // Scale-to-zero: drain idle instances above min_instances.
                let ttl_ms = pool.cfg.idle_ttl_secs * 1000;
                let mut live_after = pool.instances.iter().filter(|i| !i.draining).count() as u32;
                let mut retired_add = 0u64;
                for inst in pool.instances.iter_mut() {
                    if inst.draining {
                        continue;
                    }
                    let idle = inst.inflight == 0 && now.saturating_sub(inst.last_active_ms) > ttl_ms;
                    if idle && live_after > pool.cfg.min_instances {
                        inst.draining = true;
                        live_after -= 1;
                        retired_add += now.saturating_sub(inst.started_at_ms);
                        to_drain.push((key.clone(), inst.cell_id.clone(), inst.handle.clone()));
                    }
                }
                pool.instance_ms_retired += retired_add;
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
                        }
                        debug!(func = %key, "warm instance ready");
                    }
                    Err(e) => {
                        if let Some(pool) = f.registry.lock().get_mut(&key) {
                            pool.provisioning = pool.provisioning.saturating_sub(1);
                        }
                        warn!(func = %key, error = %e, "keep-warm cold start failed");
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
    }
}

enum LeaseDecision {
    Ready { cell_id: CellId, endpoint: CellEndpoint },
    ColdStart,
    Saturated,
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
