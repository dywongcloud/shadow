//! `hive-controlplane` — the orchestrator.
//!
//! This is the brain the Hive write-up describes as the **Control Plane**:
//! it owns job placement, the **warm pool** of pre-provisioned cells,
//! autoscaling, and the cell/job lifecycle. It drives any [`CellBackend`]
//! (mock processes today, Firecracker microVMs inside Lima).
//!
//! Design rules that keep it correct under concurrency:
//! * All shared state lives in one [`parking_lot::Mutex`]-guarded `Inner`.
//! * Locks are **never** held across an `.await`. Slow work (backend
//!   provision/build/terminate) happens outside the lock; the lock is only
//!   taken for short bookkeeping critical sections.
//! * Every capacity reservation has exactly one matching release.

pub mod config;
pub mod logbus;
pub mod records;

pub use config::{BoxConfig, HiveConfig};

use dashmap::DashMap;
use hive_backend::{CellBackend, CellSpec, LogSink};
use hive_core::{
    now_ms, BoxId, BuildJob, CellId, CellState, ClusterStatus, JobId, JobState, JobView, LogLine,
    LogStream, ResourceSpec,
};
use logbus::LogBus;
use parking_lot::Mutex;
use records::{BoxRecord, CellRecord, JobRecord};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Notify};
use tracing::{debug, info, warn};

/// All mutable cluster state, behind one lock.
struct Inner {
    boxes: Vec<BoxRecord>,
    cells: HashMap<CellId, CellRecord>,
    jobs: HashMap<JobId, JobRecord>,
    /// FIFO of queued job ids awaiting placement.
    queue: VecDeque<JobId>,
    /// image -> idle warm cells ready for instant assignment.
    warm: HashMap<String, VecDeque<CellId>>,
    /// image -> warm cells currently being provisioned (in flight).
    warm_pending: HashMap<String, usize>,
    /// Builds currently executing (bounds concurrency).
    running_builds: usize,
}

/// How many FINISHED jobs (record + its replay log bus) are retained. In-flight
/// jobs are never evicted regardless of this number.
///
/// `jobs` and `logs` were both append-only for the life of the process: one
/// `JobRecord` and one `LogBus` per build ever submitted, neither ever removed.
/// On a node that builds continuously that is a monotonic leak whose rate is
/// exactly the deploy rate, and the log half carried the actual build output —
/// megabytes per entry. Retention has to be a real number, not "forever".
const MAX_RETAINED_FINISHED_JOBS: usize = 200;

fn max_retained_finished_jobs() -> usize {
    std::env::var("HIVE_MAX_RETAINED_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(MAX_RETAINED_FINISHED_JOBS)
}

impl Inner {
    /// Pick the box with the most free vCPUs that can still fit `r` (worst-fit,
    /// spreads load across boxes).
    fn pick_box(&self, r: &ResourceSpec) -> Option<usize> {
        self.boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.can_fit(r))
            .max_by_key(|(_, b)| b.vcpus_free())
            .map(|(i, _)| i)
    }

    /// Take a warm cell for `image` that satisfies `req`, if one exists.
    fn take_warm(&mut self, image: &str, req: &ResourceSpec) -> Option<CellId> {
        let pool = self.warm.get_mut(image)?;
        // Find the first warm cell big enough.
        let pos = pool.iter().position(|cid| {
            self.cells
                .get(cid)
                .map(|c| c.state == CellState::Warm && c.resources.satisfies(req))
                .unwrap_or(false)
        })?;
        pool.remove(pos)
    }
}

pub struct Hive {
    pub cfg: HiveConfig,
    backend: Arc<dyn CellBackend>,
    inner: Mutex<Inner>,
    logs: DashMap<JobId, Arc<LogBus>>,
    /// Wakes the scheduler loop when there is new work or freed capacity.
    wake: Notify,
}

/// A scheduling decision produced under the lock, executed outside it.
enum Decision {
    Idle,
    Run {
        job: JobId,
        cell: CellId,
        /// true => cell came from the warm pool (already provisioned).
        warm: bool,
    },
}

impl Hive {
    /// Build the Hive and start its background loops. Returns a shared handle.
    pub fn start(cfg: HiveConfig, backend: Arc<dyn CellBackend>) -> Arc<Hive> {
        let boxes = cfg
            .boxes
            .iter()
            .map(|bc| BoxRecord {
                id: BoxId::new(),
                vcpus_total: bc.vcpus,
                mem_total_mib: bc.mem_mib,
                vcpus_used: 0,
                mem_used_mib: 0,
                cells: 0,
                warm_cells: 0,
            })
            .collect();

        let hive = Arc::new(Hive {
            inner: Mutex::new(Inner {
                boxes,
                cells: HashMap::new(),
                jobs: HashMap::new(),
                queue: VecDeque::new(),
                warm: HashMap::new(),
                warm_pending: HashMap::new(),
                running_builds: 0,
            }),
            logs: DashMap::new(),
            wake: Notify::new(),
            backend,
            cfg,
        });

        info!(
            hive = %hive.cfg.hive_id,
            boxes = hive.cfg.boxes.len(),
            backend = hive.backend.name(),
            "control plane starting"
        );

        // Scheduler loop: react to new work / freed capacity.
        {
            let h = hive.clone();
            tokio::spawn(async move { h.scheduler_loop().await });
        }
        // Autoscaler loop: maintain the warm pool and reap stale cells.
        {
            let h = hive.clone();
            tokio::spawn(async move { h.autoscaler_loop().await });
        }

        hive
    }

    // ---- public API ----------------------------------------------------

    /// Submit a build. Returns immediately; placement happens asynchronously.
    pub fn submit(&self, job: BuildJob) -> JobId {
        let id = job.id.clone();
        {
            let mut inner = self.inner.lock();
            inner.jobs.insert(id.clone(), JobRecord::new(job));
            inner.queue.push_back(id.clone());
        }
        self.logs
            .entry(id.clone())
            .or_insert_with(|| Arc::new(LogBus::new()));
        debug!(job = %id, "submitted");
        self.wake.notify_one();
        id
    }

    pub fn job_view(&self, id: &JobId) -> Option<JobView> {
        self.inner.lock().jobs.get(id).map(|j| j.view())
    }

    /// Replay buffered logs + a live receiver for a job.
    pub fn subscribe_logs(
        &self,
        id: &JobId,
    ) -> Option<(Vec<LogLine>, broadcast::Receiver<LogLine>)> {
        self.logs.get(id).map(|bus| bus.subscribe())
    }

    pub fn logs_done(&self, id: &JobId) -> bool {
        self.logs.get(id).map(|b| b.is_done()).unwrap_or(false)
    }

    /// Total bytes currently retained across every job's replay buffer — the
    /// gauge that makes this bound observable from outside the process
    /// (hive-cloud surfaces it on `GET /v1/debug/memory`).
    pub fn log_retention_bytes(&self) -> usize {
        self.logs.iter().map(|e| e.value().retained_bytes()).sum()
    }

    /// Number of retained job records / log buses.
    pub fn retained_job_counts(&self) -> (usize, usize) {
        // Read the two under separate, non-overlapping locks. Written as one
        // tuple expression the `inner` guard would live until the end of the
        // statement, i.e. across the DashMap access — establishing an
        // inner→logs lock order for a pair of counters that need no atomicity.
        let jobs = self.inner.lock().jobs.len();
        (jobs, self.logs.len())
    }

    /// Evict the oldest FINISHED jobs — record and replay log together — down to
    /// the retention bound. Called after every job terminates and on the
    /// autoscaler tick (so a job that finished via a path that forgot to call
    /// it is still collected).
    ///
    /// Record and bus are evicted as one unit: a `JobView` whose logs 404, or a
    /// `LogBus` no job names, are both worse than an honestly-forgotten job.
    fn reap_finished_jobs(&self) {
        let keep = max_retained_finished_jobs();
        let evict: Vec<JobId> = {
            let mut inner = self.inner.lock();
            let mut finished: Vec<(u64, JobId)> = inner
                .jobs
                .iter()
                // `is_terminal()` rather than a local variant list, so a new
                // terminal state is retained-and-reaped by default instead of
                // silently becoming immortal.
                .filter(|(_, j)| j.state.is_terminal())
                .map(|(id, j)| (j.finished_at_ms.unwrap_or(0), id.clone()))
                .collect();
            if finished.len() <= keep {
                return;
            }
            // Oldest first; ties broken by id so the choice is deterministic
            // rather than dependent on HashMap iteration order.
            finished.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let drop_n = finished.len() - keep;
            let evict: Vec<JobId> = finished.into_iter().take(drop_n).map(|(_, id)| id).collect();
            for id in &evict {
                inner.jobs.remove(id);
            }
            evict
        };
        let mut freed = 0usize;
        for id in &evict {
            if let Some((_, bus)) = self.logs.remove(id) {
                freed += bus.retained_bytes();
            }
        }
        debug!(
            evicted = evict.len(),
            freed_log_bytes = freed,
            retained = keep,
            "reaped finished job records"
        );
    }

    pub fn cluster_status(&self) -> ClusterStatus {
        let inner = self.inner.lock();
        // Recompute per-box warm counts from the cell table for accuracy.
        let mut warm_per_box: HashMap<BoxId, usize> = HashMap::new();
        for c in inner.cells.values() {
            if c.state == CellState::Warm {
                *warm_per_box.entry(c.box_id.clone()).or_default() += 1;
            }
        }
        let boxes = inner
            .boxes
            .iter()
            .map(|b| {
                let mut v = b.view();
                v.warm_cells = warm_per_box.get(&b.id).copied().unwrap_or(0);
                v
            })
            .collect();
        let cells = inner.cells.values().map(|c| c.view()).collect();
        let mut jobs: Vec<JobView> = inner.jobs.values().map(|j| j.view()).collect();
        jobs.sort_by_key(|j| j.submitted_at_ms);
        ClusterStatus {
            hive: self.cfg.hive_id.clone(),
            boxes,
            cells,
            jobs,
            queued: inner.queue.len(),
        }
    }

    // ---- scheduler -----------------------------------------------------

    async fn scheduler_loop(self: Arc<Self>) {
        loop {
            // Wait for a wake or a periodic safety tick.
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.cfg.autoscaler_interval) => {}
            }
            self.clone().schedule_pass();
        }
    }

    /// Make as many placements as capacity allows, spawning a task per cell.
    fn schedule_pass(self: Arc<Self>) {
        loop {
            let decision = self.decide_one();
            match decision {
                Decision::Idle => break,
                Decision::Run { job, cell, warm } => {
                    let h = self.clone();
                    tokio::spawn(async move { h.run_cell(job, cell, warm).await });
                }
            }
        }
    }

    /// One placement decision, taken entirely under the lock.
    fn decide_one(&self) -> Decision {
        let mut inner = self.inner.lock();
        if inner.running_builds >= self.cfg.max_concurrent_builds {
            return Decision::Idle;
        }
        let qlen = inner.queue.len();
        for idx in 0..qlen {
            let job_id = inner.queue[idx].clone();
            let (image, req) = match inner.jobs.get(&job_id) {
                Some(jr) => (jr.job.image.clone(), jr.job.resources.clone()),
                None => continue,
            };

            // 1) Warm-pool hit — the fast path Hive optimizes for.
            if let Some(cell_id) = inner.take_warm(&image, &req) {
                inner.queue.remove(idx);
                Self::assign(&mut inner, &job_id, &cell_id, false);
                inner.running_builds += 1;
                debug!(job = %job_id, cell = %cell_id, "scheduled (warm hit)");
                return Decision::Run {
                    job: job_id,
                    cell: cell_id,
                    warm: true,
                };
            }

            // 2) Cold provision — reserve capacity on a box now, boot later.
            if let Some(bidx) = inner.pick_box(&req) {
                let box_id = inner.boxes[bidx].id.clone();
                inner.boxes[bidx].reserve(&req);
                let cell_id = CellId::new();
                inner.cells.insert(
                    cell_id.clone(),
                    CellRecord {
                        id: cell_id.clone(),
                        box_id: box_id.clone(),
                        state: CellState::Provisioning,
                        image: image.clone(),
                        resources: req.clone(),
                        job: Some(job_id.clone()),
                        created_at_ms: now_ms(),
                        became_warm_at_ms: None,
                        handle: None,
                    },
                );
                inner.queue.remove(idx);
                Self::assign(&mut inner, &job_id, &cell_id, true);
                inner.running_builds += 1;
                debug!(job = %job_id, cell = %cell_id, box = %box_id, "scheduled (cold provision)");
                return Decision::Run {
                    job: job_id,
                    cell: cell_id,
                    warm: false,
                };
            }
            // else: this job can't be placed right now; try the next one.
        }
        Decision::Idle
    }

    /// Wire a cell to a job and move both into their scheduled states.
    /// `cold` controls whether the cell starts in Provisioning (true) or
    /// transitions from Warm->Assigned (false).
    fn assign(inner: &mut Inner, job_id: &JobId, cell_id: &CellId, cold: bool) {
        if let Some(cell) = inner.cells.get_mut(cell_id) {
            if !cold {
                cell.state = CellState::Assigned;
            }
            cell.job = Some(job_id.clone());
        }
        if let Some(jr) = inner.jobs.get_mut(job_id) {
            jr.state = JobState::Scheduled;
            jr.assigned_cell = Some(cell_id.clone());
            jr.assigned_box = inner.cells.get(cell_id).map(|c| c.box_id.clone());
            jr.scheduled_at_ms = Some(now_ms());
        }
    }

    // ---- per-cell execution -------------------------------------------

    /// Provision (if cold) then run the build, then tear the cell down.
    async fn run_cell(self: Arc<Self>, job_id: JobId, cell_id: CellId, warm: bool) {
        if !warm {
            // Cold path: boot the microVM / prepare the sandbox.
            let spec = {
                let inner = self.inner.lock();
                match inner.cells.get(&cell_id) {
                    Some(c) => CellSpec {
                        id: c.id.clone(),
                        image: c.image.clone(),
                        resources: c.resources.clone(),
                        // Build cells aren't tenant-attributed yet (build-pipeline
                        // tenancy is a follow-up); default to the shared tenant.
                        tenant: String::new(),
                        container: None, // build cell, not a container
                    },
                    None => return,
                }
            };
            match self.backend.provision(&spec).await {
                Ok(handle) => {
                    let mut inner = self.inner.lock();
                    if let Some(c) = inner.cells.get_mut(&cell_id) {
                        c.handle = Some(handle);
                        c.state = CellState::Assigned;
                    }
                }
                Err(e) => {
                    warn!(job = %job_id, cell = %cell_id, error = %e, "cold provision failed");
                    self.fail_provision(&job_id, &cell_id, &format!("provision failed: {e}"));
                    return;
                }
            }
        }

        self.run_build_phase(job_id, cell_id).await;
    }

    async fn run_build_phase(self: Arc<Self>, job_id: JobId, cell_id: CellId) {
        let started = now_ms();
        let (handle, job_def, submitted_at) = {
            let mut inner = self.inner.lock();
            if let Some(c) = inner.cells.get_mut(&cell_id) {
                c.state = CellState::Running;
            }
            let (job_def, submitted_at) = match inner.jobs.get_mut(&job_id) {
                Some(jr) => {
                    jr.state = JobState::Building;
                    jr.started_at_ms = Some(started);
                    jr.provision_latency_ms = Some(started.saturating_sub(jr.job.submitted_at_ms));
                    (jr.job.clone(), jr.job.submitted_at_ms)
                }
                None => return,
            };
            let handle = inner.cells.get(&cell_id).and_then(|c| c.handle.clone());
            match handle {
                Some(h) => (h, job_def, submitted_at),
                None => {
                    drop(inner);
                    self.fail_provision(&job_id, &cell_id, "cell has no handle at build time");
                    return;
                }
            }
        };
        let _ = submitted_at;

        let bus = self
            .logs
            .entry(job_id.clone())
            .or_insert_with(|| Arc::new(LogBus::new()))
            .clone();

        // mpsc -> LogBus forwarder. run_build owns the sender; when it returns,
        // the sender drops and this loop ends.
        let (tx, mut rx): (LogSink, mpsc::UnboundedReceiver<LogLine>) = mpsc::unbounded_channel();
        let bus_fwd = bus.clone();
        let fwd = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                bus_fwd.push(line);
            }
        });

        let result = self.backend.run_build(&handle, &job_def, tx).await;
        let _ = fwd.await;
        bus.mark_done();

        {
            let mut inner = self.inner.lock();
            if let Some(jr) = inner.jobs.get_mut(&job_id) {
                match &result {
                    Ok(res) => {
                        jr.state = res.job_state();
                        jr.exit_code = Some(res.exit_code);
                        jr.finished_at_ms = Some(res.finished_at_ms);
                    }
                    Err(e) => {
                        jr.state = JobState::Failed;
                        jr.exit_code = Some(-1);
                        jr.finished_at_ms = Some(now_ms());
                        bus.push(LogLine {
                            ts_ms: now_ms(),
                            stream: LogStream::System,
                            line: format!("backend error: {e}"),
                        });
                    }
                }
                info!(job = %job_id, state = ?jr.state, latency_ms = ?jr.provision_latency_ms, "build finished");
            }
        }

        self.reap_finished_jobs();
        self.teardown_cell(&cell_id).await;
    }

    /// Release a cell: terminate it, free its box capacity, drop the record,
    /// and decrement the running-build counter. Then wake the scheduler.
    async fn teardown_cell(self: Arc<Self>, cell_id: &CellId) {
        let handle = {
            let mut inner = self.inner.lock();
            if let Some(c) = inner.cells.get_mut(cell_id) {
                c.state = CellState::Draining;
            }
            inner.cells.get(cell_id).and_then(|c| c.handle.clone())
        };
        if let Some(h) = handle {
            if let Err(e) = self.backend.terminate(&h).await {
                warn!(cell = %cell_id, error = %e, "terminate failed");
            }
        }
        {
            let mut inner = self.inner.lock();
            if let Some(cell) = inner.cells.remove(cell_id) {
                if let Some(b) = inner.boxes.iter_mut().find(|b| b.id == cell.box_id) {
                    b.release(&cell.resources);
                }
            }
            inner.running_builds = inner.running_builds.saturating_sub(1);
        }
        self.wake.notify_one();
    }

    /// Handle a failed cold provision: mark cell failed, free capacity, fail job.
    fn fail_provision(&self, job_id: &JobId, cell_id: &CellId, msg: &str) {
        {
            let mut inner = self.inner.lock();
            if let Some(cell) = inner.cells.remove(cell_id) {
                if let Some(b) = inner.boxes.iter_mut().find(|b| b.id == cell.box_id) {
                    b.release(&cell.resources);
                }
            }
            if let Some(jr) = inner.jobs.get_mut(job_id) {
                jr.state = JobState::Failed;
                jr.exit_code = Some(-1);
                jr.finished_at_ms = Some(now_ms());
            }
            inner.running_builds = inner.running_builds.saturating_sub(1);
        }
        if let Some(bus) = self.logs.get(job_id) {
            bus.push(LogLine {
                ts_ms: now_ms(),
                stream: LogStream::System,
                line: msg.to_string(),
            });
            bus.mark_done();
        }
        self.reap_finished_jobs();
        self.wake.notify_one();
    }

    // ---- autoscaler / warm pool ---------------------------------------

    async fn autoscaler_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.cfg.autoscaler_interval).await;
            self.clone().reconcile_warm_pool();
            // Backstop for any terminal transition that does not run the
            // post-build reap (a job failed before `run_build_phase`, a future
            // cancellation path): retention must not depend on every writer
            // remembering to call it.
            self.reap_finished_jobs();
        }
    }

    /// Bring each image's warm pool toward its target, and reap extras/stale.
    fn reconcile_warm_pool(self: Arc<Self>) {
        // 1) Scale up.
        for image in self.cfg.warm_images() {
            let target = self.cfg.warm_target_for(&image);
            loop {
                let spawn_cell = {
                    let mut inner = self.inner.lock();
                    let current = inner.warm.get(&image).map(|p| p.len()).unwrap_or(0)
                        + inner.warm_pending.get(&image).copied().unwrap_or(0);
                    if current >= target {
                        None
                    } else if let Some(bidx) = inner.pick_box(&self.cfg.warm_spec) {
                        let box_id = inner.boxes[bidx].id.clone();
                        inner.boxes[bidx].reserve(&self.cfg.warm_spec);
                        let cell_id = CellId::new();
                        inner.cells.insert(
                            cell_id.clone(),
                            CellRecord {
                                id: cell_id.clone(),
                                box_id,
                                state: CellState::Provisioning,
                                image: image.clone(),
                                resources: self.cfg.warm_spec.clone(),
                                job: None,
                                created_at_ms: now_ms(),
                                became_warm_at_ms: None,
                                handle: None,
                            },
                        );
                        *inner.warm_pending.entry(image.clone()).or_default() += 1;
                        Some(cell_id)
                    } else {
                        None // no capacity for more warm cells right now
                    }
                };
                match spawn_cell {
                    Some(cell_id) => {
                        let h = self.clone();
                        let img = image.clone();
                        tokio::spawn(async move { h.provision_warm(img, cell_id).await });
                    }
                    None => break,
                }
            }
        }

        // 2) Reap extras and stale cells.
        let ttl_ms = self.cfg.warm_idle_ttl.as_millis() as u64;
        let now = now_ms();
        let mut reap: Vec<CellId> = Vec::new();
        {
            let mut inner = self.inner.lock();
            let images: Vec<String> = inner.warm.keys().cloned().collect();
            for image in images {
                let target = self.cfg.warm_target_for(&image);
                // Pop from the back while over target; recycle stale ones too.
                if let Some(pool) = inner.warm.get_mut(&image) {
                    let mut keep: VecDeque<CellId> = VecDeque::new();
                    while let Some(cid) = pool.pop_front() {
                        keep.push_back(cid);
                    }
                    *pool = VecDeque::new();
                    // Decide per cell.
                    let mut kept = 0usize;
                    for cid in keep {
                        let stale = inner
                            .cells
                            .get(&cid)
                            .and_then(|c| c.became_warm_at_ms)
                            .map(|t| now.saturating_sub(t) > ttl_ms)
                            .unwrap_or(false);
                        let over = kept >= target;
                        if over || stale {
                            if let Some(c) = inner.cells.get_mut(&cid) {
                                c.state = CellState::Draining;
                            }
                            reap.push(cid);
                        } else {
                            inner.warm.get_mut(&image).unwrap().push_back(cid);
                            kept += 1;
                        }
                    }
                }
            }
        }
        for cid in reap {
            let h = self.clone();
            tokio::spawn(async move { h.teardown_warm(cid).await });
        }
    }

    async fn provision_warm(self: Arc<Self>, image: String, cell_id: CellId) {
        let spec = {
            let inner = self.inner.lock();
            match inner.cells.get(&cell_id) {
                Some(c) => CellSpec {
                    id: c.id.clone(),
                    image: c.image.clone(),
                    resources: c.resources.clone(),
                    // Build cells aren't tenant-attributed yet (follow-up).
                    tenant: String::new(),
                    container: None, // build cell, not a container
                },
                None => return,
            }
        };
        match self.backend.provision(&spec).await {
            Ok(handle) => {
                {
                    let mut inner = self.inner.lock();
                    if let Some(c) = inner.cells.get_mut(&cell_id) {
                        c.handle = Some(handle);
                        c.state = CellState::Warm;
                        c.became_warm_at_ms = Some(now_ms());
                    }
                    inner
                        .warm
                        .entry(image.clone())
                        .or_default()
                        .push_back(cell_id.clone());
                    if let Some(p) = inner.warm_pending.get_mut(&image) {
                        *p = p.saturating_sub(1);
                    }
                }
                debug!(cell = %cell_id, image = %image, "warm cell ready");
                // A new warm cell might unblock a queued job.
                self.wake.notify_one();
            }
            Err(e) => {
                warn!(cell = %cell_id, image = %image, error = %e, "warm provision failed");
                let mut inner = self.inner.lock();
                if let Some(cell) = inner.cells.remove(&cell_id) {
                    if let Some(b) = inner.boxes.iter_mut().find(|b| b.id == cell.box_id) {
                        b.release(&cell.resources);
                    }
                }
                if let Some(p) = inner.warm_pending.get_mut(&image) {
                    *p = p.saturating_sub(1);
                }
            }
        }
    }

    /// Terminate a warm cell selected for reaping (already removed from pool).
    async fn teardown_warm(self: Arc<Self>, cell_id: CellId) {
        let handle = {
            let inner = self.inner.lock();
            inner.cells.get(&cell_id).and_then(|c| c.handle.clone())
        };
        if let Some(h) = handle {
            let _ = self.backend.terminate(&h).await;
        }
        let mut inner = self.inner.lock();
        if let Some(cell) = inner.cells.remove(&cell_id) {
            if let Some(b) = inner.boxes.iter_mut().find(|b| b.id == cell.box_id) {
                b.release(&cell.resources);
            }
        }
        debug!(cell = %cell_id, "warm cell reaped");
    }
}
