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

// ---------------------------------------------------------------------------
// Runtime — the ONE source of truth for "what language/engine executes this
// function's process", replacing what used to be FOUR independent copies of
// argv-basename sniffing scattered across hive-cloud/git.rs, hive-backend's
// mock.rs and firecracker.rs, and hive-cell-agent/main.rs. Orthogonal to
// `FunctionConfig::protocol` (wire protocol) and to package-manager choice (a
// BUILD-time-only concept that never reaches this struct) — a project can use
// `bun install` while still running on `runtime=nodejs`, and vice versa. Lives
// in `hive-core` (not `fluid-core`) so it's reachable from `hive-cell-agent`
// and `hive-backend`, neither of which depend on `fluid-core`.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Node,
    Bun,
    Python,
    /// Podman container (the `__container__` start_cmd sentinel).
    Container,
    /// Anything else — a raw command/binary, or genuinely unknown.
    Command,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::Command
    }
}

impl Runtime {
    /// The canonical lowercase name, matching `FunctionConfig.runtime` values
    /// this platform WRITES going forward ("nodejs"/"bun"/"python"/"container").
    pub fn as_str(&self) -> &'static str {
        match self {
            Runtime::Node => "nodejs",
            Runtime::Bun => "bun",
            Runtime::Python => "python",
            Runtime::Container => "container",
            Runtime::Command => "command",
        }
    }

    /// Parse a persisted/configured `FunctionConfig.runtime` string. Backward
    /// compatible with every value this codebase has ever written: `"auto"`
    /// (the build pipeline's historical "infer it" sentinel) and `""`/`"command"`
    /// both mean "no explicit runtime — caller must fall back to
    /// [`Runtime::infer_from_argv`]", returned here as `None` rather than
    /// guessing `Command` so a real basename-based inference still happens.
    pub fn from_config_str(s: &str) -> Option<Runtime> {
        match s.trim().to_ascii_lowercase().as_str() {
            "node" | "nodejs" | "js" | "edge" | "isolate" | "edge-isolate" => Some(Runtime::Node),
            "bun" => Some(Runtime::Bun),
            "python" | "py" => Some(Runtime::Python),
            "container" | "docker" | "microvm" | "firecracker" => Some(Runtime::Container),
            "" | "auto" | "command" => None,
            _ => None,
        }
    }

    /// Infer the runtime from a raw argv (`start_cmd`) — the fallback path when
    /// no explicit config value was resolved. Matches on the basename so an
    /// absolute path (`/usr/bin/node`, `.../bin/bun`) still counts. This is the
    /// SINGLE canonical replacement for the repo's four independent
    /// `is_node_start_cmd`-style helpers; `bun` and `node` are deliberately
    /// DISTINGUISHED here (the old helpers conflated them, which silently made
    /// Bun processes "eligible" for a V8-only bytecode cache that could never
    /// produce anything for them).
    pub fn infer_from_argv(start_cmd: &[String]) -> Runtime {
        if start_cmd.first().map(String::as_str) == Some("__container__") {
            return Runtime::Container;
        }
        let Some(first) = start_cmd.first() else { return Runtime::Command };
        let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
        match base {
            "bun" | "bunx" => Runtime::Bun,
            "node" | "npm" | "npx" | "pnpm" | "yarn" | "next" => Runtime::Node,
            "python" | "python3" => Runtime::Python,
            _ => Runtime::Command,
        }
    }

    /// Resolve the effective runtime for a function: an explicit config value
    /// wins; otherwise infer from argv. This is the ONE call every producer
    /// (build pipeline) and consumer (fluid-compute, backends, guest agent)
    /// should use instead of re-deriving it independently.
    pub fn resolve(config_runtime: &str, start_cmd: &[String]) -> Runtime {
        Runtime::from_config_str(config_runtime).unwrap_or_else(|| Runtime::infer_from_argv(start_cmd))
    }

    /// Does this runtime use Node's V8 `NODE_COMPILE_CACHE` mechanism? Only
    /// genuine Node — Bun uses JavaScriptCore and has its own, structurally
    /// different bytecode-cache path (build-time bundling with `bun build
    /// --bytecode`, not a runtime env var).
    pub fn uses_v8_compile_cache(&self) -> bool {
        matches!(self, Runtime::Node)
    }

    /// Does this runtime have a Bun-native ahead-of-time bytecode cache path?
    pub fn uses_bun_bytecode_cache(&self) -> bool {
        matches!(self, Runtime::Bun)
    }
}

/// One UDP port publish for a CONTAINER function: the port the app listens on
/// INSIDE its container and the loopback host port podman publishes it on
/// (`-p 127.0.0.1:<host_port>:<container_port>/udp`). Host ports are chosen by
/// fluid-compute's `cold_start` (which also records the mapping on the instance
/// registry so hive-cloud's UDP relay can resolve the local datagram leg); they
/// ride here so the backends emit the matching `-p …/udp` publish flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpPublish {
    pub container_port: u16,
    pub host_port: u16,
}

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
    /// Memory ceiling (MiB) for a CONTAINER function's cgroup (podman `--memory`).
    /// 0 = use the node's generous default. Ignored for microVM/process functions.
    #[serde(default)]
    pub memory_mib: u32,
    /// CPU quota for a CONTAINER function's cgroup (podman `--cpus`), e.g. 4.0.
    /// 0.0 = use the node's generous default. Clamped fleet-wide in
    /// `ContainerLimits::for_container`. Ignored for microVM/process functions
    /// (distinct from a microVM's vCPU count, sized elsewhere).
    #[serde(default)]
    pub cpus: f64,
    /// Max-PIDs ceiling for a CONTAINER function's cgroup (podman `--pids-limit`)
    /// — a fork-bomb guard. 0 = use the node's default. Clamped fleet-wide.
    /// Ignored for microVM/process functions.
    #[serde(default)]
    pub pids: u32,
    /// The resolved runtime — the SINGLE explicit signal every backend/guest
    /// agent uses to decide bytecode-cache behavior, replacing ad hoc argv
    /// re-sniffing. Always set explicitly by the constructor (fluid-compute's
    /// `cold_start`); `#[serde(default)]` only guards deserialization of a
    /// hypothetical older/foreign message, never relied on as real inference.
    #[serde(default)]
    pub runtime: Runtime,
    /// The function speaks a NON-HTTP application protocol
    /// (`fluid_core::FunctionConfig::needs_raw_proxy()`: gRPC / raw TCP / UDP —
    /// e.g. Postgres wire, Minecraft). The backend fronting the function must
    /// serve its local tunnel hop as a RAW byte splice
    /// (`fluid_tunnel::TunnelServer::serve_raw`) instead of the HTTP-framed
    /// `serve` path, whose request-line writing + chunked decoding would
    /// corrupt non-HTTP bytes. Set explicitly by fluid-compute's `cold_start`;
    /// `false` (the default, and the wire-compat default for older messages)
    /// keeps the existing HTTP-framed path byte-identical.
    #[serde(default)]
    pub raw_proxy: bool,
    /// UDP ports a CONTAINER function publishes on loopback (see [`UdpPublish`]).
    /// Empty for every non-container function and for container functions that
    /// declare no UDP port specs; `#[serde(default)]` keeps older/foreign
    /// messages wire-compatible. Ignored by the microVM/process paths.
    #[serde(default)]
    pub udp_ports: Vec<UdpPublish>,
}

fn default_max_conc() -> u32 {
    10
}

/// A single argv command to run inside an already-booted, long-lived cell —
/// the Sandboxes exec path. Deliberately separate from [`BuildJob`]/`Run`
/// (build-only: shell-string steps, merged stdout/stderr, fixed `/build` cwd,
/// no kill support, cell self-destructs after one build): Exec preserves argv
/// (no shell unless `shell` is explicitly set), keeps stdout/stderr distinct,
/// supports an explicit cwd, and can be killed by `id` from a separate
/// connection while it's still running.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Caller-chosen id (echoed back on every `ExecOutput`/`ExecDone` event and
    /// used to target `KillExec`) — the platform's `SandboxCommandRecord.id`.
    pub id: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Empty = agent picks its default working directory.
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Run as root. The agent honors this only if a `sudo` binary is present in
    /// the guest; otherwise it fails the request loudly (`ExecDone` with a
    /// nonzero/absent exit code), never silently drops the privilege request.
    #[serde(default)]
    pub sudo: bool,
    /// Explicit, informed opt-in to shell interpretation (`sh -c "<cmd> <args
    /// joined>"`). Default false = argv exec (`execvp(cmd, args)`), which makes
    /// shell injection structurally impossible for the default path.
    #[serde(default)]
    pub shell: bool,
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
    /// Run one argv command (Sandboxes). Sent on its OWN dedicated connection;
    /// the agent spawns a thread for it and keeps accepting other connections
    /// (unlike `Run`, this does NOT stop the accept loop).
    Exec(ExecRequest),
    /// Kill a still-running `Exec` by id, sent on any (typically a fresh)
    /// connection — the agent tracks live exec child PIDs in a process-global
    /// registry keyed by `ExecRequest.id`.
    KillExec { id: String },
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
    /// One line of an `Exec`'s stdout/stderr (`LogStream::System` unused here).
    ExecOutput { id: String, stream: LogStream, line: String },
    /// An `Exec` finished. `exit_code = None` means it was killed or the agent
    /// could not determine an exit status (e.g. sudo requested but
    /// unavailable) — the caller must NEVER treat `None` as success.
    ExecDone { id: String, exit_code: Option<i32> },
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn config_str_parsing_is_backward_compatible() {
        assert_eq!(Runtime::from_config_str("node"), Some(Runtime::Node));
        assert_eq!(Runtime::from_config_str("nodejs"), Some(Runtime::Node));
        assert_eq!(Runtime::from_config_str("edge"), Some(Runtime::Node));
        assert_eq!(Runtime::from_config_str("bun"), Some(Runtime::Bun));
        assert_eq!(Runtime::from_config_str("BUN"), Some(Runtime::Bun));
        assert_eq!(Runtime::from_config_str("python"), Some(Runtime::Python));
        assert_eq!(Runtime::from_config_str("container"), Some(Runtime::Container));
        assert_eq!(Runtime::from_config_str("firecracker"), Some(Runtime::Container));
        // Legacy sentinels that must fall through to argv inference.
        assert_eq!(Runtime::from_config_str("auto"), None);
        assert_eq!(Runtime::from_config_str("command"), None);
        assert_eq!(Runtime::from_config_str(""), None);
        assert_eq!(Runtime::from_config_str("bogus"), None);
    }

    #[test]
    fn argv_inference_distinguishes_node_and_bun() {
        assert_eq!(Runtime::infer_from_argv(&["node".into(), "server.js".into()]), Runtime::Node);
        assert_eq!(Runtime::infer_from_argv(&["/usr/local/bin/node".into()]), Runtime::Node);
        assert_eq!(Runtime::infer_from_argv(&["npm".into(), "start".into()]), Runtime::Node);
        assert_eq!(Runtime::infer_from_argv(&["bun".into(), "run".into(), "server.js".into()]), Runtime::Bun);
        assert_eq!(Runtime::infer_from_argv(&["bunx".into(), "next".into()]), Runtime::Bun);
        assert_eq!(Runtime::infer_from_argv(&["python3".into(), "app.py".into()]), Runtime::Python);
        assert_eq!(Runtime::infer_from_argv(&["__container__".into(), "img".into()]), Runtime::Container);
        assert_eq!(Runtime::infer_from_argv(&["./my-binary".into()]), Runtime::Command);
        assert_eq!(Runtime::infer_from_argv(&[]), Runtime::Command);
    }

    #[test]
    fn resolve_prefers_explicit_config_over_argv_inference() {
        // Explicit "bun" wins even if argv looks like node (e.g. a proxy shim).
        assert_eq!(Runtime::resolve("bun", &["node".into(), "server.js".into()]), Runtime::Bun);
        // "auto"/"command"/empty defer to argv.
        assert_eq!(Runtime::resolve("auto", &["bun".into(), "server.js".into()]), Runtime::Bun);
        assert_eq!(Runtime::resolve("command", &["node".into()]), Runtime::Node);
        assert_eq!(Runtime::resolve("", &["python3".into()]), Runtime::Python);
    }

    #[test]
    fn bytecode_cache_eligibility_is_runtime_specific() {
        assert!(Runtime::Node.uses_v8_compile_cache());
        assert!(!Runtime::Bun.uses_v8_compile_cache());
        assert!(Runtime::Bun.uses_bun_bytecode_cache());
        assert!(!Runtime::Node.uses_bun_bytecode_cache());
        assert!(!Runtime::Python.uses_v8_compile_cache() && !Runtime::Python.uses_bun_bytecode_cache());
    }

    #[test]
    fn function_launch_serializes_runtime_field() {
        let launch = FunctionLaunch {
            start_cmd: vec!["bun".into(), "run".into(), "server.js".into()],
            env: Default::default(),
            workdir: None,
            port: 3000,
            max_concurrency: 10,
            memory_mib: 0,
            cpus: 0.0,
            pids: 0,
            runtime: Runtime::Bun,
            raw_proxy: false,
            udp_ports: Vec::new(),
        };
        let json = serde_json::to_string(&launch).unwrap();
        assert!(json.contains("\"runtime\":\"bun\""));
        let back: FunctionLaunch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime, Runtime::Bun);
    }

    #[test]
    fn function_launch_runtime_defaults_when_absent_from_wire() {
        // A hypothetical older/foreign message with no `runtime` key must still
        // deserialize (never a hard wire-compat break).
        let json = r#"{"start_cmd":["node","server.js"],"env":{},"workdir":null,"port":3000,"max_concurrency":10,"memory_mib":0}"#;
        let launch: FunctionLaunch = serde_json::from_str(json).unwrap();
        assert_eq!(launch.runtime, Runtime::Command);
    }
}
