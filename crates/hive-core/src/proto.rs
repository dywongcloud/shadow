//! Wire types shared between the Control Plane, Box Daemon, Cell Daemon, API,
//! and the CLI. In the MVP these travel in-process over channels and over HTTP
//! (API <-> client); the same shapes would serialize over sockets/vsock for a
//! genuinely distributed deployment.

use crate::ids::{BoxId, CellId, HiveId, JobId};
use crate::job::{BuildJob, ResourceSpec};
use crate::state::{CellState, JobState};
use serde::{Deserialize, Serialize};

/// A single line of build output, as streamed from a cell.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    pub ts_ms: u64,
    pub stream: LogStream,
    pub line: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
    /// Emitted by the control plane / daemons, not the build itself.
    System,
}

/// Result of a finished build, reported by the cell daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildResult {
    pub job_id: JobId,
    pub exit_code: i32,
    pub timed_out: bool,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
}

impl BuildResult {
    pub fn duration_ms(&self) -> u64 {
        self.finished_at_ms.saturating_sub(self.started_at_ms)
    }
    pub fn job_state(&self) -> JobState {
        if self.timed_out {
            JobState::TimedOut
        } else if self.exit_code == 0 {
            JobState::Succeeded
        } else {
            JobState::Failed
        }
    }
}

/// Public job view returned by the API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub state: JobState,
    pub image: String,
    pub assigned_cell: Option<CellId>,
    pub assigned_box: Option<BoxId>,
    pub submitted_at_ms: u64,
    pub scheduled_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    /// Milliseconds from submission to a cell starting work — the metric Hive
    /// optimizes with warm pools (90s -> 5s).
    pub provision_latency_ms: Option<u64>,
}

/// Public cell view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellView {
    pub id: CellId,
    pub box_id: BoxId,
    pub state: CellState,
    pub image: String,
    pub resources: ResourceSpec,
    pub job: Option<JobId>,
    pub created_at_ms: u64,
}

/// Public box view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoxView {
    pub id: BoxId,
    pub vcpus_total: u32,
    pub vcpus_used: u32,
    pub mem_total_mib: u32,
    pub mem_used_mib: u32,
    pub cells: usize,
    pub warm_cells: usize,
}

/// Whole-cluster snapshot returned by the API for `hivectl status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterStatus {
    pub hive: HiveId,
    pub boxes: Vec<BoxView>,
    pub cells: Vec<CellView>,
    pub jobs: Vec<JobView>,
    pub queued: usize,
}

/// API request to submit a build.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitRequest {
    #[serde(flatten)]
    pub job: BuildJob,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub job_id: JobId,
}

/// vsock port the in-guest cell agent listens on (host <-> cell daemon control).
pub const CELL_AGENT_PORT: u32 = 5252;

/// vsock port the agent bridges to the in-guest function server (data plane).
pub const CELL_FUNCTION_PORT: u32 = 5353;

/// Guest context id for the cell's vsock device.
pub const CELL_GUEST_CID: u32 = 3;

/// How to launch a long-lived function server inside a cell (Fluid compute).
/// The process MUST listen on `$PORT` (Vercel/Heroku convention).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionLaunch {
    /// argv of the server process, e.g. ["node", "server.js"].
    pub start_cmd: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// Working dir. For the mock backend this is a host path; inside a microVM
    /// it is the guest path where the deployment was delivered.
    pub workdir: Option<String>,
    /// Port the function server should bind ($PORT). Chosen by the backend.
    pub port: u16,
    /// Max concurrent requests one instance handles (tunnel server uses it to nack).
    #[serde(default = "default_max_conc")]
    pub max_concurrency: u32,
}

fn default_max_conc() -> u32 {
    10
}

/// Message the box daemon (host) sends to the cell daemon (guest) over vsock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentRequest {
    /// Run this build inside the cell.
    Run(BuildJob),
    /// Start a long-lived function server (Fluid compute serving path).
    StartFunction(FunctionLaunch),
    /// Box daemon -> agent: a restored cache tarball (empty on miss). Sent in
    /// reply to `AgentEvent::CacheGet` during a build.
    CacheData { tar: Vec<u8> },
    /// Liveness probe; agent replies with `AgentEvent::Pong`.
    Ping,
}

/// Messages the cell daemon streams back to the box daemon over vsock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    Pong,
    Log(LogLine),
    Done(BuildResult),
    /// Function server is up and accepting requests on its port; the agent is
    /// now bridging `CELL_FUNCTION_PORT` -> the function.
    FunctionReady,
    /// Function failed to start.
    FunctionError(String),
    /// Agent -> box daemon: please send the cached tarball for `key` (build
    /// cache restore). The box daemon replies with `AgentRequest::CacheData`.
    CacheGet { key: String, paths: Vec<String> },
    /// Agent -> box daemon: persist this cache tarball for `key` (build cache
    /// save, after a successful build).
    CachePut { key: String, tar: Vec<u8> },
}
