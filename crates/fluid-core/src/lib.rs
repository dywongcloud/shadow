//! `fluid-core` — the deployment model for the serving layer.
//!
//! Where Hive builds code, Fluid *serves* it. A [`Deployment`] is the unit a
//! user ships: some static assets plus zero or more [`FunctionConfig`]s, with
//! [`Route`]s mapping request paths to either static files or a function.
//!
//! The function config carries the knobs that make compute "Fluid": an
//! in-function `max_concurrency` (many requests per instance), `min_instances`
//! (keep-warm), `max_instances` (autoscale ceiling), and an `idle_ttl` for
//! scale-to-zero.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectIncarnation(Uuid);

impl ProjectIncarnation {
    pub fn mint() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn path_component(self) -> String {
        self.0.simple().to_string()
    }
}

impl std::str::FromStr for ProjectIncarnation {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl std::fmt::Display for ProjectIncarnation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.simple())
    }
}

impl std::fmt::Debug for ProjectIncarnation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeploymentId(pub String);

impl DeploymentId {
    pub fn new() -> Self {
        DeploymentId(format!(
            "dpl-{}",
            &Uuid::new_v4().simple().to_string()[..10]
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for DeploymentId {
    fn default() -> Self {
        Self::new()
    }
}
impl From<String> for DeploymentId {
    fn from(s: String) -> Self {
        DeploymentId(s)
    }
}
impl std::fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::fmt::Debug for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Application protocol a service speaks, driving ingress/routing decisions
/// (Railway-style, configurable per service). Wire representation is the
/// existing lowercase strings already persisted in deployment records
/// ("http", "https", "ws", "wss", "grpc", "json-rpc", "tcp", "udp") — this
/// enum is a drop-in typed replacement for the old bare `String` field, not a
/// new format. An absent field or a legacy empty string both deserialize to
/// [`ServiceProtocol::Http`] (backward-compatible with older manifests).
///
/// Strict-vs-lenient split (load-bearing — do not collapse the two):
/// - `FromStr` is STRICT: an unrecognized string is an [`InvalidProtocol`]
///   error. Deploy-input boundaries (fluid.json ingestion via
///   [`Manifest::from_json`], compose port suffixes, admin API fields) use it
///   to reject malformed fresh input with a clear error.
/// - serde `Deserialize` is LENIENT: an unrecognized string coerces to
///   [`ServiceProtocol::Http`] with a `tracing::warn!`, NEVER an error. The
///   pre-enum field was an unvalidated bare `String`, so arbitrary strings
///   ("h2c", "HTTP", typos) can already sit inside persisted `state.json`
///   snapshots and gossip-replicated snapshots — and those loaders treat any
///   deserialize failure as "no state" (`unwrap_or_default()` /
///   skip-and-continue), so a hard error here would silently WIPE a node's
///   entire persisted platform state on restart. Loading stored state must
///   never fail on this field.
///
/// HTTP-family protocols (`Http`, `Https`, `Ws`, `Wss`, `JsonRpc`) ride the
/// normal L7 path. Connection-oriented ones (`Grpc`, `Tcp`, `Udp`) are
/// spliced as a raw connection cross-node — see [`FunctionConfig::needs_raw_proxy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceProtocol {
    Http,
    Https,
    Ws,
    Wss,
    Grpc,
    JsonRpc,
    Tcp,
    Udp,
}

impl ServiceProtocol {
    /// Whether a service speaking this protocol is spliced as a RAW connection
    /// (vs the buffered/streamed HTTP tunnel) when proxied — gRPC (HTTP/2
    /// trailers) and raw TCP/UDP. The single source of truth behind
    /// [`FunctionConfig::needs_raw_proxy`] AND the per-[`PortSpec`]
    /// public-port-allocation decision (each raw-protocol spec gets its own
    /// allocated public port; HTTP-family specs ride 80/443 Host routing).
    /// `json-rpc` stays HTTP-framed (it's HTTP-transported despite the name).
    pub fn needs_raw_proxy(self) -> bool {
        matches!(
            self,
            ServiceProtocol::Grpc | ServiceProtocol::Tcp | ServiceProtocol::Udp
        )
    }

    /// The lowercase wire string for this protocol — the inverse of
    /// [`ServiceProtocol`]'s `FromStr` impl and the same value serde emits.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceProtocol::Http => "http",
            ServiceProtocol::Https => "https",
            ServiceProtocol::Ws => "ws",
            ServiceProtocol::Wss => "wss",
            ServiceProtocol::Grpc => "grpc",
            ServiceProtocol::JsonRpc => "json-rpc",
            ServiceProtocol::Tcp => "tcp",
            ServiceProtocol::Udp => "udp",
        }
    }
}

impl Default for ServiceProtocol {
    /// Matches the pre-enum default ("http", via the old empty-string convention).
    fn default() -> Self {
        ServiceProtocol::Http
    }
}

impl std::fmt::Display for ServiceProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing a protocol string that isn't one of the known
/// values (`http`/`https`/`ws`/`wss`/`grpc`/`json-rpc`/`tcp`/`udp`, or empty).
/// Carries the invalid input so the caller can surface a clear, actionable
/// deploy-time error instead of silently defaulting to HTTP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidProtocol(pub String);

impl std::fmt::Display for InvalidProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown protocol {:?} — expected one of: http, https, ws, wss, grpc, json-rpc, tcp, udp",
            self.0
        )
    }
}
impl std::error::Error for InvalidProtocol {}

impl std::str::FromStr for ServiceProtocol {
    type Err = InvalidProtocol;

    /// Strict parse for raw (non-JSON) input — e.g. a compose port suffix, a
    /// CLI/UI field, a query string — so callers reject malformed protocol
    /// strings at the point of entry rather than falling through to a silent
    /// HTTP default. Mirrors the `#[serde(alias = "")]` convention: empty
    /// string is treated as "http" for backward compatibility.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" | "http" => Ok(ServiceProtocol::Http),
            "https" => Ok(ServiceProtocol::Https),
            "ws" => Ok(ServiceProtocol::Ws),
            "wss" => Ok(ServiceProtocol::Wss),
            "grpc" => Ok(ServiceProtocol::Grpc),
            "json-rpc" => Ok(ServiceProtocol::JsonRpc),
            "tcp" => Ok(ServiceProtocol::Tcp),
            "udp" => Ok(ServiceProtocol::Udp),
            other => Err(InvalidProtocol(other.to_string())),
        }
    }
}

/// LENIENT deserialization — see the strict-vs-lenient split on
/// [`ServiceProtocol`]. This impl backs every serde path that reads
/// already-stored state (persisted `state.json` snapshots, gossip-replicated
/// snapshots, guardian restores) where a hard error would be amplified into
/// silent total state loss by `unwrap_or_default()`-style loaders. Unknown
/// strings coerce to `Http` with a warning; deploy-time rejection of unknown
/// values lives at the input boundaries via the strict `FromStr`.
impl<'de> Deserialize<'de> for ServiceProtocol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ProtocolVisitor;
        impl serde::de::Visitor<'_> for ProtocolVisitor {
            type Value = ServiceProtocol;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a protocol string (http, https, ws, wss, grpc, json-rpc, tcp, udp)")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ServiceProtocol, E> {
                Ok(v.parse::<ServiceProtocol>().unwrap_or_else(|_| {
                    tracing::warn!(
                        protocol = %v,
                        "unrecognized service protocol in stored/replicated state; defaulting to http"
                    );
                    ServiceProtocol::Http
                }))
            }
        }
        deserializer.deserialize_str(ProtocolVisitor)
    }
}

/// One published port + protocol on a service. A single deployment MAY expose
/// more than one (Docker-Compose/Railway-style multi-port), e.g. a Minecraft
/// server (`itzg/minecraft-server`) publishing game (25565/tcp), rcon
/// (25575/tcp), and query (25565/udp) simultaneously. `label` is a free-form,
/// purely informational identifier surfaced to the user (e.g. "game", "rcon",
/// "query") — never matched against by routing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    pub container_port: u16,
    #[serde(default)]
    pub protocol: ServiceProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The PUBLIC port allocated for this spec when its protocol needs raw
    /// (non-HTTP) ingress — what an external client actually connects to
    /// (HTTP-family specs ride the shared 80/443 gateway and stay `None`).
    /// Stamped at deploy time by the platform's raw-port allocator
    /// (hive-cloud's `raw_ports` module), never user-supplied; the allocation
    /// is keyed by (project, function, container_port, protocol) so it is
    /// STABLE across redeploys — a new deployment of the same service is
    /// re-stamped with the same public port. `None` also for records written
    /// before the allocator existed (re-allocated on their next deploy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_port: Option<u16>,
    /// The compose-declared HOST port this spec asks to be published on
    /// (`ports: ["9000:9000"]` → 9000). This is a PUBLISH REQUEST with two
    /// effects, both matching docker-compose's own publish semantics: its
    /// presence alone makes the spec eligible for a public raw allocation even
    /// when `protocol` is Http (served as plain-TCP passthrough — exactly what
    /// `docker compose up` does with a published HTTP port; no TLS on raw
    /// ports, the shared 443 gateway remains the TLS surface), and the
    /// allocator PREFERS this exact number, falling back to its normal range
    /// with a loud build log when the number is taken fleet-wide or reserved —
    /// never a silent substitution. `public_port` above remains the GRANT.
    /// Absent for every record written before this field existed and for all
    /// non-compose paths — those keep today's behavior byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_public_port: Option<u16>,
}

impl PortSpec {
    /// Build a single-port list entry — the compatibility bridge for callers
    /// that still carry exactly one bare `u16` port (`ImageDeployReq`,
    /// `ContainerSpec`, and other single-port representations elsewhere in
    /// the codebase not rewritten in this pass). No `label`: there is no
    /// multi-port distinction to make with only one port.
    pub fn single(container_port: u16, protocol: ServiceProtocol) -> PortSpec {
        PortSpec {
            container_port,
            protocol,
            label: None,
            public_port: None,
            preferred_public_port: None,
        }
    }

    /// Same bridge for an `Option<u16>` legacy port field: `None` normalizes
    /// to an empty port list (nothing declared), `Some(port)` to a
    /// single-element list via [`PortSpec::single`].
    pub fn from_legacy_port(port: Option<u16>, protocol: ServiceProtocol) -> Vec<PortSpec> {
        port.map(|p| vec![PortSpec::single(p, protocol)])
            .unwrap_or_default()
    }
}

/// One ALLOCATED public raw-ingress binding on a deployment: a [`PortSpec`]
/// whose `public_port` has been stamped by the platform's raw-port allocator,
/// flattened together with the owning function's name. This is the shape the
/// generic raw proxy routes on — `public_port` → which function/container port
/// to splice to, over which protocol — and it rides [`DeploymentInfo`] so the
/// mapping is visible FLEET-WIDE via the existing deployment gossip (any edge
/// node must know which public ports exist and who serves them, exactly like
/// HTTP hosts ride `peer_routes`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawPortBinding {
    pub public_port: u16,
    /// Owning function (the [`FunctionConfig`] whose spec was stamped).
    pub function: String,
    pub container_port: u16,
    pub protocol: ServiceProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A single Tencent Cloud EIP (Elastic IP) purchased and associated for a
/// deployment that opted into `functions[].dedicated_ipv4` — one dotted-quad
/// address the function is reachable on, distinct from the shared edge IPs.
/// `tencent_eip_id` ("eip-xxxxxxxx") is the idempotency anchor: it is the
/// handle every `AssociateAddress`/`ReleaseAddresses` call needs, and its
/// presence in the durable claim registry (`hive-cloud::tencent_eip`) is what
/// stops a redeploy or a crash-retry from purchasing a second address for the
/// same (project, function). `owner_node` names the node whose CVM NIC the
/// address is actually associated with — only that node may bind it
/// (`raw_proxy.rs`); every other node keeps binding the wildcard address for
/// the same public port, so cross-node splice still works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedicatedIpv4 {
    pub address: String,
    pub tencent_eip_id: String,
    pub region: String,
    #[serde(default)]
    pub owner_node: String,
    #[serde(default)]
    pub allocated_ms: u64,
}

/// A serverless function within a deployment. The server process must listen on
/// `$PORT` and speak HTTP/1.1.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionConfig {
    pub name: String,
    /// Informational: "node", "python", "go", "command", ...
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// argv to start the server, e.g. ["node", "server.js"].
    pub start_cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// vCPUs per instance (microVM). Standard tier = 1, Performance tier = 2.
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    /// Per-deployment CPU quota override for a CONTAINER function (podman
    /// `--cpus`), e.g. 4.0 — from fluid.json `container.cpus` /
    /// `ImageDeployReq.cpus`. 0.0 = the node's generous env-tunable default
    /// (`ContainerLimits::default`). Clamped to a fleet-wide ceiling in
    /// `ContainerLimits::for_container` (`HIVE_CONTAINER_CPUS_MAX`) so one
    /// tenant's own request can never remove the ceiling entirely. Distinct
    /// from `vcpus` above (microVM sizing); ignored for microVM/process
    /// functions.
    #[serde(default)]
    pub cpus: f64,
    /// Per-deployment max-PIDs override for a CONTAINER function's cgroup
    /// (podman `--pids-limit`) — a fork-bomb guard — from fluid.json
    /// `container.pids` / `ImageDeployReq.pids`. 0 = the node's default.
    /// Clamped to a fleet-wide ceiling (`HIVE_CONTAINER_PIDS_MAX`). Ignored
    /// for microVM/process functions.
    #[serde(default)]
    pub pids: u32,
    /// Fluid in-function concurrency: max simultaneous requests per instance.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    /// Keep at least this many instances warm (0 = scale fully to zero).
    #[serde(default)]
    pub min_instances: u32,
    /// Autoscaling ceiling.
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    /// Scale an idle instance down after this many seconds with no requests.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,
    /// Max wall-clock duration for a single invocation (Vercel default 300s).
    /// Exceeding it returns 504 — error isolation keeps other requests alive.
    #[serde(default = "default_max_duration")]
    pub max_duration_secs: u64,
    /// Per-function region preference (`vercel.json` `functions[].regions`).
    /// Overrides the project-level default for this function when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    /// Glob of extra files to bundle (`functions[].includeFiles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_files: Option<String>,
    /// Glob of files to exclude (`functions[].excludeFiles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_files: Option<String>,
    /// Application protocol this service speaks, for ingress/routing decisions
    /// (Railway-style, configurable per service). See [`ServiceProtocol`] for
    /// the full semantics (wire strings, HTTP-family vs raw-proxied, the
    /// empty-string/absent-field backward-compat default, and the
    /// strict-FromStr-vs-lenient-serde split: malformed values are rejected
    /// at the deploy-input boundary, but stored state always loads).
    #[serde(default, skip_serializing_if = "is_default_protocol")]
    pub protocol: ServiceProtocol,
    /// Forward-looking multi-port representation: every port this service
    /// publishes, each with its own protocol and an optional label (e.g.
    /// "game"/"rcon"/"query" for a Minecraft-style multi-port service). Empty
    /// by default — no call site populates this yet; single-port callers keep
    /// using their own bare port field and bridge to this shape via
    /// [`PortSpec::single`]/[`PortSpec::from_legacy_port`] when they migrate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortSpec>,
    /// This function needs a serverless GPU. Placement then only targets nodes
    /// advertising GPUs (`NodeInfo::gpu_count > 0`) and the container launch
    /// passes the host GPUs through (CDI `nvidia.com/gpu=all`, plus gVisor
    /// `nvproxy` when the sandbox runtime is `runsc`). `false`/absent = today's
    /// behavior exactly, for every existing deployment and every pre-upgrade
    /// peer (`serde(default)`). From fluid.json `functions[].gpu` / the
    /// dashboard function-settings toggle.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gpu: bool,
    /// Explicit opt-in to browser-node execution (fluid.json
    /// `functions[].browser`) with a bounded execution policy. ABSENT means the
    /// function is NOT browser-eligible, by construction: only an opted-in
    /// function whose handler survives the build-time bundle + rejection pass
    /// ([`BrowserPolicy`]'s docs) gets a [`FunctionConfig::browser_artifact`]
    /// descriptor, and only descriptor-carrying functions can ever be admitted
    /// for browser serving. This is the contract that replaces "pretend every
    /// deployed Node/Bun/container function can run in a browser".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserPolicy>,
    /// Build-stamped descriptor of the emitted browser artifact — digest
    /// metadata ONLY (source + canonical-policy BLAKE3, size, resolved
    /// limits), never the bytes. Written by the build pipeline after the
    /// bundle pass succeeds; `None` in every user-authored manifest and for
    /// any function the build rejected or never opted in. Because this rides
    /// the manifest, it reaches the replicated deployment state
    /// (`DeployRecord`) and the gossiped [`DeploymentInfo::browser_functions`]
    /// view — that metadata is what admission ties a donor's digest to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_artifact: Option<BrowserArtifact>,
    /// Why this function is NOT browser-servable, stamped by the build pipeline
    /// whenever it evaluated the function and declined to emit an artifact — an
    /// unsupported runtime, no handler-shaped entry on disk, or the exact
    /// `browser_artifacts::bundle` rejection an AUTO-detected candidate hit.
    /// SERVER-DERIVED ONLY: the build clears any tenant-supplied value before
    /// evaluating, so a fluid.json cannot inject a fake reason. Mutually
    /// exclusive with [`FunctionConfig::browser_artifact`] (eligible functions
    /// carry a descriptor and no reason). This is what turns "the picker just
    /// doesn't list my function" into a sentence naming the cause: it rides the
    /// manifest into `DeployRecord` and out through
    /// [`DeploymentInfo::browser_ineligible`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_ineligible_reason: Option<String>,
    /// Opt-in to a dedicated public IPv4 address for this function (fluid.json
    /// `functions[].dedicatedIpv4` / the dashboard function-settings toggle),
    /// mirroring [`FunctionConfig::gpu`]'s shape. `false`/absent = today's
    /// behavior exactly: the function is reached only through the shared edge
    /// IPs. Deploy time purchases (or re-adopts, on a redeploy) a real Tencent
    /// Cloud EIP via `hive-cloud::tencent_eip`; no placement filter follows
    /// from this flag — unlike GPU, any node can hold an EIP.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dedicated_ipv4: bool,
    /// The allocated address, stamped onto the manifest by the deploy path
    /// once `tencent_eip::claim_or_allocate` succeeds — the same
    /// stamp-before-`deploy_full` shape [`RawPortBinding::public_port`] uses,
    /// so [`Manifest::dedicated_ipv4_binding`] can hoist it onto
    /// [`DeploymentInfo::dedicated_ipv4`] exactly like `raw_port_bindings`
    /// hoists ports. `None` until allocation succeeds and for every function
    /// that never opted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedicated_ipv4_alloc: Option<DedicatedIpv4>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default_protocol(p: &ServiceProtocol) -> bool {
    *p == ServiceProtocol::Http
}

impl FunctionConfig {
    /// The effective protocol as a wire string, e.g. for logging/headers.
    /// Callers needing the typed value should use `self.protocol` directly.
    pub fn protocol_or_http(&self) -> &str {
        self.protocol.as_str()
    }

    /// Whether this service needs a RAW connection splice (vs the buffered/streamed
    /// HTTP tunnel) when proxied cross-node — gRPC (HTTP/2 trailers) and raw
    /// TCP/UDP. These are spliced byte-for-byte like a WebSocket so framing
    /// survives the mesh; `json-rpc` stays HTTP-framed (it's HTTP-transported
    /// despite the name), matching the existing grpc/json-rpc distinction.
    pub fn needs_raw_proxy(&self) -> bool {
        self.protocol.needs_raw_proxy()
    }
}

fn default_runtime() -> String {
    "command".into()
}
fn default_vcpus() -> u32 {
    1
}
fn default_memory() -> u32 {
    // Standard serverless tier: 2 GB.
    2048
}
fn default_max_concurrency() -> u32 {
    10
}
fn default_max_instances() -> u32 {
    10
}
fn default_idle_ttl() -> u64 {
    30
}
fn default_max_duration() -> u64 {
    300 // Vercel Fluid default max duration (5 minutes)
}

impl Default for FunctionConfig {
    fn default() -> Self {
        FunctionConfig {
            name: String::new(),
            runtime: default_runtime(),
            start_cmd: Vec::new(),
            env: BTreeMap::new(),
            vcpus: default_vcpus(),
            memory_mib: default_memory(),
            cpus: 0.0,
            pids: 0,
            max_concurrency: default_max_concurrency(),
            min_instances: 0,
            max_instances: default_max_instances(),
            idle_ttl_secs: default_idle_ttl(),
            max_duration_secs: default_max_duration(),
            regions: Vec::new(),
            include_files: None,
            exclude_files: None,
            protocol: ServiceProtocol::default(),
            ports: Vec::new(),
            gpu: false,
            browser: None,
            browser_artifact: None,
            browser_ineligible_reason: None,
            dedicated_ipv4: false,
            dedicated_ipv4_alloc: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Browser-executable function artifact contract
// (browser-function-artifact-build-contract)
// ---------------------------------------------------------------------------
//
// A function becomes browser-eligible ONLY by opting in through fluid.json
// `functions[].browser` and surviving the build-time bundle pass. The build
// emits ONE deterministic, self-contained source string — an
// `async function (request, ops)` expression in exactly the shape
// `crates/hive-browser/www/pkg/function-worker.js` evaluates
// (`globalThis.__hive_handler = (<source>)`) — computes two BLAKE3 digests
// (source bytes; canonical policy encoding), persists the bytes
// content-addressed on the building node, and stamps only the descriptor onto
// the manifest. The digest the wire protocol routes on
// (`hive_browser_proto::encode_invoke`'s code digest,
// `BrowserFunctionRuntime.pin`'s return value) is the POLICY digest: it binds
// the exact source digest to the exact bounded policy, so a browser pinning
// the artifact re-derives — and thereby verifies — the same 64-hex value the
// deployment state carries.
//
// The canonical policy encoding below is byte-for-byte the one
// `crates/hive-browser/www/function-runtime.js`'s `policyDigest` implements.
// Any change to either side without the other silently breaks every artifact
// verification — they are ONE contract with two implementations.

/// Domain separator of the canonical policy encoding — identical to
/// function-runtime.js's `policyDigest`.
pub const BROWSER_POLICY_DIGEST_DOMAIN: &[u8] = b"hive-browser-policy-v1\0";

/// Ceiling on a single browser entry file's UTF-8 source. The artifact is one
/// self-contained source string executed inside a donor's browser tab; an
/// unbounded bundle would turn the (per-tab) QuickJS memory limit and the
/// delivery path into a denial-of-service vector. 256 KiB is generous for an
/// edge handler — larger apps belong on the container/microVM path.
pub const BROWSER_ENTRY_MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Platform defaults — identical to `normalizePolicy`'s fallbacks in
/// function-runtime.js, so an unset fluid.json limit resolves to the value
/// the browser would have assumed anyway.
pub const BROWSER_TIMEOUT_MS_DEFAULT: u64 = 1_000;
pub const BROWSER_MEMORY_BYTES_DEFAULT: u64 = 32 * 1024 * 1024;
pub const BROWSER_STACK_BYTES_DEFAULT: u64 = 512 * 1024;

/// Ceilings a tenant's own fluid.json can never exceed (the
/// `ContainerLimits::for_container` clamp precedent: a tenant request must
/// never remove the ceiling entirely). The timeout ceiling matches the
/// gateway's default browser circuit (`HIVE_BROWSER_CIRCUIT_MS`, 30 s) — a
/// longer invocation would be cut by the circuit anyway.
pub const BROWSER_TIMEOUT_MS_MAX: u64 = 30_000;
pub const BROWSER_MEMORY_BYTES_MAX: u64 = 256 * 1024 * 1024;
pub const BROWSER_STACK_BYTES_MAX: u64 = 8 * 1024 * 1024;
/// Bound on the policy's op allowlist — keeps the canonical policy encoding
/// (and thus every descriptor riding the replicated state) small.
pub const BROWSER_ALLOWED_OPS_MAX: usize = 16;

/// Execution substrate for a browser artifact. Wire strings match
/// function-runtime.js's `mode` exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserExecMode {
    /// quickjs-emscripten inside the sandboxed frame: deterministic metering
    /// (memory/stack/interrupt quotas) — the default and the only mode with
    /// engine-enforced limits.
    #[default]
    Quickjs,
    /// Same-frame Worker running the artifact directly: full-JIT throughput,
    /// no engine quotas (wall-clock timeout only).
    Native,
}

impl BrowserExecMode {
    /// The mode byte of the canonical policy encoding (function-runtime.js:
    /// `policy.mode === "native" ? 0 : 1`).
    fn policy_byte(self) -> u8 {
        match self {
            BrowserExecMode::Native => 0,
            BrowserExecMode::Quickjs => 1,
        }
    }
}

/// The browser-execution opt-in entry from fluid.json `functions[].browser`.
/// Its PRESENCE is the opt-in; every field has a platform default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BrowserPolicy {
    /// Entry file, relative to the deployment root — a single self-contained
    /// `.js`/`.mjs`/`.cjs` file assigning its handler to `module.exports`
    /// (or `exports.handler`). Required: the function's `start_cmd` entry is
    /// the long-running SERVER, a different contract from the request→response
    /// browser handler, so it is never silently reused. TypeScript entries are
    /// rejected loudly (no deterministic toolchain-free transpile exists) —
    /// ship the compiled JS.
    pub entry: String,
    #[serde(default)]
    pub mode: BrowserExecMode,
    /// Per-invocation wall-clock budget, ms. 0 = platform default, clamped to
    /// the platform ceiling.
    #[serde(default)]
    pub timeout_ms: u64,
    /// QuickJS `setMemoryLimit`. 0 = default, clamped to ceiling.
    #[serde(default)]
    pub memory_bytes: u64,
    /// QuickJS `setMaxStackSize`. 0 = default, clamped to ceiling.
    #[serde(default)]
    pub stack_bytes: u64,
    /// Host-op ids the artifact may call (`ops.call(id, payload)`), each
    /// resolvable in [`browser_host_op_abi`]. Ids are deduplicated and sorted
    /// at resolution so the canonical policy encoding is order-insensitive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ops: Vec<u64>,
}

/// A [`BrowserPolicy`] after validation: every limit concrete, op ids sorted
/// + deduplicated, every id resolvable to a host-op ABI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBrowserPolicy {
    pub mode: BrowserExecMode,
    pub timeout_ms: u64,
    pub memory_bytes: u64,
    pub stack_bytes: u64,
    pub allowed_ops: Vec<u64>,
    /// Human-readable notes for the build log — limits the tenant set that
    /// the platform ceiling clamped.
    pub notes: Vec<String>,
}

impl BrowserPolicy {
    /// Validate + resolve to concrete limits. Unknown host-op ids are a hard
    /// error (loud build rejection); over-ceiling limits clamp with a note,
    /// matching the container-limits convention.
    pub fn resolve(&self) -> Result<ResolvedBrowserPolicy, String> {
        let mut notes = Vec::new();
        let clamp = |value: u64, default: u64, max: u64, name: &str, notes: &mut Vec<String>| {
            let resolved = if value == 0 { default } else { value };
            if resolved > max {
                notes.push(format!(
                    "{name} {resolved} exceeds the platform ceiling {max} — clamped"
                ));
                max
            } else {
                resolved
            }
        };
        let timeout_ms = clamp(
            self.timeout_ms,
            BROWSER_TIMEOUT_MS_DEFAULT,
            BROWSER_TIMEOUT_MS_MAX,
            "timeout_ms",
            &mut notes,
        );
        let memory_bytes = clamp(
            self.memory_bytes,
            BROWSER_MEMORY_BYTES_DEFAULT,
            BROWSER_MEMORY_BYTES_MAX,
            "memory_bytes",
            &mut notes,
        );
        let stack_bytes = clamp(
            self.stack_bytes,
            BROWSER_STACK_BYTES_DEFAULT,
            BROWSER_STACK_BYTES_MAX,
            "stack_bytes",
            &mut notes,
        );
        let mut allowed_ops = self.allowed_ops.clone();
        allowed_ops.sort_unstable();
        allowed_ops.dedup();
        if allowed_ops.len() > BROWSER_ALLOWED_OPS_MAX {
            return Err(format!(
                "allowed_ops lists {} host ops, over the {BROWSER_ALLOWED_OPS_MAX}-op bound",
                allowed_ops.len()
            ));
        }
        for op in &allowed_ops {
            if browser_host_op_abi(*op).is_none() {
                return Err(format!(
                    "allowed_ops references unknown host op {op} — the platform browser-op registry \
                     (fluid_core::browser_host_op_abi) has no entry for it"
                ));
            }
        }
        Ok(ResolvedBrowserPolicy {
            mode: self.mode,
            timeout_ms,
            memory_bytes,
            stack_bytes,
            allowed_ops,
            notes,
        })
    }
}

/// Descriptor of one build-emitted browser artifact. This is ALL the
/// replicated deployment state ever carries about an artifact — the source
/// bytes stay in the building node's content-addressed store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserArtifact {
    /// BLAKE3 (lowercase hex) of the exact emitted source string — what
    /// `BrowserFunctionRuntime.pin` re-computes from the delivered bytes and
    /// refuses to pin on mismatch.
    pub source_digest: String,
    /// BLAKE3 of the canonical policy encoding — binds `source_digest` to
    /// this exact policy. THE wire digest: `encode_invoke`'s code digest,
    /// `pin`'s return value, the admission's `digest`.
    pub policy_digest: String,
    /// Byte length of the emitted source (UTF-8).
    pub source_bytes: u64,
    pub mode: BrowserExecMode,
    pub timeout_ms: u64,
    pub memory_bytes: u64,
    pub stack_bytes: u64,
    /// Sorted + deduplicated (canonical form).
    pub allowed_ops: Vec<u64>,
}

/// One browser-eligible function of a deployment, carried by
/// [`DeploymentInfo::browser_functions`] so a peer (the admission-validating
/// control-plane leader in particular) can tie an admission's digest to a
/// real build artifact without holding the deployment's full manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserFunctionRef {
    pub name: String,
    #[serde(flatten)]
    pub artifact: BrowserArtifact,
}

/// The NEGATIVE half of [`BrowserFunctionRef`]: one function the build
/// evaluated for browser eligibility and declined, with the reason. Carried on
/// [`DeploymentInfo::browser_ineligible`] for exactly the same reason its
/// positive twin is carried — the picker (and any operator view) resolves it
/// for deployments hosted on other nodes — and because a browser-eligibility
/// decision that only exists in a build log is invisible to the person who
/// asked for it. Absent from a deployment built before the build pipeline
/// evaluated eligibility at all, which is itself a distinguishable state (no
/// artifact AND no reason ⇒ never evaluated ⇒ redeploy).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserIneligibility {
    pub function: String,
    pub reason: String,
}

/// The platform browser host-op registry: op id → canonical ABI string. This
/// is the ONLY source of truth for which host operations a built artifact may
/// request — the browser side registers handlers under these exact ABI
/// strings (`crates/hive-browser/www/index.html`'s harness ops 1/2,
/// `node-compat.js`'s op 16), and the canonical policy digest commits to the
/// ABI text, so an id missing here is an unresolvable — and therefore loudly
/// rejected — policy.
pub fn browser_host_op_abi(op: u64) -> Option<&'static str> {
    match op {
        1 => Some("hive-browser/identity-json-v1"),
        2 => Some("hive-browser/utf8-array-buffer-v1"),
        16 => Some("hive.node-compat.fs-read/v1"),
        _ => None,
    }
}

/// BLAKE3 (lowercase hex) of an emitted artifact source string — identical to
/// function-runtime.js's `sourceDigest(hash, source)`.
pub fn browser_source_digest(source: &str) -> String {
    blake3::hash(source.as_bytes()).to_hex().to_string()
}

/// BLAKE3 (lowercase hex) of the canonical policy encoding — byte-for-byte
/// function-runtime.js's `policyDigest`:
/// `"hive-browser-policy-v1\0" || source_digest(32 raw bytes) || u32le op_count
/// || per op (ascending id): u64le id || u8 effect(0=read) || u32le abi_len || abi
/// || u8 mode (0=native, 1=quickjs) || u64le timeout_ms || u64le memory_bytes
/// || u64le stack_bytes`.
///
/// `allowed_ops` must already be in canonical (sorted, deduplicated) form and
/// every id must resolve in [`browser_host_op_abi`] — callers get both by
/// going through [`BrowserPolicy::resolve`].
pub fn browser_policy_digest(
    source_digest: &str,
    mode: BrowserExecMode,
    timeout_ms: u64,
    memory_bytes: u64,
    stack_bytes: u64,
    allowed_ops: &[u64],
) -> Result<String, String> {
    if source_digest.len() != 64
        || !source_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "source digest must be 64 lowercase hexadecimal characters, got {source_digest:?}"
        ));
    }
    let mut raw = [0u8; 32];
    for (i, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&source_digest[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("source digest is not valid hex: {e}"))?;
    }
    let mut abis = Vec::with_capacity(allowed_ops.len());
    for op in allowed_ops {
        let abi = browser_host_op_abi(*op)
            .ok_or_else(|| format!("allowed_ops references unknown host op {op}"))?;
        abis.push((*op, abi));
    }
    let mut bytes = Vec::with_capacity(
        BROWSER_POLICY_DIGEST_DOMAIN.len()
            + 32
            + 4
            + abis
                .iter()
                .map(|(_, abi)| 8 + 1 + 4 + abi.len())
                .sum::<usize>()
            + 1
            + 8 * 3,
    );
    bytes.extend_from_slice(BROWSER_POLICY_DIGEST_DOMAIN);
    bytes.extend_from_slice(&raw);
    bytes.extend_from_slice(&(abis.len() as u32).to_le_bytes());
    for (op, abi) in &abis {
        bytes.extend_from_slice(&op.to_le_bytes());
        // effect: 0 = read (the only effect the browser runtime accepts today;
        // write ops are rejected by readOperation before any of this runs).
        bytes.push(0);
        bytes.extend_from_slice(&(abi.len() as u32).to_le_bytes());
        bytes.extend_from_slice(abi.as_bytes());
    }
    bytes.push(mode.policy_byte());
    bytes.extend_from_slice(&timeout_ms.to_le_bytes());
    bytes.extend_from_slice(&memory_bytes.to_le_bytes());
    bytes.extend_from_slice(&stack_bytes.to_le_bytes());
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

// ---------------------------------------------------------------------------
// Browser-replicated database opt-in (bn-browser-db-ownership-contract)
// ---------------------------------------------------------------------------
//
// A project gets a browser-replicated cr-sqlite database ONLY by opting in
// through a fluid.json top-level `browser_db` block — the same presence-is-
// the-opt-in discipline as [`FunctionConfig::browser`], and the same fail-the-
// input-loudly boundary as [`Manifest::from_json`]'s strict validation (a
// mistyped block is a deploy error, never a silently dropped opt-in). The
// SPEC rides the replicated deployment state — `Manifest::browser_db` inside
// `DeployRecord` (persisted in `PlatformSnapshot`) and stamped verbatim onto
// [`DeploymentInfo::browser_db`] for the `/v1/fleet-deployments` gossip view —
// so the admission-issuing leader and every fleet exchange peer resolve the
// exact caps for deployments they do not host, without any client input.
//
// The full ownership/retention/naming contract (what the browser↔fleet
// exchange row implements against) is `docs/browser-db-contract.md`. The
// load-bearing shape decisions:
//
// * ONE logical database per PROJECT, not per deployment and not per
//   function. Database identity is the project — the `container_volume_cfg`
//   `hive-vol-{project}` precedent, so data survives redeploys — while the
//   opt-in spec and every grant are resolved against a specific Ready
//   deployment's descriptor. A redeploy that keeps the block re-uses the same
//   database; a redeploy that drops it stops new grants.
// * The block is REPLICATED RAW and resolved at the point of use
//   ([`BrowserDbPolicy::resolve`]) — the `InferenceSpec` precedent (raw spec
//   synced, validated at consume), so every consumer applies its own binary's
//   defaults/ceilings deterministically and a pre-upgrade peer simply carries
//   no field (`serde(default)`).
// * Resolution can only clamp (never hard-fail): there is no tenant-authored
//   value that is dangerous to the platform, only values that exceed a
//   platform ceiling, and those clamp with a build-log note — the
//   `ContainerLimits::for_container` / [`BrowserPolicy::resolve`] convention.

/// Per-replica total database size cap, enforced on BOTH the browser's OPFS
/// copy and every fleet-side replica file. 64 MiB is generous for an edge app's
/// replicated dataset; the 1 GiB ceiling exists because browser OPFS copies
/// live under the origin's storage quota inside a tab — a larger dataset
/// belongs on the platform's server-side storage paths, not replicated into
/// donors' browsers.
pub const BROWSER_DB_MAX_BYTES_DEFAULT: u64 = 64 * 1024 * 1024;
pub const BROWSER_DB_MAX_BYTES_MAX: u64 = 1024 * 1024 * 1024;
/// Cap on ONE change's value payload (a single `crsql_changes` `val`), enforced
/// at the sync boundary in both directions: an oversized value stays in its
/// origin replica and is refused replication, loudly, rather than ever being
/// truncated (truncation in an LWW store is silent permanent divergence).
/// Change rows travel as structured-clone JSON on the browser side and HCB1
/// frames on the fleet side; a payload past the 16 MiB ceiling belongs in the
/// content-addressed asset store, not in a CRR cell.
pub const BROWSER_DB_VALUE_MAX_BYTES_DEFAULT: u64 = 1024 * 1024;
pub const BROWSER_DB_VALUE_MAX_BYTES_MAX: u64 = 16 * 1024 * 1024;

/// The browser-database opt-in entry from fluid.json's top-level `browser_db`
/// block. Its PRESENCE is the opt-in; every field has a platform default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDbPolicy {
    /// Per-replica total size cap in bytes (browser OPFS copy AND each
    /// fleet-side replica file). 0 = platform default, clamped to the platform
    /// ceiling. Exceeding it refuses the applying change set with a typed
    /// quota error and rolls the batch back — never a truncated replica.
    #[serde(default)]
    pub max_bytes: u64,
    /// Single change-value payload cap in bytes. 0 = platform default, clamped
    /// to the platform ceiling. Enforced at the sync boundary (export/apply),
    /// not on local SQL execution: an oversized value persists in its origin
    /// replica but never replicates, and the sync error names the table/pk.
    #[serde(default)]
    pub max_value_bytes: u64,
    /// Allow PUBLIC-scope admissions (anonymous donors) a READ-ONLY replica of
    /// this project's database. Default `false`: only Team-scope admissions —
    /// browsers operated by members of the owning tenant — hold DB grants at
    /// all. Read-only means the fleet applies nothing originating from that
    /// grant (`changes-since` export only); a public donor must never write
    /// tenant data regardless of the toggle.
    #[serde(default, skip_serializing_if = "is_false")]
    pub public_read: bool,
    /// The database's schema as an ordered list of tables to create and
    /// upgrade to CRRs. cr-sqlite v0.17 does NOT replicate schema inside
    /// `crsql_changes`, so both replica halves (the fleet replica file and
    /// every browser OPFS copy) must create the same tables out-of-band —
    /// carrying the DDL in the spec is how every side derives it from the
    /// SAME server-replicated source (admission's `db` capability block hands
    /// it to the browser verbatim; the fleet reconcile applies it to the
    /// replica file). Author each `ddl` idempotent (`CREATE TABLE IF NOT
    /// EXISTS`); the platform runs it and then `crsql_as_crr(name)` on every
    /// replica it touches. An opted-in deployment with an EMPTY schema can
    /// still sync — no tracked tables means exports are simply empty until a
    /// later deploy adds tables (loud apply errors, never silent divergence).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schema: Vec<BrowserDbTable>,
}

/// One CRR-tracked table of a [`BrowserDbPolicy`]'s schema: the table `name`
/// (validated identifier-shaped at resolve time) plus the idempotent DDL that
/// creates it. `name` is what `crsql_as_crr` upgrades; `ddl` runs verbatim
/// first. Splitting the two (instead of parsing the DDL for its table name)
/// keeps the platform's `as_crr` call explicit and the validation trivial.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDbTable {
    /// Base table name — `[A-Za-z_][A-Za-z0-9_]*` only (the same identifier
    /// rule the browser worker's `as-crr` op enforces).
    pub name: String,
    /// Idempotent DDL creating the table (`CREATE TABLE IF NOT EXISTS ...`).
    /// Executed verbatim against each replica before `crsql_as_crr(name)`.
    pub ddl: String,
}

/// A [`BrowserDbPolicy`] after resolution: every cap concrete (defaults
/// applied, ceilings enforced), contradictions clamped with a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBrowserDbPolicy {
    pub max_bytes: u64,
    pub max_value_bytes: u64,
    pub public_read: bool,
    /// The schema subset that survived validation (identifier-shaped table
    /// name, non-empty DDL), in declared order. Rejected entries produced a
    /// note — they never silently vanish.
    pub schema: Vec<BrowserDbTable>,
    /// Human-readable notes for the build log — values the platform ceilings
    /// (or internal consistency) clamped.
    pub notes: Vec<String>,
}

impl BrowserDbPolicy {
    /// Resolve to concrete caps. Infallible by design (see the section docs):
    /// over-ceiling values clamp with a note, and a `max_value_bytes` larger
    /// than `max_bytes` — a single value that could never fit the database —
    /// clamps to `max_bytes` with a note rather than failing the deploy.
    pub fn resolve(&self) -> ResolvedBrowserDbPolicy {
        let mut notes = Vec::new();
        let clamp = |value: u64, default: u64, max: u64, name: &str, notes: &mut Vec<String>| {
            let resolved = if value == 0 { default } else { value };
            if resolved > max {
                notes.push(format!(
                    "{name} {resolved} exceeds the platform ceiling {max} — clamped"
                ));
                max
            } else {
                resolved
            }
        };
        let max_bytes = clamp(
            self.max_bytes,
            BROWSER_DB_MAX_BYTES_DEFAULT,
            BROWSER_DB_MAX_BYTES_MAX,
            "max_bytes",
            &mut notes,
        );
        let mut max_value_bytes = clamp(
            self.max_value_bytes,
            BROWSER_DB_VALUE_MAX_BYTES_DEFAULT,
            BROWSER_DB_VALUE_MAX_BYTES_MAX,
            "max_value_bytes",
            &mut notes,
        );
        if max_value_bytes > max_bytes {
            notes.push(format!(
                "max_value_bytes {max_value_bytes} exceeds max_bytes {max_bytes} — clamped to max_bytes"
            ));
            max_value_bytes = max_bytes;
        }
        let mut schema = Vec::with_capacity(self.schema.len());
        for table in &self.schema {
            let valid_name = {
                let mut chars = table.name.chars();
                matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            };
            if !valid_name {
                notes.push(format!(
                    "schema table {:?} skipped — name must match [A-Za-z_][A-Za-z0-9_]*",
                    table.name
                ));
                continue;
            }
            if table.ddl.trim().is_empty() {
                notes.push(format!(
                    "schema table {:?} skipped — ddl is empty (author CREATE TABLE IF NOT EXISTS)",
                    table.name
                ));
                continue;
            }
            schema.push(table.clone());
        }
        ResolvedBrowserDbPolicy {
            max_bytes,
            max_value_bytes,
            public_read: self.public_read,
            schema,
            notes,
        }
    }
}

// `Runtime` (the language/engine selector) lives in `hive_core` — the lower
// crate in the dependency graph, reachable from `hive-cell-agent`/
// `hive-backend` (which do NOT depend on fluid-core) as well as from here.
pub use hive_core::Runtime;

// ---------------------------------------------------------------------------
// Lossless Build Output API v3 contract
// ---------------------------------------------------------------------------

pub const BUILD_OUTPUT_V3_VERSION: u32 = 3;
/// Schema baseline pinned to the Vercel Build Output Configuration reference
/// whose authoritative page reports `last_updated: 2026-07-27`.
pub const BUILD_OUTPUT_V3_SCHEMA_REVISION: &str = "vercel-build-output-v3-2026-07-27";
pub const BUILD_OUTPUT_V3_MAX_DESCRIPTOR_BYTES: usize = 8 * 1024 * 1024;
pub const BUILD_OUTPUT_V3_MAX_ROUTES: usize = 4096;
pub const BUILD_OUTPUT_V3_MAX_FUNCTIONS: usize = 1024;
pub const BUILD_OUTPUT_V3_MAX_FILES: usize = 100_000;
pub const BUILD_OUTPUT_V3_MAX_PATH_BYTES: usize = 4096;
pub const BUILD_OUTPUT_V3_MAX_VALUE_BYTES: usize = 64 * 1024;
pub const BUILD_OUTPUT_V3_MAX_JSON_NODES: usize = 100_000;
pub const BUILD_OUTPUT_V3_MAX_DEPTH: usize = 64;
pub const BUILD_OUTPUT_V3_MAX_METHODS: usize = 32;
pub const BUILD_OUTPUT_V3_MAX_HEADERS: usize = 128;
pub const BUILD_OUTPUT_V3_MAX_WILDCARDS: usize = 1024;
pub const BUILD_OUTPUT_V3_MAX_OVERRIDES: usize = 10_000;
pub const BUILD_OUTPUT_V3_MAX_CRONS: usize = 1024;
pub const BUILD_OUTPUT_V3_MAX_CACHE_PATHS: usize = 1024;
pub const BUILD_OUTPUT_V3_MAX_IMAGE_VALUES: usize = 1024;

/// Durable, host-authority-free Build Output API v3 descriptor. `config` and
/// every function config remain exact JSON values so an older binary cannot
/// silently erase a route/runtime field it does not implement. Absolute builder
/// paths never enter this type; `assets` and `files` are validated relative
/// inventories.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3 {
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<BuildOutputV3Function>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3Function {
    pub name: String,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerender: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prerender_files: Vec<String>,
}

impl BuildOutputV3Function {
    pub fn runtime(&self) -> Option<&str> {
        self.config.get("runtime").and_then(|value| value.as_str())
    }
}

/// Typed view of the exact `config` value. `extra` is deliberately retained:
/// compilation refuses it as an unsupported capability instead of dropping it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3Config {
    #[serde(default)]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<BuildOutputV3Route>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<BuildOutputV3ImagesConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wildcard: Vec<BuildOutputV3Wildcard>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, BuildOutputV3Override>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crons: Vec<BuildOutputV3Cron>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(rename = "continue", default, skip_serializing_if = "is_false")]
    pub cont: bool,
    #[serde(
        rename = "middlewarePath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub middleware_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3Wildcard {
    pub domain: String,
    pub value: String,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3Override {
    #[serde(
        rename = "contentType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3ImagesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<String>>,
    #[serde(
        rename = "remotePatterns",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remote_patterns: Vec<BuildOutputV3RemotePattern>,
    #[serde(
        rename = "localPatterns",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub local_patterns: Option<Vec<BuildOutputV3LocalPattern>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qualities: Vec<u32>,
    #[serde(
        rename = "minimumCacheTTL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub minimum_cache_ttl: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(
        rename = "dangerouslyAllowSVG",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub dangerously_allow_svg: Option<bool>,
    #[serde(
        rename = "contentSecurityPolicy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_security_policy: Option<String>,
    #[serde(
        rename = "contentDispositionType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_disposition_type: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3RemotePattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3LocalPattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildOutputV3Cron {
    pub path: String,
    pub schedule: String,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Stable typed refusal returned at conversion/provisioning/routing boundaries.
/// Callers classify on the variant or `code()`, never message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildOutputV3Refusal {
    Invalid { field: String, detail: String },
    Unsupported { feature: String },
}

impl BuildOutputV3Refusal {
    pub const INVALID_CODE: &'static str = "BUILD_OUTPUT_V3_INVALID";
    pub const UNSUPPORTED_CODE: &'static str = "BUILD_OUTPUT_V3_CAPABILITY_UNSUPPORTED";

    pub fn invalid(field: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Invalid {
            field: field.into(),
            detail: detail.into(),
        }
    }

    pub fn unsupported(feature: impl Into<String>) -> Self {
        Self::Unsupported {
            feature: feature.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => Self::INVALID_CODE,
            Self::Unsupported { .. } => Self::UNSUPPORTED_CODE,
        }
    }
}

impl std::fmt::Display for BuildOutputV3Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { field, detail } => write!(
                formatter,
                "{} {field}: {detail}",
                BuildOutputV3Refusal::INVALID_CODE
            ),
            Self::Unsupported { feature } => write!(
                formatter,
                "{}: {feature}",
                BuildOutputV3Refusal::UNSUPPORTED_CODE
            ),
        }
    }
}

impl std::error::Error for BuildOutputV3Refusal {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildOutputV3Target {
    Static {
        path: String,
        content_type: Option<String>,
    },
    Function {
        name: String,
    },
    Response {
        status: u16,
        location: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildOutputV3Resolution {
    pub target: Option<BuildOutputV3Target>,
    pub rewritten_path: String,
    pub headers: BTreeMap<String, String>,
    pub route_matched: bool,
}

#[derive(Clone, Debug)]
pub struct BuildOutputV3Evaluator {
    routes: Vec<CompiledBuildOutputV3Route>,
    outputs: BTreeMap<String, BuildOutputV3Target>,
}

#[derive(Clone, Debug)]
enum CompiledBuildOutputV3Route {
    Filesystem,
    Source {
        source: String,
        regex: regex::Regex,
        dest: Option<String>,
        headers: BTreeMap<String, String>,
        status: Option<u16>,
        cont: bool,
        methods: Vec<String>,
    },
}

impl BuildOutputV3 {
    /// Deserialize the host-path-free envelope emitted by
    /// `fluid_build::BuildOutput::descriptor_value`, then enforce the durable
    /// contract. This serde bridge keeps the crate dependency graph acyclic.
    pub fn from_parser_value(
        value: serde_json::Value,
    ) -> Result<BuildOutputV3, BuildOutputV3Refusal> {
        let descriptor: BuildOutputV3 = serde_json::from_value(value).map_err(|error| {
            BuildOutputV3Refusal::invalid("descriptor", format!("malformed envelope: {error}"))
        })?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn config_view(&self) -> Result<BuildOutputV3Config, BuildOutputV3Refusal> {
        serde_json::from_value(self.config.clone()).map_err(|error| {
            BuildOutputV3Refusal::invalid("config", format!("malformed config.json: {error}"))
        })
    }

    pub fn validate(&self) -> Result<(), BuildOutputV3Refusal> {
        let descriptor_bytes = serde_json::to_vec(self)
            .map_err(|error| {
                BuildOutputV3Refusal::invalid(
                    "descriptor",
                    format!("cannot serialize descriptor: {error}"),
                )
            })?
            .len();
        if descriptor_bytes > BUILD_OUTPUT_V3_MAX_DESCRIPTOR_BYTES {
            return Err(BuildOutputV3Refusal::invalid(
                "descriptor",
                format!("{descriptor_bytes} bytes exceeds {BUILD_OUTPUT_V3_MAX_DESCRIPTOR_BYTES}"),
            ));
        }
        let mut nodes = 0usize;
        validate_build_output_json(&self.config, "config", 0, &mut nodes)?;
        let config = self.config_view()?;
        if config.version != BUILD_OUTPUT_V3_VERSION {
            return Err(BuildOutputV3Refusal::invalid(
                "config.version",
                format!(
                    "expected exactly {BUILD_OUTPUT_V3_VERSION}, got {}",
                    config.version
                ),
            ));
        }
        if config.routes.len() > BUILD_OUTPUT_V3_MAX_ROUTES {
            return Err(BuildOutputV3Refusal::invalid(
                "config.routes",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_ROUTES}",
                    config.routes.len()
                ),
            ));
        }
        if config.wildcard.len() > BUILD_OUTPUT_V3_MAX_WILDCARDS {
            return Err(BuildOutputV3Refusal::invalid(
                "config.wildcard",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_WILDCARDS}",
                    config.wildcard.len()
                ),
            ));
        }
        if config.overrides.len() > BUILD_OUTPUT_V3_MAX_OVERRIDES {
            return Err(BuildOutputV3Refusal::invalid(
                "config.overrides",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_OVERRIDES}",
                    config.overrides.len()
                ),
            ));
        }
        if config.crons.len() > BUILD_OUTPUT_V3_MAX_CRONS {
            return Err(BuildOutputV3Refusal::invalid(
                "config.crons",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_CRONS}",
                    config.crons.len()
                ),
            ));
        }
        if config.cache.len() > BUILD_OUTPUT_V3_MAX_CACHE_PATHS {
            return Err(BuildOutputV3Refusal::invalid(
                "config.cache",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_CACHE_PATHS}",
                    config.cache.len()
                ),
            ));
        }
        for path in &config.cache {
            validate_build_output_text(path, "config.cache[]")?;
        }
        if let Some(images) = &config.images {
            validate_build_output_images(images)?;
        }
        if self.functions.len() > BUILD_OUTPUT_V3_MAX_FUNCTIONS {
            return Err(BuildOutputV3Refusal::invalid(
                "functions",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_FUNCTIONS}",
                    self.functions.len()
                ),
            ));
        }
        if self.assets.len() > BUILD_OUTPUT_V3_MAX_FILES {
            return Err(BuildOutputV3Refusal::invalid(
                "assets",
                format!(
                    "{} entries exceeds {BUILD_OUTPUT_V3_MAX_FILES}",
                    self.assets.len()
                ),
            ));
        }
        ensure_sorted_unique(&self.assets, "assets")?;
        for asset in &self.assets {
            validate_build_output_relative_path(asset, "assets")?;
        }

        let mut prior_function: Option<&str> = None;
        let mut total_files = self.assets.len();
        for function in &self.functions {
            if prior_function.is_some_and(|prior| prior >= function.name.as_str()) {
                return Err(BuildOutputV3Refusal::invalid(
                    "functions",
                    "function names must be strictly sorted and unique",
                ));
            }
            prior_function = Some(&function.name);
            validate_build_output_relative_path(&function.name, "functions[].name")?;
            validate_build_output_json(
                &function.config,
                &format!("functions[{:?}].config", function.name),
                0,
                &mut nodes,
            )?;
            let config_object = function.config.as_object().ok_or_else(|| {
                BuildOutputV3Refusal::invalid(
                    format!("functions[{:?}].config", function.name),
                    "must be a JSON object",
                )
            })?;
            let runtime = config_object
                .get("runtime")
                .and_then(|value| value.as_str())
                .filter(|runtime| !runtime.is_empty())
                .ok_or_else(|| {
                    BuildOutputV3Refusal::invalid(
                        format!("functions[{:?}].config.runtime", function.name),
                        "must be a non-empty string",
                    )
                })?;
            let entry_key = if runtime == "edge" {
                "entrypoint"
            } else {
                "handler"
            };
            if config_object
                .get(entry_key)
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("functions[{:?}].config.{entry_key}", function.name),
                    "must be a non-empty string",
                ));
            }
            if let Some(prerender) = &function.prerender {
                if !prerender.is_object() {
                    return Err(BuildOutputV3Refusal::invalid(
                        format!("functions[{:?}].prerender", function.name),
                        "must be a JSON object",
                    ));
                }
                validate_build_output_json(
                    prerender,
                    &format!("functions[{:?}].prerender", function.name),
                    0,
                    &mut nodes,
                )?;
                validate_build_output_prerender(
                    &function.name,
                    prerender,
                    &function.prerender_files,
                )?;
            } else if !function.prerender_files.is_empty() {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("functions[{:?}].prerender_files", function.name),
                    "fallback payloads require prerender metadata",
                ));
            }
            ensure_sorted_unique(
                &function.files,
                &format!("functions[{:?}].files", function.name),
            )?;
            for file in &function.files {
                validate_build_output_relative_path(
                    file,
                    &format!("functions[{:?}].files", function.name),
                )?;
            }
            ensure_sorted_unique(
                &function.prerender_files,
                &format!("functions[{:?}].prerender_files", function.name),
            )?;
            for file in &function.prerender_files {
                validate_build_output_relative_path(
                    file,
                    &format!("functions[{:?}].prerender_files", function.name),
                )?;
            }
            total_files = total_files
                .saturating_add(function.files.len())
                .saturating_add(function.prerender_files.len());
            if total_files > BUILD_OUTPUT_V3_MAX_FILES {
                return Err(BuildOutputV3Refusal::invalid(
                    "files",
                    format!("aggregate file count exceeds {BUILD_OUTPUT_V3_MAX_FILES}"),
                ));
            }
        }
        Ok(())
    }

    /// Compile ordered routes and exact output indexes once at provisioning.
    /// Unknown top-level/route semantics and domain wildcard routing are stable
    /// capability refusals, never ignored metadata.
    pub fn compile(&self) -> Result<BuildOutputV3Evaluator, BuildOutputV3Refusal> {
        self.validate()?;
        let config = self.config_view()?;
        if let Some(field) = config.extra.keys().next() {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "config field {field:?}"
            )));
        }
        if !config.wildcard.is_empty() {
            return Err(BuildOutputV3Refusal::unsupported(
                "config.wildcard domain routing",
            ));
        }
        for (index, wildcard) in config.wildcard.iter().enumerate() {
            if let Some(field) = wildcard.extra.keys().next() {
                return Err(BuildOutputV3Refusal::unsupported(format!(
                    "config.wildcard[{index}] field {field:?}"
                )));
            }
        }
        for (path, path_override) in &config.overrides {
            if let Some(field) = path_override.extra.keys().next() {
                return Err(BuildOutputV3Refusal::unsupported(format!(
                    "config.overrides[{path:?}] field {field:?}"
                )));
            }
            validate_build_output_relative_path(path, "config.overrides key")?;
            if self.assets.binary_search(path).is_err() {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("config.overrides[{path:?}]"),
                    "does not name an indexed static asset",
                ));
            }
            if let Some(content_type) = &path_override.content_type {
                validate_build_output_text(content_type, "config.overrides.contentType")?;
                if content_type.contains(['\r', '\n']) {
                    return Err(BuildOutputV3Refusal::invalid(
                        format!("config.overrides[{path:?}].contentType"),
                        "contains a line break",
                    ));
                }
            }
        }
        for (index, cron) in config.crons.iter().enumerate() {
            if let Some(field) = cron.extra.keys().next() {
                return Err(BuildOutputV3Refusal::unsupported(format!(
                    "config.crons[{index}] field {field:?}"
                )));
            }
            validate_build_output_public_path(&cron.path, "config.crons[].path")?;
            validate_build_output_text(&cron.schedule, "config.crons[].schedule")?;
        }
        if let Some(images) = &config.images {
            if let Some(field) = images.extra.keys().next() {
                return Err(BuildOutputV3Refusal::unsupported(format!(
                    "config.images field {field:?}"
                )));
            }
            for (index, pattern) in images.remote_patterns.iter().enumerate() {
                if let Some(field) = pattern.extra.keys().next() {
                    return Err(BuildOutputV3Refusal::unsupported(format!(
                        "config.images.remotePatterns[{index}] field {field:?}"
                    )));
                }
            }
            if let Some(patterns) = &images.local_patterns {
                for (index, pattern) in patterns.iter().enumerate() {
                    if let Some(field) = pattern.extra.keys().next() {
                        return Err(BuildOutputV3Refusal::unsupported(format!(
                            "config.images.localPatterns[{index}] field {field:?}"
                        )));
                    }
                }
            }
        }
        if let Some(function) = self
            .functions
            .iter()
            .find(|function| function.prerender.is_some())
        {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "prerender/ISR function {:?}",
                function.name
            )));
        }

        let mut outputs = BTreeMap::new();
        for asset in &self.assets {
            let path_override = config.overrides.get(asset);
            let content_type = path_override.and_then(|value| value.content_type.clone());
            let public_path = match path_override.and_then(|value| value.path.as_deref()) {
                Some(path) => normalize_build_output_public_path(path)?,
                None => format!("/{asset}"),
            };
            insert_build_output_target(
                &mut outputs,
                public_path,
                BuildOutputV3Target::Static {
                    path: asset.clone(),
                    content_type: content_type.clone(),
                },
            )?;
            if path_override
                .and_then(|value| value.path.as_ref())
                .is_none()
            {
                if asset == "index.html" {
                    insert_build_output_target(
                        &mut outputs,
                        "/".to_string(),
                        BuildOutputV3Target::Static {
                            path: asset.clone(),
                            content_type: content_type.clone(),
                        },
                    )?;
                } else if let Some(directory) = asset.strip_suffix("/index.html") {
                    insert_build_output_target(
                        &mut outputs,
                        format!("/{directory}/"),
                        BuildOutputV3Target::Static {
                            path: asset.clone(),
                            content_type: content_type.clone(),
                        },
                    )?;
                }
            }
        }
        for function in &self.functions {
            insert_build_output_target(
                &mut outputs,
                format!("/{}", function.name),
                BuildOutputV3Target::Function {
                    name: function.name.clone(),
                },
            )?;
        }

        let mut routes = Vec::with_capacity(config.routes.len());
        for (index, route) in config.routes.into_iter().enumerate() {
            if let Some(field) = route.extra.keys().next() {
                return Err(BuildOutputV3Refusal::unsupported(format!(
                    "config.routes[{index}] field {field:?}"
                )));
            }
            if route.middleware_path.is_some() {
                return Err(BuildOutputV3Refusal::unsupported(format!(
                    "config.routes[{index}].middlewarePath"
                )));
            }
            if let Some(handle) = route.handle {
                if route.src.is_some()
                    || route.dest.is_some()
                    || !route.headers.is_empty()
                    || route.status.is_some()
                    || route.cont
                    || !route.methods.is_empty()
                {
                    return Err(BuildOutputV3Refusal::unsupported(format!(
                        "config.routes[{index}] handle {handle:?} with src/dest/status fields"
                    )));
                }
                if handle != "filesystem" {
                    return Err(BuildOutputV3Refusal::unsupported(format!(
                        "config.routes[{index}] handle {handle:?}"
                    )));
                }
                routes.push(CompiledBuildOutputV3Route::Filesystem);
                continue;
            }
            let source = route.src.ok_or_else(|| {
                BuildOutputV3Refusal::invalid(
                    format!("config.routes[{index}].src"),
                    "is required when handle is absent",
                )
            })?;
            validate_build_output_text(&source, "config.routes[].src")?;
            if source.is_empty() {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("config.routes[{index}].src"),
                    "must not be empty",
                ));
            }
            if route.methods.len() > BUILD_OUTPUT_V3_MAX_METHODS {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("config.routes[{index}].methods"),
                    format!("exceeds {BUILD_OUTPUT_V3_MAX_METHODS} entries"),
                ));
            }
            let mut methods = Vec::with_capacity(route.methods.len());
            for method in route.methods {
                if method.is_empty() || method.len() > 32 || !method.bytes().all(is_http_token_byte)
                {
                    return Err(BuildOutputV3Refusal::invalid(
                        format!("config.routes[{index}].methods"),
                        format!("invalid HTTP method {method:?}"),
                    ));
                }
                methods.push(method.to_ascii_uppercase());
            }
            methods.sort();
            methods.dedup();
            if route.headers.len() > BUILD_OUTPUT_V3_MAX_HEADERS {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("config.routes[{index}].headers"),
                    format!("exceeds {BUILD_OUTPUT_V3_MAX_HEADERS} entries"),
                ));
            }
            let mut header_names = Vec::with_capacity(route.headers.len());
            for (name, value) in &route.headers {
                validate_build_output_header(index, name, value)?;
                let lower = name.to_ascii_lowercase();
                if header_names.contains(&lower) {
                    return Err(BuildOutputV3Refusal::invalid(
                        format!("config.routes[{index}].headers"),
                        format!("duplicate case-insensitive header name {name:?}"),
                    ));
                }
                header_names.push(lower);
            }
            if let Some(status) = route.status {
                if !(200..=599).contains(&status) {
                    return Err(BuildOutputV3Refusal::invalid(
                        format!("config.routes[{index}].status"),
                        "must be in 200..=599",
                    ));
                }
                if route.cont {
                    return Err(BuildOutputV3Refusal::unsupported(format!(
                        "config.routes[{index}] combines status with continue"
                    )));
                }
                if route.dest.is_some() {
                    return Err(BuildOutputV3Refusal::unsupported(format!(
                        "config.routes[{index}] combines status with destination"
                    )));
                }
            }
            if let Some(dest) = &route.dest {
                validate_build_output_text(dest, "config.routes[].dest")?;
                if dest.contains(['\r', '\n']) {
                    return Err(BuildOutputV3Refusal::invalid(
                        format!("config.routes[{index}].dest"),
                        "contains a line break",
                    ));
                }
                let external = dest.contains("://") || dest.starts_with("//");
                let redirect = route
                    .status
                    .is_some_and(|status| (300..=399).contains(&status));
                if external && !redirect {
                    return Err(BuildOutputV3Refusal::unsupported(format!(
                        "config.routes[{index}] external rewrite destination"
                    )));
                }
            }
            let regex = regex::RegexBuilder::new(&source)
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024)
                .build()
                .map_err(|error| {
                    BuildOutputV3Refusal::unsupported(format!(
                        "config.routes[{index}].src regex {source:?}: {error}"
                    ))
                })?;
            routes.push(CompiledBuildOutputV3Route::Source {
                source,
                regex,
                dest: route.dest,
                headers: route.headers,
                status: route.status,
                cont: route.cont,
                methods,
            });
        }
        Ok(BuildOutputV3Evaluator { routes, outputs })
    }
}

impl BuildOutputV3Evaluator {
    pub fn resolve(
        &self,
        method: &str,
        path: &str,
    ) -> Result<BuildOutputV3Resolution, BuildOutputV3Refusal> {
        let mut rewritten_path = normalize_build_output_internal_path(path)?;
        let method = method.to_ascii_uppercase();
        let mut headers = BTreeMap::new();
        let mut route_matched = false;
        for route in &self.routes {
            match route {
                CompiledBuildOutputV3Route::Filesystem => {
                    if let Some(target) = self.output_for(&rewritten_path) {
                        return Ok(BuildOutputV3Resolution {
                            target: Some(target),
                            rewritten_path,
                            headers,
                            route_matched,
                        });
                    }
                }
                CompiledBuildOutputV3Route::Source {
                    source,
                    regex,
                    dest,
                    headers: route_headers,
                    status,
                    cont,
                    methods,
                } => {
                    if !methods.is_empty()
                        && methods
                            .binary_search_by(|candidate| candidate.as_str().cmp(&method))
                            .is_err()
                    {
                        continue;
                    }
                    let current_path = build_output_path_only(&rewritten_path);
                    let Some(captures) = regex.captures(current_path) else {
                        continue;
                    };
                    route_matched = true;
                    for (name, template) in route_headers {
                        let mut value = String::new();
                        captures.expand(template, &mut value);
                        validate_build_output_text(
                            &value,
                            &format!("expanded header {name:?} from route {source:?}"),
                        )?;
                        if value.contains(['\r', '\n']) {
                            return Err(BuildOutputV3Refusal::invalid(
                                format!("route {source:?} header {name:?}"),
                                "expanded value contains a line break",
                            ));
                        }
                        headers.insert(name.clone(), value);
                    }
                    let expanded_dest = dest.as_ref().map(|template| {
                        let mut value = String::new();
                        captures.expand(template, &mut value);
                        value
                    });
                    if let Some(status) = status {
                        return Ok(BuildOutputV3Resolution {
                            target: Some(BuildOutputV3Target::Response {
                                status: *status,
                                location: None,
                            }),
                            rewritten_path,
                            headers,
                            route_matched,
                        });
                    }
                    if let Some(destination) = expanded_dest {
                        rewritten_path = normalize_build_output_internal_path(&destination)?;
                    }
                    if !cont {
                        return Ok(BuildOutputV3Resolution {
                            target: self.output_for(&rewritten_path),
                            rewritten_path,
                            headers,
                            route_matched,
                        });
                    }
                }
            }
        }
        Ok(BuildOutputV3Resolution {
            target: self.output_for(&rewritten_path),
            rewritten_path,
            headers,
            route_matched,
        })
    }

    fn output_for(&self, path: &str) -> Option<BuildOutputV3Target> {
        self.outputs.get(build_output_path_only(path)).cloned()
    }
}

fn validate_build_output_images(
    images: &BuildOutputV3ImagesConfig,
) -> Result<(), BuildOutputV3Refusal> {
    let sizes = images
        .sizes
        .as_ref()
        .ok_or_else(|| BuildOutputV3Refusal::invalid("config.images.sizes", "is required"))?;
    let domains = images
        .domains
        .as_ref()
        .ok_or_else(|| BuildOutputV3Refusal::invalid("config.images.domains", "is required"))?;
    for (field, count) in [
        ("sizes", sizes.len()),
        ("domains", domains.len()),
        ("remotePatterns", images.remote_patterns.len()),
        (
            "localPatterns",
            images.local_patterns.as_ref().map(Vec::len).unwrap_or(0),
        ),
        ("qualities", images.qualities.len()),
        ("formats", images.formats.len()),
    ] {
        if count > BUILD_OUTPUT_V3_MAX_IMAGE_VALUES {
            return Err(BuildOutputV3Refusal::invalid(
                format!("config.images.{field}"),
                format!("{count} entries exceeds {BUILD_OUTPUT_V3_MAX_IMAGE_VALUES}"),
            ));
        }
    }
    if sizes.iter().any(|size| !(1..=4096).contains(size)) {
        return Err(BuildOutputV3Refusal::invalid(
            "config.images.sizes",
            "every width must be in 1..=4096",
        ));
    }
    if images
        .qualities
        .iter()
        .any(|quality| !(1..=100).contains(quality))
    {
        return Err(BuildOutputV3Refusal::invalid(
            "config.images.qualities",
            "every quality must be in 1..=100",
        ));
    }
    for domain in domains {
        validate_build_output_text(domain, "config.images.domains[]")?;
        if domain.is_empty() || domain.contains("://") || domain.contains(['/', '\\', '\r', '\n']) {
            return Err(BuildOutputV3Refusal::invalid(
                "config.images.domains[]",
                format!("invalid domain {domain:?}"),
            ));
        }
    }
    for format in &images.formats {
        validate_build_output_text(format, "config.images.formats[]")?;
        if !matches!(format.as_str(), "image/avif" | "image/webp") {
            return Err(BuildOutputV3Refusal::invalid(
                "config.images.formats[]",
                format!("unsupported schema value {format:?}"),
            ));
        }
    }
    for (index, pattern) in images.remote_patterns.iter().enumerate() {
        if let Some(protocol) = &pattern.protocol {
            if !matches!(protocol.as_str(), "http" | "https") {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("config.images.remotePatterns[{index}].protocol"),
                    "must be http or https",
                ));
            }
        }
        validate_build_output_text(
            &pattern.hostname,
            &format!("config.images.remotePatterns[{index}].hostname"),
        )?;
        if pattern.hostname.is_empty() {
            return Err(BuildOutputV3Refusal::invalid(
                format!("config.images.remotePatterns[{index}].hostname"),
                "must not be empty",
            ));
        }
        for (field, value) in [
            ("port", pattern.port.as_deref()),
            ("pathname", pattern.pathname.as_deref()),
            ("search", pattern.search.as_deref()),
        ] {
            if let Some(value) = value {
                validate_build_output_text(
                    value,
                    &format!("config.images.remotePatterns[{index}].{field}"),
                )?;
            }
        }
    }
    if let Some(patterns) = &images.local_patterns {
        for (index, pattern) in patterns.iter().enumerate() {
            for (field, value) in [
                ("pathname", pattern.pathname.as_deref()),
                ("search", pattern.search.as_deref()),
            ] {
                if let Some(value) = value {
                    validate_build_output_text(
                        value,
                        &format!("config.images.localPatterns[{index}].{field}"),
                    )?;
                }
            }
        }
    }
    for (field, value) in [
        (
            "contentSecurityPolicy",
            images.content_security_policy.as_deref(),
        ),
        (
            "contentDispositionType",
            images.content_disposition_type.as_deref(),
        ),
    ] {
        if let Some(value) = value {
            validate_build_output_text(value, &format!("config.images.{field}"))?;
            if value.contains(['\r', '\n']) {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("config.images.{field}"),
                    "contains a line break",
                ));
            }
        }
    }
    Ok(())
}

fn validate_build_output_prerender(
    name: &str,
    prerender: &serde_json::Value,
    fallback_files: &[String],
) -> Result<(), BuildOutputV3Refusal> {
    let object = prerender.as_object().ok_or_else(|| {
        BuildOutputV3Refusal::invalid(
            format!("functions[{name:?}].prerender"),
            "must be an object",
        )
    })?;
    match object.get("expiration") {
        Some(serde_json::Value::Bool(false)) => {}
        Some(serde_json::Value::Number(number)) if number.as_u64().is_some() => {}
        _ => {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender.expiration"),
                "must be a non-negative integer or false",
            ))
        }
    }
    if object
        .get("group")
        .is_some_and(|value| value.as_u64().is_none())
    {
        return Err(BuildOutputV3Refusal::invalid(
            format!("functions[{name:?}].prerender.group"),
            "must be a non-negative integer",
        ));
    }
    for field in ["bypassToken", "fallback"] {
        if object
            .get(field)
            .is_some_and(|value| value.as_str().is_none())
        {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender.{field}"),
                "must be a string",
            ));
        }
    }
    if let Some(values) = object.get("allowQuery") {
        let valid = values.as_array().is_some_and(|values| {
            values.len() <= 1024 && values.iter().all(|value| value.as_str().is_some())
        });
        if !valid {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender.allowQuery"),
                "must contain at most 1024 strings",
            ));
        }
    }
    for field in ["passQuery", "exposeErrBody"] {
        if object
            .get(field)
            .is_some_and(|value| value.as_bool().is_none())
        {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender.{field}"),
                "must be a boolean",
            ));
        }
    }
    if let Some(headers) = object.get("initialHeaders") {
        let valid = headers.as_object().is_some_and(|headers| {
            headers.len() <= BUILD_OUTPUT_V3_MAX_HEADERS
                && headers.values().all(|value| value.as_str().is_some())
        });
        if !valid {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender.initialHeaders"),
                "must contain at most 128 string values",
            ));
        }
    }
    if object.get("initialStatus").is_some_and(|status| {
        !status
            .as_u64()
            .is_some_and(|status| (100..=599).contains(&status))
    }) {
        return Err(BuildOutputV3Refusal::invalid(
            format!("functions[{name:?}].prerender.initialStatus"),
            "must be in 100..=599",
        ));
    }
    let expected_fallback = object
        .get("fallback")
        .and_then(serde_json::Value::as_str)
        .map(|fallback| {
            validate_build_output_relative_path(fallback, "prerender fallback")?;
            let parent = name.rsplit_once('/').map(|(parent, _)| parent);
            let path = parent
                .map(|parent| format!("{parent}/{fallback}"))
                .unwrap_or_else(|| fallback.to_string());
            validate_build_output_relative_path(&path, "prerender fallback")?;
            Ok::<String, BuildOutputV3Refusal>(path)
        })
        .transpose()?;
    match expected_fallback {
        Some(expected) if fallback_files.len() == 1 && fallback_files[0] == expected => {}
        Some(expected) => {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender_files"),
                format!("must contain exactly referenced fallback {expected:?}"),
            ))
        }
        None if fallback_files.is_empty() => {}
        None => {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{name:?}].prerender_files"),
                "contains a fallback without prerender.fallback",
            ))
        }
    }
    Ok(())
}

fn validate_build_output_json(
    value: &serde_json::Value,
    field: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), BuildOutputV3Refusal> {
    if depth > BUILD_OUTPUT_V3_MAX_DEPTH {
        return Err(BuildOutputV3Refusal::invalid(
            field,
            format!("JSON depth exceeds {BUILD_OUTPUT_V3_MAX_DEPTH}"),
        ));
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > BUILD_OUTPUT_V3_MAX_JSON_NODES {
        return Err(BuildOutputV3Refusal::invalid(
            field,
            format!("JSON node count exceeds {BUILD_OUTPUT_V3_MAX_JSON_NODES}"),
        ));
    }
    match value {
        serde_json::Value::String(text) => validate_build_output_text(text, field)?,
        serde_json::Value::Array(values) => {
            for value in values {
                validate_build_output_json(value, field, depth + 1, nodes)?;
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                validate_build_output_text(key, field)?;
                validate_build_output_json(value, field, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_build_output_text(value: &str, field: &str) -> Result<(), BuildOutputV3Refusal> {
    if value.len() > BUILD_OUTPUT_V3_MAX_VALUE_BYTES {
        return Err(BuildOutputV3Refusal::invalid(
            field,
            format!("string exceeds {BUILD_OUTPUT_V3_MAX_VALUE_BYTES} bytes"),
        ));
    }
    if value.contains('\0') {
        return Err(BuildOutputV3Refusal::invalid(field, "contains a NUL byte"));
    }
    Ok(())
}

fn ensure_sorted_unique(values: &[String], field: &str) -> Result<(), BuildOutputV3Refusal> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(BuildOutputV3Refusal::invalid(
            field,
            "must be strictly sorted and unique",
        ));
    }
    Ok(())
}

fn validate_build_output_relative_path(
    value: &str,
    field: &str,
) -> Result<(), BuildOutputV3Refusal> {
    validate_build_output_text(value, field)?;
    if value.is_empty()
        || value.len() > BUILD_OUTPUT_V3_MAX_PATH_BYTES
        || value.starts_with('/')
        || value.ends_with('/')
        || value
            .bytes()
            .any(|byte| byte == b'\\' || byte == b':' || byte.is_ascii_control())
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(BuildOutputV3Refusal::invalid(
            field,
            format!("path {value:?} is not a portable normalized relative path"),
        ));
    }
    Ok(())
}

fn validate_build_output_public_path(value: &str, field: &str) -> Result<(), BuildOutputV3Refusal> {
    normalize_build_output_public_path(value)
        .map(|_| ())
        .map_err(|error| match error {
            BuildOutputV3Refusal::Invalid { detail, .. } => {
                BuildOutputV3Refusal::invalid(field, detail)
            }
            other => other,
        })
}

fn normalize_build_output_public_path(value: &str) -> Result<String, BuildOutputV3Refusal> {
    validate_build_output_text(value, "public path")?;
    if value.contains(['?', '#'])
        || value.contains("://")
        || value.starts_with("//")
        || value
            .bytes()
            .any(|byte| byte == b'\\' || byte.is_ascii_control())
    {
        return Err(BuildOutputV3Refusal::invalid(
            "public path",
            format!("path {value:?} is not an internal URL path"),
        ));
    }
    let trimmed = value.trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok("/".to_string());
    }
    validate_build_output_relative_path(trimmed.trim_end_matches('/'), "public path")?;
    Ok(if value.ends_with('/') {
        format!("/{}/", trimmed.trim_end_matches('/'))
    } else {
        format!("/{trimmed}")
    })
}

fn normalize_build_output_internal_path(value: &str) -> Result<String, BuildOutputV3Refusal> {
    validate_build_output_text(value, "route destination")?;
    if value.contains('#')
        || value.contains("://")
        || value.starts_with("//")
        || value
            .bytes()
            .any(|byte| byte == b'\\' || byte.is_ascii_control())
    {
        return Err(BuildOutputV3Refusal::unsupported(format!(
            "external or non-portable route destination {value:?}"
        )));
    }
    let (path, query) = value.split_once('?').unwrap_or((value, ""));
    let path = if path.is_empty() { "/" } else { path };
    let normalized = normalize_build_output_public_path(path)?;
    if query.is_empty() {
        Ok(normalized)
    } else {
        validate_build_output_text(query, "route destination query")?;
        Ok(format!("{normalized}?{query}"))
    }
}

fn build_output_path_only(value: &str) -> &str {
    value.split_once('?').map(|(path, _)| path).unwrap_or(value)
}

fn insert_build_output_target(
    outputs: &mut BTreeMap<String, BuildOutputV3Target>,
    path: String,
    target: BuildOutputV3Target,
) -> Result<(), BuildOutputV3Refusal> {
    let path = normalize_build_output_public_path(&path)?;
    if outputs.insert(path.clone(), target).is_some() {
        return Err(BuildOutputV3Refusal::invalid(
            "outputs",
            format!("more than one output claims public path {path:?}"),
        ));
    }
    Ok(())
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn validate_build_output_header(
    route: usize,
    name: &str,
    value: &str,
) -> Result<(), BuildOutputV3Refusal> {
    if name.is_empty() || !name.bytes().all(is_http_token_byte) {
        return Err(BuildOutputV3Refusal::invalid(
            format!("config.routes[{route}].headers"),
            format!("invalid header name {name:?}"),
        ));
    }
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    ) {
        return Err(BuildOutputV3Refusal::unsupported(format!(
            "config.routes[{route}] hop-by-hop header {name:?}"
        )));
    }
    validate_build_output_text(value, "route header value")?;
    if value.contains(['\r', '\n']) {
        return Err(BuildOutputV3Refusal::invalid(
            format!("config.routes[{route}].headers[{name:?}]"),
            "contains a line break",
        ));
    }
    Ok(())
}

/// What a route serves.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Static,
    Function(String),
}

/// A path-prefix route. Longest matching prefix wins (computed at match time).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub pattern: String,
    pub target: RouteTarget,
}

/// Runtime-facing classification of a (Next.js) route (#16). The canonical home
/// for the cache/retry/concurrency POLICY a route kind implies — `fluid-build`
/// discovers the kind at build time (`per_route::RouteKind`) and `hive-cloud`
/// maps it onto this when persisting the per-route manifest into [`Manifest`].
/// The snake_case wire form is the cross-crate contract with `RouteKind::class_name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteClass {
    Static,
    Isr,
    ApiNode,
    RouteHandler,
    SsrPage,
    Edge,
    Middleware,
}

impl RouteClass {
    /// Parse the cross-crate class string (`RouteKind::class_name`). Unknown
    /// strings map to `SsrPage` (the safe dynamic default: defers caching to the
    /// origin and uses the method gate for retries).
    pub fn from_name(s: &str) -> RouteClass {
        match s {
            "static" => RouteClass::Static,
            "isr" => RouteClass::Isr,
            "api_node" => RouteClass::ApiNode,
            "route_handler" => RouteClass::RouteHandler,
            "ssr_page" => RouteClass::SsrPage,
            "edge" => RouteClass::Edge,
            "middleware" => RouteClass::Middleware,
            _ => RouteClass::SsrPage,
        }
    }

    /// The snake_case class name (inverse of [`RouteClass::from_name`]); the
    /// stable wire/observability form, e.g. surfaced as `x-hive-route-class`.
    pub fn name(self) -> &'static str {
        match self {
            RouteClass::Static => "static",
            RouteClass::Isr => "isr",
            RouteClass::ApiNode => "api_node",
            RouteClass::RouteHandler => "route_handler",
            RouteClass::SsrPage => "ssr_page",
            RouteClass::Edge => "edge",
            RouteClass::Middleware => "middleware",
        }
    }

    /// Default shared-cache policy. Drives the `Cache-Control` the gateway
    /// synthesizes ONLY when the origin response didn't set its own.
    pub fn cache_policy(self, revalidate: Option<i64>) -> RouteCachePolicy {
        match self {
            RouteClass::Static => RouteCachePolicy::Immutable,
            RouteClass::Isr => RouteCachePolicy::Revalidate(revalidate.unwrap_or(0).max(0)),
            _ => RouteCachePolicy::Origin,
        }
    }

    /// Whether a response is safe to replay on failover regardless of HTTP method.
    /// Pure reads (Static/ISR/SSR page render) are idempotent; side-effect-capable
    /// kinds must still pass the method+idempotency gate (`hive-cloud retry`).
    /// Returns only "definitely safe", never "definitely unsafe".
    pub fn always_replayable(self) -> bool {
        matches!(
            self,
            RouteClass::Static | RouteClass::Isr | RouteClass::SsrPage
        )
    }

    /// Whether serving this route consumes a runtime instance (function
    /// concurrency). Static/ISR are served from cache/CDN and never lease a cell.
    pub fn uses_runtime(self) -> bool {
        !matches!(self, RouteClass::Static | RouteClass::Isr)
    }
}

/// Shared-cache policy a [`RouteClass`] implies (#16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteCachePolicy {
    /// Fully prerendered/static — long shared + browser cache, immutable.
    Immutable,
    /// ISR — serve from shared cache for N seconds, then revalidate in the
    /// background (`stale-while-revalidate`). N is clamped to >= 1 when rendered.
    Revalidate(i64),
    /// Dynamic — no synthetic policy; honor whatever the origin sent.
    Origin,
}

impl RouteCachePolicy {
    /// The `Cache-Control` value to apply, or `None` to leave the origin's own
    /// header untouched (dynamic routes).
    pub fn cache_control(self) -> Option<String> {
        match self {
            RouteCachePolicy::Immutable => Some("public, max-age=31536000, immutable".to_string()),
            RouteCachePolicy::Revalidate(n) => Some(format!(
                "public, s-maxage={}, stale-while-revalidate",
                n.max(1)
            )),
            RouteCachePolicy::Origin => None,
        }
    }
}

/// One route's runtime policy, persisted into the deployment [`Manifest`] (#16).
/// `pattern` is the Next.js route pattern (e.g. `/blog/[slug]`, `/api/claw`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoutePolicy {
    pub pattern: String,
    pub class: RouteClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revalidate: Option<i64>,
}

/// A redirect rule mapped from the framework build (Next.js `redirects()`,
/// Build Output API routes with a 3xx status) or from `vercel.json`. Evaluated
/// by the gateway before routing — first match wins. `status` is the resolved
/// HTTP code (308 permanent / 307 temporary / explicit `statusCode`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Redirect {
    pub source: String,
    pub destination: String,
    #[serde(default = "default_redirect_status")]
    pub status: u16,
    /// Conditional matching (`vercel.json` `has`) — all must be present/match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<RuleCondition>,
    /// Conditional matching (`vercel.json` `missing`) — all must be absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<RuleCondition>,
}
fn default_redirect_status() -> u16 {
    308
}

/// A rewrite rule (path is rewritten server-side, client URL unchanged).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rewrite {
    pub source: String,
    pub destination: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<RuleCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<RuleCondition>,
}

/// A single response header (`vercel.json` `headers[].headers[]`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub key: String,
    pub value: String,
}

/// A response-header rule (`vercel.json` `headers`). When `source` (+ optional
/// `has`/`missing`) matches a request path, the gateway injects `headers` onto
/// the response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeaderRule {
    pub source: String,
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<RuleCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<RuleCondition>,
}

/// A scheduled job (`vercel.json` `crons`). Registered against the production
/// deployment; the scheduler invokes `path` on `schedule` (cron expression).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronSpec {
    pub path: String,
    pub schedule: String,
}

/// A `has`/`missing` condition matched against the request (`vercel.json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleCondition {
    /// One of: `host`, `header`, `cookie`, `query`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CondValue>,
}

/// The `value` of a condition — a literal string, or an expressive
/// prefix/suffix matcher (`{ "pre": "...", "suf": "..." }`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CondValue {
    Text(String),
    Expr {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suf: Option<String>,
    },
}

impl CondValue {
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            CondValue::Text(t) => candidate == t,
            CondValue::Expr { pre, suf } => {
                pre.as_deref()
                    .map(|p| candidate.starts_with(p))
                    .unwrap_or(true)
                    && suf
                        .as_deref()
                        .map(|s| candidate.ends_with(s))
                        .unwrap_or(true)
            }
        }
    }
}

/// Image Optimization configuration (`vercel.json` `images`) — enforced by the
/// gateway's `/_vercel/image` endpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImagesConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qualities: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_cache_ttl: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_patterns: Vec<RemotePattern>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_patterns: Vec<LocalPattern>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerously_allow_svg: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_security_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_disposition_type: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemotePattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalPattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// Minimal request context for evaluating `has`/`missing` conditions and
/// host-scoped matching. Built cheaply by the gateway per request.
#[derive(Clone, Debug, Default)]
pub struct ReqCtx {
    pub host: String,
    /// (lowercased key, value) pairs.
    pub headers: Vec<(String, String)>,
    /// Raw query string (without leading `?`).
    pub query: String,
}

impl ReqCtx {
    pub fn header(&self, key: &str) -> Option<String> {
        let k = key.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(hk, _)| *hk == k)
            .map(|(_, v)| v.clone())
    }
    pub fn cookie(&self, key: &str) -> Option<String> {
        let raw = self.header("cookie")?;
        for part in raw.split(';') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }
    pub fn query_param(&self, key: &str) -> Option<String> {
        for part in self.query.split('&') {
            if let Some((k, v)) = part.split_once('=') {
                if k == key {
                    return Some(v.to_string());
                }
            } else if part == key {
                return Some(String::new());
            }
        }
        None
    }
}

/// Evaluate one condition against the request.
fn cond_matches(c: &RuleCondition, ctx: &ReqCtx) -> bool {
    let actual: Option<String> = match c.kind.as_str() {
        "host" => Some(ctx.host.clone()),
        "header" => c.key.as_ref().and_then(|k| ctx.header(k)),
        "cookie" => c.key.as_ref().and_then(|k| ctx.cookie(k)),
        "query" => c.key.as_ref().and_then(|k| ctx.query_param(k)),
        _ => None,
    };
    match (&c.value, actual) {
        (None, Some(_)) => true, // presence only
        (None, None) => false,
        (Some(v), Some(a)) => v.matches(&a),
        (Some(_), None) => false,
    }
}

/// `has`: every condition must match. `missing`: every condition must NOT match.
fn conditions_pass(has: &[RuleCondition], missing: &[RuleCondition], ctx: &ReqCtx) -> bool {
    has.iter().all(|c| cond_matches(c, ctx)) && missing.iter().all(|c| !cond_matches(c, ctx))
}

/// Resolved redirect status for a redirect built from `vercel.json`:
/// explicit `statusCode` wins; else `permanent` => 308 / 307; else default 308.
pub fn redirect_status(permanent: Option<bool>, status_code: Option<u16>) -> u16 {
    if let Some(sc) = status_code {
        return sc;
    }
    match permanent {
        Some(false) => 307,
        _ => 308,
    }
}

// ---- path-to-regexp-lite matcher (Vercel `:param` / `:param*` + inline regex) ----

/// Compile a Vercel source pattern (`/blog/:slug`, `/post/:p(\\d+)`,
/// `/proxy/:m*`, `/(.*)`) into an anchored regex with named captures. Returns
/// `None` if the pattern isn't regex-like or fails to compile (caller falls back
/// to literal/prefix matching).
fn compile_source(source: &str) -> Option<regex::Regex> {
    if !(source.contains(':') || source.contains('(') || source.contains('*')) {
        return None;
    }
    let mut out = String::from("^");
    let mut lit = String::new();
    let flush = |out: &mut String, lit: &mut String| {
        if !lit.is_empty() {
            out.push_str(&regex::escape(lit));
            lit.clear();
        }
    };
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ':' => {
                flush(&mut out, &mut lit);
                i += 1;
                let mut name = String::new();
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    name.push(chars[i]);
                    i += 1;
                }
                if name.is_empty() {
                    lit.push(':');
                    continue;
                }
                // Optional modifier or inline regex.
                if i < chars.len() && chars[i] == '(' {
                    // Balanced custom pattern.
                    let mut depth = 0i32;
                    let mut body = String::new();
                    while i < chars.len() {
                        let ch = chars[i];
                        if ch == '(' {
                            depth += 1;
                            if depth == 1 {
                                i += 1;
                                continue;
                            }
                        } else if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        body.push(ch);
                        i += 1;
                    }
                    out.push_str(&format!("(?P<{name}>{body})"));
                } else if i < chars.len() && chars[i] == '*' {
                    out.push_str(&format!("(?P<{name}>.*)"));
                    i += 1;
                } else if i < chars.len() && chars[i] == '+' {
                    out.push_str(&format!("(?P<{name}>.+)"));
                    i += 1;
                } else {
                    out.push_str(&format!("(?P<{name}>[^/]+)"));
                }
            }
            '(' => {
                // Raw regex group passes through verbatim (balanced copy).
                flush(&mut out, &mut lit);
                let mut depth = 0i32;
                while i < chars.len() {
                    let ch = chars[i];
                    out.push(ch);
                    if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
            }
            '*' => {
                flush(&mut out, &mut lit);
                out.push_str(".*");
                i += 1;
            }
            _ => {
                lit.push(c);
                i += 1;
            }
        }
    }
    flush(&mut out, &mut lit);
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// Substitute `:name` / `:name*` references in a destination with values
/// captured from the source match.
fn subst_dest(dest: &str, caps: &regex::Captures) -> String {
    let chars: Vec<char> = dest.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ':' {
            i += 1;
            let mut name = String::new();
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                name.push(chars[i]);
                i += 1;
            }
            // Consume an optional trailing `*`/`+` modifier in the destination.
            if i < chars.len() && (chars[i] == '*' || chars[i] == '+') {
                i += 1;
            }
            if let Some(m) = caps.name(&name) {
                out.push_str(m.as_str());
            } else if name.is_empty() {
                out.push(':');
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Try to match `path` against `source` (param/regex aware), returning the
/// resolved destination if it matches. Falls back to literal/prefix matching.
pub fn rule_apply(source: &str, dest: &str, path: &str) -> Option<String> {
    if let Some(re) = compile_source(source) {
        if let Some(caps) = re.captures(path) {
            return Some(subst_dest(dest, &caps));
        }
        // A regex-like source that didn't match: also try the literal fallback,
        // since lookahead-bearing sources may have failed to compile elsewhere.
        if rule_match(source, path) {
            return Some(rule_target(source, dest, path));
        }
        return None;
    }
    if rule_match(source, path) {
        Some(rule_target(source, dest, path))
    } else {
        None
    }
}

/// Whether `source` matches `path` (used by header rules that have no dest).
pub fn rule_matches(source: &str, path: &str) -> bool {
    if let Some(re) = compile_source(source) {
        re.is_match(path) || rule_match(source, path)
    } else {
        rule_match(source, path)
    }
}

/// Middleware / proxy (`middleware.ts` / `proxy.ts`) detected in the build. Runs
/// in the edge runtime ahead of routing; `matcher` lists the path patterns it
/// applies to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Middleware {
    #[serde(default)]
    pub matcher: Vec<String>,
    #[serde(default = "default_edge_runtime")]
    pub runtime: String,
}
fn default_edge_runtime() -> String {
    "edge".into()
}

/// `fluid.json` — what a user writes to describe their deployment.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub project: String,
    /// Relative dir (within the deployment root) holding static assets.
    #[serde(default)]
    pub static_dir: Option<String>,
    /// Per-deployment cell image key. When set, the function pool provisions
    /// cells with this image instead of the node's default, so an isolated
    /// backend (Firecracker) can attach this deployment's delivered build
    /// artifact. `None` => use the node's default image (mock / same-host).
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub functions: Vec<FunctionConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Server-derived Build Output API v3 descriptor. `Some` is authoritative:
    /// the gateway evaluates its ordered regex routes and exact output inventory
    /// directly. `None` preserves legacy prefix routing byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_output_v3: Option<BuildOutputV3>,
    /// Redirects mapped from the framework build (gateway honors these).
    #[serde(default)]
    pub redirects: Vec<Redirect>,
    /// Server-side rewrites mapped from the framework build.
    #[serde(default)]
    pub rewrites: Vec<Rewrite>,
    /// Edge middleware / proxy detected in the build, if any.
    #[serde(default)]
    pub middleware: Option<Middleware>,
    /// Response-header rules (`vercel.json` `headers`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderRule>,
    /// Scheduled jobs (`vercel.json` `crons`) — registered on production deploy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crons: Vec<CronSpec>,
    /// `vercel.json` `cleanUrls` — strip `.html` and redirect extension paths.
    #[serde(default)]
    pub clean_urls: bool,
    /// `vercel.json` `trailingSlash` — `Some(true)` enforce, `Some(false)` strip,
    /// `None` no normalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_slash: Option<bool>,
    /// `vercel.json` `images` — Image Optimization config (gateway enforces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImagesConfig>,
    /// Per-route runtime policies (#16) from Next.js per-route classification.
    /// Empty for non-Next deployments / when per-route discovery is disabled — in
    /// which case the serve path behaves byte-for-byte as before. When present,
    /// the gateway consults [`Manifest::route_policy`] to apply route-type-aware
    /// caching/retry without changing the common (empty) case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_policies: Vec<RoutePolicy>,
    /// The origin/SSR function a Static route falls through to when the requested
    /// asset doesn't exist on disk (the CDN→function model). Used by adapter
    /// frameworks (OpenNext, vinext) whose server function renders dynamic routes
    /// while immutable assets are served from `static_dir`. `None` (the default)
    /// means a Static miss stays a 404/SPA-fallback — behavior unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_function: Option<String>,
    /// Explicit opt-in to a browser-replicated cr-sqlite database for this
    /// project (fluid.json top-level `browser_db`). ABSENT means the project
    /// gets NO browser database, by construction: only a deployment whose
    /// manifest carries the block can ever ground a DB grant. See
    /// [`BrowserDbPolicy`]'s docs and `docs/browser-db-contract.md` for the
    /// full ownership/retention/naming contract this rides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db: Option<BrowserDbPolicy>,
    /// The detected/build-config framework SLUG (nextjs, vite, astro, remix,
    /// docker, …), stamped at build time so the dashboard can show each
    /// project's real framework logo. Empty = unknown → the UI falls back to
    /// the default mark; absent on pre-upgrade builds, which is the same
    /// unknown state.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub framework: String,
}

/// Strict deploy-time protocol validation for raw `fluid.json` text: walk
/// every `functions[*].protocol` and `functions[*].ports[*].protocol` string
/// in the RAW JSON and reject unrecognized values via the strict
/// `ServiceProtocol::FromStr`. Must run on the raw text (not the deserialized
/// [`Manifest`]) because the lenient serde impl has already coerced unknowns
/// to `http` by the time the typed value exists. Non-string values are left
/// for serde to reject with its own type error.
fn validate_protocols_strict(s: &str) -> Result<(), serde_json::Error> {
    use serde::de::Error as _;
    let raw: serde_json::Value = serde_json::from_str(s)?;
    if raw.get("build_output_v3").is_some() {
        return Err(serde_json::Error::custom(
            "build_output_v3 is server-derived and cannot be supplied by fluid.json",
        ));
    }
    let Some(functions) = raw.get("functions").and_then(|f| f.as_array()) else {
        return Ok(());
    };
    for f in functions {
        let name = f
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("<unnamed>");
        let mut check = |v: &serde_json::Value, ctx: &str| -> Result<(), serde_json::Error> {
            if let Some(p) = v.as_str() {
                p.parse::<ServiceProtocol>().map_err(|e| {
                    serde_json::Error::custom(format!("function {name:?}{ctx}: {e}"))
                })?;
            }
            Ok(())
        };
        if let Some(p) = f.get("protocol") {
            check(p, "")?;
        }
        if let Some(ports) = f.get("ports").and_then(|p| p.as_array()) {
            for (i, port) in ports.iter().enumerate() {
                if let Some(p) = port.get("protocol") {
                    check(p, &format!(" ports[{i}]"))?;
                }
            }
        }
    }
    Ok(())
}

impl Manifest {
    /// Parse a fresh, user-authored `fluid.json`. This is a DEPLOY-INPUT
    /// boundary, so unknown protocol strings are a hard, clearly-worded error
    /// here — checked against the RAW strings via the strict
    /// `ServiceProtocol::FromStr` BEFORE the lenient serde impl coerces them
    /// to `http` (see the strict-vs-lenient split on [`ServiceProtocol`]).
    /// Persisted-state loaders must NOT go through this function; they
    /// deserialize [`Manifest`] directly and get the lenient behavior.
    pub fn from_json(s: &str) -> Result<Manifest, serde_json::Error> {
        validate_protocols_strict(s)?;
        serde_json::from_str(s)
    }

    pub fn function(&self, name: &str) -> Option<&FunctionConfig> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// Every STAMPED raw-ingress binding in this manifest (specs whose
    /// `public_port` the raw-port allocator filled in) — the flattened
    /// `public_port` → function/container-port/protocol view the generic raw
    /// proxy serves, also surfaced on [`DeploymentInfo`] so it gossips
    /// fleet-wide. Only raw-protocol specs ever get stamped, so this is empty
    /// for HTTP-only deployments.
    pub fn raw_port_bindings(&self) -> Vec<RawPortBinding> {
        let mut out = Vec::new();
        for f in &self.functions {
            for spec in &f.ports {
                if let Some(pp) = spec.public_port {
                    out.push(RawPortBinding {
                        public_port: pp,
                        function: f.name.clone(),
                        container_port: spec.container_port,
                        protocol: spec.protocol,
                        label: spec.label.clone(),
                    });
                }
            }
        }
        out
    }

    /// The deployment's dedicated public IPv4 allocation, if any function
    /// opted in and the deploy-time claim succeeded — hoisted from whichever
    /// function carries a stamp, the same manifest-then-`view_of` shape
    /// [`Manifest::raw_port_bindings`] uses. `DeploymentInfo` carries a single
    /// slot (not one per function): today's contract is at most one dedicated
    /// address per deployment, so the first stamped function wins.
    pub fn dedicated_ipv4_binding(&self) -> Option<DedicatedIpv4> {
        self.functions
            .iter()
            .find_map(|f| f.dedicated_ipv4_alloc.clone())
    }

    /// Resolve a request path to a route target using longest-prefix match.
    /// Falls back to Static if nothing matches.
    pub fn resolve(&self, path: &str) -> RouteTarget {
        let mut best: Option<&Route> = None;
        for r in &self.routes {
            if path_matches(&r.pattern, path) {
                match best {
                    // Specificity is the matched PREFIX, not the raw pattern
                    // text — else a catch-all `/*` (2 chars) would outrank the
                    // `/api` (4 chars) it is meant to be the fallback for the
                    // moment wildcards became matchable.
                    Some(b) if route_prefix(&b.pattern).len() >= route_prefix(&r.pattern).len() => {
                    }
                    _ => best = Some(r),
                }
            }
        }
        best.map(|r| r.target.clone())
            .unwrap_or(RouteTarget::Static)
    }

    /// Did any declared route actually match `path`? `resolve` answers
    /// `RouteTarget::Static` for BOTH "a route says static" and "no route
    /// matched at all", which is exactly the ambiguity that let an unroutable
    /// deployment 404 as if it were an ordinary missing file. Callers that need
    /// to explain a 404 ask this.
    pub fn route_matched(&self, path: &str) -> bool {
        self.routes.iter().any(|r| path_matches(&r.pattern, path))
    }

    /// The per-route runtime policy (#16) for a request path, if any. Matches the
    /// request against the persisted Next.js route patterns (exact + dynamic
    /// `[seg]` / catch-all `[...seg]`), preferring the most specific (fewest
    /// dynamic segments, then longest) match. Returns `None` when no per-route
    /// policy is configured — the common case, leaving behavior unchanged.
    pub fn route_policy(&self, path: &str) -> Option<&RoutePolicy> {
        if self.route_policies.is_empty() {
            return None;
        }
        let req = path.split(['?', '#']).next().unwrap_or(path);
        let mut best: Option<(&RoutePolicy, u32)> = None; // (policy, specificity)
        for p in &self.route_policies {
            if let Some(spec) = next_route_match(&p.pattern, req) {
                match best {
                    Some((_, bs)) if bs >= spec => {}
                    _ => best = Some((p, spec)),
                }
            }
        }
        best.map(|(p, _)| p)
    }

    /// The first matching redirect for `path`, as (location, status).
    /// Back-compat path-only entry point (no `has`/`missing` context).
    pub fn redirect_for(&self, path: &str) -> Option<(String, u16)> {
        self.redirect_for_ctx(path, &ReqCtx::default())
    }

    /// The first matching redirect for `path`, honoring `has`/`missing`
    /// conditions and `:param` / regex source patterns.
    pub fn redirect_for_ctx(&self, path: &str, ctx: &ReqCtx) -> Option<(String, u16)> {
        for r in &self.redirects {
            if !conditions_pass(&r.has, &r.missing, ctx) {
                continue;
            }
            if let Some(dest) = rule_apply(&r.source, &r.destination, path) {
                return Some((dest, r.status));
            }
        }
        None
    }

    /// Apply the first matching rewrite, returning the (possibly) rewritten path.
    /// Back-compat path-only entry point.
    pub fn rewrite_path(&self, path: &str) -> String {
        self.rewrite_path_ctx(path, &ReqCtx::default())
    }

    /// Apply the first matching rewrite, honoring `has`/`missing` + `:param`.
    pub fn rewrite_path_ctx(&self, path: &str, ctx: &ReqCtx) -> String {
        for r in &self.rewrites {
            if !conditions_pass(&r.has, &r.missing, ctx) {
                continue;
            }
            if let Some(dest) = rule_apply(&r.source, &r.destination, path) {
                return dest;
            }
        }
        path.to_string()
    }

    /// All response headers to inject for `path` (every matching rule, in order).
    pub fn headers_for(&self, path: &str, ctx: &ReqCtx) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for rule in &self.headers {
            if !conditions_pass(&rule.has, &rule.missing, ctx) {
                continue;
            }
            if rule_matches(&rule.source, path) {
                for h in &rule.headers {
                    out.push((h.key.clone(), h.value.clone()));
                }
            }
        }
        out
    }

    /// Trailing-slash normalization for `path`. Returns `Some(new_path)` when a
    /// 308 redirect should be issued, else `None`. Paths with a file extension in
    /// the last segment are never given a trailing slash (Vercel semantics).
    pub fn trailing_slash_redirect(&self, path: &str) -> Option<String> {
        let want = self.trailing_slash?;
        if path == "/" {
            return None;
        }
        let has_slash = path.ends_with('/');
        if want && !has_slash {
            let last = path.rsplit('/').next().unwrap_or("");
            if last.contains('.') {
                return None; // file with extension
            }
            Some(format!("{path}/"))
        } else if !want && has_slash {
            Some(path.trim_end_matches('/').to_string())
        } else {
            None
        }
    }

    /// Count of edge-runtime functions in this deployment.
    pub fn edge_function_count(&self) -> usize {
        self.functions
            .iter()
            .filter(|f| f.runtime == "edge")
            .count()
    }
}

/// Exact match, or prefix match when `source` ends with `/`.
fn rule_match(source: &str, path: &str) -> bool {
    if let Some(prefix) = source.strip_suffix('/') {
        path == prefix || path.starts_with(source)
    } else {
        path == source
    }
}

/// Build a redirect/rewrite target, preserving the remainder for prefix sources.
fn rule_target(source: &str, destination: &str, path: &str) -> String {
    if let Some(prefix) = source.strip_suffix('/') {
        if let Some(rest) = path.strip_prefix(prefix) {
            let dest = destination.trim_end_matches('/');
            return format!("{dest}{rest}");
        }
    }
    destination.to_string()
}

/// The matching PREFIX a route pattern denotes, with a trailing wildcard
/// stripped. `"/"` → `""`, `"/*"` → `""`, `"*"` → `""`, `"/api/*"` → `"/api"`,
/// `"/api"` → `"/api"`.
///
/// The wildcard forms are why this exists. `path_matches` used to be a bare
/// `strip_prefix`, so a `fluid.json` route written the natural way —
/// `{"pattern": "/*", "target": {"function": "web"}}` — matched NOTHING: no
/// request path begins with the literal two characters `/*`. Every request then
/// fell through `Manifest::resolve`'s `RouteTarget::Static` default, so the
/// deployment 404'd `not found` on every path while reporting Ready with its
/// function registered, and `try_browser` (reachable only from the
/// `RouteTarget::Function` branch) was never consulted either — a deployment
/// that could serve neither from the fleet nor from a donor browser. Witnessed
/// live on `archive-zip.shadw.app` (2026-08-05): `GET /` → 404 `not found`,
/// while `GET /fluid.json` returned the project's own source as a static file.
///
/// Only a TRAILING wildcard is understood — a pattern with one in the middle
/// (`/api/*/x`) keeps its literal, matches-nothing behaviour rather than being
/// silently reinterpreted as something it does not say.
pub fn route_prefix(pattern: &str) -> &str {
    pattern.trim().trim_end_matches('*').trim_end_matches('/')
}

/// Prefix match with `/` boundary awareness. `"/api"` matches `/api` and
/// `/api/x` but not `/apixyz`. `"/"`, `"/*"` and `"*"` match everything;
/// `"/api/*"` matches exactly what `"/api"` does.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let prefix = route_prefix(pattern);
    if prefix.is_empty() {
        return true;
    }
    if let Some(rest) = path.strip_prefix(prefix) {
        rest.is_empty() || rest.starts_with('/')
    } else {
        false
    }
}

/// Match a request path against a Next.js route pattern (#16), supporting exact
/// segments, dynamic `[seg]` (one segment), and catch-all `[...seg]` /
/// `[[...seg]]` (one-or-more / zero-or-more trailing segments). Returns a
/// specificity score on match (higher = more specific: static segments score
/// highest, single-dynamic lower, catch-all lowest), or `None` if it doesn't
/// match. Used only for per-route policy lookup; the normal router is unaffected.
pub fn next_route_match(pattern: &str, path: &str) -> Option<u32> {
    let pat = pattern.trim_matches('/');
    let req = path.trim_matches('/');
    let pseg: Vec<&str> = if pat.is_empty() {
        vec![]
    } else {
        pat.split('/').collect()
    };
    let rseg: Vec<&str> = if req.is_empty() {
        vec![]
    } else {
        req.split('/').collect()
    };

    let mut score: u32 = 0;
    let mut i = 0;
    while i < pseg.len() {
        let p = pseg[i];
        // Catch-all: `[...x]` (>=1 remaining) or optional `[[...x]]` (>=0 remaining).
        if p.starts_with("[[...") && p.ends_with("]]") {
            return Some(score + 1); // optional catch-all matches the rest (incl. none)
        }
        if p.starts_with("[...") && p.ends_with(']') {
            // Requires at least one remaining segment.
            return if i < rseg.len() {
                Some(score + 2)
            } else {
                None
            };
        }
        // Out of request segments but pattern still has required parts -> no match.
        if i >= rseg.len() {
            return None;
        }
        if p.starts_with('[') && p.ends_with(']') {
            score += 10; // dynamic single segment
        } else if p == rseg[i] {
            score += 100; // exact segment is most specific
        } else {
            return None;
        }
        i += 1;
    }
    // All pattern segments consumed: match iff request has no leftover segments.
    if rseg.len() == pseg.len() {
        Some(score)
    } else {
        None
    }
}

/// Git provenance for a deployment (shown Vercel-style in the dashboard).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitSource {
    pub repo_url: String,
    pub branch: String,
    pub commit: String,
    pub commit_message: String,
}

impl GitSource {
    /// `repo_url` is a real git remote (github.com/... etc), not the synthetic
    /// `upload://`/`image://` pseudo-URLs `run_build` also stamps into this same
    /// field for zip/prebuilt-image deploys (so build metadata/webhook payloads
    /// still have a source string). A caller resolving "the repo this project's
    /// pushes should match" (e.g. GitHub webhook dispatch) must skip non-git
    /// records — otherwise a zip/image redeploy becoming a project's NEWEST
    /// deployment poisons that lookup, silently breaking future git-push
    /// auto-deploys for the project's real, unrelated repo.
    pub fn is_real_git(&self) -> bool {
        !self.repo_url.starts_with("upload://") && !self.repo_url.starts_with("image://")
    }
}

/// Lifecycle state of a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployState {
    Queued,
    Building,
    Ready,
    Error,
    /// The in-flight build was stopped by an explicit user cancel
    /// (`POST /v1/builds/:id/cancel`) — distinct from `Error` (a build the
    /// platform tried and failed) so the dashboard can tell the two apart.
    Cancelled,
}
impl Default for DeployState {
    fn default() -> Self {
        DeployState::Ready
    }
}

/// Serializable snapshot of a deployment (for persistence + restore).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployRecord {
    pub id: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_incarnation: Option<ProjectIncarnation>,
    pub root: String,
    /// Function cwd inside the serving backend. `None` is legacy and may only
    /// reuse `root` when the active backend proves it is same-host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_workdir: Option<String>,
    pub manifest: Manifest,
    pub created_at_ms: u64,
    pub creator: String,
    pub git: Option<GitSource>,
    pub production: bool,
    /// Environment the deployment was built for: "production" | "preview".
    /// Immutable; `production` only reflects whether it currently holds the prod
    /// alias. Defaults to empty (derive from `production`) for old snapshots.
    #[serde(default)]
    pub target: String,
    /// Final lifecycle state (so a failed build stays "error" across restarts).
    /// Defaults to `ready` for back-compat with snapshots written before this field.
    #[serde(default)]
    pub state: DeployState,
    /// Owning team/tenant. `#[serde(default)]` keeps pre-tenancy snapshots
    /// loadable (they normalize to "personal"); on restore this re-registers the
    /// deployment's function pools under the correct tenant.
    #[serde(default)]
    pub tenant: String,
}

/// A registered deployment (manifest + where its files live).
#[derive(Clone, Debug)]
pub struct Deployment {
    pub id: DeploymentId,
    pub project: String,
    pub project_incarnation: Option<ProjectIncarnation>,
    /// Canonical host path used only by static/origin serving.
    pub root: std::path::PathBuf,
    /// Function cwd inside the serving backend. `None` exists only for restored
    /// legacy records and is not permission to guess on an isolated backend.
    pub runtime_workdir: Option<std::path::PathBuf>,
    pub manifest: Manifest,
    pub created_at_ms: u64,
    pub state: DeployState,
    pub creator: String,
    pub git: Option<GitSource>,
    /// Whether this deployment currently holds the project's PRODUCTION alias
    /// (Vercel's "promoted" flag). Flips on promote/rollback — it does NOT change
    /// `target`.
    pub production: bool,
    /// The environment the deployment was BUILT for: "production" | "preview".
    /// Immutable for the life of the deployment (a superseded production build
    /// keeps target=production even after a newer one is promoted). Empty string
    /// means "derive from `production`" (back-compat for old in-memory values).
    pub target: String,
    /// Owning team/tenant slug (empty = "personal"). Set at deploy time from the
    /// project's team; flows into each cell's `CellSpec` and the Fluid pool so
    /// compute is partitioned and quota'd per tenant.
    pub tenant: String,
}

/// Admin API: request to create a deployment. For the mock backend the gateway
/// reads files directly from `root` (same host); a real deploy would upload a
/// tarball / build artifact instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployRequest {
    pub root: String,
    pub manifest: Manifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_incarnation: Option<ProjectIncarnation>,
    #[serde(default)]
    pub creator: Option<String>,
    #[serde(default)]
    pub git: Option<GitSource>,
    #[serde(default)]
    pub production: bool,
}

/// Admin API: deploy directly from a git repository URL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitDeployRequest {
    pub repo_url: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Exact commit SHA to build, when known (parsed from a GitHub webhook
    /// payload's `after` for a push, or `pull_request.head.sha` for a PR). When
    /// Some, the clone pins to this EXACT commit instead of the branch tip —
    /// closes a race where a rapid double-push could otherwise silently build a
    /// newer commit than the one GitHub actually notified about. None (manual
    /// deploys, redeploys, the "New Project" import flow) has no specific commit
    /// to pin to and falls back to a plain branch-tip clone.
    #[serde(default)]
    pub commit: Option<String>,
    /// When a webhook-triggered PR build's HEAD commit lives on a FORK (not the
    /// base repo `repo_url` points at), this is the fork's clone URL — the PR's
    /// branch (and, for an older host without SHA-fetch support, the commit
    /// itself) only exists on the fork's remote, so cloning/fetching against
    /// `repo_url` would find nothing. When Some, the clone/fetch step in
    /// `run_build` sources from THIS url instead of `repo_url`. `repo_url` itself
    /// is left untouched everywhere else — project ownership/matching, the
    /// displayed `Build.repo_url`, and commit-status reporting all stay on the
    /// BASE repo, since a fork PR still belongs to the base project. None for
    /// same-repo PRs, pushes, and every non-webhook deploy (manual import,
    /// redeploy, "New Project").
    #[serde(default)]
    pub head_repo_url: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_incarnation: Option<ProjectIncarnation>,
    #[serde(default)]
    pub creator: Option<String>,
    #[serde(default = "default_prod")]
    pub production: bool,
    /// Explicit deploy target: "production" | "preview". When None (the default),
    /// the target is CLASSIFIED from the branch — a push to the project's
    /// production branch is production, every other branch / PR is a preview
    /// (Vercel's model). Webhooks set this to "preview" for PR events; the import
    /// + redeploy flows leave it None so the branch decides.
    #[serde(default)]
    pub target: Option<String>,
    /// Whether to reuse the existing dependency build cache. Defaults to true.
    /// A redeploy can set this false ("Use existing Build Cache" unchecked) to
    /// force a clean install — when a package-lock.json is present that means
    /// `npm ci` instead of `npm install`, and the cached node_modules is skipped.
    #[serde(default = "default_prod")]
    pub use_cache: bool,
    /// Subdirectory within the repo to build (for monorepo templates, e.g.
    /// `examples/nextjs`). Empty/None = repo root.
    #[serde(default)]
    pub root_dir: Option<String>,
    /// Environment variables supplied by a direct creation request. The server
    /// persists them as runtime-only variables for the server-derived deployment
    /// environment; fork-sourced requests never persist them. Build variables are
    /// configured explicitly through the project environment API with build scope.
    #[serde(default)]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// When true, this node deploys LOCALLY only (build + host) and does NOT run
    /// the placement scheduler / fanout. The coordinator sets this on the
    /// per-target deploys it dispatches, so a target node "just hosts this" and
    /// placement never recurses.
    #[serde(default)]
    pub no_fanout: bool,
    /// Set by the coordinator on every per-target deploy of a MULTI-target
    /// fanout EXCEPT the designated primary (the first placement target). A
    /// `no_fanout` sub-build skips the coordinator's placement gates entirely,
    /// so this flag is how the stateful/single-writer fanout guard reaches the
    /// pure-remote fanout path: at initial placement a Dockerfile/compose
    /// deploy's container-ness (and thus statefulness) is UNKNOWN, so the
    /// coordinator may legitimately fan an unknown deploy out to several
    /// regions — and only each target's own build discovers whether the result
    /// is a stateful single-writer service (container volume / raw protocol).
    /// A target that discovers it IS stateful while `fanout_secondary` is true
    /// declines to host (see the guard in `git.rs::run_build`), collapsing the
    /// deploy to the primary region instead of silently creating independent,
    /// diverging per-region volumes (split-brain). False (the default) on
    /// direct deploys, single-target dispatches, and the primary target —
    /// stateless multi-region fanout is unaffected because the guard also
    /// requires the post-build stateful signal.
    #[serde(default)]
    pub fanout_secondary: bool,
    /// Project BuildConfig (framework/install/build/output/root), forwarded by the
    /// coordinator on a fanout deploy so the target builds with the SAME settings
    /// the user configured — not just whatever it auto-detects. Opaque JSON to
    /// avoid a fluid-core → hive-cloud dependency. None on direct user deploys.
    #[serde(default)]
    pub build_config: Option<serde_json::Value>,
    /// Project FunctionSettings (vcpus/memory/regions/…), forwarded on fanout so a
    /// remotely-placed deployment honors the user's compute tier. Opaque JSON.
    #[serde(default)]
    pub function_settings: Option<serde_json::Value>,
    /// Set by the "New Deployment" modal on a project's own page: this is a fresh
    /// deployment of an EXISTING project, not a new-project create. When the named
    /// project already exists for the requesting tenant, its name is used verbatim
    /// (no `-N` suffix, no "already exists" 409) even if the source repo/branch
    /// differs — the user is intentionally deploying a new source into that project.
    #[serde(default)]
    pub redeploy: bool,
    /// Base64 of an uploaded ZIP archive — an alternative SOURCE to `repo_url`.
    /// When set, the build EXTRACTS this instead of `git clone`. It rides inside the
    /// request so the existing placement/fanout ships it to the target node (bounded
    /// by the gossip frame; the upload endpoint enforces ~10 MB raw). Cleared right
    /// after extraction so it is never persisted or logged.
    #[serde(default)]
    pub zip_b64: Option<String>,
    /// A PRE-BUILT OCI image reference to run directly (Docker Hub / Quay / any
    /// registry), e.g. `fruitbox12/simplifi:latest` or `quay.io/org/img:tag`. When
    /// set, the deploy SKIPS clone + build entirely: the target node `podman pull`s
    /// the image, auto-detects its port, and runs it as a container with an automatic
    /// persistent volume + the project's env vars. Rides the normal placement/fanout.
    #[serde(default)]
    pub image_ref: Option<String>,
    /// Optional explicit container port for an `image_ref` deploy. When None, the port
    /// is auto-detected from the image's `ExposedPorts` (falling back to 8080).
    #[serde(default)]
    pub image_port: Option<u16>,
    /// Optional explicit protocol for an `image_ref` deploy (Railway-style; see
    /// [`ServiceProtocol`]), e.g. `Udp` for a UDP-only service (Minecraft Bedrock,
    /// port 19132/udp — an image exposing no TCP port at all can't be auto-detected
    /// as anything but this). `None` falls back to whatever `image_container_manifest`
    /// (hive-cloud) detects from the image's `ExposedPorts` alongside `image_port`
    /// (defaulting to `Http` when nothing is exposed either). Independent of
    /// `image_port`: either may be set without the other — the resolution order lives
    /// in `image_container_manifest`, not here.
    #[serde(default)]
    pub image_protocol: Option<ServiceProtocol>,
    /// Memory ceiling override for an `image_ref` deploy's container, e.g. "4g",
    /// "2048m", "512" — same string format/semantics as a Dockerfile-build
    /// project's fluid.json `container.memory`. `None` = the node's generous
    /// env-tunable default. Always clamped to a fleet-wide ceiling
    /// (`ContainerLimits::for_container`) so a request can never remove the
    /// ceiling entirely.
    #[serde(default)]
    pub image_memory: Option<String>,
    /// CPU quota override for an `image_ref` deploy's container, e.g. "2.0",
    /// "0.5" — same format as fluid.json `container.cpus`. `None` = the node's
    /// default. Clamped fleet-wide.
    #[serde(default)]
    pub image_cpus: Option<String>,
    /// Max-PIDs override for an `image_ref` deploy's container cgroup (fork-bomb
    /// guard) — same as fluid.json `container.pids`. `None` = the node's
    /// default. Clamped fleet-wide.
    #[serde(default)]
    pub image_pids: Option<u32>,
    /// Full multi-port declaration for an `image_ref` deploy — when non-empty,
    /// this REPLACES the single `image_port`/`image_protocol` resolution
    /// entirely (the first entry is the primary/listen port, used the same way
    /// `image_port` would be); when empty/absent, the single-port path is
    /// unchanged. Lets a service that needs more than one raw port declared
    /// (e.g. a game server's play + query ports) describe all of them in one
    /// deploy, the same way a compose service's `x-shadw-expose` already can.
    /// NOTE: only the PRIMARY (first) port is forwardable through the mesh
    /// splice today (`mesh_raw::resolve`'s existing, separately-tracked
    /// `spec_idx != 0` limitation) — a secondary port here gets a real public
    /// allocation but cross-node forwarding to it isn't wired yet.
    #[serde(default)]
    pub image_ports: Option<Vec<PortSpec>>,
    /// GitHub access token for cloning a PRIVATE repo. Injected on the build node as
    /// `https://x-access-token:<token>@github.com/...` basic auth for the `git clone`
    /// only — never written into `repo_url`, never logged (clone stderr is scrubbed),
    /// and cleared (`take()`) right after the clone so no persisted/gossiped/displayed
    /// record retains it. Rides placement/fanout like `zip_b64`; `skip_serializing_if`
    /// omits it entirely when absent (public repos / no connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_token: Option<String>,
    /// Upload-source lineage ids for a server-resolved redeploy. Despite the
    /// compatibility field name, values are platform-issued BUILD ids stamped in
    /// `GitSource.commit`: checkout dirs and retained archives use build ids, while
    /// gateway deployment ids are independently minted. Empty means no proven
    /// upload lineage and must fail closed rather than scan newest-any source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_deployment_ids: Vec<String>,
}
fn default_prod() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub id: DeploymentId,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_incarnation: Option<ProjectIncarnation>,
    pub functions: Vec<String>,
    pub created_at_ms: u64,
    /// Convenience: the project (production-domain) Host alias `<project>.localhost`.
    pub alias: String,
    /// Immutable per-commit Host alias `<project>-<shortsha>.localhost` (Vercel's
    /// commit URL). Empty when the deployment has no git commit. Always resolves
    /// to THIS exact deployment.
    #[serde(default)]
    pub commit_alias: String,
    /// Per-branch Host alias `<project>-git-<branch>.localhost` (Vercel's branch
    /// URL) — resolves to the latest deployment on that branch. Empty without git.
    #[serde(default)]
    pub branch_alias: String,
    /// Immutable per-deployment Host alias `<id>.localhost`, always this deployment.
    #[serde(default)]
    pub id_alias: String,
    /// Build environment: "production" | "preview" — IMMUTABLE (unlike
    /// `production`, which is the live "is currently promoted" flag).
    #[serde(default)]
    pub target: String,
    #[serde(default = "default_ready")]
    pub state: DeployState,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub git: Option<GitSource>,
    #[serde(default)]
    pub production: bool,
    /// Type label for the UI: "static" | "function" | "fullstack".
    #[serde(default)]
    pub kind: String,
    /// The build's framework slug (nextjs/vite/astro/docker/…) — carried from
    /// the manifest so the dashboard shows each project's real framework logo,
    /// with a default fallback. Empty/absent = unknown.
    #[serde(default)]
    pub framework: String,
    /// Framework features mapped onto this deployment (redirects, middleware…).
    #[serde(default)]
    pub features: DeploymentFeatures,
    /// Owning team/tenant slug (empty = "personal").
    #[serde(default)]
    pub tenant: String,
    /// Allocated public raw-ingress bindings (TCP/UDP/gRPC public ports stamped
    /// on this deployment's manifest by the raw-port allocator). Carried here —
    /// unlike the rest of the manifest — because the generic raw proxy on EVERY
    /// edge node needs the fleet-wide `public_port` → deployment mapping, and
    /// `DeploymentInfo` is exactly what the `/v1/fleet-deployments` gossip
    /// already replicates into each node's `peer_deployments`. Empty for
    /// HTTP-only deployments and for peers running older binaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw_ports: Vec<RawPortBinding>,
    /// The deployment's dedicated public IPv4 (fluid.json
    /// `functions[].dedicatedIpv4`), if any function opted in and the deploy
    /// path's Tencent EIP purchase/associate succeeded. Hoisted here for the
    /// same reason as `raw_ports`: `raw_proxy.rs`'s bind reconcile loop on
    /// EVERY node needs the address to decide which local IP to bind, and DNS
    /// / the dashboard need it fleet-wide. `None` for every deployment with no
    /// opt-in and for peers running older binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedicated_ipv4: Option<DedicatedIpv4>,
    /// Browser-eligible functions with their build-stamped artifact descriptors
    /// (digest metadata only, never bytes — see [`BrowserArtifact`]). Carried
    /// here for the same reason as `raw_ports`: the control-plane leader
    /// validates browser admissions against deployments it may not host, and
    /// this gossip view is what it sees. Empty for every deployment with no
    /// browser opt-in and for peers running older binaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browser_functions: Vec<BrowserFunctionRef>,
    /// Functions the build EVALUATED for browser eligibility and declined, each
    /// with the reason ([`BrowserIneligibility`]). Replicated alongside
    /// `browser_functions` because the two are one answer: a deployment absent
    /// from the run-node picker is either not ready, listed here with a cause,
    /// or — carrying neither an artifact nor a reason — was built before
    /// eligibility was evaluated and needs a redeploy. Empty for peers running
    /// older binaries, which is the "never evaluated" state by construction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browser_ineligible: Vec<BrowserIneligibility>,
    /// The deployment's browser-database opt-in block, VERBATIM from the
    /// manifest (raw policy — resolved at the point of use via
    /// [`BrowserDbPolicy::resolve`], the `InferenceSpec` raw-spec-replicated
    /// precedent). Carried for the same reason as `browser_functions`: the
    /// admission-issuing leader and the fleet exchange peers resolve DB grants
    /// and caps for deployments they may not host, and this gossip view is
    /// what they see. `None` for every deployment with no `browser_db` opt-in
    /// and for peers running older binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_db: Option<BrowserDbPolicy>,
}
fn default_ready() -> DeployState {
    DeployState::Ready
}

/// Summary of framework build features the platform mapped onto a deployment —
/// surfaced in the dashboard (service graph, overview).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeploymentFeatures {
    pub redirects: usize,
    pub rewrites: usize,
    pub middleware: bool,
    pub edge_functions: usize,
    pub serverless_functions: usize,
}

/// Structured platform failure taxonomy (Fluid hardening #18). Maps each way a
/// request can fail to route/serve to a STABLE public code + correct HTTP status.
/// The public body is the code ONLY — internal error detail (`{e}`, peer ids,
/// tunnel internals) is logged/evented, never returned to the caller. Shared by the
/// gateway (lease/serve) and the edge (mesh routing) so the taxonomy is one source
/// of truth. Pure + dep-free (status as u16; callers convert to their HTTP type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    /// Tenant exceeded its fairness limit (rate / quota). 429.
    TenantThrottled,
    /// All instances at safe concurrency and the pool/tenant can't scale out. 503.
    CapacityExhausted,
    /// A deployment's circuit breaker is open (crash-looping). 503.
    DeploymentCircuitOpen,
    /// THIS NODE is missing a cell artifact it must have to boot anything: the
    /// per-image rootfs, the shared base rootfs, or the guest kernel. 503.
    ///
    /// A NODE PROVISIONING fault — neither an app fault nor a lack of capacity,
    /// and the only remedy is an operator reprovisioning the node's images.
    /// Deliberately distinct from [`FailureClass::CapacityExhausted`], which is
    /// where it used to land: witnessed on fc-sanjose-cvm-2, a missing
    /// `/var/lib/hive/rootfs/default.ext4` was reported as CAPACITY_EXHAUSTED on
    /// a node with 923 GB free disk and 2046 free podman locks, which sent the
    /// operator hunting a capacity problem that did not exist. Also distinct from
    /// [`FailureClass::DeploymentCircuitOpen`]: the missing rootfs failed every
    /// cold start, so the pool circuited too and the user-visible code alternated
    /// between two labels that both named the wrong thing.
    NodeImageMissing,
    /// THIS NODE's isolation backend cannot run cells at all — no `/dev/kvm`, no
    /// firecracker binary, wrong OS. 503.
    ///
    /// Split from [`FailureClass::NodeImageMissing`] because the remedy is
    /// different: the artifacts are fine and the HYPERVISOR is not there. On this
    /// fleet that is usually the documented PVM failure mode (`kvm_pvm` refuses
    /// to load while host PTI is active, so `/dev/kvm` silently disappears), not
    /// anything a copy of the rootfs would fix.
    NodeBackendUnavailable,
    /// THIS NODE's container lock pool is exhausted and nothing was reclaimable,
    /// so NO container can start here until an operator resizes it. 503.
    ///
    /// podman takes one lock from a fixed per-HOST pool (`num_locks`, default
    /// 2048) per container AND per volume, so a leak starves every tenant on the
    /// node. Reported as CAPACITY_EXHAUSTED it reads as "you need more capacity"
    /// when the truth is "this host's lock pool has to be renumbered" — the
    /// self-heal already reclaims what it safely can, and this class is what is
    /// left when it reclaimed nothing.
    NodeLockPoolExhausted,
    /// THIS NODE does not have the interpreter/runtime this deployment declares
    /// (e.g. a `runtime: "wasmer"` function on a node with no `wasmer` binary on
    /// the filesystem its cells exec against). 503.
    ///
    /// Split from every class around it because each names a different remedy
    /// and this one's is "provision the runtime on this node" — for Firecracker
    /// specifically that means the GUEST rootfs image, since the cell agent
    /// execs inside the microVM and a host-side install is invisible to it.
    /// Without its own class this lands in [`FailureClass::DeploymentCircuitOpen`]
    /// (the cold starts DO circuit the pool), which tells the tenant to go debug
    /// an entrypoint that is perfectly fine — the same inversion
    /// [`FailureClass::NodeImageMissing`] exists to prevent, one layer up.
    ///
    /// Placement's capability filter is meant to make this unreachable; see
    /// `hive_core::fault::NODE_RUNTIME_MISSING` for when it still fires.
    NodeRuntimeMissing,
    /// No healthy peer in the mesh can serve this deployment. 503.
    NoHealthyPeer,
    /// No healthy node in the deployment's configured region(s). 503.
    NoHealthyRegion,
    /// The runtime tunnel to the chosen instance failed. 502.
    RuntimeTunnelFailed,
    /// The tunnel CONNECTED and the function never produced a response head,
    /// on every instance we rerouted to. 504.
    ///
    /// Deliberately distinct from [`FailureClass::RuntimeTunnelFailed`]: both
    /// used to report `RUNTIME_TUNNEL_FAILED`, and because that name blames the
    /// transport, several debugging sessions were spent on vsock/tunnel plumbing
    /// while the real fault was an app that accepted the connection and then hung.
    /// Also distinct from `FUNCTION_INVOCATION_TIMEOUT`, which means one
    /// invocation outran its own `max_duration` on an otherwise healthy instance.
    FunctionNoResponse,
    /// The chosen peer was unreachable over both iroh and HTTP. 502.
    PeerUnreachable,
    /// First attempt failed and the request was not safe to retry (#6/#7/#8). 502.
    NotRetryable,
    /// The per-request deadline can't be met. 504.
    DeadlineExceeded,
    /// The host maps to no known deployment. 404.
    DeploymentNotFound,
    /// The Host header isn't a deployment host this node accepts. 404.
    HostRejected,
}

impl FailureClass {
    /// HTTP status as a u16 (dep-free; callers convert to their status type).
    pub fn status(self) -> u16 {
        match self {
            FailureClass::TenantThrottled => 429,
            FailureClass::CapacityExhausted
            | FailureClass::DeploymentCircuitOpen
            | FailureClass::NodeImageMissing
            | FailureClass::NodeBackendUnavailable
            | FailureClass::NodeLockPoolExhausted
            | FailureClass::NodeRuntimeMissing
            | FailureClass::NoHealthyPeer
            | FailureClass::NoHealthyRegion => 503,
            FailureClass::RuntimeTunnelFailed
            | FailureClass::PeerUnreachable
            | FailureClass::NotRetryable => 502,
            FailureClass::DeadlineExceeded | FailureClass::FunctionNoResponse => 504,
            FailureClass::DeploymentNotFound | FailureClass::HostRejected => 404,
        }
    }
    /// Stable, public, non-leaky machine code (safe to return + put in `x-hive-error`).
    pub fn code(self) -> &'static str {
        match self {
            FailureClass::TenantThrottled => "TENANT_THROTTLED",
            FailureClass::CapacityExhausted => "CAPACITY_EXHAUSTED",
            FailureClass::DeploymentCircuitOpen => "DEPLOYMENT_CIRCUIT_OPEN",
            FailureClass::NodeImageMissing => "NODE_IMAGE_MISSING",
            FailureClass::NodeBackendUnavailable => "NODE_BACKEND_UNAVAILABLE",
            FailureClass::NodeLockPoolExhausted => "NODE_LOCK_POOL_EXHAUSTED",
            FailureClass::NodeRuntimeMissing => "NODE_RUNTIME_MISSING",
            FailureClass::NoHealthyPeer => "NO_HEALTHY_PEER",
            FailureClass::NoHealthyRegion => "NO_HEALTHY_REGION",
            FailureClass::RuntimeTunnelFailed => "RUNTIME_TUNNEL_FAILED",
            FailureClass::FunctionNoResponse => "FUNCTION_NO_RESPONSE",
            FailureClass::PeerUnreachable => "PEER_UNREACHABLE",
            FailureClass::NotRetryable => "NOT_RETRYABLE",
            FailureClass::DeadlineExceeded => "DEADLINE_EXCEEDED",
            FailureClass::DeploymentNotFound => "DEPLOYMENT_NOT_FOUND",
            FailureClass::HostRejected => "HOST_REJECTED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        let m = Manifest {
            project: "p".into(),
            static_dir: Some("public".into()),
            functions: vec![],
            routes: vec![
                Route {
                    pattern: "/".into(),
                    target: RouteTarget::Static,
                },
                Route {
                    pattern: "/api".into(),
                    target: RouteTarget::Function("api".into()),
                },
                Route {
                    pattern: "/api/admin".into(),
                    target: RouteTarget::Function("admin".into()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.resolve("/index.html"), RouteTarget::Static);
        assert_eq!(m.resolve("/api/users"), RouteTarget::Function("api".into()));
        assert_eq!(
            m.resolve("/api/admin/x"),
            RouteTarget::Function("admin".into())
        );
        assert!(!path_matches("/api", "/apixyz"));
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[test]
    fn function_protocol_defaults_and_raw_classification() {
        let mut f = FunctionConfig::default();
        // Empty/legacy protocol normalizes to http; http is NOT raw-proxied.
        assert_eq!(f.protocol_or_http(), "http");
        assert!(!f.needs_raw_proxy());
        // json-rpc + ws ride the normal HTTP/L7 path (not raw).
        f.protocol = ServiceProtocol::JsonRpc;
        assert!(!f.needs_raw_proxy());
        f.protocol = ServiceProtocol::Ws;
        assert!(!f.needs_raw_proxy());
        // grpc + tcp + udp are connection-spliced cross-node.
        f.protocol = ServiceProtocol::Grpc;
        assert_eq!(f.protocol_or_http(), "grpc");
        assert!(f.needs_raw_proxy());
        f.protocol = ServiceProtocol::Tcp;
        assert!(f.needs_raw_proxy());
        f.protocol = ServiceProtocol::Udp;
        assert_eq!(f.protocol_or_http(), "udp");
        assert!(f.needs_raw_proxy());
        // A default (http) protocol is skipped on the wire (backward-compatible manifests).
        let j = serde_json::to_string(&FunctionConfig::default()).unwrap();
        assert!(
            !j.contains("\"protocol\""),
            "default protocol omitted from JSON"
        );
        // And a manifest without `protocol` deserializes to the http default.
        let back: FunctionConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(back.protocol_or_http(), "http");
        // Legacy empty-string protocol (old manifests) still deserializes to http.
        let legacy: FunctionConfig =
            serde_json::from_str(r#"{"name":"f","start_cmd":[],"protocol":""}"#).unwrap();
        assert_eq!(legacy.protocol, ServiceProtocol::Http);
        // Every known wire string round-trips through serde.
        for (variant, wire) in [
            (ServiceProtocol::Http, "http"),
            (ServiceProtocol::Https, "https"),
            (ServiceProtocol::Ws, "ws"),
            (ServiceProtocol::Wss, "wss"),
            (ServiceProtocol::Grpc, "grpc"),
            (ServiceProtocol::JsonRpc, "json-rpc"),
            (ServiceProtocol::Tcp, "tcp"),
            (ServiceProtocol::Udp, "udp"),
        ] {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(wire.parse::<ServiceProtocol>().unwrap(), variant);
            let stored: FunctionConfig = serde_json::from_str(&format!(
                r#"{{"name":"f","start_cmd":[],"protocol":"{wire}"}}"#
            ))
            .unwrap();
            assert_eq!(
                stored.protocol, variant,
                "stored deployment protocol {wire:?} must round-trip"
            );
        }
        // Strict-vs-lenient split: FromStr (the deploy-input boundary) rejects
        // unknown strings; serde Deserialize (the stored-state path) NEVER
        // fails on them — it coerces to http (see lenient_deserialize tests).
        assert!("quic".parse::<ServiceProtocol>().is_err());
        // PortSpec compatibility bridge from a legacy bare `Option<u16>` port.
        assert_eq!(
            PortSpec::from_legacy_port(None, ServiceProtocol::Tcp),
            vec![]
        );
        assert_eq!(
            PortSpec::from_legacy_port(Some(25565), ServiceProtocol::Tcp),
            vec![PortSpec::single(25565, ServiceProtocol::Tcp)]
        );
        // FunctionConfig.ports is empty by default and round-trips when set
        // (e.g. a Minecraft-style multi-port service).
        assert!(FunctionConfig::default().ports.is_empty());
        f.ports = vec![
            PortSpec {
                container_port: 25565,
                protocol: ServiceProtocol::Tcp,
                label: Some("game".into()),
                public_port: None,
                preferred_public_port: None,
            },
            PortSpec {
                container_port: 25575,
                protocol: ServiceProtocol::Tcp,
                label: Some("rcon".into()),
                public_port: None,
                preferred_public_port: None,
            },
            PortSpec {
                container_port: 25565,
                protocol: ServiceProtocol::Udp,
                label: Some("query".into()),
                public_port: None,
                preferred_public_port: None,
            },
        ];
        let j2 = serde_json::to_string(&f).unwrap();
        let back2: FunctionConfig = serde_json::from_str(&j2).unwrap();
        assert_eq!(back2.ports, f.ports);
    }

    #[test]
    fn deploy_state_defaults_to_ready() {
        assert_eq!(DeployState::default(), DeployState::Ready);
    }

    #[test]
    fn route_class_policy_semantics() {
        // Cache.
        assert_eq!(
            RouteClass::Static.cache_policy(None),
            RouteCachePolicy::Immutable
        );
        assert_eq!(
            RouteClass::Static
                .cache_policy(None)
                .cache_control()
                .as_deref(),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(
            RouteClass::Isr.cache_policy(Some(60)),
            RouteCachePolicy::Revalidate(60)
        );
        assert_eq!(
            RouteClass::Isr
                .cache_policy(Some(60))
                .cache_control()
                .as_deref(),
            Some("public, s-maxage=60, stale-while-revalidate")
        );
        // revalidate 0 / None / negative all clamp to a valid >=1 s-maxage.
        for r in [Some(0), None, Some(-9)] {
            assert_eq!(
                RouteClass::Isr.cache_policy(r).cache_control().as_deref(),
                Some("public, s-maxage=1, stale-while-revalidate")
            );
        }
        for c in [
            RouteClass::SsrPage,
            RouteClass::ApiNode,
            RouteClass::RouteHandler,
            RouteClass::Edge,
            RouteClass::Middleware,
        ] {
            assert_eq!(c.cache_policy(Some(10)), RouteCachePolicy::Origin);
            assert_eq!(c.cache_policy(None).cache_control(), None);
        }
        // Replay / runtime.
        for c in [RouteClass::Static, RouteClass::Isr, RouteClass::SsrPage] {
            assert!(c.always_replayable());
        }
        for c in [
            RouteClass::ApiNode,
            RouteClass::RouteHandler,
            RouteClass::Edge,
            RouteClass::Middleware,
        ] {
            assert!(!c.always_replayable());
        }
        assert!(!RouteClass::Static.uses_runtime() && !RouteClass::Isr.uses_runtime());
        assert!(RouteClass::SsrPage.uses_runtime() && RouteClass::ApiNode.uses_runtime());
    }

    #[test]
    fn route_class_from_name_roundtrip_and_unknown() {
        // Mirrors fluid_build::per_route::RouteKind::class_name strings.
        for (s, c) in [
            ("static", RouteClass::Static),
            ("isr", RouteClass::Isr),
            ("api_node", RouteClass::ApiNode),
            ("route_handler", RouteClass::RouteHandler),
            ("ssr_page", RouteClass::SsrPage),
            ("edge", RouteClass::Edge),
            ("middleware", RouteClass::Middleware),
        ] {
            assert_eq!(RouteClass::from_name(s), c);
            assert_eq!(c.name(), s, "name() is the inverse of from_name()");
        }
        // Unknown is treated as a dynamic SSR page (safe default).
        assert_eq!(RouteClass::from_name("whatever"), RouteClass::SsrPage);
    }

    #[test]
    fn next_route_match_exact_dynamic_catchall() {
        // Exact.
        assert!(next_route_match("/api/claw", "/api/claw").is_some());
        assert!(next_route_match("/api/claw", "/api/claws").is_none());
        assert!(next_route_match("/api/claw", "/api/claw/x").is_none());
        // Dynamic single segment.
        assert!(next_route_match("/blog/[slug]", "/blog/hello").is_some());
        assert!(next_route_match("/blog/[slug]", "/blog/hello/world").is_none());
        assert!(next_route_match("/blog/[slug]", "/blog").is_none());
        // Catch-all requires >=1 segment.
        assert!(next_route_match("/docs/[...path]", "/docs/a/b/c").is_some());
        assert!(next_route_match("/docs/[...path]", "/docs").is_none());
        // Optional catch-all matches zero-or-more.
        assert!(next_route_match("/shop/[[...slug]]", "/shop").is_some());
        assert!(next_route_match("/shop/[[...slug]]", "/shop/a/b").is_some());
        // Root.
        assert!(next_route_match("/", "/").is_some());
        assert!(next_route_match("/", "/x").is_none());
        // Specificity ordering: exact > dynamic.
        assert!(
            next_route_match("/blog/featured", "/blog/featured").unwrap()
                > next_route_match("/blog/[slug]", "/blog/featured").unwrap()
        );
    }

    #[test]
    fn manifest_route_policy_prefers_most_specific() {
        let m = Manifest {
            route_policies: vec![
                RoutePolicy {
                    pattern: "/blog/[slug]".into(),
                    class: RouteClass::Isr,
                    revalidate: Some(60),
                },
                RoutePolicy {
                    pattern: "/blog/featured".into(),
                    class: RouteClass::Static,
                    revalidate: None,
                },
                RoutePolicy {
                    pattern: "/api/claw".into(),
                    class: RouteClass::ApiNode,
                    revalidate: None,
                },
            ],
            ..Default::default()
        };
        // Exact static beats the dynamic ISR for the same path.
        assert_eq!(
            m.route_policy("/blog/featured").unwrap().class,
            RouteClass::Static
        );
        // Dynamic ISR for any other slug.
        let p = m.route_policy("/blog/hello").unwrap();
        assert_eq!(p.class, RouteClass::Isr);
        assert_eq!(p.revalidate, Some(60));
        // API route.
        assert_eq!(
            m.route_policy("/api/claw?x=1").unwrap().class,
            RouteClass::ApiNode
        );
        // No policy for unmatched path.
        assert!(m.route_policy("/nope").is_none());
        // Empty policies (common case) -> always None, no allocation/iteration.
        assert!(Manifest::default().route_policy("/anything").is_none());
    }

    #[test]
    fn manifest_resolve_longest_prefix_wins() {
        let m = Manifest {
            routes: vec![
                Route {
                    pattern: "/".into(),
                    target: RouteTarget::Static,
                },
                Route {
                    pattern: "/api".into(),
                    target: RouteTarget::Function("api".into()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.resolve("/api/users"), RouteTarget::Function("api".into()));
        assert_eq!(m.resolve("/index.html"), RouteTarget::Static);
    }

    #[test]
    fn manifest_redirect_and_rewrite() {
        let m = Manifest {
            redirects: vec![Redirect {
                source: "/old".into(),
                destination: "/new".into(),
                status: 308,
                has: vec![],
                missing: vec![],
            }],
            rewrites: vec![Rewrite {
                source: "/proxy".into(),
                destination: "/internal".into(),
                has: vec![],
                missing: vec![],
            }],
            ..Default::default()
        };
        assert_eq!(m.redirect_for("/old"), Some(("/new".to_string(), 308)));
        assert_eq!(m.redirect_for("/nope"), None);
        assert_eq!(m.rewrite_path("/proxy"), "/internal");
        assert_eq!(m.rewrite_path("/untouched"), "/untouched");
    }

    fn red(
        source: &str,
        dest: &str,
        status: u16,
        has: Vec<RuleCondition>,
        missing: Vec<RuleCondition>,
    ) -> Redirect {
        Redirect {
            source: source.into(),
            destination: dest.into(),
            status,
            has,
            missing,
        }
    }

    #[test]
    fn param_matching_and_substitution() {
        let m = Manifest {
            redirects: vec![
                red("/blog/:slug", "/news/:slug", 308, vec![], vec![]),
                red("/proxy/:path*", "/internal/:path*", 307, vec![], vec![]),
                red("/post/:p(\\d+)", "/n/:p", 308, vec![], vec![]),
            ],
            ..Default::default()
        };
        assert_eq!(
            m.redirect_for("/blog/hello"),
            Some(("/news/hello".into(), 308))
        );
        assert_eq!(
            m.redirect_for("/proxy/a/b/c"),
            Some(("/internal/a/b/c".into(), 307))
        );
        assert_eq!(m.redirect_for("/post/42"), Some(("/n/42".into(), 308)));
        assert_eq!(m.redirect_for("/post/abc"), None); // non-numeric fails the inline regex
    }

    #[test]
    fn has_missing_conditions() {
        let m = Manifest {
            rewrites: vec![Rewrite {
                source: "/dashboard".into(),
                destination: "/login".into(),
                has: vec![],
                missing: vec![RuleCondition {
                    kind: "cookie".into(),
                    key: Some("auth_token".into()),
                    value: None,
                }],
            }],
            ..Default::default()
        };
        // No auth cookie -> rewrite to /login.
        let ctx_no = ReqCtx::default();
        assert_eq!(m.rewrite_path_ctx("/dashboard", &ctx_no), "/login");
        // With auth cookie present -> NOT rewritten.
        let ctx_yes = ReqCtx {
            headers: vec![("cookie".into(), "auth_token=abc".into())],
            ..Default::default()
        };
        assert_eq!(m.rewrite_path_ctx("/dashboard", &ctx_yes), "/dashboard");
    }

    #[test]
    fn header_rules_inject() {
        let m = Manifest {
            headers: vec![HeaderRule {
                source: "/(.*)".into(),
                headers: vec![Header {
                    key: "X-Frame-Options".into(),
                    value: "DENY".into(),
                }],
                has: vec![],
                missing: vec![],
            }],
            ..Default::default()
        };
        let got = m.headers_for("/anything", &ReqCtx::default());
        assert_eq!(
            got,
            vec![("X-Frame-Options".to_string(), "DENY".to_string())]
        );
    }

    #[test]
    fn trailing_slash_normalization() {
        let strip = Manifest {
            trailing_slash: Some(false),
            ..Default::default()
        };
        assert_eq!(
            strip.trailing_slash_redirect("/about/"),
            Some("/about".into())
        );
        assert_eq!(strip.trailing_slash_redirect("/about"), None);
        let add = Manifest {
            trailing_slash: Some(true),
            ..Default::default()
        };
        assert_eq!(
            add.trailing_slash_redirect("/about"),
            Some("/about/".into())
        );
        assert_eq!(add.trailing_slash_redirect("/styles.css"), None); // file ext untouched
        let none = Manifest {
            trailing_slash: None,
            ..Default::default()
        };
        assert_eq!(none.trailing_slash_redirect("/about/"), None);
    }

    #[test]
    fn redirect_status_resolution() {
        assert_eq!(redirect_status(None, None), 308);
        assert_eq!(redirect_status(Some(false), None), 307);
        assert_eq!(redirect_status(Some(true), None), 308);
        assert_eq!(redirect_status(Some(true), Some(301)), 301);
    }

    #[test]
    fn failure_class_taxonomy() {
        assert_eq!(FailureClass::TenantThrottled.status(), 429);
        assert_eq!(FailureClass::CapacityExhausted.status(), 503);
        assert_eq!(FailureClass::NoHealthyRegion.status(), 503);
        assert_eq!(FailureClass::PeerUnreachable.status(), 502);
        assert_eq!(FailureClass::NotRetryable.status(), 502);
        assert_eq!(FailureClass::DeadlineExceeded.status(), 504);
        assert_eq!(FailureClass::DeploymentNotFound.status(), 404);
        assert_eq!(FailureClass::CapacityExhausted.code(), "CAPACITY_EXHAUSTED");
        // codes are stable + uppercase machine tokens, never internal detail
        for c in [
            FailureClass::TenantThrottled,
            FailureClass::CapacityExhausted,
            FailureClass::NoHealthyPeer,
            FailureClass::PeerUnreachable,
            FailureClass::DeadlineExceeded,
            FailureClass::DeploymentNotFound,
        ] {
            assert!(c
                .code()
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch == '_'));
        }
    }

    #[test]
    fn stored_state_with_unknown_protocol_loads_leniently_as_http() {
        // The data-loss landmine this guards: the pre-enum protocol field was
        // an unvalidated bare String, so persisted snapshots / gossip payloads
        // can contain arbitrary strings ("h2c", "HTTP", typos) — and both
        // loaders (persist::load's unwrap_or_default, guardian's
        // skip-on-parse-failure) turn a deserialize error into total silent
        // state loss. Deserialize must therefore NEVER fail on this field.
        let f: FunctionConfig =
            serde_json::from_str(r#"{"name":"f","start_cmd":[],"protocol":"h2c"}"#)
                .expect("unknown stored protocol must NOT fail deserialization");
        assert_eq!(f.protocol, ServiceProtocol::Http);
        // Same through the snapshot-shaped nesting (DeployRecord -> Manifest
        // -> FunctionConfig, the exact chain inside PlatformSnapshot), plus a
        // legacy-cased value and an unknown ports[].protocol.
        let json = r#"{"id":"d1","project":"p","root":"/tmp","manifest":{"project":"p",
            "functions":[{"name":"api","start_cmd":["node"],"protocol":"HTTP",
                          "ports":[{"container_port":9,"protocol":"h2c"}]}]},
            "created_at_ms":0,"creator":"you","git":null,"production":true}"#;
        let rec: DeployRecord =
            serde_json::from_str(json).expect("stored snapshot row must load despite bad protocol");
        assert_eq!(rec.manifest.functions[0].protocol, ServiceProtocol::Http);
        assert_eq!(
            rec.manifest.functions[0].ports[0].protocol,
            ServiceProtocol::Http
        );
    }

    #[test]
    fn manifest_from_json_still_rejects_unknown_protocol_in_fresh_input() {
        // Deploy-input boundary stays STRICT: from_json checks the raw
        // strings before lenient serde can coerce them.
        let err = Manifest::from_json(
            r#"{"project":"p","functions":[{"name":"api","start_cmd":["node"],"protocol":"h2c"}]}"#,
        )
        .expect_err("unknown protocol in fresh fluid.json must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("h2c") && msg.contains("unknown protocol"),
            "clear error, got: {msg}"
        );
        // ports[].protocol is validated too.
        assert!(Manifest::from_json(
            r#"{"project":"p","functions":[{"name":"api","start_cmd":["node"],
                "ports":[{"container_port":9,"protocol":"htp"}]}]}"#,
        )
        .is_err());
        // Known values (and legacy empty string) still parse fine.
        let ok = Manifest::from_json(
            r#"{"project":"p","functions":[{"name":"api","start_cmd":["node"],"protocol":"grpc"},
                                            {"name":"web","start_cmd":["node"],"protocol":""}]}"#,
        )
        .expect("valid manifest must parse");
        assert_eq!(ok.functions[0].protocol, ServiceProtocol::Grpc);
        assert_eq!(ok.functions[1].protocol, ServiceProtocol::Http);
    }

    #[test]
    fn deploy_record_state_defaults_when_absent() {
        // Snapshots written before `state` existed deserialize to Ready.
        let json = r#"{"id":"d1","project":"p","root":"/tmp","manifest":{"project":"p"},"created_at_ms":0,"creator":"you","git":null,"production":true}"#;
        let rec: DeployRecord = serde_json::from_str(json).expect("deserializes without state");
        assert_eq!(rec.state, DeployState::Ready);
    }
}
