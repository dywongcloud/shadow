//! Wire types shared between the Control Plane, Box Daemon, Cell Daemon, API and the CLI —
//! in-process over channels and over HTTP today, the same shapes over sockets/vsock when distributed.

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Node,
    Bun,
    Python,
    /// Podman container (the `__container__` start_cmd sentinel).
    Container,
    /// A WebAssembly/WASIX module run by the `wasmer` CLI as an ordinary host process under any
    /// backend; guest code must bind `0.0.0.0` (a `.wasm` module cannot take the loopback-rescue
    /// shim Node/Bun get).
    Wasmer,
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
            Runtime::Wasmer => "wasmer",
            Runtime::Command => "command",
        }
    }

    /// Parse a persisted `FunctionConfig.runtime`; `"auto"`, `""` and `"command"` mean "no explicit
    /// runtime" and return `None` so [`Runtime::infer_from_argv`] still runs.
    pub fn from_config_str(s: &str) -> Option<Runtime> {
        match s.trim().to_ascii_lowercase().as_str() {
            "node" | "nodejs" | "js" | "edge" | "isolate" | "edge-isolate" => Some(Runtime::Node),
            "bun" => Some(Runtime::Bun),
            "python" | "py" => Some(Runtime::Python),
            "container" | "docker" | "microvm" | "firecracker" => Some(Runtime::Container),
            "wasmer" | "wasm" | "wasix" | "wasi" => Some(Runtime::Wasmer),
            "" | "auto" | "command" => None,
            _ => None,
        }
    }

    /// Infer the runtime from argv by basename (absolute paths count); `bun` and `node` are
    /// deliberately distinguished — only V8 has the bytecode cache.
    pub fn infer_from_argv(start_cmd: &[String]) -> Runtime {
        if start_cmd.first().map(String::as_str) == Some("__container__") {
            return Runtime::Container;
        }
        let Some(first) = start_cmd.first() else {
            return Runtime::Command;
        };
        let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
        match base {
            "bun" | "bunx" => Runtime::Bun,
            "node" | "npm" | "npx" | "pnpm" | "yarn" | "next" => Runtime::Node,
            "python" | "python3" => Runtime::Python,
            "wasmer" => Runtime::Wasmer,
            _ => Runtime::Command,
        }
    }

    /// Resolve the effective runtime: an explicit config value wins, else infer from argv — the ONE
    /// call every producer and consumer uses instead of re-deriving it.
    pub fn resolve(config_runtime: &str, start_cmd: &[String]) -> Runtime {
        Runtime::from_config_str(config_runtime)
            .unwrap_or_else(|| Runtime::infer_from_argv(start_cmd))
    }

    /// Whether this runtime uses Node's V8 `NODE_COMPILE_CACHE`: only genuine Node (Bun's bytecode
    /// cache is a build-time `bun build --bytecode`, not a runtime env var).
    pub fn uses_v8_compile_cache(&self) -> bool {
        matches!(self, Runtime::Node)
    }

    /// Does this runtime have a Bun-native ahead-of-time bytecode cache path?
    pub fn uses_bun_bytecode_cache(&self) -> bool {
        matches!(self, Runtime::Bun)
    }
}

/// One UDP port publish for a CONTAINER function: the in-container port and the loopback host
/// port podman publishes it on (`-p 127.0.0.1:<host>:<container>/udp`); host ports are chosen
/// by `cold_start`, which also records the mapping for the UDP relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpPublish {
    pub container_port: u16,
    pub host_port: u16,
}

/// One TCP port publish for a CONTAINER function, [`UdpPublish`]'s stream-side twin: every
/// raw/published TCP port, the PRIMARY included (host = the launch's `port`), gets its own
/// loopback host port so the raw proxy can splice each directly instead of through the
/// HTTP-framed tunnel; `-p` emitters dedupe against the primary pairing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpPublish {
    pub container_port: u16,
    pub host_port: u16,
}

pub const RUNTIME_ARTIFACT_PROTOCOL_VERSION: u16 = 1;
pub const RUNTIME_ARTIFACT_PACKAGE_PROTOCOL_VERSION: u16 = 1;

/// Complete host-to-guest wire contract version: adding, removing, defaulting or reinterpreting
/// any AgentRequest/AgentEvent variant or FunctionLaunch field MUST increment it;
/// runtime-artifact identity is versioned independently.
pub const AGENT_WIRE_PROTOCOL_VERSION: u16 = 2;
/// The peer implements the complete AgentRequest/AgentEvent v2 schema.
pub const AGENT_WIRE_CAPABILITY_COMPLETE_SCHEMA: u64 = 1 << 0;
/// The peer implements every FunctionLaunch v2 field and its launch semantics.
pub const AGENT_WIRE_CAPABILITY_FUNCTION_LAUNCH: u64 = 1 << 1;
/// The peer validates and returns the exact rootfs/agent boot proof during the
/// challenge handshake, before it accepts a tenant launch.
pub const AGENT_WIRE_CAPABILITY_AUTHENTICATED_BOOT: u64 = 1 << 2;
/// Node-attributed launch failures use a typed event whose origin is independent
/// of tenant-controlled process diagnostics.
pub const AGENT_WIRE_CAPABILITY_TYPED_FUNCTION_FAULTS: u64 = 1 << 3;
/// The peer implements the `ExecPty`/`PtyInput`/`PtyResize`/`PtyOutput`/`PtyExited` session (a
/// real `openpty`/`forkpty` shell, not line-buffered `Exec`).
pub const AGENT_WIRE_CAPABILITY_EXEC_PTY: u64 = 1 << 4;
/// Exact capability set for AGENT_WIRE_PROTOCOL_VERSION. Peers require equality,
/// not subset negotiation, so an unknown launch shape is never silently dropped.
pub const AGENT_WIRE_CAPABILITIES: u64 = AGENT_WIRE_CAPABILITY_COMPLETE_SCHEMA
    | AGENT_WIRE_CAPABILITY_FUNCTION_LAUNCH
    | AGENT_WIRE_CAPABILITY_AUTHENTICATED_BOOT
    | AGENT_WIRE_CAPABILITY_TYPED_FUNCTION_FAULTS
    | AGENT_WIRE_CAPABILITY_EXEC_PTY;
pub const AGENT_HANDSHAKE_NONCE_BYTES: usize = 32;
pub const AGENT_HANDSHAKE_TRANSCRIPT_DOMAIN: &[u8] = b"hive-agent-wire-handshake-v2\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWireProtocol {
    pub protocol: u16,
    pub capabilities: u64,
}

impl AgentWireProtocol {
    pub const fn current() -> Self {
        Self {
            protocol: AGENT_WIRE_PROTOCOL_VERSION,
            capabilities: AGENT_WIRE_CAPABILITIES,
        }
    }
}

pub const RUNTIME_ARTIFACT_MARKER_FILE: &str = ".hive-runtime-artifact-v1.json";
/// Read-only marker baked into the guest rootfs beside the exact cell-agent
/// binary that implements both independently-versioned protocols.
pub const RUNTIME_ARTIFACT_ROOTFS_MARKER_PATH: &str = "/etc/hive/runtime-artifact-protocol.json";
/// Host-side content proof beside `<image>.ext4`, verified against the exact image bytes before
/// advertising or booting; existence alone is never a capability.
pub const RUNTIME_ARTIFACT_ROOTFS_SIDECAR_SUFFIX: &str = ".runtime-artifact-protocol.json";
pub const RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION: u16 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactRootfsMarker {
    pub schema: u16,
    /// Runtime-artifact identity protocol, independent from agent wire protocol.
    pub protocol: u16,
    pub agent_wire_protocol: u16,
    pub agent_wire_capabilities: u64,
    pub agent_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactRootfsMetadata {
    pub schema: u16,
    /// Runtime-artifact identity protocol, independent from agent wire protocol.
    pub protocol: u16,
    pub agent_wire_protocol: u16,
    pub agent_wire_capabilities: u64,
    pub agent_sha256: String,
    pub image_sha256: String,
    pub image_bytes: u64,
}

/// Exact boot fact authenticated by the host's whole-image verification, the
/// guest's running-executable digest check, and a fresh same-connection nonce.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBootProof {
    pub rootfs_schema: u16,
    pub runtime_artifact_protocol: u16,
    pub agent_wire_protocol: u16,
    pub agent_wire_capabilities: u64,
    pub agent_sha256: String,
    pub rootfs_image_sha256: String,
    pub rootfs_image_bytes: u64,
}

impl RuntimeArtifactRootfsMetadata {
    pub fn agent_boot_proof(&self) -> AgentBootProof {
        AgentBootProof {
            rootfs_schema: self.schema,
            runtime_artifact_protocol: self.protocol,
            agent_wire_protocol: self.agent_wire_protocol,
            agent_wire_capabilities: self.agent_wire_capabilities,
            agent_sha256: self.agent_sha256.clone(),
            rootfs_image_sha256: self.image_sha256.clone(),
            rootfs_image_bytes: self.image_bytes,
        }
    }
}

/// First application frame on a versioned host-to-guest connection; the nonce prevents replay,
/// `expected_boot` is verified by both sides independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHandshake {
    pub nonce: String,
    pub expected_boot: AgentBootProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHandshakeReady {
    pub nonce: String,
    pub proof: AgentBootProof,
    pub transcript_sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProtocolFaultCode {
    Malformed,
    HandshakeRequired,
    UnsupportedWireProtocol,
    CapabilityMismatch,
    RuntimeArtifactProtocolMismatch,
    InvalidNonce,
    Replay,
    DuplicateHandshake,
    OutOfOrder,
    RootfsProofMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProtocolFault {
    pub code: AgentProtocolFaultCode,
    pub message: String,
}

impl AgentProtocolFault {
    pub fn new(code: AgentProtocolFaultCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFunctionFaultCode {
    NodeImageMissing,
    NodeRuntimeMissing,
}

/// A node-attributed function-start failure, built only from typed platform error origins;
/// tenant stderr stays in `AgentEvent::FunctionError` and can never select this code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFunctionFault {
    pub code: AgentFunctionFaultCode,
    pub message: String,
}

impl AgentFunctionFault {
    pub fn new(code: AgentFunctionFaultCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Canonical bytes hashed into `AgentHandshakeReady::transcript_sha256`: freshness, exact rootfs
/// bytes, packaged agent digest and both protocol versions.
pub fn agent_handshake_transcript(nonce: &str, proof: &AgentBootProof) -> Vec<u8> {
    let nonce = nonce.as_bytes();
    let agent = proof.agent_sha256.as_bytes();
    let image = proof.rootfs_image_sha256.as_bytes();
    let mut transcript = Vec::with_capacity(
        AGENT_HANDSHAKE_TRANSCRIPT_DOMAIN.len() + nonce.len() + agent.len() + image.len() + 64,
    );
    transcript.extend_from_slice(AGENT_HANDSHAKE_TRANSCRIPT_DOMAIN);
    transcript.extend_from_slice(&(nonce.len() as u64).to_be_bytes());
    transcript.extend_from_slice(nonce);
    transcript.extend_from_slice(&proof.rootfs_schema.to_be_bytes());
    transcript.extend_from_slice(&proof.runtime_artifact_protocol.to_be_bytes());
    transcript.extend_from_slice(&proof.agent_wire_protocol.to_be_bytes());
    transcript.extend_from_slice(&proof.agent_wire_capabilities.to_be_bytes());
    transcript.extend_from_slice(&(agent.len() as u64).to_be_bytes());
    transcript.extend_from_slice(agent);
    transcript.extend_from_slice(&(image.len() as u64).to_be_bytes());
    transcript.extend_from_slice(image);
    transcript.extend_from_slice(&proof.rootfs_image_bytes.to_be_bytes());
    transcript
}

/// Immutable identity of the runtime tree attached to a cell: derived by the host, echoed by the
/// guest only after validating the mounted marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactIdentity {
    pub protocol: u16,
    pub id: String,
    pub content_sha256: String,
}

/// Transfer identity of the package carrying one semantic runtime tree: the package digest
/// addresses transport bytes, `semantic_tree_sha256` is the execution identity every target
/// recomputes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactPackageDescriptor {
    pub protocol: u16,
    pub package_sha256: String,
    pub semantic_tree_sha256: String,
    pub package_bytes: u64,
    pub logical_bytes: u64,
    pub materialized_bytes: u64,
    pub entries: u64,
    pub app_rel: String,
    pub include_rel: Vec<String>,
}

/// How to launch a long-lived function server inside a cell (Fluid compute).
/// The process MUST listen on `$PORT` (Vercel/Heroku convention).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionLaunch {
    /// argv of the server process, e.g. ["node", "server.js"].
    pub start_cmd: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// Working dir. For the mock backend this is a host path; inside a microVM
    /// it is the guest path where the deployment was delivered.
    pub workdir: Option<String>,
    /// Exact isolated-runtime artifact expected by the host; `None` for same-host/container paths.
    /// Isolated hosts always send `Some` and require the guest's validated echo before
    /// `FunctionReady`; the frozen 13-field pre-v2 frame omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_artifact: Option<RuntimeArtifactIdentity>,
    /// Port the function server should bind ($PORT). Chosen by the backend.
    pub port: u16,
    /// Max concurrent requests one instance handles (tunnel server uses it to nack).
    #[serde(default = "default_max_conc")]
    pub max_concurrency: u32,
    /// Memory ceiling (MiB) for a CONTAINER function's cgroup (podman `--memory`).
    /// 0 = use the node's generous default. Ignored for microVM/process functions.
    #[serde(default)]
    pub memory_mib: u32,
    /// CPU quota for a CONTAINER cgroup (podman `--cpus`); 0.0 = node default, clamped in
    /// `ContainerLimits::for_container`, ignored for microVM/process functions.
    #[serde(default)]
    pub cpus: f64,
    /// Max-PIDs ceiling for a CONTAINER cgroup (podman `--pids-limit`), a fork-bomb guard; 0 = node
    /// default, clamped fleet-wide, ignored for microVM/process functions.
    #[serde(default)]
    pub pids: u32,
    /// The resolved runtime — the single signal every backend/guest uses for bytecode-cache
    /// decisions; always set by `cold_start`, `serde(default)` only for older messages.
    #[serde(default)]
    pub runtime: Runtime,
    /// The function speaks a non-HTTP protocol (`FunctionConfig::needs_raw_proxy()`: gRPC / raw TCP
    /// / UDP), so the backend serves its local tunnel hop as a raw splice (`TunnelServer::serve_raw`),
    /// never the HTTP-framed path. Set by `cold_start`; `false` is the wire-compat default.
    #[serde(default)]
    pub raw_proxy: bool,
    /// UDP ports a CONTAINER function publishes on loopback ([`UdpPublish`]); empty for non-container
    /// functions and pre-upgrade peers, ignored by microVM/process paths.
    #[serde(default)]
    pub udp_ports: Vec<UdpPublish>,
    /// TCP ports a CONTAINER function publishes on loopback ([`TcpPublish`], primary included); same
    /// wire-compat posture as `udp_ports`.
    #[serde(default)]
    pub tcp_ports: Vec<TcpPublish>,
    /// Pass the host's GPUs into a CONTAINER cell (CDI `nvidia.com/gpu=all`, gVisor `nvproxy` under
    /// `runsc`); set from `FunctionConfig::gpu`, `false` is the wire-compat default, ignored by
    /// microVM/process paths.
    #[serde(default)]
    pub gpu: bool,
}

fn default_max_conc() -> u32 {
    10
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRequestFrameKind {
    Handshake,
    StartFunction,
    Other,
}

/// Inspect only the raw JSON envelope before typed deserialization, so an added outer key cannot
/// be ignored by a permissive schema.
pub fn agent_request_frame_kind(frame: &[u8]) -> Result<AgentRequestFrameKind, String> {
    let value: serde_json::Value =
        serde_json::from_slice(frame).map_err(|error| format!("invalid JSON: {error}"))?;
    match value {
        serde_json::Value::String(_) => Ok(AgentRequestFrameKind::Other),
        serde_json::Value::Object(fields) if fields.len() == 1 => {
            match fields.keys().next().map(String::as_str) {
                Some("Handshake") => Ok(AgentRequestFrameKind::Handshake),
                Some("StartFunction") => Ok(AgentRequestFrameKind::StartFunction),
                Some(_) => Ok(AgentRequestFrameKind::Other),
                None => Err("agent request envelope is empty".to_string()),
            }
        }
        serde_json::Value::Object(_) => {
            Err("agent request envelope must contain exactly one variant".to_string())
        }
        _ => Err("agent request envelope must be a variant string or singleton object".to_string()),
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(untagged)]
enum ExactNullableRuntimeArtifact {
    Null(()),
    Identity(ExactRuntimeArtifactIdentity),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactBootProof {
    #[serde(rename = "rootfs_schema")]
    _rootfs_schema: serde_json::Value,
    #[serde(rename = "runtime_artifact_protocol")]
    _runtime_artifact_protocol: serde_json::Value,
    #[serde(rename = "agent_wire_protocol")]
    _agent_wire_protocol: serde_json::Value,
    #[serde(rename = "agent_wire_capabilities")]
    _agent_wire_capabilities: serde_json::Value,
    #[serde(rename = "agent_sha256")]
    _agent_sha256: serde_json::Value,
    #[serde(rename = "rootfs_image_sha256")]
    _rootfs_image_sha256: serde_json::Value,
    #[serde(rename = "rootfs_image_bytes")]
    _rootfs_image_bytes: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactHandshake {
    #[serde(rename = "nonce")]
    _nonce: serde_json::Value,
    #[serde(rename = "expected_boot")]
    _expected_boot: ExactBootProof,
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum ExactHandshakeRequestFrame {
    Handshake(ExactHandshake),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactRuntimeArtifactIdentity {
    #[serde(rename = "protocol")]
    _protocol: serde_json::Value,
    #[serde(rename = "id")]
    _id: serde_json::Value,
    #[serde(rename = "content_sha256")]
    _content_sha256: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactPortPublish {
    #[serde(rename = "container_port")]
    _container_port: serde_json::Value,
    #[serde(rename = "host_port")]
    _host_port: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactFunctionLaunch {
    #[serde(rename = "start_cmd")]
    _start_cmd: serde_json::Value,
    #[serde(rename = "env")]
    _env: serde_json::Value,
    #[serde(rename = "workdir")]
    _workdir: serde_json::Value,
    #[serde(rename = "runtime_artifact")]
    _runtime_artifact: ExactNullableRuntimeArtifact,
    #[serde(rename = "port")]
    _port: serde_json::Value,
    #[serde(rename = "max_concurrency")]
    _max_concurrency: serde_json::Value,
    #[serde(rename = "memory_mib")]
    _memory_mib: serde_json::Value,
    #[serde(rename = "cpus")]
    _cpus: serde_json::Value,
    #[serde(rename = "pids")]
    _pids: serde_json::Value,
    #[serde(rename = "runtime")]
    _runtime: serde_json::Value,
    #[serde(rename = "raw_proxy")]
    _raw_proxy: serde_json::Value,
    #[serde(rename = "udp_ports")]
    _udp_ports: Vec<ExactPortPublish>,
    #[serde(rename = "tcp_ports")]
    _tcp_ports: Vec<ExactPortPublish>,
    #[serde(rename = "gpu")]
    _gpu: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactLegacyFunctionLaunch {
    #[serde(rename = "start_cmd")]
    _start_cmd: serde_json::Value,
    #[serde(rename = "env")]
    _env: serde_json::Value,
    #[serde(rename = "workdir")]
    _workdir: serde_json::Value,
    #[serde(rename = "port")]
    _port: serde_json::Value,
    #[serde(rename = "max_concurrency")]
    _max_concurrency: serde_json::Value,
    #[serde(rename = "memory_mib")]
    _memory_mib: serde_json::Value,
    #[serde(rename = "cpus")]
    _cpus: serde_json::Value,
    #[serde(rename = "pids")]
    _pids: serde_json::Value,
    #[serde(rename = "runtime")]
    _runtime: serde_json::Value,
    #[serde(rename = "raw_proxy")]
    _raw_proxy: serde_json::Value,
    #[serde(rename = "udp_ports")]
    _udp_ports: Vec<ExactPortPublish>,
    #[serde(rename = "tcp_ports")]
    _tcp_ports: Vec<ExactPortPublish>,
    #[serde(rename = "gpu")]
    _gpu: serde_json::Value,
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum ExactStartFunctionRequestFrame {
    StartFunction(ExactFunctionLaunch),
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum ExactLegacyStartFunctionRequestFrame {
    StartFunction(ExactLegacyFunctionLaunch),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactHandshakeReady {
    #[serde(rename = "nonce")]
    _nonce: serde_json::Value,
    #[serde(rename = "proof")]
    _proof: ExactBootProof,
    #[serde(rename = "transcript_sha256")]
    _transcript_sha256: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactProtocolFault {
    #[serde(rename = "code")]
    _code: serde_json::Value,
    #[serde(rename = "message")]
    _message: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactFunctionFault {
    #[serde(rename = "code")]
    _code: serde_json::Value,
    #[serde(rename = "message")]
    _message: serde_json::Value,
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum ExactHandshakeResponseFrame {
    HandshakeReady(ExactHandshakeReady),
    ProtocolFault(ExactProtocolFault),
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum ExactVersionedLaunchEventFrame {
    RuntimeArtifactReady(ExactRuntimeArtifactIdentity),
    FunctionReady,
    FunctionError(serde_json::Value),
    FunctionFault(ExactFunctionFault),
    ProtocolFault(ExactProtocolFault),
    HandshakeReady(ExactHandshakeReady),
}

fn validate_exact<T: for<'de> Deserialize<'de>>(frame: &[u8], label: &str) -> Result<(), String> {
    serde_json::from_slice::<T>(frame)
        .map(|_| ())
        .map_err(|error| format!("{label} does not have its exact frozen field set: {error}"))
}

pub fn validate_agent_handshake_request_frame(frame: &[u8]) -> Result<(), String> {
    validate_exact::<ExactHandshakeRequestFrame>(frame, "Handshake")
}

pub fn validate_agent_start_function_request_frame(frame: &[u8]) -> Result<(), String> {
    validate_exact::<ExactStartFunctionRequestFrame>(frame, "StartFunction v2")
}

pub fn validate_legacy_agent_start_function_request_frame(frame: &[u8]) -> Result<(), String> {
    validate_exact::<ExactLegacyStartFunctionRequestFrame>(frame, "legacy StartFunction")
}

pub fn validate_agent_handshake_response_frame(frame: &[u8]) -> Result<(), String> {
    validate_exact::<ExactHandshakeResponseFrame>(frame, "agent handshake response")
}

pub fn validate_agent_versioned_launch_event_frame(frame: &[u8]) -> Result<(), String> {
    validate_exact::<ExactVersionedLaunchEventFrame>(frame, "versioned launch event")
}

/// One argv command inside an already-booted cell (the Sandboxes exec path): argv-exact unless
/// `shell` is set, distinct stdout/stderr, explicit cwd, killable by `id` from another connection.
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
    /// Run as root; honored only when `sudo` exists in the guest, otherwise the request fails loudly
    /// (`ExecDone` without a zero exit code), never silently downgraded.
    #[serde(default)]
    pub sudo: bool,
    /// Explicit opt-in to shell interpretation (`sh -c`); the default argv exec (`execvp`) makes shell
    /// injection structurally impossible.
    #[serde(default)]
    pub shell: bool,
}

/// One interactive PTY session inside an already-booted cell (the Sandboxes terminal): a real
/// `openpty`/`forkpty` shell with raw bytes both ways, on one dedicated vsock connection that
/// also carries its `PtyInput`/`PtyResize` frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecPtyRequest {
    /// Caller-chosen id (echoed on every `PtyOutput`/`PtyExited` event) — the
    /// platform's `SandboxShellSession.id`.
    pub id: String,
    /// Shell to launch, e.g. `/bin/sh` or `/bin/bash`. Empty = agent picks a
    /// sane default (`$SHELL`, falling back to `/bin/sh`).
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Initial terminal size, applied to the pty before the shell forks so full-screen programs
    /// render from their first frame.
    #[serde(default = "default_pty_cols")]
    pub cols: u16,
    #[serde(default = "default_pty_rows")]
    pub rows: u16,
    /// Programs the shell must be able to run besides the fixed tool set (the sandbox's own runtime);
    /// Firecracker's full rootfs ignores it, litebox stages each one with its dynamic closure into
    /// the shell's guest tar. Additive and defaulted.
    #[serde(default)]
    pub programs: Vec<String>,
}

fn default_pty_cols() -> u16 {
    80
}
fn default_pty_rows() -> u16 {
    24
}

/// Message the box daemon (host) sends to the cell daemon (guest) over vsock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentRequest {
    /// Mandatory first frame for the versioned launch path.
    Handshake(AgentHandshake),
    /// Run this build inside the cell.
    Run(BuildJob),
    /// Start a long-lived function server (Fluid compute serving path).
    StartFunction(FunctionLaunch),
    /// Box daemon -> agent: a restored cache tarball (empty on miss). Sent in
    /// reply to `AgentEvent::CacheGet` during a build.
    CacheData { tar: Vec<u8> },
    /// Liveness probe; agent replies with `AgentEvent::Pong`.
    Ping,
    /// Run one argv command (Sandboxes) on its own dedicated connection; unlike `Run`, the agent
    /// keeps accepting other connections.
    Exec(ExecRequest),
    /// Kill a still-running `Exec` by id from any connection; the agent keeps a process-global
    /// registry keyed by `ExecRequest.id`.
    KillExec { id: String },
    /// Open an interactive PTY session: the mandatory first frame on its own connection, which then
    /// carries only this session's `PtyInput`/`PtyResize` until it closes.
    ExecPty(ExecPtyRequest),
    /// Raw bytes typed into the terminal — sent on the SAME connection
    /// `ExecPty` opened, any number of times.
    PtyInput { bytes: Vec<u8> },
    /// Browser terminal resized, on the same connection `ExecPty` opened; applied via `TIOCSWINSZ`,
    /// the shell's `SIGWINCH` does the rest.
    PtyResize { cols: u16, rows: u16 },
}

/// Messages the cell daemon streams back to the box daemon over vsock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    /// Exact boot proof for the immediately preceding fresh handshake.
    HandshakeReady(AgentHandshakeReady),
    /// Typed fail-closed refusal; host classifies this as a node/guest fault.
    ProtocolFault(AgentProtocolFault),
    Pong,
    Log(LogLine),
    Done(BuildResult),
    /// Guest proof that the mounted runtime tree matches what the host requested; a host must
    /// receive it before accepting `FunctionReady`, so old agents fail closed.
    RuntimeArtifactReady(RuntimeArtifactIdentity),
    /// Function server is up and accepting requests on its port; the agent is
    /// now bridging `CELL_FUNCTION_PORT` -> the function.
    FunctionReady,
    /// Function failed to start for tenant-controlled reasons. This text may
    /// include tenant stderr and must never be inspected for node-fault markers.
    FunctionError(String),
    /// Function failed to start because of a typed guest/platform fault whose
    /// origin is independent of tenant-controlled diagnostics.
    FunctionFault(AgentFunctionFault),
    /// Agent -> box daemon: please send the cached tarball for `key` (build
    /// cache restore). The box daemon replies with `AgentRequest::CacheData`.
    CacheGet {
        key: String,
        paths: Vec<String>,
    },
    /// Agent -> box daemon: persist this cache tarball for `key` (build cache
    /// save, after a successful build).
    CachePut {
        key: String,
        tar: Vec<u8>,
    },
    /// One line of an `Exec`'s stdout/stderr (`LogStream::System` unused here).
    ExecOutput {
        id: String,
        stream: LogStream,
        line: String,
    },
    /// An `Exec` finished. `exit_code = None` means killed or undeterminable (e.g. sudo
    /// unavailable) — never success.
    ExecDone {
        id: String,
        exit_code: Option<i32>,
    },
    /// Raw bytes from the pty master, forwarded as read (not line-buffered) so in-place repaints
    /// look right.
    PtyOutput { id: String, bytes: Vec<u8> },
    /// The pty's shell exited (normally or killed) or the session was torn
    /// down. Same `exit_code = None` meaning as `ExecDone`.
    PtyExited { id: String, exit_code: Option<i32> },
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
        assert_eq!(
            Runtime::from_config_str("container"),
            Some(Runtime::Container)
        );
        assert_eq!(
            Runtime::from_config_str("firecracker"),
            Some(Runtime::Container)
        );
        // Legacy sentinels that must fall through to argv inference.
        assert_eq!(Runtime::from_config_str("auto"), None);
        assert_eq!(Runtime::from_config_str("command"), None);
        assert_eq!(Runtime::from_config_str(""), None);
        assert_eq!(Runtime::from_config_str("bogus"), None);
    }

    #[test]
    fn argv_inference_distinguishes_node_and_bun() {
        assert_eq!(
            Runtime::infer_from_argv(&["node".into(), "server.js".into()]),
            Runtime::Node
        );
        assert_eq!(
            Runtime::infer_from_argv(&["/usr/local/bin/node".into()]),
            Runtime::Node
        );
        assert_eq!(
            Runtime::infer_from_argv(&["npm".into(), "start".into()]),
            Runtime::Node
        );
        assert_eq!(
            Runtime::infer_from_argv(&["bun".into(), "run".into(), "server.js".into()]),
            Runtime::Bun
        );
        assert_eq!(
            Runtime::infer_from_argv(&["bunx".into(), "next".into()]),
            Runtime::Bun
        );
        assert_eq!(
            Runtime::infer_from_argv(&["python3".into(), "app.py".into()]),
            Runtime::Python
        );
        assert_eq!(
            Runtime::infer_from_argv(&["__container__".into(), "img".into()]),
            Runtime::Container
        );
        assert_eq!(
            Runtime::infer_from_argv(&["./my-binary".into()]),
            Runtime::Command
        );
        assert_eq!(Runtime::infer_from_argv(&[]), Runtime::Command);
    }

    #[test]
    fn resolve_prefers_explicit_config_over_argv_inference() {
        // Explicit "bun" wins even if argv looks like node (e.g. a proxy shim).
        assert_eq!(
            Runtime::resolve("bun", &["node".into(), "server.js".into()]),
            Runtime::Bun
        );
        // "auto"/"command"/empty defer to argv.
        assert_eq!(
            Runtime::resolve("auto", &["bun".into(), "server.js".into()]),
            Runtime::Bun
        );
        assert_eq!(Runtime::resolve("command", &["node".into()]), Runtime::Node);
        assert_eq!(Runtime::resolve("", &["python3".into()]), Runtime::Python);
    }

    #[test]
    fn bytecode_cache_eligibility_is_runtime_specific() {
        assert!(Runtime::Node.uses_v8_compile_cache());
        assert!(!Runtime::Bun.uses_v8_compile_cache());
        assert!(Runtime::Bun.uses_bun_bytecode_cache());
        assert!(!Runtime::Node.uses_bun_bytecode_cache());
        assert!(
            !Runtime::Python.uses_v8_compile_cache() && !Runtime::Python.uses_bun_bytecode_cache()
        );
    }

    #[test]
    fn function_launch_serializes_runtime_field() {
        let launch = FunctionLaunch {
            start_cmd: vec!["bun".into(), "run".into(), "server.js".into()],
            env: Default::default(),
            workdir: None,
            runtime_artifact: None,
            port: 3000,
            max_concurrency: 10,
            memory_mib: 0,
            cpus: 0.0,
            pids: 0,
            runtime: Runtime::Bun,
            raw_proxy: false,
            udp_ports: Vec::new(),
            tcp_ports: Vec::new(),
            gpu: false,
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
