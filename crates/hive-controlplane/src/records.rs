//! In-memory bookkeeping records held by the control plane.

use hive_backend::CellHandle;
use hive_core::{
    BoxId, BoxView, BuildJob, CellId, CellState, CellView, JobId, JobState, JobView, ResourceSpec,
};

/// A box's live capacity ledger.
#[derive(Clone, Debug)]
pub struct BoxRecord {
    pub id: BoxId,
    pub vcpus_total: u32,
    pub mem_total_mib: u32,
    pub vcpus_used: u32,
    pub mem_used_mib: u32,
    pub cells: usize,
    pub warm_cells: usize,
}

impl BoxRecord {
    pub fn vcpus_free(&self) -> u32 {
        self.vcpus_total.saturating_sub(self.vcpus_used)
    }
    pub fn mem_free_mib(&self) -> u32 {
        self.mem_total_mib.saturating_sub(self.mem_used_mib)
    }
    pub fn can_fit(&self, r: &ResourceSpec) -> bool {
        self.vcpus_free() >= r.vcpus && self.mem_free_mib() >= r.mem_mib
    }
    pub fn reserve(&mut self, r: &ResourceSpec) {
        self.vcpus_used += r.vcpus;
        self.mem_used_mib += r.mem_mib;
        self.cells += 1;
    }
    pub fn release(&mut self, r: &ResourceSpec) {
        self.vcpus_used = self.vcpus_used.saturating_sub(r.vcpus);
        self.mem_used_mib = self.mem_used_mib.saturating_sub(r.mem_mib);
        self.cells = self.cells.saturating_sub(1);
    }
    pub fn view(&self) -> BoxView {
        BoxView {
            id: self.id.clone(),
            vcpus_total: self.vcpus_total,
            vcpus_used: self.vcpus_used,
            mem_total_mib: self.mem_total_mib,
            mem_used_mib: self.mem_used_mib,
            cells: self.cells,
            warm_cells: self.warm_cells,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CellRecord {
    pub id: CellId,
    pub box_id: BoxId,
    pub state: CellState,
    pub image: String,
    pub resources: ResourceSpec,
    pub job: Option<JobId>,
    pub created_at_ms: u64,
    pub became_warm_at_ms: Option<u64>,
    /// Populated once the backend finishes provisioning.
    pub handle: Option<CellHandle>,
}

impl CellRecord {
    pub fn view(&self) -> CellView {
        CellView {
            id: self.id.clone(),
            box_id: self.box_id.clone(),
            state: self.state,
            image: self.image.clone(),
            resources: self.resources.clone(),
            job: self.job.clone(),
            created_at_ms: self.created_at_ms,
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobRecord {
    pub job: BuildJob,
    pub state: JobState,
    pub assigned_cell: Option<CellId>,
    pub assigned_box: Option<BoxId>,
    pub scheduled_at_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub provision_latency_ms: Option<u64>,
}

impl JobRecord {
    pub fn new(job: BuildJob) -> Self {
        JobRecord {
            job,
            state: JobState::Queued,
            assigned_cell: None,
            assigned_box: None,
            scheduled_at_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            exit_code: None,
            provision_latency_ms: None,
        }
    }
    pub fn view(&self) -> JobView {
        JobView {
            id: self.job.id.clone(),
            state: self.state,
            image: self.job.image.clone(),
            assigned_cell: self.assigned_cell.clone(),
            assigned_box: self.assigned_box.clone(),
            submitted_at_ms: self.job.submitted_at_ms,
            scheduled_at_ms: self.scheduled_at_ms,
            finished_at_ms: self.finished_at_ms,
            exit_code: self.exit_code,
            provision_latency_ms: self.provision_latency_ms,
        }
    }
}
