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
    /// A compiled WebAssembly/WASIX module executed via the `wasmer` CLI
    /// (`wasmer run --net --forward-host-env <module>.wasm`). Runs as an
    /// ordinary host process under whichever backend placed it (Mock,
    /// Litebox, Firecracker) — Wasmer needs no dedicated `CellBackend`,
    /// exactly like Node/Bun/Python don't. Live-verified: a real
    /// axum+tokio server built with `cargo wasix build --release`
    /// (target `wasm32-wasmer-wasi`) binds and serves a real HTTP
    /// request under `wasmer run --net`, and `--forward-host-env`
    /// correctly carries the dynamically-assigned `$PORT` from the
    /// spawning backend into the guest — no build-time port baking.
    /// Guest code MUST bind `0.0.0.0`, never a literal `127.0.0.1`:
    /// unlike Node/Bun (which get a host-injected `_listen2` shim to
    /// rescue a hardcoded-loopback bind, see `litebox.rs`), a compiled
    /// `.wasm` module cannot be monkeypatched, so a loopback-bound guest
    /// is unreachable through Litebox's per-cell TUN device the same way
    /// an unshimmed Node app would be.
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
            "wasmer" | "wasm" | "wasix" | "wasi" => Some(Runtime::Wasmer),
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

    /// Resolve the effective runtime for a function: an explicit config value
    /// wins; otherwise infer from argv. This is the ONE call every producer
    /// (build pipeline) and consumer (fluid-compute, backends, guest agent)
    /// should use instead of re-deriving it independently.
    pub fn resolve(config_runtime: &str, start_cmd: &[String]) -> Runtime {
        Runtime::from_config_str(config_runtime)
            .unwrap_or_else(|| Runtime::infer_from_argv(start_cmd))
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
#[serde(deny_unknown_fields)]
pub struct UdpPublish {
    pub container_port: u16,
    pub host_port: u16,
}

/// One TCP port publish for a CONTAINER function — same shape and role as
/// [`UdpPublish`] but for the stream side: every raw/published TCP-transport
/// port spec (extra Tcp/Grpc specs of a multi-port service, and any compose
/// PUBLISHED Http port) gets its own loopback host port
/// (`-p 127.0.0.1:<host_port>:<container_port>`), so the raw proxy's mesh
/// resolver has a direct local splice leg per port instead of only the
/// primary's tunnel. The PRIMARY port rides here too (host = the launch's
/// assigned `port`), which is what lets a published Http primary (MinIO's
/// :9000) be spliced without the HTTP-framed tunnel corrupting it — emitters
/// of `-p` flags dedupe against the primary pairing they already publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpPublish {
    pub container_port: u16,
    pub host_port: u16,
}

pub const RUNTIME_ARTIFACT_PROTOCOL_VERSION: u16 = 1;
pub const RUNTIME_ARTIFACT_PACKAGE_PROTOCOL_VERSION: u16 = 1;

/// Complete host-to-guest wire contract. This version covers every
/// AgentRequest and AgentEvent variant plus every serialized field and semantic
/// of FunctionLaunch. Adding, removing, defaulting, or reinterpreting any of
/// those is a protocol change and MUST monotonically increment this constant;
/// runtime-artifact identity remains independently versioned above.
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
/// The peer implements `ExecPty`/`PtyInput`/`PtyResize`/`PtyOutput`/`PtyExited`
/// (Sandboxes interactive terminal) — a real `openpty`/`forkpty` session, not
/// just line-buffered `Exec`.
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
/// Host-side content proof written next to <image>.ext4. The host verifies this
/// descriptor against the exact image bytes before advertising or booting work;
/// existence or printable version text alone is never a capability.
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

/// First application frame on a versioned host-to-guest connection. The nonce
/// prevents replay; expected_boot is independently verified from exact image
/// bytes by the host and against the running executable by the guest.
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

/// A node-attributed function-start failure. The guest constructs this only
/// from typed platform error origins; tenant stderr remains exclusively in
/// `AgentEvent::FunctionError` and can never select this code.
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

/// Canonical bytes hashed into AgentHandshakeReady::transcript_sha256. The
/// transcript binds freshness, exact rootfs bytes, packaged agent digest, and
/// both independently-versioned protocols without adding a hash dependency to
/// hive-core itself.
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

/// Immutable identity of the exact runtime tree attached to an isolated cell.
/// The host derives it while materializing the build and the guest echoes it
/// only after validating the marker inside the mounted artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactIdentity {
    pub protocol: u16,
    pub id: String,
    pub content_sha256: String,
}

/// Transfer identity for the deterministic package carrying one semantic runtime
/// tree. The package digest addresses transport bytes; `semantic_tree_sha256`
/// remains the backend-neutral execution identity every target must recompute.
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
    /// Exact isolated-runtime artifact expected by the host. `None` is normal for
    /// same-host/container paths. An upgraded guest also accepts only the exact
    /// frozen 13-field pre-v2 frame: `runtime_artifact` is absent while `workdir`
    /// remains a required (possibly null) field. Upgraded isolated hosts always
    /// send `Some` and require validation and echo it before `FunctionReady` is
    /// accepted.
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
    /// TCP ports a CONTAINER function publishes on loopback (see
    /// [`TcpPublish`]; includes the primary). Same wire-compat posture as
    /// `udp_ports`: empty for non-container functions and pre-upgrade peers.
    #[serde(default)]
    pub tcp_ports: Vec<TcpPublish>,
    /// Pass the host's GPUs through to a CONTAINER function's cell (CDI
    /// `nvidia.com/gpu=all`; with a `runsc` sandbox runtime, gVisor `nvproxy`).
    /// Set by fluid-compute's `cold_start` from `FunctionConfig::gpu`. `false`
    /// (the wire-compat default) launches exactly as before. Ignored by the
    /// microVM/process paths — Firecracker has no PCI passthrough, and on FC
    /// nodes containers run via host podman, which is where the GPU lives.
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

/// Inspect only the raw JSON envelope. Versioned callers use this before typed
/// deserialization so an added outer key cannot be ignored by a permissive
/// future schema.
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

/// A single interactive PTY session inside an already-booted, long-lived cell
/// — the Sandboxes interactive-terminal path. Unlike [`ExecRequest`] (one-shot
/// argv, line-buffered output, no stdin), this allocates a real pseudo-terminal
/// in the guest (`openpty`/`forkpty`) running the caller's shell: raw byte
/// stream in both directions, so `vim`/`less`/tab-completion/`^C` all work
/// exactly as they would over SSH. One dedicated vsock connection per session,
/// held open for the session's whole life; `PtyInput`/`PtyResize` ride the
/// SAME connection (the agent's accept loop distinguishes them by frame type,
/// not by a fresh connection per call — unlike `KillExec`, which the caller
/// may not have a live connection to send on).
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
    /// Initial terminal size; a real `struct winsize` is set on the pty before
    /// the shell forks so full-screen programs (vim, less, htop) render
    /// correctly from their very first frame.
    #[serde(default = "default_pty_cols")]
    pub cols: u16,
    #[serde(default = "default_pty_rows")]
    pub rows: u16,
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
    /// Run one argv command (Sandboxes). Sent on its OWN dedicated connection;
    /// the agent spawns a thread for it and keeps accepting other connections
    /// (unlike `Run`, this does NOT stop the accept loop).
    Exec(ExecRequest),
    /// Kill a still-running `Exec` by id, sent on any (typically a fresh)
    /// connection — the agent tracks live exec child PIDs in a process-global
    /// registry keyed by `ExecRequest.id`.
    KillExec { id: String },
    /// Open an interactive PTY session (Sandboxes terminal). Mandatory FIRST
    /// frame on its own dedicated connection — every subsequent frame on that
    /// same connection is `PtyInput`/`PtyResize` for this exact session until
    /// the connection closes or the shell exits.
    ExecPty(ExecPtyRequest),
    /// Raw bytes typed into the terminal — sent on the SAME connection
    /// `ExecPty` opened, any number of times.
    PtyInput { bytes: Vec<u8> },
    /// Browser terminal was resized — sent on the SAME connection `ExecPty`
    /// opened. The agent applies it via `TIOCSWINSZ` and the shell's own
    /// `SIGWINCH` handling takes it from there (full-screen programs redraw).
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
    /// Guest proof that the mounted runtime tree matches the exact protocol/id/
    /// content identity requested by the host. A host must receive this before
    /// accepting `FunctionReady`; old agents therefore fail closed.
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
    /// An `Exec` finished. `exit_code = None` means it was killed or the agent
    /// could not determine an exit status (e.g. sudo requested but
    /// unavailable) — the caller must NEVER treat `None` as success.
    ExecDone {
        id: String,
        exit_code: Option<i32>,
    },
    /// Raw bytes read from the pty master (shell stdout+stderr interleaved, as
    /// a real terminal produces) — unlike `ExecOutput`, NOT line-buffered:
    /// forwarded to the browser as soon as the agent reads them, so a program
    /// that repaints a line in place (a progress bar, `vim`'s status line)
    /// looks correct.
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
