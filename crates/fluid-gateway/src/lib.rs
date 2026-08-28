//! `fluid-gateway` — the public router.
//!
//! This is the Vercel "Functions router" analogue. For each incoming request it:
//! 1. selects the target deployment (by `Host` subdomain, else the default),
//! 2. resolves the path to a route (static asset vs function),
//! 3. serves the file, or leases a Fluid instance and proxies the request to it.
//!
//! Each instance is reached over a single **multiplexed tunnel**
//! ([`fluid_tunnel::TunnelClient`]): one persistent connection carries many
//! concurrent requests (stream-id framing) plus in-band metrics and nack. The
//! gateway keeps one tunnel per instance and reuses it for every request.

mod static_files;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use fluid_compute::{func_key, Fluid, FunctionStats, Lease};
use fluid_core::{
    BuildOutputV3Evaluator, BuildOutputV3Refusal, BuildOutputV3Target, DeployRequest, Deployment,
    DeploymentId, DeploymentInfo, Manifest, ProjectIncarnation, RouteTarget,
};
use fluid_tunnel::TunnelClient;
use hive_backend::{connect_endpoint, RuntimeArtifactPaths, RuntimeArtifactSpec};
use hive_core::{now_ms, CellId};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

struct GwState {
    deployments: HashMap<DeploymentId, Deployment>,
    /// Open directory identities held for each deployment's whole registered
    /// lifetime. Static reads never re-resolve a replaceable root pathname.
    static_roots: HashMap<DeploymentId, static_files::StaticRoot>,
    /// project name -> current deployment id.
    aliases: HashMap<String, DeploymentId>,
    /// FULL hostname (lowercased, e.g. `numo.gg`, `www.numo.gg`) -> deployment
    /// id, for tenant-attached custom domains. Checked BEFORE the label map in
    /// every resolution path: an attached domain must route its exact host and
    /// nothing else — first-label keying would also serve `numo.attacker.tld`
    /// and let `www.numo.gg` shadow the platform's own `www` label.
    aliases_full: HashMap<String, DeploymentId>,
    /// Compiled, authoritative Build Output v3 route state. A refusal is retained
    /// beside the deployment so no request can fall through to legacy routing.
    build_output_v3: HashMap<DeploymentId, BuildOutputV3RouteState>,
    default: Option<DeploymentId>,
}

#[derive(Clone)]
enum BuildOutputV3RouteState {
    Ready(Arc<BuildOutputV3Evaluator>),
    Refused(BuildOutputV3Refusal),
}

#[derive(Clone)]
struct SelectedDeployment {
    deployment: Deployment,
    static_root: Option<static_files::StaticRoot>,
    build_output_v3: Option<BuildOutputV3RouteState>,
}

impl std::ops::Deref for SelectedDeployment {
    type Target = Deployment;

    fn deref(&self) -> &Self::Target {
        &self.deployment
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserScope {
    Team,
    Public,
}

impl Default for BrowserScope {
    fn default() -> Self {
        Self::Team
    }
}

/// One short-lived browser serving registration. The tenant and exact
/// deployment/function are part of the key; a content digest alone is never an
/// authorization capability.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserTarget {
    pub tenant: String,
    pub deployment: String,
    pub function: String,
    pub endpoint_id: String,
    pub addr_json: String,
    pub digest: String,
    pub expires_ms: u64,
    #[serde(default)]
    pub scope: BrowserScope,
}

/// Hard ceiling on serving registrations one browser endpoint may hold
/// (browser-auto-serve-eligible-set). The admission side bounds its own
/// eligible set well below this; this is the gateway's own independent refusal
/// so a bug (or a future caller) upstream can never make one tab's routing
/// table unbounded.
pub const MAX_BROWSER_TARGETS_PER_ENDPOINT: usize = 64;

#[derive(Clone, Debug)]
pub struct BrowserInvokeFailure {
    pub sent: bool,
    pub message: String,
}

pub type BrowserInvokeFuture =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, BrowserInvokeFailure>> + Send>>;
pub type BrowserInvoker = Arc<dyn Fn(BrowserTarget, String) -> BrowserInvokeFuture + Send + Sync>;
/// Resolves the caller's authenticated tenant from the request's own headers
/// (platform JWT bearer / cookie / API key — whichever hive-cloud's own
/// `auth` module already accepts elsewhere), or `None` if unauthenticated.
/// fluid-gateway has no knowledge of hive-cloud's Clerk/platform-JWT auth
/// system (it's the lower-level, generic crate hive-cloud embeds, never the
/// other way — see `rum`'s doc comment above for the same asymmetry), so this
/// is injected exactly like `BrowserInvoker` rather than implemented here.
pub type BrowserClaimsResolver = Arc<dyn Fn(&HeaderMap) -> Option<String> + Send + Sync>;

#[derive(Default)]
struct BrowserRoutes {
    invoker: Option<BrowserInvoker>,
    claims_resolver: Option<BrowserClaimsResolver>,
    by_function: HashMap<String, Vec<BrowserTarget>>,
    circuit_until: HashMap<String, u64>,
    /// Per-endpoint invocation quota (bn-p2p-heartbeat-lease): a fixed
    /// window `(window_start_ms, count_in_window)` keyed on `endpoint_id`.
    /// An admitted-but-unrevoked browser has an unbounded invoke rate today
    /// otherwise — a lease bounds HOW LONG it can be invoked, never how
    /// OFTEN, and that's a real abuse surface a compromised or careless
    /// caller can hit (volunteer-compute-trust-admission-models research:
    /// borrowed from BOINC's `max_results_day`-style throttle, quota-shaped
    /// rather than a binary revoke).
    invoke_quota: HashMap<String, (u64, u32)>,
}

/// Validate and compile a manifest's authoritative Build Output v3 route table.
/// This is public so the build planner can fail before registration and direct
/// executable witnesses can exercise the same capability gate as deployment.
pub fn build_output_v3_evaluator(
    manifest: &Manifest,
) -> Result<Option<BuildOutputV3Evaluator>, BuildOutputV3Refusal> {
    let Some(descriptor) = manifest.build_output_v3.as_ref() else {
        return Ok(None);
    };
    let evaluator = descriptor.compile()?;
    let config = descriptor.config_view()?;
    if !descriptor.assets.is_empty() && manifest.static_dir.is_none() {
        return Err(BuildOutputV3Refusal::invalid(
            "manifest.static_dir",
            "indexed Build Output assets require a server-derived static_dir",
        ));
    }
    if config.images.is_some() && manifest.images.is_none() {
        return Err(BuildOutputV3Refusal::invalid(
            "manifest.images",
            "Build Output image configuration was not projected into the executable manifest",
        ));
    }
    if let Some(images) = manifest.images.as_ref() {
        validate_build_output_v3_image_capabilities(images)?;
    }
    if !config.crons.is_empty() && manifest.crons.is_empty() {
        return Err(BuildOutputV3Refusal::invalid(
            "manifest.crons",
            "Build Output cron configuration was not projected into the executable manifest",
        ));
    }
    if manifest.functions.len() != descriptor.functions.len() {
        return Err(BuildOutputV3Refusal::invalid(
            "manifest.functions",
            "must contain exactly the validated Build Output function set",
        ));
    }
    for function in &descriptor.functions {
        let runtime = function.runtime().ok_or_else(|| {
            BuildOutputV3Refusal::invalid(
                format!("functions[{:?}].config.runtime", function.name),
                "is missing",
            )
        })?;
        if !is_supported_build_output_runtime(runtime) {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "function runtime {runtime:?} for {:?}",
                function.name
            )));
        }
        let projected = manifest.function(&function.name).ok_or_else(|| {
            BuildOutputV3Refusal::invalid(
                format!("manifest.functions[{:?}]", function.name),
                "the v3 function was not provisioned into the executable manifest",
            )
        })?;
        validate_build_output_v3_function_projection(function, projected)?;
    }
    Ok(Some(evaluator))
}

/// Exact, closed allowlist — never a pattern match. Vercel's own currently
/// supported Node.js Lambda runtimes are 20.x/22.x/24.x; any other value
/// (an EOL Node line, a not-yet-released one, or a typo) is an unsupported
/// capability, not merely "not yet tested".
pub const SUPPORTED_BUILD_OUTPUT_NODE_RUNTIMES: &[&str] =
    &["nodejs20.x", "nodejs22.x", "nodejs24.x"];

fn is_supported_build_output_runtime(runtime: &str) -> bool {
    SUPPORTED_BUILD_OUTPUT_NODE_RUNTIMES.contains(&runtime)
}

fn validate_build_output_v3_function_projection(
    function: &fluid_core::BuildOutputV3Function,
    projected: &fluid_core::FunctionConfig,
) -> Result<(), BuildOutputV3Refusal> {
    let object = function.config.as_object().ok_or_else(|| {
        BuildOutputV3Refusal::invalid(
            format!("functions[{:?}].config", function.name),
            "must be an object",
        )
    })?;
    const KNOWN: &[&str] = &[
        "runtime",
        "handler",
        "memory",
        "architecture",
        "maxDuration",
        "environment",
        "regions",
        "supportsWrapper",
        "supportsResponseStreaming",
        "launcherType",
        "shouldAddHelpers",
        "shouldAddSourcemapSupport",
        "awsLambdaHandler",
    ];
    if let Some(field) = object.keys().find(|field| !KNOWN.contains(&field.as_str())) {
        return Err(BuildOutputV3Refusal::unsupported(format!(
            "function {:?} runtime field {field:?}",
            function.name
        )));
    }
    if !matches!(projected.runtime.as_str(), "node" | "bun") {
        return Err(BuildOutputV3Refusal::unsupported(format!(
            "function {:?} projected runtime {:?}",
            function.name, projected.runtime
        )));
    }
    if projected.start_cmd.is_empty() {
        return Err(BuildOutputV3Refusal::unsupported(format!(
            "function {:?} has no real HTTP launcher",
            function.name
        )));
    }
    if let Some(launcher) = object.get("launcherType") {
        if launcher.as_str() != Some("Nodejs") {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "function {:?} launcherType {:?}",
                function.name, launcher
            )));
        }
    }
    for field in ["supportsWrapper", "shouldAddHelpers"] {
        if object
            .get(field)
            .is_some_and(|value| value.as_bool() != Some(false))
        {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "function {:?} runtime capability {field}",
                function.name
            )));
        }
    }
    for field in ["supportsResponseStreaming", "shouldAddSourcemapSupport"] {
        if object
            .get(field)
            .is_some_and(|value| value.as_bool().is_none())
        {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{:?}].config.{field}", function.name),
                "must be a boolean",
            ));
        }
    }
    if object
        .get("awsLambdaHandler")
        .is_some_and(|value| value.as_str().is_none_or(|value| !value.is_empty()))
    {
        return Err(BuildOutputV3Refusal::unsupported(format!(
            "function {:?} AWS Lambda handler launcher",
            function.name
        )));
    }
    if let Some(architecture) = object.get("architecture") {
        let Some(architecture) = architecture.as_str() else {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{:?}].config.architecture", function.name),
                "must be a string",
            ));
        };
        if architecture != "x86_64" {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "function {:?} architecture {architecture:?}",
                function.name
            )));
        }
    }
    if let Some(memory) = object.get("memory") {
        let memory = memory.as_u64().and_then(|value| u32::try_from(value).ok());
        if memory != Some(projected.memory_mib) {
            return Err(BuildOutputV3Refusal::invalid(
                format!("manifest.functions[{:?}].memory_mib", function.name),
                "does not preserve .vc-config.json memory",
            ));
        }
    }
    if let Some(duration) = object.get("maxDuration") {
        let duration = duration.as_u64();
        if duration != Some(projected.max_duration_secs) {
            return Err(BuildOutputV3Refusal::invalid(
                format!("manifest.functions[{:?}].max_duration_secs", function.name),
                "does not preserve .vc-config.json maxDuration",
            ));
        }
    }
    if let Some(regions) = object.get("regions") {
        let Some(regions) = regions.as_array() else {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{:?}].config.regions", function.name),
                "must be an array for a Node.js function",
            ));
        };
        let raw: Option<Vec<&str>> = regions.iter().map(serde_json::Value::as_str).collect();
        let projected_regions: Vec<&str> = projected.regions.iter().map(String::as_str).collect();
        if raw.as_deref() != Some(projected_regions.as_slice()) {
            return Err(BuildOutputV3Refusal::invalid(
                format!("manifest.functions[{:?}].regions", function.name),
                "does not preserve .vc-config.json regions",
            ));
        }
    }
    if let Some(environment) = object.get("environment") {
        let Some(environment) = environment.as_object() else {
            return Err(BuildOutputV3Refusal::invalid(
                format!("functions[{:?}].config.environment", function.name),
                "must be an object of string values",
            ));
        };
        for (name, value) in environment {
            let Some(value) = value.as_str() else {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("functions[{:?}].config.environment", function.name),
                    "must contain only string values",
                ));
            };
            if projected.env.get(name).map(String::as_str) != Some(value) {
                return Err(BuildOutputV3Refusal::invalid(
                    format!("manifest.functions[{:?}].env", function.name),
                    format!("does not preserve runtime variable {name:?}"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_build_output_v3_image_capabilities(
    images: &fluid_core::ImagesConfig,
) -> Result<(), BuildOutputV3Refusal> {
    if images.formats.iter().any(|format| format != "image/webp") {
        return Err(BuildOutputV3Refusal::unsupported(
            "Build Output image format other than image/webp",
        ));
    }
    for pattern in &images.remote_patterns {
        let hostname = pattern.hostname.as_str();
        let wildcard = hostname
            .strip_prefix("**.")
            .or_else(|| hostname.strip_prefix("*."));
        let hostname = wildcard.unwrap_or(hostname);
        if hostname.is_empty()
            || hostname
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
        {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "image remote hostname pattern {:?}",
                pattern.hostname
            )));
        }
        if pattern
            .pathname
            .as_deref()
            .is_some_and(|pattern| !supported_build_output_image_path_pattern(pattern))
        {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "image remote pathname pattern {:?}",
                pattern.pathname
            )));
        }
    }
    for pattern in &images.local_patterns {
        if pattern
            .pathname
            .as_deref()
            .is_some_and(|pattern| !supported_build_output_image_path_pattern(pattern))
        {
            return Err(BuildOutputV3Refusal::unsupported(format!(
                "image local pathname pattern {:?}",
                pattern.pathname
            )));
        }
    }
    Ok(())
}

fn supported_build_output_image_path_pattern(pattern: &str) -> bool {
    let stripped = pattern
        .strip_prefix('^')
        .unwrap_or(pattern)
        .strip_suffix('$')
        .unwrap_or(pattern);
    let literal = stripped
        .strip_suffix(".*")
        .or_else(|| stripped.strip_suffix("/**"))
        .or_else(|| stripped.strip_suffix("/*"))
        .unwrap_or(stripped);
    literal.starts_with('/')
        && !literal.bytes().any(|byte| {
            matches!(
                byte,
                b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'+' | b'?' | b'|'
            )
        })
}

/// Resolve the CWD a function actually launches from. Every existing
/// runtime shares one deployment-wide `base` (unchanged behavior); a
/// function carrying [`fluid_core::FunctionConfig::cwd_relative`] — today,
/// only Build Output API v3 Node `.func` bundles — launches from its OWN
/// subdirectory instead, so sibling functions in the same deployment can
/// never resolve each other's relative `import()`s.
fn function_launch_workdir(base: &str, function: &fluid_core::FunctionConfig) -> String {
    match function.cwd_relative.as_deref() {
        Some(rel) if !rel.is_empty() => Path::new(base).join(rel).to_string_lossy().into_owned(),
        _ => base.to_string(),
    }
}

fn build_output_v3_route_state(manifest: &Manifest) -> Option<BuildOutputV3RouteState> {
    match build_output_v3_evaluator(manifest) {
        Ok(Some(evaluator)) => Some(BuildOutputV3RouteState::Ready(Arc::new(evaluator))),
        Ok(None) => None,
        Err(refusal) => Some(BuildOutputV3RouteState::Refused(refusal)),
    }
}

pub struct Gateway {
    fluid: Arc<Fluid>,
    /// Image/rootfs used for function cells (matters for the firecracker backend).
    image: String,
    state: Mutex<GwState>,
    /// One multiplexed tunnel per instance (cell), reused for all its requests.
    /// Async mutex so creation is serialized per gateway (no duplicate/orphan
    /// tunnels race) — held only briefly across the connect.
    tunnels: tokio::sync::Mutex<HashMap<CellId, Arc<TunnelClient>>>,
    tunnels_opened: AtomicU64,
    tunnels_reused: AtomicU64,
    /// Real User Monitoring samples from the `@vercel/speed-insights` beacon
    /// (see `handle_public`'s `/_vercel/speed-insights/vitals` handling). Lives
    /// here (not in hive-cloud's CloudState) because `Deployment.tenant` — the
    /// only tenant attribution available at the point the beacon is received —
    /// is resolved via `Gateway::select`, and `handle_public` has no CloudState
    /// access at all (fluid-gateway is a lower-level crate hive-cloud embeds,
    /// never the other way around).
    rum: RumStore,
    /// Low-trust browser serving targets and their independent circuit state.
    /// Kept outside Fluid's lease pools so a frozen tab can never be classified
    /// as host capacity exhaustion.
    browser: RwLock<BrowserRoutes>,
}

impl Gateway {
    pub fn new(fluid: Arc<Fluid>, image: String) -> Arc<Gateway> {
        Arc::new(Gateway {
            fluid,
            image,
            state: Mutex::new(GwState {
                deployments: HashMap::new(),
                static_roots: HashMap::new(),
                aliases: HashMap::new(),
                aliases_full: HashMap::new(),
                build_output_v3: HashMap::new(),
                default: None,
            }),
            tunnels: tokio::sync::Mutex::new(HashMap::new()),
            tunnels_opened: AtomicU64::new(0),
            tunnels_reused: AtomicU64::new(0),
            rum: RumStore::new(),
            browser: RwLock::new(BrowserRoutes::default()),
        })
    }

    /// Record one `/_vercel/speed-insights/vitals` beacon payload
    /// (`{"href":"...","vitals":{"FCP":...,"LCP":...,"CLS":...,"INP":...,"TTFB":...}}`)
    /// under `tenant`. Malformed bodies are silently dropped (a beacon is
    /// best-effort telemetry, never worth a request failure).
    pub fn record_vitals(&self, tenant: &str, device: RumDevice, body: &[u8]) {
        self.rum.record(tenant, device, body);
    }

    /// Real-User-Monitoring summary for `tenant` over the last `minutes`,
    /// optionally narrowed to one device class — p75/p90/p95/p99 per vital,
    /// a computed Real Experience Score, real top routes, and the true sample
    /// count (so the dashboard can show an honest "collecting" state at 0
    /// rather than the previous permanently-empty stub). LOCAL to this node
    /// only — hive-cloud's `/v1/speed-insights` handler fans this out across
    /// the fleet via `rum_raw` + `RumRaw::merge` before calling `summarize()`,
    /// same reason `/v1/metrics` fans out (a tenant's visitors can land on
    /// any node).
    pub fn rum_summary(
        &self,
        tenant: &str,
        minutes: usize,
        device: Option<RumDevice>,
    ) -> RumSummary {
        self.rum.summary(tenant, minutes, device, now_ms())
    }

    /// This node's local raw RUM data for `tenant` — the mergeable unit a
    /// fleet-wide `/v1/speed-insights` fan-out combines via `RumRaw::merge`.
    pub fn rum_raw(&self, tenant: &str, minutes: usize, device: Option<RumDevice>) -> RumRaw {
        self.rum.raw(tenant, minutes, device, now_ms())
    }

    pub fn set_browser_invoker(&self, invoker: BrowserInvoker) {
        self.browser.write().invoker = Some(invoker);
    }

    /// Wires the caller-tenant resolver used to gate `BrowserScope::Team`
    /// targets in `try_browser` — see `BrowserClaimsResolver`'s doc comment.
    /// `Public`-scoped targets are unaffected: they remain reachable by any
    /// caller regardless of whether this is ever set.
    pub fn set_browser_claims_resolver(&self, resolver: BrowserClaimsResolver) {
        self.browser.write().claims_resolver = Some(resolver);
    }

    /// Insert or replace the one target owned by an endpoint for a function.
    /// Replacement is atomic: stale digest/address data cannot survive renewal.
    ///
    /// Thin wrapper over [`Gateway::set_browser_targets`] — a one-element set —
    /// kept because the single-target shape is still the explicit-pin case.
    pub fn upsert_browser_target(&self, target: BrowserTarget) -> Result<(), &'static str> {
        let endpoint_id = target.endpoint_id.clone();
        self.set_browser_targets(&endpoint_id, vec![target])
    }

    /// Replace the COMPLETE set of serving registrations owned by one endpoint,
    /// atomically (browser-auto-serve-eligible-set).
    ///
    /// One browser endpoint may serve SEVERAL (deployment, function) pairs — a
    /// donor is admitted for every browser-eligible function its tenant owns,
    /// not one hand-picked target — but it still owns exactly ONE registration
    /// per function key, and a renewal replaces the whole set under a single
    /// write lock. That is what keeps the original invariant intact: a target
    /// dropped from the set (redeploy rotated its digest, deployment deleted,
    /// tenant/scope moved) is unreachable the instant the new set lands, never
    /// left behind as a stale sibling.
    ///
    /// Every member is validated independently and identically to the
    /// single-target path — empty tenant/deployment/function, a non-64-hex
    /// endpoint id or digest, or a member naming a DIFFERENT endpoint than the
    /// one being replaced all reject the whole call without mutating anything.
    pub fn set_browser_targets(
        &self,
        endpoint_id: &str,
        targets: Vec<BrowserTarget>,
    ) -> Result<(), &'static str> {
        if endpoint_id.len() != 64
            || !endpoint_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid browser serving endpoint id");
        }
        if targets.len() > MAX_BROWSER_TARGETS_PER_ENDPOINT {
            return Err("too many browser serving targets for one endpoint");
        }
        let mut validated: Vec<(String, BrowserTarget)> = Vec::with_capacity(targets.len());
        for target in targets {
            if target.tenant.trim().is_empty()
                || target.deployment.trim().is_empty()
                || target.function.trim().is_empty()
                || target.endpoint_id != endpoint_id
                || target.digest.len() != 64
                || !target
                    .digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("invalid browser serving target");
            }
            let key = func_key(&target.deployment, &target.function);
            // One registration per function key per endpoint: a duplicated
            // pair in the incoming set is caller confusion, never two routes.
            if validated.iter().any(|(existing, _)| *existing == key) {
                return Err("duplicate browser serving target for one function");
            }
            validated.push((key, target));
        }
        let mut browser = self.browser.write();
        for existing in browser.by_function.values_mut() {
            existing.retain(|old| old.endpoint_id != endpoint_id);
        }
        browser.by_function.retain(|_, targets| !targets.is_empty());
        for (key, target) in validated {
            let targets = browser.by_function.entry(key).or_default();
            targets.push(target);
            targets.sort_by(|a, b| (&a.endpoint_id, &a.digest).cmp(&(&b.endpoint_id, &b.digest)));
        }
        Ok(())
    }

    pub fn remove_browser_endpoint(&self, endpoint_id: &str) -> usize {
        let mut browser = self.browser.write();
        let mut removed = 0usize;
        for targets in browser.by_function.values_mut() {
            let before = targets.len();
            targets.retain(|target| target.endpoint_id != endpoint_id);
            removed += before - targets.len();
        }
        browser.by_function.retain(|_, targets| !targets.is_empty());
        browser
            .circuit_until
            .retain(|key, _| !key.starts_with(endpoint_id));
        browser.invoke_quota.remove(endpoint_id);
        removed
    }

    pub fn browser_targets(&self) -> Vec<BrowserTarget> {
        let browser = self.browser.read();
        let mut out: Vec<_> = browser
            .by_function
            .values()
            .flat_map(|targets| targets.iter().cloned())
            .collect();
        out.sort_by(|a, b| {
            (
                &a.tenant,
                &a.deployment,
                &a.function,
                &a.endpoint_id,
                &a.digest,
            )
                .cmp(&(
                    &b.tenant,
                    &b.deployment,
                    &b.function,
                    &b.endpoint_id,
                    &b.digest,
                ))
        });
        out
    }

    /// Get the live tunnel for an instance, opening one if needed. Creation is
    /// serialized under the async lock so concurrent first-requests to the same
    /// instance share ONE tunnel (no orphan connections).
    async fn tunnel_for(
        &self,
        cell: &CellId,
        ep: &hive_backend::CellEndpoint,
    ) -> anyhow::Result<(Arc<TunnelClient>, bool)> {
        let mut map = self.tunnels.lock().await;
        if let Some(c) = map.get(cell) {
            if !c.is_closed() {
                self.tunnels_reused.fetch_add(1, Ordering::Relaxed);
                return Ok((c.clone(), true));
            }
        }
        let stream = connect_endpoint(ep).await?;
        let client = Arc::new(TunnelClient::new(stream));
        map.insert(cell.clone(), client.clone());
        self.tunnels_opened.fetch_add(1, Ordering::Relaxed);
        Ok((client, false))
    }

    async fn drop_tunnel(&self, cell: &CellId) {
        self.tunnels.lock().await.remove(cell);
    }

    /// Register a deployment: wire its functions into the Fluid pool and make it
    /// routable. Becomes the default (most-recent) deployment.
    pub fn deploy(&self, root: String, manifest: Manifest) -> DeploymentInfo {
        self.deploy_full(
            root,
            manifest,
            "you".into(),
            None,
            true,
            fluid_core::DeployState::Ready,
            String::new(),
        )
    }

    /// Name of the active isolation backend ("mock" | "firecracker").
    pub fn backend_name(&self) -> &'static str {
        self.fluid.backend_name()
    }

    /// Derive the host static root and function runtime workdir together. Callers
    /// must retain both; an isolated guest path must never replace the host root.
    pub fn runtime_artifact_paths(
        &self,
        artifact: &RuntimeArtifactSpec,
    ) -> anyhow::Result<RuntimeArtifactPaths> {
        self.fluid.runtime_artifact_paths(artifact)
    }

    /// Legacy guest-path query retained for callers that have not yet adopted
    /// [`Gateway::runtime_artifact_paths`].
    pub fn delivered_workdir(
        &self,
        artifact: &RuntimeArtifactSpec,
    ) -> anyhow::Result<Option<String>> {
        self.fluid.delivered_workdir(artifact)
    }

    /// Pack a built deployment's output so the serving cells can reach it (only
    /// meaningful for an isolated backend; a no-op for the same-host mock).
    pub async fn deliver_build(
        &self,
        image: &str,
        artifact: &RuntimeArtifactSpec,
    ) -> anyhow::Result<()> {
        self.fluid.deliver_build(image, artifact).await
    }

    /// Full deploy for callers that do not already hold a paired runtime-artifact
    /// descriptor. Reuse the host path only when the active backend proves that it
    /// executes on the same host; isolated backends retain static serving but never
    /// guess a guest cwd from a host path.
    #[allow(clippy::too_many_arguments)]
    pub fn deploy_full(
        &self,
        root: String,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
        state: fluid_core::DeployState,
        tenant: String,
    ) -> DeploymentInfo {
        let runtime_workdir = self
            .legacy_same_host_workdir(&root)
            .map(|path| path.to_string_lossy().into_owned());
        self.deploy_full_with_runtime(
            root,
            runtime_workdir,
            manifest,
            creator,
            git,
            production,
            state,
            tenant,
        )
    }

    /// Register one same-host deployment under exact project-incarnation
    /// authority. This is the direct-deploy twin of
    /// [`Self::deploy_full_with_runtime_exact`].
    #[allow(clippy::too_many_arguments)]
    pub fn deploy_full_exact(
        &self,
        root: String,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
        state: fluid_core::DeployState,
        tenant: String,
        project_incarnation: ProjectIncarnation,
    ) -> DeploymentInfo {
        let runtime_workdir = self
            .legacy_same_host_workdir(&root)
            .map(|path| path.to_string_lossy().into_owned());
        self.deploy_full_with_runtime_exact(
            root,
            runtime_workdir,
            manifest,
            creator,
            git,
            production,
            state,
            tenant,
            project_incarnation,
        )
    }

    /// Register one deployment with distinct host-static and function-runtime
    /// locations. `host_static_root` is pinned for the deployment lifetime;
    /// `runtime_workdir` is passed only to function pools and persisted for restore.
    /// `None` is static-only and never authorizes an isolated backend to infer a
    /// guest path from the host path.
    #[allow(clippy::too_many_arguments)]
    pub fn deploy_full_with_runtime(
        &self,
        host_static_root: String,
        runtime_workdir: Option<String>,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
        state: fluid_core::DeployState,
        tenant: String,
    ) -> DeploymentInfo {
        self.deploy_full_with_runtime_incarnation(
            host_static_root,
            runtime_workdir,
            manifest,
            creator,
            git,
            production,
            state,
            tenant,
            None,
            None,
        )
    }

    /// Register a deployment owned by one server-issued project incarnation.
    /// Production demotion and alias movement are scoped to the same identity,
    /// so a delayed record from a deleted incarnation cannot become current.
    #[allow(clippy::too_many_arguments)]
    pub fn deploy_full_with_runtime_exact(
        &self,
        host_static_root: String,
        runtime_workdir: Option<String>,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
        state: fluid_core::DeployState,
        tenant: String,
        project_incarnation: ProjectIncarnation,
    ) -> DeploymentInfo {
        self.deploy_full_with_runtime_exact_marketplace(
            host_static_root,
            runtime_workdir,
            manifest,
            creator,
            git,
            production,
            state,
            tenant,
            project_incarnation,
            None,
        )
    }

    /// Exact-incarnation deployment registration with a previously validated,
    /// immutable Marketplace placement snapshot. Only the server-side DevHub
    /// Marketplace consumer supplies this; callers cannot mutate it after this
    /// point because it is copied directly into the deployment record.
    #[allow(clippy::too_many_arguments)]
    pub fn deploy_full_with_runtime_exact_marketplace(
        &self,
        host_static_root: String,
        runtime_workdir: Option<String>,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
        state: fluid_core::DeployState,
        tenant: String,
        project_incarnation: ProjectIncarnation,
        marketplace_placement: Option<fluid_core::MarketplacePlacementSnapshot>,
    ) -> DeploymentInfo {
        self.deploy_full_with_runtime_incarnation(
            host_static_root,
            runtime_workdir,
            manifest,
            creator,
            git,
            production,
            state,
            tenant,
            Some(project_incarnation),
            marketplace_placement,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn deploy_full_with_runtime_incarnation(
        &self,
        host_static_root: String,
        runtime_workdir: Option<String>,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
        mut state: fluid_core::DeployState,
        tenant: String,
        project_incarnation: Option<ProjectIncarnation>,
        marketplace_placement: Option<fluid_core::MarketplacePlacementSnapshot>,
    ) -> DeploymentInfo {
        // Normalize the owner once at the boundary so the stored record, the
        // function pools, and every cell agree on the tenant (empty => "personal").
        let tenant = if tenant.trim().is_empty() {
            "personal".to_string()
        } else {
            tenant
        };
        let id = DeploymentId::new();
        let cell_image = manifest.image.clone().unwrap_or_else(|| self.image.clone());
        let build_output_v3 = build_output_v3_route_state(&manifest);
        let build_output_refused =
            matches!(&build_output_v3, Some(BuildOutputV3RouteState::Refused(_)));
        if let Some(BuildOutputV3RouteState::Refused(ref refusal)) = build_output_v3 {
            warn!(
                deployment = %id,
                code = refusal.code(),
                error = %refusal,
                "Build Output v3 provisioning refused"
            );
            state = fluid_core::DeployState::Error;
        }
        if !build_output_refused {
            if let Some(function_workdir) = runtime_workdir.as_ref() {
                for f in &manifest.functions {
                    let key = func_key(id.as_str(), &f.name);
                    self.fluid.register(
                        key,
                        f.clone(),
                        cell_image.clone(),
                        function_launch_workdir(function_workdir, f),
                        tenant.clone(),
                    );
                }
            } else if !manifest.functions.is_empty() {
                warn!(
                    deployment = %id,
                    "function registration refused: isolated backend has no server-derived runtime workdir"
                );
                state = fluid_core::DeployState::Error;
            }
        }
        let dep = Deployment {
            id: id.clone(),
            project: manifest.project.clone(),
            project_incarnation,
            root: PathBuf::from(host_static_root),
            runtime_workdir: runtime_workdir.map(PathBuf::from),
            manifest: manifest.clone(),
            created_at_ms: now_ms(),
            state,
            creator,
            git,
            production,
            // The build target is immutable; `production` (promoted) may later flip.
            target: if production {
                "production".into()
            } else {
                "preview".into()
            },
            tenant,
            marketplace_placement,
        };
        let info = view_of(&dep);
        // A typed Build Contract refusal (including one caught HERE, at deploy
        // time, by a re-check the settings/vercel.json overlay pipeline can
        // invalidate after `build_output_manifest` already validated the
        // manifest once) must register NEITHER a Ready NOR an Error deployment
        // record, and must never claim the project's production alias — a
        // refused build leaves the CURRENT production deployment (if any)
        // exactly as it was. `info` above already carries `state: Error` for
        // the caller's own logging; nothing below this point may be persisted.
        if build_output_refused {
            return info;
        }
        let project = dep.project.clone();
        let mut st = self.state.lock();
        if let Ok(root) = static_files::StaticRoot::open(&dep.root) {
            st.static_roots.insert(id.clone(), root);
        }
        if let Some(routes) = build_output_v3 {
            st.build_output_v3.insert(id.clone(), routes);
        }
        // Does this project already have a (different) production deployment? If
        // not, this deploy claims the bare production domain even when it isn't
        // itself a production deploy — so the very first deploy and the "Building…"
        // placeholder are reachable at <project>.<host> right away.
        let has_production = st.deployments.values().any(|d| {
            d.project == project
                && d.production
                && d.id != id
                && project_incarnation
                    .is_none_or(|incarnation| d.project_incarnation == Some(incarnation))
        });
        // Invariant: at most ONE production deployment per project. Promoting this
        // one to production demotes any prior production deployment of the same
        // project, so the production alias can't later resolve to a stale deployment
        // after a restart (which would serve the OLD build).
        if production {
            for d in st.deployments.values_mut() {
                if d.project == project
                    && project_incarnation
                        .is_none_or(|incarnation| d.project_incarnation == Some(incarnation))
                {
                    d.production = false;
                }
            }
        }
        st.deployments.insert(id.clone(), dep);
        // Vercel's 3 URL types: the immutable per-deployment + commit URLs and the
        // mutable branch URL (latest on that branch).
        insert_deploy_aliases(&mut st, &id);
        // Production domain (<project>) + default fallback move only on a production
        // deploy — a preview deploy of an existing project must NOT hijack prod.
        // Exception: the project's first-ever deploy claims it so the URL resolves.
        if production || !has_production {
            // Custom domains follow the project's production deployment —
            // their whole point is to be the production URL, so a redeploy or
            // promote must not strand them on the superseded build. The old
            // production id is simply the project alias's previous target.
            let old_production = st.aliases.get(&project).cloned().filter(|old| {
                project_incarnation.is_none_or(|incarnation| {
                    st.deployments.get(old).is_some_and(|deployment| {
                        deployment.project_incarnation == Some(incarnation)
                    })
                })
            });
            st.aliases.insert(project, id.clone());
            st.default = Some(id.clone());
            if let Some(old) = old_production {
                if old != id {
                    for v in st.aliases_full.values_mut() {
                        if *v == old {
                            *v = id.clone();
                        }
                    }
                }
            }
        }
        info
    }

    /// Re-stamp a deployment's creation time — the causal-stamp seam for
    /// project-delete generations: a deployment registered over a fresh
    /// tombstone must dominate it (`remove_project_through` sweeps
    /// `created_at_ms <= deleted_ms`), so hive-cloud floors the stamp at
    /// tombstone+1 right after registration. Gossiped copies carry the same
    /// value, so every node classifies the record identically.
    pub fn restamp_created(&self, id: &str, created_at_ms: u64) {
        let mut st = self.state.lock();
        if let Some(d) = st.deployments.get_mut(&DeploymentId::from(id.to_string())) {
            d.created_at_ms = created_at_ms;
        }
    }

    /// Resolve which project serves a given request host (the same way the
    /// public router selects), so events can be attributed to a project.
    pub fn project_for_host(&self, host: &str) -> Option<String> {
        self.select(Some(host)).map(|d| d.project.clone())
    }

    /// The full deployment a request `host` resolves to (same alias resolution the
    /// public router uses). Exposes `target`/`production` so the preview gate can
    /// decide protection by the deployment's ACTUAL environment — not by guessing
    /// from the subdomain (which wrongly flags a production deployment's commit/id
    /// URLs as previews).
    pub fn deployment_for_host(&self, host: &str) -> Option<DeploymentInfo> {
        self.select(Some(host)).map(|d| view_of(&d))
    }

    /// Attach a custom domain to a project. The FULL lowercased hostname is
    /// keyed to the project's current PRODUCTION deployment (never its first
    /// label — label keying would also serve every root sharing that label).
    /// Returns true if a local production deployment exists.
    ///
    /// A custom domain is the project's production URL: pinning it to the
    /// bare project alias would serve a first-ever PREVIEW deploy as
    /// production (a preview claims the project alias when no production
    /// exists yet) and never move — the follow logic only tracks the
    /// production alias target. No local production deployment = false, so
    /// activation forwards to a node that really hosts one (adversarial
    /// finding).
    pub fn add_alias(&self, domain: &str, project: &str) -> bool {
        let host = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let mut st = self.state.lock();
        let target = st
            .deployments
            .values()
            .find(|d| d.project == project && d.production)
            .map(|d| d.id.clone());
        if let Some(id) = target {
            st.aliases_full.insert(host, id);
            true
        } else {
            false
        }
    }

    /// Detach a custom domain from the resolution map (idempotent). The
    /// LABEL map is deliberately untouched: it holds only platform aliases
    /// (project names, per-deploy/commit/branch labels) and pre-gate
    /// grandfathered attaches, none of which belong to this full-host entry —
    /// dropping any of them would sever unrelated routing (adversarial
    /// finding: an attach/detach pair on `numo.evil.com` wiped a victim's
    /// grandfathered `numo` label). Grandfathered labels already die with
    /// their deployment via `remove()`'s retain.
    pub fn remove_alias(&self, domain: &str) {
        let host = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let mut st = self.state.lock();
        st.aliases_full.remove(&host);
    }

    /// Promote an existing deployment to be its project's production (rollback /
    /// instant promote). Re-points the project alias + default to it.
    pub fn promote(&self, id: &str) -> Option<DeploymentInfo> {
        self.promote_incarnation(id, None)
    }

    /// Update the project→deployment alias WITHOUT requiring the deployment
    /// record to exist in this node's local gateway state. Used when a promote
    /// was proxied to the host node — the host already owns the record, and this
    /// node only needs the alias so mesh routing can forward project-hostname
    /// requests to the host. Idempotent: calling it on a node that already has
    /// the deployment record is harmless (the alias is already correct).
    pub fn set_project_alias(&self, project: &str, deployment_id: &str) {
        let did = DeploymentId::from(deployment_id.to_string());
        let mut st = self.state.lock();
        st.aliases.insert(project.to_string(), did);
    }

    pub fn promote_exact(&self, id: &str, expected: ProjectIncarnation) -> Option<DeploymentInfo> {
        self.promote_incarnation(id, Some(expected))
    }

    fn promote_incarnation(
        &self,
        id: &str,
        expected: Option<ProjectIncarnation>,
    ) -> Option<DeploymentInfo> {
        let did = DeploymentId::from(id.to_string());
        let mut st = self.state.lock();
        let selected = st.deployments.get(&did)?;
        if expected.is_some_and(|incarnation| selected.project_incarnation != Some(incarnation)) {
            return None;
        }
        let project = selected.project.clone();
        let incarnation = selected.project_incarnation;
        // Flip production flags only within the selected incarnation.
        for d in st.deployments.values_mut() {
            if d.project == project && expected.is_none_or(|_| d.project_incarnation == incarnation)
            {
                d.production = d.id == did;
            }
        }
        let old_production = st.aliases.get(&project).cloned().filter(|old| {
            expected.is_none_or(|_| {
                st.deployments
                    .get(old)
                    .is_some_and(|deployment| deployment.project_incarnation == incarnation)
            })
        });
        st.aliases.insert(project, did.clone());
        st.default = Some(did.clone());
        // Custom domains follow the promote too (see deploy_full's note).
        if let Some(old) = old_production {
            if old != did {
                for v in st.aliases_full.values_mut() {
                    if *v == old {
                        *v = did.clone();
                    }
                }
            }
        }
        st.deployments.get(&did).map(view_of)
    }

    /// Mutate an existing deployment's manifest IN PLACE (same record id — no
    /// new deployment, no rebuild) and sync every function's updated config
    /// into the Fluid pool so FUTURE instance launches see it (running
    /// instances keep their launch-time shape until recycled). The
    /// settings-edit hook behind hive-cloud's `PUT /v1/projects/:project/network`
    /// (exposing raw TCP/UDP ports without a redeploy). Pool sync goes through
    /// [`Fluid::update_config`] — never `register`, which would replace the
    /// whole pool and orphan its live instances. Lock order (state, then fluid
    /// registry) matches `reconcile_keepwarm`. Returns the updated view, or
    /// `None` for an unknown id.
    pub fn update_manifest(
        &self,
        id: &str,
        mutate: impl FnOnce(&mut fluid_core::Manifest),
    ) -> Option<DeploymentInfo> {
        let did = DeploymentId::from(id.to_string());
        let mut st = self.state.lock();
        let dep = st.deployments.get_mut(&did)?;
        mutate(&mut dep.manifest);
        let build_output_v3 = build_output_v3_route_state(&dep.manifest);
        if matches!(&build_output_v3, Some(BuildOutputV3RouteState::Refused(_))) {
            dep.state = fluid_core::DeployState::Error;
        }
        for f in &dep.manifest.functions {
            self.fluid
                .update_config(&func_key(did.as_str(), &f.name), f.clone());
        }
        let info = view_of(dep);
        match build_output_v3 {
            Some(routes) => {
                st.build_output_v3.insert(did, routes);
            }
            None => {
                st.build_output_v3.remove(&did);
            }
        }
        Some(info)
    }

    /// Keep-warm reconciliation: only the PRODUCTION deployment of each project
    /// keeps its configured `min_instances` warm; every superseded (non-production)
    /// deployment is drained to zero. Without this, each redeploy left an old
    /// deployment pinning an idle warm instance (N warm microVMs per project).
    /// Idempotent — safe to call on a timer and after deploy/promote.
    pub fn reconcile_keepwarm(&self) {
        let st = self.state.lock();
        for d in st.deployments.values() {
            for f in &d.manifest.functions {
                let key = func_key(d.id.as_str(), &f.name);
                let n = if d.production { f.min_instances } else { 0 };
                self.fluid.set_min_instances(&key, n);
            }
        }
    }

    /// Delete a single deployment: unregister its functions and drop it. Returns
    /// the project it belonged to (so callers can persist / re-point).
    pub async fn remove(&self, id: &str) -> Option<String> {
        let did = DeploymentId::from(id.to_string());
        let (project, keys) = {
            let st = self.state.lock();
            let dep = st.deployments.get(&did)?;
            let keys: Vec<String> = dep
                .manifest
                .functions
                .iter()
                .map(|f| func_key(did.as_str(), &f.name))
                .collect();
            (dep.project.clone(), keys)
        };
        for k in keys {
            self.fluid.unregister(&k).await;
        }
        let mut st = self.state.lock();
        st.deployments.remove(&did);
        st.static_roots.remove(&did);
        st.build_output_v3.remove(&did);
        // Drop any aliases that pointed at this deployment.
        st.aliases.retain(|_, v| *v != did);
        st.aliases_full.retain(|_, v| *v != did);
        if st.default.as_ref() == Some(&did) {
            st.default = st
                .deployments
                .values()
                .max_by_key(|d| d.created_at_ms)
                .map(|d| d.id.clone());
        }
        // Re-point the project alias to its newest remaining deployment.
        if let Some(newest) = st
            .deployments
            .values()
            .filter(|d| d.project == project)
            .max_by_key(|d| d.created_at_ms)
            .map(|d| d.id.clone())
        {
            st.aliases.insert(project.clone(), newest);
        }
        Some(project)
    }

    /// Stamp only legacy records whose exact platform-issued ids were captured
    /// while an existing settings row was upgraded under the lifecycle writer.
    /// Names and timestamps are deliberately insufficient: either can overlap a
    /// deleted predecessor or a prefix-sharing tenant.
    pub fn adopt_legacy_deployments(
        &self,
        project: &str,
        incarnation: ProjectIncarnation,
        allowed_ids: &[String],
    ) -> Vec<String> {
        if allowed_ids.is_empty() {
            return Vec::new();
        }
        let allowed: std::collections::HashSet<&str> =
            allowed_ids.iter().map(String::as_str).collect();
        let mut adopted = Vec::new();
        let mut st = self.state.lock();
        for deployment in st.deployments.values_mut() {
            if deployment.project == project
                && deployment.project_incarnation.is_none()
                && allowed.contains(deployment.id.as_str())
            {
                deployment.project_incarnation = Some(incarnation);
                adopted.push(deployment.id.to_string());
            }
        }
        adopted.sort();
        adopted
    }

    /// Delete one deployment only when it still belongs to `expected`. The
    /// identity is re-checked after async pool teardown, before any gateway
    /// mutation, so a stale request cannot remove a same-id replacement.
    pub async fn remove_exact(
        &self,
        id: &str,
        expected: ProjectIncarnation,
    ) -> Option<fluid_core::DeployRecord> {
        let did = DeploymentId::from(id.to_string());
        let record = self
            .deployment_records()
            .into_iter()
            .find(|record| record.id == id && record.project_incarnation == Some(expected))?;
        let keys: Vec<String> = record
            .manifest
            .functions
            .iter()
            .map(|function| func_key(&record.id, &function.name))
            .collect();
        for key in keys {
            self.fluid.unregister(&key).await;
        }
        let mut st = self.state.lock();
        let still_owned = st
            .deployments
            .get(&did)
            .is_some_and(|deployment| deployment.project_incarnation == Some(expected));
        if !still_owned {
            return None;
        }
        st.deployments.remove(&did);
        st.static_roots.remove(&did);
        st.build_output_v3.remove(&did);
        st.aliases.retain(|_, target| *target != did);
        st.aliases_full.retain(|_, target| *target != did);
        if st.default.as_ref() == Some(&did) {
            st.default = st
                .deployments
                .values()
                .max_by_key(|deployment| deployment.created_at_ms)
                .map(|deployment| deployment.id.clone());
        }
        if let Some(newest) = st
            .deployments
            .values()
            .filter(|deployment| {
                deployment.project == record.project
                    && deployment.project_incarnation == Some(expected)
            })
            .max_by_key(|deployment| deployment.created_at_ms)
            .map(|deployment| deployment.id.clone())
        {
            st.aliases.insert(record.project.clone(), newest);
        }
        Some(record)
    }

    /// Snapshot the records proved to belong to one project incarnation.
    pub fn deployment_records_for_incarnation(
        &self,
        project: &str,
        incarnation: ProjectIncarnation,
    ) -> Vec<fluid_core::DeployRecord> {
        self.deployment_records()
            .into_iter()
            .filter(|record| {
                record.project == project && record.project_incarnation == Some(incarnation)
            })
            .collect()
    }

    /// Remove only deployments owned by `incarnation`, then clear every route
    /// still pointing at this project. The caller holds the project lifecycle
    /// writer and has proved this is the active incarnation, so a legacy record
    /// may be retained for safety but can never remain reachable by accident.
    pub async fn remove_project_incarnation(
        &self,
        project: &str,
        incarnation: ProjectIncarnation,
    ) -> Vec<fluid_core::DeployRecord> {
        let records = self.deployment_records_for_incarnation(project, incarnation);
        for record in &records {
            self.remove_exact(&record.id, incarnation).await;
        }
        let mut st = self.state.lock();
        let project_ids: std::collections::HashSet<DeploymentId> = st
            .deployments
            .values()
            .filter(|deployment| deployment.project == project)
            .map(|deployment| deployment.id.clone())
            .collect();
        st.aliases
            .retain(|label, target| label != project && !project_ids.contains(target));
        st.aliases_full
            .retain(|_, target| !project_ids.contains(target));
        if st
            .default
            .as_ref()
            .is_some_and(|target| project_ids.contains(target))
        {
            st.default = st
                .deployments
                .values()
                .filter(|deployment| deployment.project != project)
                .max_by_key(|deployment| deployment.created_at_ms)
                .map(|deployment| deployment.id.clone());
        }
        records
    }

    pub fn image_is_referenced(&self, image: &str) -> bool {
        self.state
            .lock()
            .deployments
            .values()
            .any(|deployment| deployment.manifest.image.as_deref() == Some(image))
    }

    /// Delete every deployment for a project. Returns the removed deployment ids.
    /// Relocation-scoped removal: drop this node's PRODUCTION-lane deployment
    /// records for a project (target != "preview"; empty target = pre-target
    /// legacy = production lane) and KEEP every preview. The relocation reaper
    /// runs after every promotable production build against every non-target
    /// node — using the full [`Registry::remove_project`] there destroyed
    /// preview deployments hosted on nodes the new production placement did
    /// not pick (preview URL 404, row gone from the table), and vice versa.
    /// A preview is not a "stale copy" of anything: it was placed where it
    /// was placed, and only a user delete or preview-retention policy may
    /// remove it.
    pub async fn remove_project_superseded(&self, project: &str) -> Vec<String> {
        let ids: Vec<String> = {
            let st = self.state.lock();
            st.deployments
                .values()
                .filter(|d| d.project == project && d.target != "preview")
                .map(|d| d.id.to_string())
                .collect()
        };
        for id in &ids {
            self.remove(id).await;
        }
        ids
    }

    /// Delete every deployment for a project whose creation is not newer than
    /// `deleted_ms`. A causal recreation gets a strictly newer creation stamp
    /// and survives an old deletion generation, including a retry racing that
    /// redeploy. Returns the removed deployment ids.
    pub async fn remove_project_through(&self, project: &str, deleted_ms: u64) -> Vec<String> {
        let ids: Vec<String> = {
            let st = self.state.lock();
            st.deployments
                .values()
                .filter(|d| d.project == project && d.created_at_ms <= deleted_ms)
                .map(|d| d.id.to_string())
                .collect()
        };
        for id in &ids {
            self.remove(id).await;
        }
        ids
    }

    pub async fn remove_project(&self, project: &str) -> Vec<String> {
        self.remove_project_through(project, u64::MAX).await
    }

    /// The git source of a project's newest deployment (for "redeploy"), skipping
    /// any deployment whose `git` is a synthetic `upload://`/`image://` pseudo-
    /// source rather than a real git remote (see `GitSource::is_real_git`) — a
    /// zip-upload or prebuilt-image "New Deployment" becoming the project's
    /// newest record must not shadow its actual git repo for callers matching
    /// future GitHub pushes.
    pub fn git_for_project(&self, project: &str) -> Option<fluid_core::GitSource> {
        let st = self.state.lock();
        st.deployments
            .values()
            .filter(|d| d.project == project)
            .filter(|d| d.git.as_ref().is_some_and(|g| g.is_real_git()))
            .max_by_key(|d| d.created_at_ms)
            .and_then(|d| d.git.clone())
    }

    pub fn list(&self) -> Vec<DeploymentInfo> {
        let st = self.state.lock();
        let mut out: Vec<DeploymentInfo> = st.deployments.values().map(view_of).collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.created_at_ms));
        out
    }

    /// Serializable snapshot of all deployments (for persistence).
    pub fn deployment_records(&self) -> Vec<fluid_core::DeployRecord> {
        let st = self.state.lock();
        st.deployments
            .values()
            .map(|d| fluid_core::DeployRecord {
                id: d.id.to_string(),
                project: d.project.clone(),
                project_incarnation: d.project_incarnation,
                root: d.root.to_string_lossy().into_owned(),
                runtime_workdir: d
                    .runtime_workdir
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                manifest: d.manifest.clone(),
                created_at_ms: d.created_at_ms,
                creator: d.creator.clone(),
                git: d.git.clone(),
                production: d.production,
                target: d.target.clone(),
                state: d.state,
                tenant: d.tenant.clone(),
                marketplace_placement: d.marketplace_placement.clone(),
            })
            .collect()
    }

    /// Restore a deployment from a persisted record (preserves its id), and
    /// re-register its functions with the Fluid pool. Used on boot.
    pub fn restore(&self, rec: fluid_core::DeployRecord) {
        let id = DeploymentId::from(rec.id.clone());
        let runtime_workdir = rec
            .runtime_workdir
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| self.legacy_same_host_workdir(&rec.root));
        let restored_tenant = if rec.tenant.trim().is_empty() {
            "__untagged__".to_string()
        } else {
            rec.tenant.clone()
        };
        let manifest = rec.manifest;
        let cell_image = manifest.image.clone().unwrap_or_else(|| self.image.clone());
        let build_output_v3 = build_output_v3_route_state(&manifest);
        let build_output_refused =
            matches!(&build_output_v3, Some(BuildOutputV3RouteState::Refused(_)));
        let mut restored_state = rec.state;
        if let Some(BuildOutputV3RouteState::Refused(ref refusal)) = build_output_v3 {
            warn!(
                deployment = %id,
                code = refusal.code(),
                error = %refusal,
                "restored Build Output v3 provisioning refused"
            );
            restored_state = fluid_core::DeployState::Error;
        }
        if !build_output_refused {
            if let Some(workdir) = runtime_workdir.as_ref() {
                for f in &manifest.functions {
                    let key = func_key(id.as_str(), &f.name);
                    let workdir_str = workdir.to_string_lossy().into_owned();
                    self.fluid.register(
                        key,
                        f.clone(),
                        cell_image.clone(),
                        function_launch_workdir(&workdir_str, f),
                        restored_tenant.clone(),
                    );
                }
            } else if !manifest.functions.is_empty() {
                warn!(
                    deployment = %id,
                    "restore refused function registration: legacy record has no runtime workdir and this backend requires delivered artifacts"
                );
                if manifest.build_output_v3.is_some() {
                    restored_state = fluid_core::DeployState::Error;
                }
            }
        }
        let dep = Deployment {
            id: id.clone(),
            project: rec.project.clone(),
            project_incarnation: rec.project_incarnation,
            root: PathBuf::from(&rec.root),
            runtime_workdir,
            manifest,
            created_at_ms: rec.created_at_ms,
            state: restored_state,
            creator: rec.creator,
            git: rec.git,
            production: rec.production,
            // Old snapshots have no target — derive it from the production flag.
            target: if rec.target.is_empty() {
                if rec.production {
                    "production".into()
                } else {
                    "preview".into()
                }
            } else {
                rec.target
            },
            // Unlike `deploy_full`'s own empty=>"personal" default (a deliberate,
            // generic single-tenant convenience for callers that don't use
            // tenancy at all), a RESTORED record's empty tag is tag LOSS — a
            // pre-tenancy snapshot, or one written by a stale/rolling-upgrade
            // binary. Fail closed: never adopt it into a live tenant.
            tenant: restored_tenant,
            marketplace_placement: rec.marketplace_placement,
        };
        let project = dep.project.clone();
        let static_root = static_files::StaticRoot::open(&dep.root).ok();
        let mut st = self.state.lock();
        if let Some(root) = static_root {
            st.static_roots.insert(id.clone(), root);
        }
        if let Some(routes) = build_output_v3 {
            st.build_output_v3.insert(id.clone(), routes);
        }
        st.deployments.insert(id.clone(), dep);
        set_alias_if_newer(&mut st, &project, &id);
        insert_deploy_aliases(&mut st, &id);
        st.default.get_or_insert(id);
    }

    fn legacy_same_host_workdir(&self, root: &str) -> Option<PathBuf> {
        let artifact = RuntimeArtifactSpec::new(root, ".");
        let paths = self.fluid.runtime_artifact_paths(&artifact).ok()?;
        (!paths.delivery_required && paths.guest_workdir == paths.host_static_root)
            .then_some(paths.guest_workdir)
    }

    /// Pick the deployment for a request: `<project>.<host>` subdomain, else the
    /// most recent deployment.
    /// Resolve the function a request `path` routes to for the deployment
    /// served by `host` — the SAME resolution `handle_public`/`proxy_function`
    /// use for an ordinary request — and lease its instance. Exposed
    /// (`select` and the lease pool are otherwise private to this crate) for
    /// hive-cloud's edge layer to splice a raw WebSocket-upgrade connection
    /// directly into a LOCALLY-hosted instance, instead of replaying the
    /// upgrade back through this same node's own public router over the mesh
    /// (which would needlessly self-dial and re-evaluate routing a second
    /// time). Returns `None` for no matching deployment, a Static route (no
    /// function to upgrade to), or a lease failure (cold-start/capacity) —
    /// the caller falls back to its normal mesh path on a miss.
    pub async fn lease_for_path(&self, host: Option<&str>, path: &str) -> Option<Lease> {
        let dep = self.select(host)?;
        let name = match dep.build_output_v3.as_ref() {
            Some(BuildOutputV3RouteState::Ready(evaluator)) => {
                match evaluator.resolve("GET", path).ok()?.target? {
                    BuildOutputV3Target::Function { name } => name,
                    _ => return None,
                }
            }
            Some(BuildOutputV3RouteState::Refused(_)) => return None,
            None if dep.manifest.build_output_v3.is_some() => return None,
            None => match dep.manifest.resolve(path) {
                RouteTarget::Function(name) => name,
                RouteTarget::Static => return None,
            },
        };
        let key = func_key(dep.id.as_str(), &name);
        self.fluid.lease(&key).await.ok()
    }

    fn select(&self, host: Option<&str>) -> Option<SelectedDeployment> {
        let st = self.state.lock();
        let selected = |id: &DeploymentId| {
            st.deployments
                .get(id)
                .cloned()
                .map(|deployment| SelectedDeployment {
                    static_root: st.static_roots.get(id).cloned(),
                    build_output_v3: st.build_output_v3.get(id).cloned(),
                    deployment,
                })
        };
        if let Some(h) = host {
            let h = h.split(':').next().unwrap_or(h); // strip port
            if let Some(id) = st.aliases_full.get(&h.to_ascii_lowercase()) {
                return selected(id);
            }
            let sub = h.split('.').next().unwrap_or(h);
            if let Some(id) = st.aliases.get(sub) {
                return selected(id);
            }
        }
        st.default.as_ref().and_then(selected)
    }

    /// The deployment id a request `host` resolves to (its subdomain alias), if
    /// any. Exposes the same alias resolution `select` uses — handy for debugging
    /// and for asserting which deployment a project host points at.
    pub fn host_deployment_id(&self, host: &str) -> Option<String> {
        let h = host.split(':').next().unwrap_or(host);
        let st = self.state.lock();
        if let Some(id) = st.aliases_full.get(&h.to_ascii_lowercase()) {
            return Some(id.as_str().to_string());
        }
        let sub = h.split('.').next().unwrap_or(h);
        st.aliases.get(sub).map(|id| id.as_str().to_string())
    }

    /// Does this node's full-host alias for `domain` resolve to a deployment
    /// of `project`? The detach rule: an alias must die when it names a
    /// deployment of the DETACHED project (the rebind case — a domain moved
    /// to a different project otherwise leaves the previous project's binding
    /// live forever, serving the OLD version on every node that held it),
    /// never when it names a deployment of a project still attached (the
    /// two-projects-one-domain case, where the live attachment must survive).
    pub fn alias_points_at_project(&self, domain: &str, project: &str) -> bool {
        let h = domain
            .split(':')
            .next()
            .unwrap_or(domain)
            .to_ascii_lowercase();
        let st = self.state.lock();
        st.aliases_full
            .get(&h)
            .and_then(|id| st.deployments.get(id))
            .is_some_and(|d| d.project == project)
    }

    /// EXACT host attribution for event/log tagging: the `(deployment id,
    /// project)` the host's subdomain alias actually names — with NO
    /// default-deployment fallback. `select`'s fallback is correct for SERVING
    /// (an unmatched host still gets an answer) but wrong for ATTRIBUTION: it
    /// stamps every unmatched host (bot probes on the platform apex, other
    /// tenants' DB hosts, peer-hosted projects routed through this node) with
    /// whatever project happens to be this node's default deployment — which is
    /// how foreign requests leaked into that project's log view. Unresolved
    /// hosts return `None` and must be recorded UNATTRIBUTED.
    pub fn attribution_for_host(&self, host: &str) -> Option<(String, String)> {
        let h = host.split(':').next().unwrap_or(host);
        let st = self.state.lock();
        let id = st
            .aliases_full
            .get(&h.to_ascii_lowercase())
            .or_else(|| st.aliases.get(h.split('.').next().unwrap_or(h)))?
            .clone();
        let project = st.deployments.get(&id)?.project.clone();
        Some((id.as_str().to_string(), project))
    }

    /// Does THIS node actually have a deployment aliased for `host`'s subdomain?
    /// Exact alias match (no default fallback) — used by mesh routing to decide
    /// whether to serve locally or proxy to the peer that really hosts it.
    pub fn serves_host(&self, host: &str) -> bool {
        let h = host.split(':').next().unwrap_or(host);
        let st = self.state.lock();
        if st.aliases_full.contains_key(&h.to_ascii_lowercase()) {
            return true;
        }
        let sub = h.split('.').next().unwrap_or(h);
        st.aliases.contains_key(sub)
    }

    /// The state of the deployment this host's subdomain alias EXACTLY names —
    /// no default-deployment fallback, and `None` for both "no alias here" and a
    /// DANGLING alias (one whose deployment record is gone).
    ///
    /// `serves_host` answers "is there an alias", which is not the same question
    /// as "can this node serve it": an orphaned `Building…` placeholder (its
    /// build's task died before it could be removed, then reconciled to `Error`
    /// on the next boot) keeps the project alias forever, and because the edge
    /// treated any alias as authoritative, that node served the dead placeholder
    /// locally and never proxied to the peer holding the project's READY
    /// deployment. Witnessed live on `archive-zip.shadw.app` (2026-08-05).
    pub fn host_deploy_state(&self, host: &str) -> Option<fluid_core::DeployState> {
        let h = host.split(':').next().unwrap_or(host);
        let st = self.state.lock();
        if let Some(id) = st.aliases_full.get(&h.to_ascii_lowercase()) {
            return st.deployments.get(id).map(|d| d.state);
        }
        let sub = h.split('.').next().unwrap_or(h);
        let id = st.aliases.get(sub)?;
        st.deployments.get(id).map(|d| d.state)
    }

    /// All host subdomains this node serves (project aliases + deployment ids
    /// + full custom-domain hosts), published to peers so the mesh knows where
    /// each deployment lives.
    pub fn served_hosts(&self) -> Vec<String> {
        let st = self.state.lock();
        st.aliases
            .keys()
            .cloned()
            .chain(st.aliases_full.keys().cloned())
            .collect()
    }

    /// Every alias LABEL or full host that resolves to a deployment of
    /// `project` — the project's own name, its per-commit/branch/deployment
    /// labels, and any custom domain attached to it (custom domains are keyed
    /// by FULL hostname; platform aliases by first label).
    ///
    /// Exists so a tenant-controlled surface can be scoped to exactly the
    /// hostnames it legitimately speaks for: the raw-port TLS terminator used
    /// the fleet-wide SNI resolver, which holds every platform and every other
    /// tenant's certificate.
    pub fn alias_labels_for_project(&self, project: &str) -> Vec<String> {
        let st = self.state.lock();
        let ids: std::collections::HashSet<&DeploymentId> = st
            .deployments
            .values()
            .filter(|d| d.project.eq_ignore_ascii_case(project))
            .map(|d| &d.id)
            .collect();
        st.aliases
            .iter()
            .chain(st.aliases_full.iter())
            .filter(|(_, id)| ids.contains(id))
            .map(|(label, _)| label.clone())
            .collect()
    }

    /// The subset of [`Gateway::served_hosts`] whose deployment is actually
    /// `Ready`. Anything that steers traffic to ONE node (DNS affinity records)
    /// must use this, not `served_hosts`: a specific A record beats the
    /// wildcard, so publishing a label at a node holding only a failed build or
    /// an orphaned placeholder pins every client to the one node that cannot
    /// answer. `served_hosts` itself stays state-blind — the mesh route table
    /// legitimately wants to know a node holds the label at all.
    pub fn served_hosts_ready(&self) -> Vec<String> {
        let st = self.state.lock();
        st.aliases
            .iter()
            .chain(st.aliases_full.iter())
            .filter(|(_, id)| {
                st.deployments
                    .get(*id)
                    .is_some_and(|d| d.state == fluid_core::DeployState::Ready)
            })
            .map(|(label, _)| label.clone())
            .collect()
    }

    /// Projects this node hosts that are **container** deployments (a function with
    /// the `container` runtime) — these are the stateful workloads coordinated by a
    /// single-owner lease. Functions/static sites are excluded (stateless).
    pub fn container_projects(&self) -> Vec<String> {
        let st = self.state.lock();
        let mut out: Vec<String> = st
            .deployments
            .values()
            .filter(|d| {
                d.manifest
                    .functions
                    .iter()
                    .any(|f| f.runtime == "container")
            })
            .map(|d| d.project.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Is the deployment behind `host` a container deployment?
    pub fn is_container_host(&self, host: &str) -> bool {
        let h = host.split(':').next().unwrap_or(host);
        let st = self.state.lock();
        let resolved = st
            .aliases_full
            .get(&h.to_ascii_lowercase())
            .or_else(|| st.aliases.get(h.split('.').next().unwrap_or(h)));
        resolved
            .and_then(|id| st.deployments.get(id))
            .map(|d| {
                d.manifest
                    .functions
                    .iter()
                    .any(|f| f.runtime == "container")
            })
            .unwrap_or(false)
    }
}

fn static_dir_path(dep: &Deployment) -> PathBuf {
    let path = dep
        .manifest
        .static_dir
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or(".");
    PathBuf::from(path)
}

// ---- routers ---------------------------------------------------------------

pub fn public_router(gw: Arc<Gateway>) -> Router {
    Router::new().fallback(handle_public).with_state(gw)
}

pub fn admin_router(gw: Arc<Gateway>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/deployments", post(admin_deploy).get(admin_list))
        .route("/stats", get(admin_stats))
        .route("/tunnels", get(admin_tunnels))
        .with_state(gw)
}

#[derive(Serialize, Clone, Default)]
pub struct TunnelStats {
    pub tunnels_opened: u64,
    pub tunnels_reused: u64,
    pub reuse_pct: f64,
    pub live_tunnels: usize,
    /// Aggregate tunnel byte/backpressure metering across live tunnels (#14).
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Sum of current write-queue depth across live tunnels (downstream backpressure).
    pub queue_depth: u32,
    /// Cumulative backpressure high-water trips across live tunnels.
    pub backpressure_events: u64,
    /// Live tunnels currently showing a non-empty write queue (under backpressure).
    pub tunnels_backpressured: usize,
}

impl Gateway {
    /// Tunnel reuse + #14 byte/backpressure metering, aggregated across live
    /// tunnels. Exposed so the node admin API can surface it.
    pub async fn tunnel_stats(&self) -> TunnelStats {
        let opened = self.tunnels_opened.load(Ordering::Relaxed);
        let reused = self.tunnels_reused.load(Ordering::Relaxed);
        let total = opened + reused;
        let reuse_pct = if total > 0 {
            reused as f64 / total as f64
        } else {
            0.0
        };
        let (mut bytes_in, mut bytes_out, mut queue_depth, mut bp, mut backpressured) =
            (0u64, 0u64, 0u32, 0u64, 0usize);
        let live = self.tunnels.lock().await;
        for client in live.values() {
            let h = client.health();
            bytes_in += h.bytes_in;
            bytes_out += h.bytes_out;
            queue_depth += h.queue_depth;
            bp += h.backpressure_events;
            if h.queue_depth > 0 {
                backpressured += 1;
            }
        }
        let live_tunnels = live.len();
        drop(live);
        TunnelStats {
            tunnels_opened: opened,
            tunnels_reused: reused,
            reuse_pct,
            live_tunnels,
            bytes_in,
            bytes_out,
            queue_depth,
            backpressure_events: bp,
            tunnels_backpressured: backpressured,
        }
    }
}

async fn admin_tunnels(State(gw): State<Arc<Gateway>>) -> Json<TunnelStats> {
    Json(gw.tunnel_stats().await)
}

pub async fn serve_public(gw: Arc<Gateway>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "fluid gateway (public) listening");
    axum::serve(l, public_router(gw)).await?;
    Ok(())
}

pub async fn serve_admin(gw: Arc<Gateway>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "fluid gateway (admin) listening");
    axum::serve(l, admin_router(gw)).await?;
    Ok(())
}

async fn admin_deploy(
    State(gw): State<Arc<Gateway>>,
    Json(req): Json<DeployRequest>,
) -> Json<DeploymentInfo> {
    Json(gw.deploy(req.root, req.manifest))
}

async fn admin_list(State(gw): State<Arc<Gateway>>) -> Json<Vec<DeploymentInfo>> {
    Json(gw.list())
}

async fn admin_stats(State(gw): State<Arc<Gateway>>) -> Json<Vec<FunctionStats>> {
    Json(gw.fluid.stats())
}

// ---- public request handling ----------------------------------------------

fn build_output_v3_refusal_response(refusal: &BuildOutputV3Refusal) -> Response {
    let status = match refusal {
        BuildOutputV3Refusal::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        BuildOutputV3Refusal::Invalid { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = (status, refusal.to_string()).into_response();
    response
        .headers_mut()
        .insert("x-hive-error", HeaderValue::from_static(refusal.code()));
    response
}

fn build_output_v3_miss_response(path: &str) -> Response {
    let mut response = (
        StatusCode::NOT_FOUND,
        format!("BUILD_OUTPUT_V3_NO_MATCH: no declared output serves {path:?}"),
    )
        .into_response();
    response.headers_mut().insert(
        "x-hive-error",
        HeaderValue::from_static("BUILD_OUTPUT_V3_NO_MATCH"),
    );
    response
}

fn build_output_v3_status_response(status: u16, location: Option<&str>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = Response::builder().status(status);
    if let Some(location) = location {
        let Ok(location) = HeaderValue::from_str(location) else {
            return build_output_v3_refusal_response(&BuildOutputV3Refusal::invalid(
                "route location",
                "expanded redirect location is not a valid HTTP header value",
            ));
        };
        response = response.header(header::LOCATION, location);
    }
    response
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
        .into_response()
}

async fn handle_public(State(gw): State<Arc<Gateway>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let path = parts.uri.path().to_string();
    let path_q = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    // Vercel analytics / speed-insights compatibility: the `@vercel/analytics`
    // and `@vercel/speed-insights` packages load a same-origin script and beacon
    // their data here. Handle these before deployment routing so any deployed app
    // using the official packages works unchanged.
    if path.starts_with("/_vercel/") {
        // The vitals beacon is the one `/_vercel/` path that needs tenant
        // attribution (to store the sample under the right tenant) — resolve
        // the deployment here (vercel_insights itself is a pure fn with no
        // Gateway access) rather than plumbing Gateway through it for one path.
        if parts.method == Method::POST && path == "/_vercel/speed-insights/vitals" {
            if let Some(dep) = gw.select(host.as_deref()) {
                let device = parts
                    .headers
                    .get(header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .map(RumDevice::from_user_agent)
                    .unwrap_or(RumDevice::Desktop);
                if let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await {
                    gw.record_vitals(&dep.tenant, device, &bytes);
                }
            }
            return (
                StatusCode::ACCEPTED,
                [(header::CONTENT_TYPE, "text/plain")],
                "ok",
            )
                .into_response();
        }
        if let Some(resp) = vercel_insights(&parts.method, &path) {
            return resp;
        }
    }

    let dep = match gw.select(host.as_deref()) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no deployment").into_response(),
    };

    // Image Optimization API (`vercel.json` `images`). Next.js' `<Image>` loader
    // hits `/_next/image`; the Vercel runtime endpoint is `/_vercel/image`.
    if path == "/_vercel/image" || path == "/_next/image" {
        if dep.manifest.build_output_v3.is_some() {
            match dep.build_output_v3.as_ref() {
                Some(BuildOutputV3RouteState::Ready(_)) => {}
                Some(BuildOutputV3RouteState::Refused(refusal)) => {
                    return build_output_v3_refusal_response(refusal)
                }
                None => {
                    return build_output_v3_refusal_response(&BuildOutputV3Refusal::invalid(
                        "gateway route state",
                        "descriptor is present but no compiled route state exists",
                    ))
                }
            }
            if dep.manifest.images.is_none() {
                let mut response =
                    (StatusCode::NOT_FOUND, "BUILD_OUTPUT_V3_IMAGE_CONFIG_ABSENT").into_response();
                response.headers_mut().insert(
                    "x-hive-error",
                    HeaderValue::from_static("BUILD_OUTPUT_V3_IMAGE_CONFIG_ABSENT"),
                );
                return response;
            }
        }
        return serve_optimized_image(&dep, parts.uri.query().unwrap_or(""), &parts.headers).await;
    }

    // Request context for `has`/`missing` conditions + host-scoped matching.
    let query = parts.uri.query().unwrap_or("").to_string();
    let with_query = |loc: String| -> String {
        if query.is_empty() {
            loc
        } else {
            format!("{loc}?{query}")
        }
    };
    let ctx = fluid_core::ReqCtx {
        host: host.clone().unwrap_or_default(),
        headers: parts
            .headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|vs| (k.as_str().to_ascii_lowercase(), vs.to_string()))
            })
            .collect(),
        query: query.clone(),
    };
    // The original path drives `headers` matching (Vercel matches the incoming
    // path, before any rewrite).
    let orig_path = path.clone();
    let redirect = |status: u16, location: String| -> Response {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
        Response::builder()
            .status(code)
            .header(header::LOCATION, location)
            .body(Body::empty())
            .unwrap()
            .into_response()
    };

    // 1) trailingSlash normalization (308 add/remove the trailing slash).
    if let Some(newp) = dep.manifest.trailing_slash_redirect(&path) {
        return redirect(308, with_query(newp));
    }
    // 2) cleanUrls: a request for `/about.html` 308-redirects to `/about`
    //    (the extensionless form is served directly — see serve_static).
    if dep.manifest.clean_urls && path.ends_with(".html") {
        let mut clean = path.trim_end_matches(".html").to_string();
        if clean.ends_with("/index") {
            clean.truncate(clean.len() - "index".len()); // ".../index" -> ".../"
        }
        if clean.is_empty() {
            clean = "/".into();
        }
        if clean != path {
            return redirect(308, with_query(clean));
        }
    }
    // 3) Redirects (vercel.json + framework), honoring has/missing + :param.
    if let Some((location, status)) = dep.manifest.redirect_for_ctx(&path, &ctx) {
        return redirect(status, location);
    }
    // 4) Rewrites map the public path to an internal one (client URL unchanged).
    let path = dep.manifest.rewrite_path_ctx(&path, &ctx);

    // 5) Response headers from `vercel.json` `headers` (matched on the incoming
    //    path) are injected onto whatever the route produces.
    let extra_headers = dep.manifest.headers_for(&orig_path, &ctx);

    // A Build Output v3 descriptor is authoritative. Evaluate its ordered regex
    // routes and exact output inventory directly; a miss/refusal never falls
    // through to longest-prefix legacy routing or the SPA index fallback.
    if dep.manifest.build_output_v3.is_some() {
        let resolution = match dep.build_output_v3.as_ref() {
            Some(BuildOutputV3RouteState::Ready(evaluator)) => {
                match evaluator.resolve(parts.method.as_str(), &path) {
                    Ok(resolution) => resolution,
                    Err(refusal) => return build_output_v3_refusal_response(&refusal),
                }
            }
            Some(BuildOutputV3RouteState::Refused(refusal)) => {
                return build_output_v3_refusal_response(refusal)
            }
            None => {
                return build_output_v3_refusal_response(&BuildOutputV3Refusal::invalid(
                    "gateway route state",
                    "descriptor is present but no compiled route state exists",
                ))
            }
        };
        let fluid_core::BuildOutputV3Resolution {
            target,
            rewritten_path,
            headers: route_headers,
            ..
        } = resolution;
        let mut response_headers: Vec<(String, String)> = route_headers.into_iter().collect();
        // Explicit vercel.json overlays are applied by the caller into the
        // legacy header layer and win duplicate names by being inserted last.
        response_headers.extend(extra_headers);
        let response = match target {
            Some(BuildOutputV3Target::Static { path, content_type }) => {
                serve_build_output_v3_asset(
                    &dep,
                    &path,
                    content_type.as_deref(),
                    &orig_path,
                    accepted_encodings(&parts.headers),
                )
                .await
            }
            Some(BuildOutputV3Target::Function { name }) => {
                let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
                    Ok(bytes) => bytes,
                    Err(_) => return (StatusCode::BAD_REQUEST, "body too large").into_response(),
                };
                let function_path = if rewritten_path.contains('?') || query.is_empty() {
                    rewritten_path
                } else {
                    format!("{rewritten_path}?{query}")
                };
                proxy_function(
                    &gw,
                    &dep,
                    &name,
                    &parts.method,
                    &function_path,
                    &parts.headers,
                    body_bytes,
                )
                .await
            }
            Some(BuildOutputV3Target::Response { status, location }) => {
                build_output_v3_status_response(status, location.as_deref())
            }
            None => build_output_v3_miss_response(&rewritten_path),
        };
        let response = apply_route_policy(response, &dep, &orig_path);
        return inject_headers(response, &response_headers);
    }

    let resp = match dep.manifest.resolve(&path) {
        RouteTarget::Static => {
            // Adapter frameworks (OpenNext / vinext): immutable assets serve from
            // `static_dir`; on a MISS the request falls through to the origin/SSR
            // function (the CDN→function model) so dynamic routes still render.
            // When no `origin_function` is set (the common case) this is exactly
            // the previous behavior — `serve_static` with its SPA/404 fallback.
            let enc = accepted_encodings(&parts.headers);
            // UNROUTABLE-BUT-FUNCTIONAL deployment: a manifest that declares
            // function(s) but NO routes and NO static_dir resolves every path
            // to `Static` (see `Manifest::resolve`'s fallback), then finds no
            // file to serve and answers 404 NO_ROUTE_MATCHED — for a project
            // whose whole purpose is the function. Measured live:
            // `shoomoo.shadw.app` had a Ready production deployment on
            // fc-virginia with `functions:[{name:"web",runtime:"node",
            // start_cmd:["node","server.js"]}]`, `routes: []`,
            // `static_dir: null` — and 404'd every request. `route_matched`
            // exists precisely because `resolve` cannot distinguish "a route
            // said static" from "nothing matched at all"; this is the arm that
            // finally acts on that distinction.
            //
            // Deliberately narrow so a genuine static site is untouched: only
            // when NOTHING matched, the manifest declares no static_dir, and
            // there IS a function to serve. A static site either has a
            // static_dir, or has no functions, and takes the unchanged path
            // below. Fixing it here rather than only at build time also
            // rescues deployments ALREADY on disk with this manifest shape,
            // which would otherwise stay dark until someone redeployed them.
            let implicit_fn = (!dep.manifest.route_matched(&path)
                && dep.manifest.static_dir.is_none()
                && dep.manifest.origin_function.is_none())
            .then(|| {
                dep.manifest
                    .functions
                    .iter()
                    .find(|f| f.name == "web")
                    .or_else(|| dep.manifest.functions.first())
                    .map(|f| f.name.clone())
            })
            .flatten();
            if let Some(name) = implicit_fn {
                let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
                    Ok(b) => b,
                    Err(_) => return (StatusCode::BAD_REQUEST, "body too large").into_response(),
                };
                // Falls through to the same post-match route-policy + header
                // injection every other arm gets.
                proxy_function(
                    &gw,
                    &dep,
                    &name,
                    &parts.method,
                    &path_q,
                    &parts.headers,
                    body_bytes,
                )
                .await
            } else {
                match dep.manifest.origin_function.clone() {
                    Some(origin) => match read_static_file(&dep, &path, enc).await {
                        Ok(Some(r)) => r,
                        Ok(None) => {
                            let body_bytes =
                                match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
                                    Ok(b) => b,
                                    Err(_) => {
                                        return (StatusCode::BAD_REQUEST, "body too large")
                                            .into_response()
                                    }
                                };
                            proxy_function(
                                &gw,
                                &dep,
                                &origin,
                                &parts.method,
                                &path_q,
                                &parts.headers,
                                body_bytes,
                            )
                            .await
                        }
                        Err(error) => static_read_error(error),
                    },
                    None => serve_static(&dep, &path, enc).await,
                }
            }
        }
        RouteTarget::Function(name) => {
            let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "body too large").into_response(),
            };
            proxy_function(
                &gw,
                &dep,
                &name,
                &parts.method,
                &path_q,
                &parts.headers,
                body_bytes,
            )
            .await
        }
    };
    // Per-route policy (#16): when this deployment carries Next.js per-route
    // classification, apply route-type-aware caching to the response. Matched on
    // the ORIGINAL request path (route patterns are user-facing, pre-rewrite).
    // No-op when `route_policies` is empty (the common case) -> byte-identical.
    let resp = apply_route_policy(resp, &dep, &orig_path);
    inject_headers(resp, &extra_headers)
}

/// Apply a deployment's per-route policy (#16) to a response: tag it with the
/// matched route class for observability, and — for Static/ISR routes whose
/// origin didn't set its own `Cache-Control` — synthesize the route-type cache
/// header (Static => immutable, ISR => `s-maxage=revalidate, SWR`). Purely
/// additive: returns the response untouched when no policy matches, when the
/// origin already set caching, or for non-success statuses.
fn apply_route_policy(mut resp: Response, dep: &Deployment, path: &str) -> Response {
    let Some(policy) = dep.manifest.route_policy(path) else {
        return resp;
    };
    // Observability: surfaces which class served the request (enables live verify).
    resp.headers_mut().insert(
        "x-hive-route-class",
        HeaderValue::from_static(policy.class.name()),
    );
    // Only synthesize caching for cacheable (2xx) responses that don't already
    // carry a Cache-Control from the origin (don't override the app's intent).
    if !resp.status().is_success() || resp.headers().contains_key(header::CACHE_CONTROL) {
        return resp;
    }
    if let Some(cc) = policy.class.cache_policy(policy.revalidate).cache_control() {
        if let Ok(v) = HeaderValue::from_str(&cc) {
            resp.headers_mut().insert(header::CACHE_CONTROL, v);
        }
    }
    resp
}

/// Apply configured response headers (`vercel.json` `headers`) onto a response.
fn inject_headers(mut resp: Response, extra: &[(String, String)]) -> Response {
    if extra.is_empty() {
        return resp;
    }
    let h = resp.headers_mut();
    for (k, v) in extra {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            h.insert(name, val);
        }
    }
    resp
}

// ============================ Real User Monitoring ============================
//
// Storage + percentile/RES scoring for the `@vercel/speed-insights` beacon
// (see `vercel_insights`'s SPEED_JS below for what it collects). Previously
// the beacon fired and was received (202 Accepted) but the payload was
// discarded entirely — the dashboard's Speed Insights page was hardcoded to
// an empty "no RUM ingest yet" stub because there was, in fact, no ingest at
// all. This closes that gap: real samples in, real percentiles + a real
// score out.

/// Coarse device class, sniffed server-side from the beacon POST's
/// `User-Agent` header (the beacon itself sends no device field — adding one
/// would mean shipping new client JS; a UA regex is a one-line, no-client-
/// change alternative that's plenty accurate for the Desktop/Mobile toggle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RumDevice {
    Desktop,
    Mobile,
}

impl RumDevice {
    pub fn from_user_agent(ua: &str) -> RumDevice {
        let ua = ua.to_ascii_lowercase();
        if ua.contains("mobi") || ua.contains("android") || ua.contains("iphone") {
            RumDevice::Mobile
        } else {
            RumDevice::Desktop
        }
    }
}

/// One beacon's worth of vitals (all optional — a real page load may not
/// populate every observer, e.g. a page with zero layout shift never fires
/// the CLS `PerformanceObserver` callback at all).
#[derive(Clone, serde::Deserialize)]
struct VitalsIn {
    #[serde(default)]
    href: String,
    #[serde(default)]
    vitals: VitalsPayload,
}

#[derive(Clone, Default, serde::Deserialize)]
struct VitalsPayload {
    #[serde(rename = "FCP")]
    fcp: Option<f64>,
    #[serde(rename = "LCP")]
    lcp: Option<f64>,
    #[serde(rename = "CLS")]
    cls: Option<f64>,
    #[serde(rename = "INP")]
    inp: Option<f64>,
    #[serde(rename = "TTFB")]
    ttfb: Option<f64>,
}

#[derive(Clone)]
struct VitalSample {
    t_ms: u64,
    route: String,
    device: RumDevice,
    v: VitalsPayload,
}

/// Bounded ring buffer per tenant (newest-N samples, not a time-bucketed
/// rollup like `MetricsStore` — percentiles need the raw distribution, not a
/// sum/count, and a capped buffer bounds memory without a separate eviction
/// policy per resolution). Not persisted: RUM is a live-window UX signal
/// (Vercel's own dashboard only ever shows a rolling window too), and a
/// restart refilling within minutes of real traffic is an acceptable
/// trade-off here — unlike the hour/day usage rollups, there's no billing or
/// long-term-trend consumer relying on this surviving a restart.
const RUM_CAP_PER_TENANT: usize = 5_000;

#[derive(Default)]
struct RumStore {
    by_tenant: parking_lot::RwLock<HashMap<String, std::collections::VecDeque<VitalSample>>>,
}

impl RumStore {
    fn new() -> RumStore {
        RumStore::default()
    }

    fn record(&self, tenant: &str, device: RumDevice, body: &[u8]) {
        let Ok(payload) = serde_json::from_slice::<VitalsIn>(body) else {
            return;
        };
        let route = path_of_href(&payload.href);
        let mut map = self.by_tenant.write();
        let dq = map.entry(tenant.to_string()).or_default();
        dq.push_back(VitalSample {
            t_ms: now_ms(),
            route,
            device,
            v: payload.vitals,
        });
        while dq.len() > RUM_CAP_PER_TENANT {
            dq.pop_front();
        }
    }

    /// This node's LOCAL samples only, grouped by route — the mergeable unit
    /// for a fleet-wide read. Percentiles can't be meaningfully averaged
    /// across nodes (the average of two p75s is not the true p75 of the
    /// combined population), so a fleet-wide caller merges these raw
    /// per-route arrays (`RumRaw::merge`) and computes percentiles ONCE on
    /// the combined, fully-sorted set — same "merge raw, compute once" shape
    /// as `metrics.rs`'s `Bucket::add`, applied to a distribution instead of
    /// a sum. Grouping by route (rather than one flat pool) is what lets the
    /// dashboard's Poor/Needs Improvement/Great route buckets carry a REAL
    /// per-route score instead of dumping every route into one bucket
    /// regardless of its actual performance.
    fn raw(&self, tenant: &str, minutes: usize, device: Option<RumDevice>, now_ms: u64) -> RumRaw {
        let cutoff = now_ms.saturating_sub((minutes as u64) * 60_000);
        let map = self.by_tenant.read();
        let samples: Vec<&VitalSample> = map
            .get(tenant)
            .map(|dq| {
                dq.iter()
                    .filter(|s| s.t_ms >= cutoff && device.is_none_or(|d| s.device == d))
                    .collect()
            })
            .unwrap_or_default();
        let mut by_route: HashMap<String, RouteRaw> = HashMap::new();
        for s in &samples {
            let r = by_route.entry(s.route.clone()).or_default();
            if let Some(v) = s.v.fcp {
                r.fcp.push(v);
            }
            if let Some(v) = s.v.lcp {
                r.lcp.push(v);
            }
            if let Some(v) = s.v.cls {
                r.cls.push(v);
            }
            if let Some(v) = s.v.inp {
                r.inp.push(v);
            }
            if let Some(v) = s.v.ttfb {
                r.ttfb.push(v);
            }
            r.count += 1;
        }
        for r in by_route.values_mut() {
            r.sort();
        }
        RumRaw {
            by_route,
            sample_count: samples.len(),
        }
    }

    fn summary(
        &self,
        tenant: &str,
        minutes: usize,
        device: Option<RumDevice>,
        now_ms: u64,
    ) -> RumSummary {
        self.raw(tenant, minutes, device, now_ms).summarize()
    }
}

/// One route's sorted-ascending vital-sample arrays + count — sorted so a
/// merge across nodes is a cheap concatenate-then-resort, and so percentiles
/// are computed once, on the final (possibly fleet-merged) set.
#[derive(Clone, Default, Serialize, serde::Deserialize)]
pub struct RouteRaw {
    pub fcp: Vec<f64>,
    pub lcp: Vec<f64>,
    pub cls: Vec<f64>,
    pub inp: Vec<f64>,
    pub ttfb: Vec<f64>,
    pub count: u64,
}

impl RouteRaw {
    fn sort(&mut self) {
        for v in [
            &mut self.fcp,
            &mut self.lcp,
            &mut self.cls,
            &mut self.inp,
            &mut self.ttfb,
        ] {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        }
    }
    fn merge(&mut self, other: &RouteRaw) {
        for (dst, src) in [
            (&mut self.fcp, &other.fcp),
            (&mut self.lcp, &other.lcp),
            (&mut self.cls, &other.cls),
            (&mut self.inp, &other.inp),
            (&mut self.ttfb, &other.ttfb),
        ] {
            dst.extend_from_slice(src);
            dst.sort_by(|a, b| a.partial_cmp(b).unwrap());
        }
        self.count += other.count;
    }
    fn percentiles(&self, p: f64) -> VitalPercentiles {
        let pct = |sorted: &[f64]| -> Option<f64> {
            if sorted.is_empty() {
                return None;
            }
            let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
            sorted.get(idx.min(sorted.len() - 1)).copied()
        };
        VitalPercentiles {
            fcp: pct(&self.fcp),
            lcp: pct(&self.lcp),
            cls: pct(&self.cls),
            inp: pct(&self.inp),
            ttfb: pct(&self.ttfb),
        }
    }
}

/// Mergeable raw RUM data for one scope (a single node, or the fleet-wide
/// merge of every node's local `raw()`), grouped by route.
#[derive(Clone, Default, Serialize, serde::Deserialize)]
pub struct RumRaw {
    pub by_route: HashMap<String, RouteRaw>,
    pub sample_count: usize,
}

/// Real Experience Score: each Core Web Vital scored 0-100 against
/// Google/Vercel's published good/poor thresholds (linear between them),
/// weighted LCP 25 / INP 25 / CLS 25 / FCP 15 / TTFB 10 — the commonly-
/// published breakdown for Vercel's RES. `None` when none of the weighted
/// vitals have data (honest "collecting"), never a fabricated score.
fn res_score(p75: &VitalPercentiles) -> Option<u32> {
    let score_of = |value: Option<f64>, good: f64, poor: f64| -> Option<f64> {
        let v = value?;
        // Every one of these 5 vitals is "lower is better".
        let frac = ((poor - v) / (poor - good)).clamp(0.0, 1.0);
        Some(frac * 100.0)
    };
    let weighted = [
        (score_of(p75.lcp, 2500.0, 4000.0), 25.0),
        (score_of(p75.inp, 200.0, 500.0), 25.0),
        (score_of(p75.cls, 0.1, 0.25), 25.0),
        (score_of(p75.fcp, 1800.0, 3000.0), 15.0),
        (score_of(p75.ttfb, 800.0, 1800.0), 10.0),
    ];
    let (sum, weight): (f64, f64) = weighted
        .iter()
        .filter_map(|(s, w)| s.map(|s| (s * w, *w)))
        .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
    if weight > 0.0 {
        Some((sum / weight).round() as u32)
    } else {
        None
    }
}

impl RumRaw {
    /// Fold `other` (a peer's raw data) into `self`.
    pub fn merge(&mut self, other: &RumRaw) {
        for (route, r) in &other.by_route {
            self.by_route.entry(route.clone()).or_default().merge(r);
        }
        self.sample_count += other.sample_count;
    }

    /// Compute the aggregate percentiles/RES (pooling every route together)
    /// plus a per-route breakdown, from this (possibly fleet-merged) raw data.
    pub fn summarize(&self) -> RumSummary {
        let mut agg = RouteRaw::default();
        for r in self.by_route.values() {
            agg.merge(r);
        }
        let p75 = agg.percentiles(0.75);
        let mut routes: Vec<RouteScore> = self
            .by_route
            .iter()
            .map(|(route, r)| {
                let p75 = r.percentiles(0.75);
                RouteScore {
                    route: route.clone(),
                    count: r.count,
                    res: res_score(&p75),
                    p75,
                }
            })
            .collect();
        routes.sort_by(|a, b| b.count.cmp(&a.count));
        RumSummary {
            sample_count: self.sample_count,
            res: res_score(&p75),
            p75,
            p90: agg.percentiles(0.90),
            p95: agg.percentiles(0.95),
            p99: agg.percentiles(0.99),
            routes,
        }
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub struct VitalPercentiles {
    pub fcp: Option<f64>,
    pub lcp: Option<f64>,
    pub cls: Option<f64>,
    pub inp: Option<f64>,
    pub ttfb: Option<f64>,
}

/// One route's own p75 + Real Experience Score — what actually justifies
/// classifying a route into the dashboard's Poor/Needs Improvement/Great
/// buckets (rather than every route landing in the same bucket regardless of
/// its real performance, which a per-route-count-only breakdown would do).
#[derive(Serialize)]
pub struct RouteScore {
    pub route: String,
    pub count: u64,
    pub res: Option<u32>,
    pub p75: VitalPercentiles,
}

#[derive(Serialize)]
pub struct RumSummary {
    pub sample_count: usize,
    /// Real Experience Score (0-100), `None` until at least one weighted
    /// vital has a sample.
    pub res: Option<u32>,
    pub p75: VitalPercentiles,
    pub p90: VitalPercentiles,
    pub p95: VitalPercentiles,
    pub p99: VitalPercentiles,
    /// Per-route breakdown, sorted by sample count desc.
    pub routes: Vec<RouteScore>,
}

/// Extract the path portion of a beacon's `href` (`https://host/a/b?q=1` ->
/// `/a/b`) without pulling in a URL-parsing crate for one field.
fn path_of_href(href: &str) -> String {
    let after_scheme = href.split("://").nth(1).unwrap_or(href);
    let path_and_after = after_scheme
        .splitn(2, '/')
        .nth(1)
        .map(|s| format!("/{s}"))
        .unwrap_or_else(|| "/".to_string());
    path_and_after
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .to_string()
}

/// Vercel Web Analytics + Speed Insights endpoints.
///
/// * `GET  /_vercel/insights/script.js`        — analytics loader (sends pageviews/events)
/// * `POST /_vercel/insights/view|event`       — beacon sink (202)
/// * `GET  /_vercel/speed-insights/script.js`  — web-vitals collector
/// * `POST /_vercel/speed-insights/vitals`     — beacon sink (202)
fn vercel_insights(method: &Method, path: &str) -> Option<Response> {
    const ANALYTICS_JS: &str = r#"(function(){function send(t,d){try{navigator.sendBeacon('/_vercel/insights/'+t,JSON.stringify(d||{}))}catch(e){}}
function va(){var a=[].slice.call(arguments),k=a[0];if(k==='event')send('event',a[1]||{});else send('view',a[1]||{})}
var q=window.vaq||[];window.va=va;window.vaq={push:function(args){va.apply(null,args)}};
q.forEach(function(args){va.apply(null,args)});send('view',{u:location.pathname});})();"#;

    const SPEED_JS: &str = r#"(function(){var v={};function send(){try{navigator.sendBeacon('/_vercel/speed-insights/vitals',JSON.stringify({href:location.href,vitals:v}))}catch(e){}}
try{new PerformanceObserver(function(l){l.getEntries().forEach(function(e){if(e.name==='first-contentful-paint')v.FCP=e.startTime})}).observe({type:'paint',buffered:true});
new PerformanceObserver(function(l){var es=l.getEntries();v.LCP=es[es.length-1].startTime}).observe({type:'largest-contentful-paint',buffered:true});
var cls=0;new PerformanceObserver(function(l){l.getEntries().forEach(function(e){if(!e.hadRecentInput)cls+=e.value});v.CLS=cls}).observe({type:'layout-shift',buffered:true});
new PerformanceObserver(function(l){l.getEntries().forEach(function(e){v.INP=Math.max(v.INP||0,e.duration)})}).observe({type:'event',buffered:true,durationThreshold:40})}catch(e){}
try{var n=performance.getEntriesByType('navigation')[0];if(n)v.TTFB=n.responseStart}catch(e){}
addEventListener('visibilitychange',function(){if(document.visibilityState==='hidden')send()});addEventListener('pagehide',send);})();"#;

    let js = |body: &'static str| -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            )
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(body))
            .unwrap()
            .into_response()
    };
    let accepted = || {
        (
            StatusCode::ACCEPTED,
            [(header::CONTENT_TYPE, "text/plain")],
            "ok",
        )
            .into_response()
    };

    match (method, path) {
        (&Method::GET, "/_vercel/insights/script.js") => Some(js(ANALYTICS_JS)),
        (&Method::GET, "/_vercel/speed-insights/script.js") => Some(js(SPEED_JS)),
        (&Method::POST, "/_vercel/insights/view")
        | (&Method::POST, "/_vercel/insights/event")
        | (&Method::POST, "/_vercel/speed-insights/vitals") => Some(accepted()),
        // Image Optimization is handled per-deployment after selection.
        (_, "/_vercel/image") => None,
        // Unknown _vercel path: 204 so the client never sees a hard 404.
        _ => Some((StatusCode::NO_CONTENT, "").into_response()),
    }
}

/// Vercel-standard `Cache-Control` for a served static asset. Content-hashed
/// build assets (Next.js `/_next/static/**`, or Vite/webpack `name.<hex>.ext`)
/// are immutable and cached for a year; everything else uses Vercel's default
/// (`public, max-age=0, must-revalidate`) — which our CDN treats as
/// non-storable, so a redeploy never serves stale non-hashed content.
fn static_cache_control(path: &str) -> &'static str {
    let file = path.rsplit('/').next().unwrap_or("");
    if path.contains("/_next/static/") || is_hashed_asset(file) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=0, must-revalidate"
    }
}

/// Which content encodings the client accepts, best-first. Honors an explicit
/// `q=0` (RFC 9110's "not acceptable"), which is the one case where naive
/// substring matching would hand a client bytes it told us it cannot decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcceptedEncodings {
    pub br: bool,
    pub gzip: bool,
}

fn accepted_encodings(headers: &HeaderMap) -> AcceptedEncodings {
    let raw = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut out = AcceptedEncodings::default();
    for part in raw.split(',') {
        let mut it = part.split(';');
        let name = it.next().unwrap_or("").trim().to_ascii_lowercase();
        // `;q=0` (and `q=0.0`) means explicitly NOT acceptable.
        let refused = it.any(|p| {
            let p = p.trim().to_ascii_lowercase();
            p.strip_prefix("q=")
                .map(|q| q.parse::<f32>().map(|v| v <= 0.0).unwrap_or(false))
                .unwrap_or(false)
        });
        if refused {
            continue;
        }
        match name.as_str() {
            "br" => out.br = true,
            "gzip" => out.gzip = true,
            "*" => {
                out.br = true;
                out.gzip = true;
            }
            _ => {}
        }
    }
    out
}

/// Is this content type worth compressing? Already-compressed binary formats
/// (png/jpeg/webp/woff2/zip/video) get nothing but wasted CPU from a second
/// pass — Vercel draws the same line.
fn is_compressible(ctype: &str) -> bool {
    let c = ctype.split(';').next().unwrap_or("").trim();
    c.starts_with("text/")
        || matches!(
            c,
            "application/javascript"
                | "text/javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/ld+json"
                | "application/xml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "image/svg+xml"
                | "application/wasm"
                | "application/x-font-ttf"
                | "font/ttf"
                | "font/otf"
        )
}

/// Below this, framing overhead outweighs any saving.
const MIN_COMPRESS_BYTES: usize = 1024;
/// Above this we serve identity rather than block a request on a large inline
/// compression; a pre-generated `.br`/`.gz` sibling can still cover large assets.
const MAX_INLINE_COMPRESS_BYTES: usize = 8 * 1024 * 1024;

/// Is `relative` a usable precompressed copy of `src`? The sidecar must be at
/// least as new, and the source is reopened after the sidecar read to prove it
/// still names the generation whose identity bytes we selected.
async fn fresh_sibling(
    root: &static_files::StaticRoot,
    static_dir: &Path,
    relative: &Path,
    source_relative: &Path,
    source: &static_files::ContainedFile,
) -> Result<Option<Vec<u8>>, static_files::ReadError> {
    let sibling =
        match static_files::read_variant(root, static_dir, relative, source_relative, source).await
        {
            Ok(sibling) => sibling,
            Err(static_files::ReadError::Missing) => return Ok(None),
            Err(error) => return Err(error),
        };
    match (source.modified, sibling.modified) {
        (Some(source), Some(precompressed)) if precompressed >= source => Ok(Some(sibling.bytes)),
        _ => Ok(None),
    }
}

fn compress_bytes(bytes: &[u8], br: bool) -> Option<Vec<u8>> {
    use std::io::Write;
    if br {
        let mut out = Vec::with_capacity(bytes.len() / 3);
        {
            // q6/lgwin22: the knee of the ratio-vs-time curve for text served
            // once and then cached as a sibling.
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, 6, 22);
            w.write_all(bytes).ok()?;
            w.flush().ok()?;
        }
        Some(out)
    } else {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        e.write_all(bytes).ok()?;
        e.finish().ok()
    }
}

/// Build the response for a static asset, negotiating `Content-Encoding`.
///
/// An on-disk precompressed sibling (`<file>.br`, then `<file>.gz`) is
/// preferred. Failing that, the response is compressed inline. The request path
/// never writes into a tenant checkout: doing so safely under concurrent tenant
/// mutation requires the same directory-fd boundary as reads, and a cache is an
/// optimization rather than a reason to widen the filesystem authority here.
/// `Vary: Accept-Encoding` is always set, including on identity responses,
/// because the same URL genuinely varies by request header.
async fn static_asset_response(
    root: &static_files::StaticRoot,
    static_dir: &Path,
    relative: &Path,
    source: static_files::ContainedFile,
    ctype: &str,
    cache_control: &str,
    allow_precompressed: bool,
    enc: AcceptedEncodings,
) -> Result<Response, static_files::ReadError> {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(ctype) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(cache_control) {
        headers.insert(header::CACHE_CONTROL, v);
    }
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    let want_br = enc.br;
    let want_gz = enc.gzip;
    if (want_br || want_gz) && is_compressible(ctype) && source.bytes.len() >= MIN_COMPRESS_BYTES {
        // 1) Precompressed sibling, bound to the source generation selected above.
        // Descriptor-authoritative v3 serving disables this probe: only an output
        // selected by the compiled descriptor may supply bytes.
        if allow_precompressed {
            for (want, ext, label) in [(want_br, "br", "br"), (want_gz, "gz", "gzip")] {
                if !want {
                    continue;
                }
                let sibling = PathBuf::from(format!("{}.{ext}", relative.display()));
                if let Some(pre) =
                    fresh_sibling(root, static_dir, &sibling, relative, &source).await?
                {
                    headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(label));
                    return Ok((headers, pre).into_response());
                }
            }
        }
        // 2) Compress inline without mutating the tenant checkout.
        if source.bytes.len() <= MAX_INLINE_COMPRESS_BYTES {
            let use_br = want_br;
            let src = source.bytes.clone();
            if let Ok(Some(out)) =
                tokio::task::spawn_blocking(move || compress_bytes(&src, use_br)).await
            {
                // Only worth serving if it actually got smaller.
                if out.len() < source.bytes.len() {
                    headers.insert(
                        header::CONTENT_ENCODING,
                        HeaderValue::from_static(if use_br { "br" } else { "gzip" }),
                    );
                    return Ok((headers, out).into_response());
                }
            }
        }
    }
    Ok((headers, source.bytes).into_response())
}

/// True if a filename carries a content hash (a `.`/`-`-delimited hex token of
/// 8+ chars containing a digit), e.g. `index-4f3a9c2b.js`, `main.1a2b3c4d.css`.
fn is_hashed_asset(file: &str) -> bool {
    file.split(['.', '-']).any(|seg| {
        seg.len() >= 8
            && seg.bytes().all(|b| b.is_ascii_hexdigit())
            && seg.bytes().any(|b| b.is_ascii_digit())
    })
}

/// Serve one descriptor-indexed static file exactly. No clean-url probe, index
/// synthesis, or SPA fallback is permitted on this authoritative path.
async fn serve_build_output_v3_asset(
    dep: &SelectedDeployment,
    asset: &str,
    content_type_override: Option<&str>,
    request_path: &str,
    enc: AcceptedEncodings,
) -> Response {
    let Some(root) = dep.static_root.as_ref() else {
        return static_read_error(static_files::ReadError::Unavailable);
    };
    let static_dir = static_dir_path(dep);
    let relative = Path::new(asset);
    match static_files::read(root, &static_dir, relative).await {
        Ok(source) => {
            let logical_file = dep.root.join(&static_dir).join(relative);
            let content_type = content_type_override.unwrap_or_else(|| content_type(&logical_file));
            match static_asset_response(
                root,
                &static_dir,
                relative,
                source,
                content_type,
                static_cache_control(request_path),
                false,
                enc,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => static_read_error(error),
            }
        }
        Err(static_files::ReadError::Missing) => {
            let mut response = (
                StatusCode::SERVICE_UNAVAILABLE,
                "BUILD_OUTPUT_V3_OUTPUT_MISSING",
            )
                .into_response();
            response.headers_mut().insert(
                "x-hive-error",
                HeaderValue::from_static("BUILD_OUTPUT_V3_OUTPUT_MISSING"),
            );
            response
        }
        Err(error) => static_read_error(error),
    }
}

/// Try to read a concrete static asset for `path` from the deployment's
/// `static_dir`. Only `Ok(None)` is an origin fallthrough; authorization and
/// availability failures retain their type.
async fn read_static_file(
    dep: &SelectedDeployment,
    path: &str,
    enc: AcceptedEncodings,
) -> Result<Option<Response>, static_files::ReadError> {
    let root = dep
        .static_root
        .as_ref()
        .ok_or(static_files::ReadError::Unavailable)?;
    let static_dir = static_dir_path(dep);
    let rel = path.trim_start_matches('/');
    let mut relative = PathBuf::from(rel);
    if path.ends_with('/') || rel.is_empty() {
        relative = relative.join("index.html");
    }
    match static_files::read(root, &static_dir, &relative).await {
        Ok(source) => {
            let logical_file = dep.root.join(&static_dir).join(&relative);
            let response = static_asset_response(
                root,
                &static_dir,
                &relative,
                source,
                content_type(&logical_file),
                static_cache_control(path),
                true,
                enc,
            )
            .await?;
            return Ok(Some(response));
        }
        Err(static_files::ReadError::Missing) => {}
        Err(error) => return Err(error),
    }
    if dep.manifest.clean_urls && !rel.is_empty() && !path.ends_with('/') {
        let relative = PathBuf::from(format!("{rel}.html"));
        match static_files::read(root, &static_dir, &relative).await {
            Ok(source) => {
                return static_asset_response(
                    root,
                    &static_dir,
                    &relative,
                    source,
                    "text/html; charset=utf-8",
                    "public, max-age=0, must-revalidate",
                    true,
                    enc,
                )
                .await
                .map(Some);
            }
            Err(static_files::ReadError::Missing) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn static_read_error(error: static_files::ReadError) -> Response {
    let (status, body, code) = match error {
        static_files::ReadError::Missing => (StatusCode::NOT_FOUND, "not found", "NOT_FOUND"),
        static_files::ReadError::Forbidden => {
            (StatusCode::FORBIDDEN, "forbidden", "STATIC_FORBIDDEN")
        }
        static_files::ReadError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "static asset unavailable",
            "STATIC_UNAVAILABLE",
        ),
    };
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert("x-hive-error", HeaderValue::from_static(code));
    response
}

/// The 404 for "no static file here", made to distinguish its two very
/// different causes.
///
/// A bare `not found` is honest for a static site missing a file. It is a lie
/// for a deployment whose `routes` never matched the request at all: nothing
/// was ever going to serve that path — not the fleet function, not a browser
/// donor (`try_browser` hangs off the `RouteTarget::Function` branch alone) —
/// and the response read identically to an unknown host, so the deployment
/// looked healthy from every angle while serving nothing. Name the miss and the
/// patterns that produced it instead.
fn no_static_file(dep: &Deployment, path: &str) -> Response {
    if dep.manifest.functions.is_empty() || dep.manifest.route_matched(path) {
        let mut resp = (StatusCode::NOT_FOUND, "not found").into_response();
        resp.headers_mut()
            .insert("x-hive-error", HeaderValue::from_static("NOT_FOUND"));
        return resp;
    }
    let patterns = if dep.manifest.routes.is_empty() {
        "none declared".to_string()
    } else {
        dep.manifest
            .routes
            .iter()
            .map(|r| format!("{:?}", r.pattern))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let functions = dep
        .manifest
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        "NO_ROUTE_MATCHED: no route in this deployment matches {path:?}, and no static file \
         exists there either.\n\nDeclared routes: {patterns}\nDeclared functions: {functions}\n\n\
         Add a catch-all to fluid.json, e.g. \"routes\": [{{ \"pattern\": \"/*\", \"target\": \
         {{ \"function\": \"{first}\" }} }}]. Patterns are prefix matches: \"/\", \"/*\" and \
         \"*\" match every path; \"/api\" and \"/api/*\" match /api and /api/...\n",
        first = dep
            .manifest
            .functions
            .first()
            .map(|f| f.name.as_str())
            .unwrap_or("web"),
    );
    let mut resp = (StatusCode::NOT_FOUND, body).into_response();
    resp.headers_mut()
        .insert("x-hive-error", HeaderValue::from_static("NO_ROUTE_MATCHED"));
    resp
}

async fn serve_static(dep: &SelectedDeployment, path: &str, enc: AcceptedEncodings) -> Response {
    let Some(root) = dep.static_root.as_ref() else {
        return static_read_error(static_files::ReadError::Unavailable);
    };
    let static_dir = static_dir_path(dep);
    let rel = path.trim_start_matches('/');
    let mut relative = PathBuf::from(rel);
    if path.ends_with('/') || rel.is_empty() {
        relative = relative.join("index.html");
    }
    match static_files::read(root, &static_dir, &relative).await {
        Ok(source) => {
            let logical_file = dep.root.join(&static_dir).join(&relative);
            return match static_asset_response(
                root,
                &static_dir,
                &relative,
                source,
                content_type(&logical_file),
                static_cache_control(path),
                true,
                enc,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => static_read_error(error),
            };
        }
        Err(static_files::ReadError::Missing) => {}
        Err(error) => return static_read_error(error),
    }

    // cleanUrls: serve `about.html` for a request to `/about`.
    if dep.manifest.clean_urls && !rel.is_empty() && !path.ends_with('/') {
        let relative = PathBuf::from(format!("{rel}.html"));
        match static_files::read(root, &static_dir, &relative).await {
            Ok(source) => {
                return match static_asset_response(
                    root,
                    &static_dir,
                    &relative,
                    source,
                    "text/html; charset=utf-8",
                    "public, max-age=0, must-revalidate",
                    true,
                    enc,
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => static_read_error(error),
                };
            }
            Err(static_files::ReadError::Missing) => {}
            Err(error) => return static_read_error(error),
        }
    }

    // SPA-ish fallback: try index.html at the static root only after typed misses.
    let index = Path::new("index.html");
    match static_files::read(root, &static_dir, index).await {
        Ok(source) => match static_asset_response(
            root,
            &static_dir,
            index,
            source,
            "text/html",
            "public, max-age=0, must-revalidate",
            true,
            enc,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => static_read_error(error),
        },
        Err(static_files::ReadError::Missing) => no_static_file(dep, path),
        Err(error) => static_read_error(error),
    }
}

/// Vercel Image Optimization API (`/_vercel/image`, also `/_next/image`).
/// Validates the request against the deployment's `images` config, fetches the
/// source (local asset or allow-listed remote), and re-encodes it at the
/// requested width/quality. Resizing uses the pure-Rust `image` crate.
async fn serve_optimized_image(
    dep: &SelectedDeployment,
    query: &str,
    req_headers: &HeaderMap,
) -> Response {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_string()).into_response();

    // ---- parse query ----
    let mut url = String::new();
    let mut w: Option<u32> = None;
    let mut q: u32 = 75;
    for (k, v) in parse_query(query) {
        match k.as_str() {
            "url" => url = v,
            "w" => w = v.parse().ok(),
            "q" => {
                if let Ok(n) = v.parse() {
                    q = n;
                }
            }
            _ => {}
        }
    }
    if url.is_empty() {
        return bad("missing `url`");
    }
    let Some(width) = w else {
        return bad("missing `w`");
    };
    if width == 0 || width > 4096 {
        return bad("invalid `w`");
    }

    let cfg = dep.manifest.images.as_ref();
    // Enforce the allow-lists when configured.
    if let Some(c) = cfg {
        if !c.sizes.is_empty() && !c.sizes.contains(&width) {
            return bad("`w` not in images.sizes");
        }
        if !c.qualities.is_empty() && !c.qualities.contains(&q) {
            return bad("`q` not in images.qualities");
        }
    }
    let q = q.clamp(1, 100) as u8;

    // ---- resolve + fetch the source ----
    let is_remote = url.starts_with("http://") || url.starts_with("https://");
    let (bytes, is_svg): (Vec<u8>, bool) = if is_remote {
        // Remote sources require an allow-list (no open proxy / SSRF).
        let allowed = cfg.map(|c| remote_allowed(c, &url)).unwrap_or(false);
        if !allowed {
            return bad("remote url not allowed by images.remotePatterns/domains");
        }
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(_) => return (StatusCode::BAD_GATEWAY, "image fetch failed").into_response(),
        };
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let svg = r
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|t| t.contains("svg"))
                    .unwrap_or(false);
                match r.bytes().await {
                    Ok(b) if b.len() <= 16 * 1024 * 1024 => {
                        (b.to_vec(), svg || url.ends_with(".svg"))
                    }
                    _ => return (StatusCode::BAD_GATEWAY, "image too large").into_response(),
                }
            }
            _ => return (StatusCode::BAD_GATEWAY, "image fetch failed").into_response(),
        }
    } else {
        // Local asset: validate localPatterns (when set), preserving the v3
        // distinction between undefined (allow all) and an explicit empty array
        // (deny all), which the legacy manifest vector cannot represent alone.
        let v3_local_allowed = dep
            .manifest
            .build_output_v3
            .as_ref()
            .and_then(|descriptor| descriptor.config_view().ok())
            .and_then(|config| config.images)
            .and_then(|images| images.local_patterns)
            .map(|patterns| {
                let (pathname, search) = url
                    .split_once('?')
                    .map(|(path, query)| (path, format!("?{query}")))
                    .unwrap_or((url.as_str(), String::new()));
                patterns.iter().any(|pattern| {
                    pattern
                        .pathname
                        .as_deref()
                        .map(|pattern| pattern_matches(pattern, pathname))
                        .unwrap_or(true)
                        && pattern
                            .search
                            .as_deref()
                            .map(|expected| expected.is_empty() || expected == search)
                            .unwrap_or(true)
                })
            });
        if v3_local_allowed == Some(false) {
            return bad("local url not allowed by images.localPatterns");
        }
        if v3_local_allowed.is_none() {
            if let Some(c) = cfg {
                if !c.local_patterns.is_empty() && !local_allowed(c, &url) {
                    return bad("local url not allowed by images.localPatterns");
                }
            }
        }
        let Some(root) = dep.static_root.as_ref() else {
            return static_read_error(static_files::ReadError::Unavailable);
        };
        let static_dir = static_dir_path(dep);
        let requested = url.split('?').next().unwrap_or(&url);
        let rel = if dep.manifest.build_output_v3.is_some() {
            let Some(BuildOutputV3RouteState::Ready(evaluator)) = dep.build_output_v3.as_ref()
            else {
                return build_output_v3_refusal_response(&BuildOutputV3Refusal::invalid(
                    "gateway route state",
                    "Build Output image source has no compiled evaluator",
                ));
            };
            match evaluator.resolve("GET", requested) {
                Ok(resolution) => match resolution.target {
                    Some(BuildOutputV3Target::Static { path, .. }) => path,
                    _ => return bad("local image source is not a declared static output"),
                },
                Err(refusal) => return build_output_v3_refusal_response(&refusal),
            }
        } else {
            requested.trim_start_matches('/').to_string()
        };
        let relative = PathBuf::from(&rel);
        match static_files::read(root, &static_dir, &relative).await {
            Ok(source) => (source.bytes, rel.ends_with(".svg")),
            Err(error) => return static_read_error(error),
        }
    };

    // ---- SVG: not rasterized; passthrough only when explicitly allowed ----
    if is_svg {
        let allow_svg = cfg.and_then(|c| c.dangerously_allow_svg).unwrap_or(false);
        if !allow_svg {
            return bad("SVG optimization disabled (set images.dangerouslyAllowSVG)");
        }
        return image_response(bytes, "image/svg+xml", cfg);
    }

    // ---- decode → resize → encode (CPU-bound: off the async runtime) ----
    let accept_webp = req_headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("image/webp"))
        .unwrap_or(false);
    let formats = cfg.map(|c| c.formats.clone()).unwrap_or_default();

    // CONTENT-ADDRESSED CACHE, checked before any CPU is spent. Vercel's
    // optimizer is a cache keyed on (source, width, quality, output format);
    // without one, every single request re-fetched, re-decoded, Lanczos3-
    // resized and re-encoded the same image — ~100-800ms of blocking CPU per
    // request for a 1-4 MP source, and a trivially self-inflicted DoS on the
    // node serving a popular page (the response's own `max-age` is only 60s by
    // default, so browsers come back for it). The key covers the SOURCE BYTES
    // (not its URL), so a changed asset is a different entry with no
    // invalidation step, and the negotiated output format, so a webp-accepting
    // and a jpeg-only client never read each other's bytes.
    let key = image_cache_key(&bytes, width, q, accept_webp, &formats);
    let cache_dir = image_cache_dir();
    let cache_file = cache_dir.join(&key);
    if let Ok(hit) = tokio::fs::read(&cache_file).await {
        // Stored as `<ctype>\n<bytes>` so the negotiated content type survives
        // the round trip without a second sidecar file to keep in sync.
        if let Some(nl) = hit.iter().position(|b| *b == b'\n') {
            let (ct, body) = hit.split_at(nl);
            if let Ok(ct) = std::str::from_utf8(ct) {
                return image_response(body[1..].to_vec(), &ct.to_string(), cfg);
            }
        }
    }

    let encoded = tokio::task::spawn_blocking(move || {
        optimize_bytes(&bytes, width, q, accept_webp, &formats)
    })
    .await;
    match encoded {
        Ok(Some((out, ctype))) => {
            // Persist for next time (tmp+rename so a concurrent reader never
            // sees a partial file). Best-effort and off the response path.
            let payload = {
                let mut v = Vec::with_capacity(ctype.len() + 1 + out.len());
                v.extend_from_slice(ctype.as_bytes());
                v.push(b'\n');
                v.extend_from_slice(&out);
                v
            };
            tokio::spawn(async move {
                if tokio::fs::create_dir_all(&cache_dir).await.is_ok() {
                    let tmp = cache_file.with_extension(format!("{}.tmp", std::process::id()));
                    if tokio::fs::write(&tmp, &payload).await.is_ok() {
                        let _ = tokio::fs::rename(&tmp, &cache_file).await;
                    }
                }
            });
            image_response(out, ctype, cfg)
        }
        _ => (StatusCode::UNPROCESSABLE_ENTITY, "could not process image").into_response(),
    }
}

/// Where optimized-image bytes are cached on this node. Node-local derived
/// data, so it lives under the node's data dir and never rides replicated
/// state (the `dns_geo.json` precedent).
fn image_cache_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HIVE_DATA").unwrap_or_else(|_| "/var/lib/hive".into()))
        .join("image-cache")
}

/// Content-address an optimizer result. Keyed on the SOURCE BYTES plus every
/// input that changes the output, so it is correct by construction: a
/// re-deployed or edited image hashes differently and simply misses.
fn image_cache_key(
    src: &[u8],
    width: u32,
    quality: u8,
    accept_webp: bool,
    formats: &[String],
) -> String {
    // FNV-1a over the source bytes + parameters. Not a security boundary (the
    // inputs are already trusted, server-derived), just a collision-resistant
    // enough content address for a local cache.
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |b: &[u8]| {
        for x in b {
            h ^= *x as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    mix(src);
    mix(&width.to_le_bytes());
    mix(&[quality, accept_webp as u8]);
    for f in formats {
        mix(f.as_bytes());
    }
    format!("{h:016x}-{}-{}", width, quality)
}

/// Build the optimized-image response with caching / disposition / CSP headers.
fn image_response(body: Vec<u8>, ctype: &str, cfg: Option<&fluid_core::ImagesConfig>) -> Response {
    let ttl = cfg.and_then(|c| c.minimum_cache_ttl).unwrap_or(60);
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ctype)
        .header(
            header::CACHE_CONTROL,
            format!("public, max-age={ttl}, must-revalidate"),
        );
    if let Some(c) = cfg {
        if let Some(disp) = &c.content_disposition_type {
            b = b.header(header::CONTENT_DISPOSITION, disp.clone());
        }
        if let Some(csp) = &c.content_security_policy {
            b = b.header("content-security-policy", csp.clone());
        }
    }
    b.body(Body::from(body)).unwrap().into_response()
}

/// Decode, resize to `width` (preserving aspect), and re-encode. Returns the
/// encoded bytes + content-type, or `None` if the input isn't a decodable image.
fn optimize_bytes(
    bytes: &[u8],
    width: u32,
    quality: u8,
    accept_webp: bool,
    formats: &[String],
) -> Option<(Vec<u8>, &'static str)> {
    use image::imageops::FilterType;
    let img = image::load_from_memory(bytes).ok()?;
    let resized = if img.width() > width {
        let h = ((img.height() as u64 * width as u64) / img.width().max(1) as u64).max(1) as u32;
        img.resize(width, h, FilterType::Lanczos3)
    } else {
        img
    };

    // Prefer WebP when the client accepts it and config permits (image 0.25's
    // WebP encoder is lossless; fall back to JPEG/PNG on any error).
    let want_webp =
        accept_webp && (formats.is_empty() || formats.iter().any(|f| f == "image/webp"));
    if want_webp {
        let mut buf = std::io::Cursor::new(Vec::new());
        if resized.write_to(&mut buf, image::ImageFormat::WebP).is_ok() {
            return Some((buf.into_inner(), "image/webp"));
        }
    }
    if resized.color().has_alpha() {
        let mut buf = std::io::Cursor::new(Vec::new());
        resized.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some((buf.into_inner(), "image/png"))
    } else {
        use image::ImageEncoder;
        let mut buf = Vec::new();
        let rgb = resized.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
            .write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .ok()?;
        Some((buf, "image/jpeg"))
    }
}

/// Minimal `application/x-www-form-urlencoded` query parser with percent-decode.
fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect()
}

fn pct_decode(s: &str) -> String {
    let b = s.replace('+', " ");
    let bytes = b.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Is a remote image URL permitted by `images.remotePatterns` / `images.domains`?
fn remote_allowed(cfg: &fluid_core::ImagesConfig, url: &str) -> bool {
    // Parse scheme://host[:port]/path?query without a url crate.
    let after = url.splitn(2, "://").nth(1).unwrap_or("");
    let (authority, rest) = after
        .split_once('/')
        .map(|(a, r)| (a, format!("/{r}")))
        .unwrap_or((after, "/".to_string()));
    let (host, port) = authority
        .split_once(':')
        .map(|(h, p)| (h, Some(p)))
        .unwrap_or((authority, None));
    let (pathname, search) = rest
        .split_once('?')
        .map(|(p, s)| (p.to_string(), format!("?{s}")))
        .unwrap_or((rest.clone(), String::new()));
    let scheme = url.split("://").next().unwrap_or("");

    if cfg.domains.iter().any(|d| d == host) {
        return true;
    }
    cfg.remote_patterns.iter().any(|p| {
        p.protocol.as_deref().map(|pr| pr == scheme).unwrap_or(true)
            && host_matches(&p.hostname, host)
            && p.port
                .as_deref()
                .map(|pt| pt.is_empty() || Some(pt) == port)
                .unwrap_or(true)
            && p.pathname
                .as_deref()
                .map(|pn| pattern_matches(pn, &pathname))
                .unwrap_or(true)
            && p.search
                .as_deref()
                .map(|s| s.is_empty() || s == search)
                .unwrap_or(true)
    })
}

fn local_allowed(cfg: &fluid_core::ImagesConfig, url: &str) -> bool {
    let (pathname, search) = url
        .split_once('?')
        .map(|(p, s)| (p.to_string(), format!("?{s}")))
        .unwrap_or((url.to_string(), String::new()));
    cfg.local_patterns.iter().any(|p| {
        p.pathname
            .as_deref()
            .map(|pn| pattern_matches(pn, &pathname))
            .unwrap_or(true)
            && p.search
                .as_deref()
                .map(|s| s.is_empty() || s == search)
                .unwrap_or(true)
    })
}

/// Hostname match supporting a single leading `**.` (any subdepth) or `*.`
/// (one label) wildcard, à la Next.js remotePatterns.
fn host_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("**.") {
        host == suffix || host.ends_with(&format!(".{suffix}"))
    } else if let Some(suffix) = pattern.strip_prefix("*.") {
        host.strip_suffix(suffix)
            .map(|p| p.ends_with('.') && !p[..p.len() - 1].contains('.'))
            .unwrap_or(false)
    } else {
        pattern == host
    }
}

/// Match a remotePattern `pathname` (supports a trailing `/**` and a `^...$`
/// regex-ish form via simple prefix/glob) against a path. Best-effort.
fn pattern_matches(pattern: &str, value: &str) -> bool {
    // Treat an anchored regex like `^/account123/.*$` as a prefix on the literal
    // segment before `.*`.
    let pat = pattern.trim_start_matches('^').trim_end_matches('$');
    if let Some(prefix) = pat.strip_suffix(".*") {
        return value.starts_with(prefix);
    }
    if let Some(prefix) = pat.strip_suffix("/**") {
        return value == prefix || value.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pat.strip_suffix("/*") {
        return value
            .strip_prefix(&format!("{prefix}/"))
            .map(|r| !r.contains('/'))
            .unwrap_or(false);
    }
    pat == value
}

enum BrowserAttempt {
    None,
    Response(Response),
    Failed(BrowserInvokeFailure),
}

#[derive(Deserialize)]
struct BrowserHttpEnvelope {
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "bodyBase64")]
    body_base64: Option<String>,
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let padding = if chunk[2] == b'=' {
            if chunk[3] != b'=' {
                return None;
            }
            2
        } else if chunk[3] == b'=' {
            1
        } else {
            0
        };
        if !last && padding != 0 {
            return None;
        }
        let a = value(chunk[0])?;
        let b = value(chunk[1])?;
        let c = if padding == 2 { 0 } else { value(chunk[2])? };
        let d = if padding == 0 { value(chunk[3])? } else { 0 };
        if padding == 2 && b & 15 != 0 || padding == 1 && c & 3 != 0 {
            return None;
        }
        out.push(a << 2 | b >> 4);
        if padding < 2 {
            out.push(b << 4 | c >> 2);
        }
        if padding == 0 {
            out.push(c << 6 | d);
        }
    }
    Some(out)
}

fn browser_response(bytes: &[u8]) -> Result<Response, String> {
    let envelope: BrowserHttpEnvelope =
        serde_json::from_slice(bytes).map_err(|e| format!("malformed browser response: {e}"))?;
    // `StatusCode::from_u16` alone is NOT the 100..599 check this error message
    // claims -- the `http` crate accepts any value up to 999 (reserved for
    // extension codes), so a browser peer returning e.g. 999 sailed straight
    // through to a real client with no error, verbatim, live-witnessed via
    // `crates/hive-p2p/examples/fake_browser_peer.rs` returning
    // `{"status":999,...}` and curl receiving a literal `HTTP/1.1 999 <none>`
    // response. The explicit range bound is the actual enforcement point.
    if !(100..=599).contains(&envelope.status) {
        return Err("browser response status is outside 100..599".to_string());
    }
    let status = StatusCode::from_u16(envelope.status)
        .map_err(|_| "browser response status is outside 100..599".to_string())?;
    let body = match envelope.body_base64 {
        Some(encoded) => base64_decode(&encoded)
            .ok_or_else(|| "browser response bodyBase64 is not canonical base64".to_string())?,
        None => envelope.body.into_bytes(),
    };
    if body.len() > (1 << 20) {
        return Err("browser response body exceeds frame limit".into());
    }
    let mut response = Response::builder().status(status);
    for (name, value) in envelope.headers {
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
            continue;
        }
        let name = HeaderName::from_bytes(lower.as_bytes())
            .map_err(|_| format!("invalid browser response header name: {name}"))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| format!("invalid browser response header value: {name}"))?;
        response = response.header(name, value);
    }
    let mut response = response
        .body(Body::from(body))
        .map_err(|e| format!("browser response build failed: {e}"))?
        .into_response();
    response
        .headers_mut()
        .insert("x-hive-runtime", HeaderValue::from_static("browser"));
    Ok(response)
}

async fn try_browser(
    gw: &Arc<Gateway>,
    dep: &Deployment,
    name: &str,
    method: &Method,
    path_q: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> BrowserAttempt {
    let key = func_key(dep.id.as_str(), name);
    let now = now_ms();
    let quota_window_ms = std::env::var("HIVE_BROWSER_INVOKE_QUOTA_WINDOW_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(60_000);
    // 10 req/s average sustained over the window — generous for a real
    // owner-served app, well below what would meaningfully burden a single
    // browser tab, and the actual protection this exists for: an
    // admitted-but-unrevoked endpoint otherwise has NO invoke-rate bound at
    // all for the rest of its lease.
    let quota_max = std::env::var("HIVE_BROWSER_INVOKE_QUOTA_PER_WINDOW")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(600);
    let (target, invoker) = {
        let browser = gw.browser.read();
        let Some(invoker) = browser.invoker.clone() else {
            return BrowserAttempt::None;
        };
        let Some(targets) = browser.by_function.get(&key) else {
            return BrowserAttempt::None;
        };
        // Team-scoped targets require the CALLER to be an authenticated
        // member of the owning tenant — resolved once here (only matters if
        // a Team-scoped candidate actually exists below) rather than per
        // candidate, since it's the same answer for every one of them. `None`
        // (no resolver wired, or the caller presented no valid session) means
        // no Team-scoped target is reachable; Public-scoped targets are
        // completely unaffected either way.
        let caller_tenant = browser
            .claims_resolver
            .as_ref()
            .and_then(|resolve| resolve(headers));
        let target = targets.iter().find(|target| {
            let circuit = format!("{}:{}", target.endpoint_id, target.digest);
            let scope_ok = match target.scope {
                BrowserScope::Public => true,
                BrowserScope::Team => caller_tenant.as_deref() == Some(target.tenant.as_str()),
            };
            let quota_ok = browser.invoke_quota.get(&target.endpoint_id).is_none_or(
                |(window_start, count)| {
                    now.saturating_sub(*window_start) >= quota_window_ms || *count < quota_max
                },
            );
            target.tenant == dep.tenant
                && target.deployment == dep.id.as_str()
                && target.function == name
                && scope_ok
                && target.expires_ms > now
                && browser.circuit_until.get(&circuit).copied().unwrap_or(0) <= now
                && quota_ok
        });
        let Some(target) = target.cloned() else {
            return BrowserAttempt::None;
        };
        (target, invoker)
    };
    // Record this invocation against the endpoint's quota window — a brief
    // separate write lock, same pattern as the circuit-opening write below
    // (never held across the actual network invoke).
    {
        let mut browser = gw.browser.write();
        let entry = browser
            .invoke_quota
            .entry(target.endpoint_id.clone())
            .or_insert((now, 0));
        if now.saturating_sub(entry.0) >= quota_window_ms {
            *entry = (now, 1);
        } else {
            entry.1 += 1;
        }
    }

    let forwarded: HashMap<String, String> = headers
        .iter()
        .filter(|(name, _)| {
            // authorization/cookie/proxy-authorization MUST NOT reach the
            // browser tab: Team-scoped targets are admittable by any
            // authenticated member (no owner/admin gate), so forwarding the
            // caller's live bearer JWT / hive_ API key handed it straight to
            // a low-trust peer -- an in-tenant privilege escalation. Every
            // other header is fine to forward as request context.
            !matches!(
                name.as_str(),
                "connection"
                    | "content-length"
                    | "host"
                    | "transfer-encoding"
                    | "upgrade"
                    | "authorization"
                    | "cookie"
                    | "proxy-authorization"
            )
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    let request = serde_json::json!({
        "method": method.as_str(),
        "path": path_q,
        "headers": forwarded,
        "body": std::str::from_utf8(body).unwrap_or(""),
        "bodyBase64": base64_encode(body),
    })
    .to_string();
    let invoke_started = now_ms();
    let result = invoker(target.clone(), request).await;
    let failure = match result {
        Ok(bytes) => match browser_response(&bytes) {
            Ok(response) => {
                // The ONLY metering chokepoint for browser-served traffic:
                // this path never calls gw.fluid.lease(), so release() (which
                // increments FunctionPool::requests) never runs for it.
                gw.fluid
                    .record_browser_request(&key, now_ms().saturating_sub(invoke_started));
                return BrowserAttempt::Response(response);
            }
            Err(message) => BrowserInvokeFailure {
                sent: true,
                message,
            },
        },
        Err(failure) => failure,
    };
    let circuit_ms = std::env::var("HIVE_BROWSER_CIRCUIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(30_000);
    let circuit = format!("{}:{}", target.endpoint_id, target.digest);
    gw.browser
        .write()
        .circuit_until
        .insert(circuit, now_ms().saturating_add(circuit_ms));
    tracing::warn!(
        endpoint_id = %target.endpoint_id,
        digest = %target.digest,
        sent = failure.sent,
        error = %failure.message,
        "browser function failed; circuit opened"
    );
    BrowserAttempt::Failed(failure)
}

async fn proxy_function(
    gw: &Arc<Gateway>,
    dep: &Deployment,
    name: &str,
    method: &Method,
    path_q: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let key = func_key(dep.id.as_str(), name);

    // Collect forwardable request headers once.
    //
    // `host` is deliberately DROPPED here and re-added below together with the
    // `x-forwarded-*` set, because the platform is the only party that knows
    // the public origin and the function has no other way to learn it.
    //
    // Previously `host` was dropped and nothing replaced it, so
    // `fluid_tunnel`'s server fell back to its hardcoded `host:
    // fluid.internal` and no `x-forwarded-*` header was ever sent. A tenant app
    // therefore had NOTHING to build an absolute URL from, and every framework
    // that emits one — an auth redirect, an OAuth callback, a canonical link,
    // a `Location` on a 30x — fell back to its own bind address. Measured live:
    // `https://hive.shadw.app/` answered `302 Location:
    // http://0.0.0.0:3000/ui/login?next=%2F`, i.e. it sent the browser to the
    // container's own listen address. `dashboard_proxy` in hive-cloud already
    // sets `x-forwarded-host`/`x-forwarded-proto` for exactly this reason; the
    // tenant-facing path simply never did, which is the inconsistency.
    //
    // Scheme is hardcoded `https`: every public entry point to a deployment is
    // TLS-terminated (the edge listener, the anycast front, the apps zone), so
    // an app that trusts `x-forwarded-proto` builds `https://` URLs — the only
    // correct answer for a link a browser will follow back in.
    let mut hvec: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| {
            let n = k.as_str();
            n != "connection"
                && n != "content-length"
                && n != "host"
                // Re-derived below from THIS hop; never trust a client-supplied
                // value (a spoofed x-forwarded-host is a cache-poisoning and
                // password-reset-link primitive).
                && n != "x-forwarded-host"
                && n != "x-forwarded-proto"
        })
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    if let Some(public_host) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.trim().is_empty())
    {
        // Preserve the ORIGINAL Host so frameworks that read it (Next.js and
        // most Node servers do) generate correct absolute URLs by default,
        // and add the explicit forwarded pair for everything that prefers it.
        hvec.push(("host".into(), public_host.to_string()));
        hvec.push(("x-forwarded-host".into(), public_host.to_string()));
        hvec.push(("x-forwarded-proto".into(), "https".into()));
    }

    match try_browser(gw, dep, name, method, path_q, headers, &body).await {
        BrowserAttempt::Response(response) => return response,
        BrowserAttempt::Failed(failure)
            if failure.sent && method != Method::GET && method != Method::HEAD =>
        {
            // The browser may already have executed this mutation. Replaying it
            // on fleet compute would turn a transport failure into a duplicate
            // side effect, so fail explicitly instead of lying about capacity.
            let mut response =
                (StatusCode::BAD_GATEWAY, "BROWSER_EXECUTION_UNCERTAIN").into_response();
            response.headers_mut().insert(
                "x-hive-error",
                HeaderValue::from_static("BROWSER_EXECUTION_UNCERTAIN"),
            );
            return response;
        }
        BrowserAttempt::Failed(_) | BrowserAttempt::None => {
            // Pre-send failures are safe for every method; GET/HEAD remain safe
            // after send. The normal Fluid path is the hard fallback.
        }
    }

    // Per-function max duration (Vercel default 300s) — bounds the whole
    // invocation; on timeout we return 504 without affecting other requests
    // sharing the instance (error isolation).
    let max_dur = Duration::from_secs(gw.fluid.max_duration_secs(&key).unwrap_or(300).max(1));

    const MAX_REROUTES: usize = 3;
    let mut last_err = String::from("unknown");
    // WHICH SHAPE the last attempt failed in. Both shapes used to report
    // RUNTIME_TUNNEL_FAILED, and that single label is why the shoomoo outage was
    // chased through vsock/tunnel plumbing for multiple sessions: the real
    // `last_err` was "timed out waiting for response head", i.e. the tunnel had
    // connected fine and the app never answered. Keep them apart.
    let mut upstream_silent = false;
    for attempt in 0..MAX_REROUTES {
        let lease = match gw.fluid.lease(&key).await {
            Ok(l) => l,
            Err(e) => {
                // Structured failure (#18): classify, return a STABLE public code +
                // correct status — never leak the internal error to the caller.
                let es = e.to_string();
                let class = classify_lease_error(&es);
                warn!(func = %key, error = %es, code = class.code(), "lease failed");
                let status =
                    StatusCode::from_u16(class.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
                let mut resp = (status, class.code()).into_response();
                resp.headers_mut()
                    .insert("x-hive-error", HeaderValue::from_static(class.code()));
                return resp;
            }
        };
        let cell = lease.cell_id().clone();
        let ep = lease.endpoint.clone();

        let (client, reused) = match gw.tunnel_for(&cell, &ep).await {
            Ok(c) => c,
            Err(e) => {
                last_err = e.to_string();
                upstream_silent = false; // the transport itself never came up
                tracing::debug!(cell = %cell, attempt, error = %last_err, "tunnel connect failed");
                drop(lease);
                gw.fluid.mark_dead(&key, &cell).await;
                gw.drop_tunnel(&cell).await;
                continue;
            }
        };

        tracing::debug!(cell = %cell, reused, attempt, "dispatching request over tunnel");
        // `head_timeout` MUST equal `max_dur`: the inner tunnel wait and this
        // outer `tokio::time::timeout` guard the SAME budget, and if the inner
        // one is ever shorter it fires first, turning a legitimately-slow
        // (but within-budget) invocation into a spurious "upstream_silent"
        // retry below instead of the correct, no-retry FUNCTION_INVOCATION_TIMEOUT
        // 504 at line ~2550. See client.rs's `request` doc for the incident
        // this caused.
        let req_fut = client.request(method.as_str(), path_q, hvec.clone(), &body, max_dur);
        match tokio::time::timeout(max_dur, req_fut).await {
            Err(_) => {
                // Exceeded max duration — 504, do not reroute (the instance is
                // fine; only this invocation is over budget).
                drop(lease);
                return (StatusCode::GATEWAY_TIMEOUT, "FUNCTION_INVOCATION_TIMEOUT")
                    .into_response();
            }
            Ok(Ok(resp)) => {
                tracing::debug!(cell = %cell, status = resp.status, "got response head");
                return build_response(lease, cell, reused, attempt, resp, max_dur).await;
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
                tracing::debug!(cell = %cell, reused, attempt, error = %last_err, "request failed");
                drop(lease);
                // Tunnel-level failure: if it's closed the instance is gone.
                if client.is_closed() {
                    upstream_silent = false;
                    gw.fluid.mark_dead(&key, &cell).await;
                    gw.drop_tunnel(&cell).await;
                } else {
                    // The tunnel is still OPEN and the request failed anyway —
                    // response-head timeout, nack, overload. The transport is
                    // fine; the function is what didn't answer.
                    upstream_silent = true;
                    // The instance may already be MID-EXECUTION (it received
                    // the request over the still-open tunnel; we just gave up
                    // waiting for a response). Looping back to `lease()` and
                    // retrying a non-idempotent method here is the exact
                    // "BROWSER_EXECUTION_UNCERTAIN" hazard this function
                    // already refuses to risk for the browser path above
                    // (lines ~2477-2490) — replaying a POST/PUT/PATCH/DELETE
                    // risks a second LLM call, a second Telegram send, a
                    // second charge. Fail explicitly instead of silently
                    // duplicating a side effect; GET/HEAD are always safe to
                    // retry and fall through to the loop as before.
                    if method != Method::GET && method != Method::HEAD {
                        break;
                    }
                }
            }
        }
    }
    // Reroute budget exhausted. Public code only; the internal `last_err`
    // stays in the log (#18) — but the CLASS now matches which half broke.
    let class = if upstream_silent {
        fluid_core::FailureClass::FunctionNoResponse
    } else {
        fluid_core::FailureClass::RuntimeTunnelFailed
    };
    warn!(
        func = %key,
        error = %last_err,
        code = class.code(),
        "upstream failed after reroute budget"
    );
    let mut resp = (
        StatusCode::from_u16(class.status()).unwrap_or(StatusCode::BAD_GATEWAY),
        class.code(),
    )
        .into_response();
    resp.headers_mut()
        .insert("x-hive-error", HeaderValue::from_static(class.code()));
    resp
}

/// Classify a `Fluid::lease` error into a stable public [`FailureClass`] (#18).
/// A tenant hitting its cross-pool instance quota is a 429 (back off, you're
/// throttled); a broken deployment circuits; a NODE that is missing artifacts or
/// a hypervisor, or whose container lock pool is empty, names itself as a node
/// fault; only genuine saturation / cold-start-cap failures are a 503 capacity
/// problem. Coupled to the `NackReason` Debug name surfaced by `fluid-compute`'s
/// `bail!("... ({reason:?})")` and to the `hive_core::fault` markers backends
/// embed — the `classify_lease_error_*` tests lock that contract so a format
/// change can't silently downgrade quota throttles to generic capacity errors.
///
/// The `else` arm is a CATCH-ALL, and every fault that reaches it is published to
/// the user as "the host is out of capacity". That is a lie for anything that is
/// not saturation, so a new backend failure mode belongs in a class of its own
/// with its own `hive_core::fault` marker — never left to fall through here.
fn classify_lease_error(es: &str) -> fluid_core::FailureClass {
    if es.contains("TenantQuota") {
        fluid_core::FailureClass::TenantThrottled
    } else if es.contains(hive_core::fault::NODE_IMAGE_MISSING) {
        // THIS NODE is missing a base/per-image rootfs or its guest kernel. Not
        // the app, not capacity — witnessed on fc-sanjose-cvm-2, where a missing
        // `/var/lib/hive/rootfs/default.ext4` was published as
        // CAPACITY_EXHAUSTED while the node held 923 GB free and 2046 free
        // podman locks, and the operator went looking for space. Checked BEFORE
        // the circuit arm below because the failing cold starts ALSO open the
        // pool's circuit, so both markers can be live on the same error — and of
        // the two only the node fault names the remedy.
        fluid_core::FailureClass::NodeImageMissing
    } else if es.contains(hive_core::fault::NODE_BACKEND_UNAVAILABLE) {
        // No `/dev/kvm` / no firecracker binary on this node.
        fluid_core::FailureClass::NodeBackendUnavailable
    } else if es.contains(hive_core::fault::NODE_LOCK_POOL_EXHAUSTED) {
        // podman's per-HOST lock pool is empty with nothing reclaimable: a host
        // resource fault whose only remedy is `num_locks` + `podman system
        // renumber`, which no amount of free disk or memory substitutes for.
        fluid_core::FailureClass::NodeLockPoolExhausted
    } else if es.contains(hive_core::fault::NODE_RUNTIME_MISSING) {
        // The declared interpreter is not on the filesystem this node execs
        // cells against. Checked BEFORE the circuit arm below for the same
        // reason NODE_IMAGE_MISSING is: the failing cold starts also open the
        // pool's circuit, so both markers ride the same error string, and only
        // this one names a remedy the operator can act on. Left to fall
        // through it would tell the tenant to debug an entrypoint that works.
        fluid_core::FailureClass::NodeRuntimeMissing
    } else if es.contains("DeploymentCircuitOpen")
        || es.contains(hive_core::fault::DEPLOYMENT_START_FAILED)
    {
        // The DEPLOYMENT is broken (its instances keep exiting right after start),
        // not the host. Reporting this as CAPACITY_EXHAUSTED sent users hunting a
        // platform capacity problem that did not exist while their container was
        // dying on a missing env var.
        //
        // `DEPLOYMENT_START_FAILED` joins it because the circuit only opens on the
        // THIRD consecutive failure: witnessed live, the first two failures of an
        // app that never bound its port still reported CAPACITY_EXHAUSTED. Same
        // class, same remedy (read the app's logs), so the very first one now says
        // so instead of blaming the host.
        fluid_core::FailureClass::DeploymentCircuitOpen
    } else if es.contains("no such function") {
        // The pool is not registered on this node at all, so no cold start was
        // ever attempted and no capacity was ever consumed. It used to report
        // CAPACITY_EXHAUSTED, which points an operator at the host while the
        // truth is that this node has no such deployment (mid-deploy
        // registration, an unregistered pool, a stale mesh route).
        fluid_core::FailureClass::DeploymentNotFound
    } else {
        fluid_core::FailureClass::CapacityExhausted
    }
}

/// Turn a tunnel response into an axum response.
///
/// Normal responses are **buffered** and returned with a correct content-length
/// (always terminates cleanly). Streaming responses (`text/event-stream`) are
/// passed through chunked. The lease is held for the body (and any `waitUntil`
/// window) before the instance slot is released.
async fn build_response(
    lease: fluid_compute::Lease,
    cell: CellId,
    reused: bool,
    attempt: usize,
    mut resp: fluid_tunnel::TunnelResponse,
    max_dur: Duration,
) -> Response {
    let mut hdrs = HeaderMap::new();
    // Stream incrementally for ANY response the function didn't declare a fixed
    // length for — matching Vercel streaming-functions behavior. That covers SSE
    // (text/event-stream), React Server Component streaming (text/x-component,
    // chunked HTML), and AI-SDK/ReadableStream responses (chunked, no
    // content-length). A response WITH a content-length is a finished, sized body
    // → buffer it (sized, clean termination), exactly as before.
    let mut has_content_length = false;
    let mut forced_stream = false;
    for (k, v) in &resp.headers {
        let kl = k.to_ascii_lowercase();
        if kl == "content-length" {
            has_content_length = true;
            continue;
        }
        if kl == "transfer-encoding" {
            if v.to_ascii_lowercase().contains("chunked") {
                forced_stream = true;
            }
            continue;
        }
        if kl == "connection" {
            continue;
        }
        let vl = v.to_ascii_lowercase();
        if kl == "content-type"
            && (vl.contains("event-stream") || vl.contains("x-component") || vl.contains("stream"))
        {
            forced_stream = true;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            hdrs.append(name, val);
        }
    }
    let is_stream = forced_stream || !has_content_length;
    hdrs.insert(
        HeaderName::from_static("x-fluid-instance"),
        HeaderValue::from_str(&cell.to_string()).unwrap_or(HeaderValue::from_static("?")),
    );
    hdrs.insert(
        HeaderName::from_static("x-fluid-reused"),
        HeaderValue::from_static(if reused { "true" } else { "false" }),
    );
    if attempt > 0 {
        if let Ok(v) = HeaderValue::from_str(&attempt.to_string()) {
            hdrs.insert(HeaderName::from_static("x-fluid-rerouted"), v);
        }
    }

    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let wait_until_ms = resp.wait_until_ms;

    // Helper to release the lease, honoring waitUntil.
    let release = move |lease: fluid_compute::Lease| {
        if wait_until_ms > 0 {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(wait_until_ms)).await;
                drop(lease);
            });
        } else {
            drop(lease);
        }
    };

    if is_stream {
        // Pass-through streaming (chunked) for event streams.
        let st = BodyState {
            body: resp.body,
            lease: Some(lease),
            wait_until_ms,
        };
        let body_stream = futures::stream::unfold(st, |mut st| async move {
            match st.body.recv().await {
                Some(chunk) => Some((Ok::<_, std::io::Error>(chunk), st)),
                None => {
                    if let Some(lease) = st.lease.take() {
                        if st.wait_until_ms > 0 {
                            let w = st.wait_until_ms;
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(w)).await;
                                drop(lease);
                            });
                        } else {
                            drop(lease);
                        }
                    }
                    None
                }
            }
        });
        let mut out = Response::new(Body::from_stream(body_stream));
        *out.status_mut() = status;
        *out.headers_mut() = hdrs;
        return out;
    }

    // Buffered path: collect the whole body, return a sized response. Bounded by
    // the deployment's OWN max_duration_secs (default 300s), matching the head
    // wait above — a fixed 30s here truncated any legitimately slow-but-within-
    // budget non-streaming response (e.g. a large buffered JSON body assembled
    // from a slow upstream call) the exact same way the old fixed 30s head
    // timeout did for the head itself.
    let mut buf = Vec::new();
    let drained = tokio::time::timeout(max_dur, async {
        while let Some(chunk) = resp.body.recv().await {
            buf.extend_from_slice(&chunk);
        }
    })
    .await;
    release(lease);
    if drained.is_err() {
        return (StatusCode::GATEWAY_TIMEOUT, "upstream timed out").into_response();
    }
    let mut out = (status, buf).into_response();
    *out.headers_mut() = hdrs;
    out
}

struct BodyState {
    body: tokio::sync::mpsc::UnboundedReceiver<Bytes>,
    lease: Option<fluid_compute::Lease>,
    wait_until_ms: u64,
}

/// Build a `DeploymentInfo` view from a stored deployment.
/// DNS-safe subdomain slug: lowercase, only `[a-z0-9-]`, no leading/trailing or
/// repeated dashes. Used to build branch/commit alias labels from arbitrary
/// branch names (e.g. `feature/Login` -> `feature-login`).
fn slug(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    mapped
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Immutable commit URL label: `<project>-<shortsha>` (Vercel's per-commit URL).
fn commit_alias(project: &str, commit: &str) -> String {
    let short: String = commit.chars().take(7).collect();
    format!("{}-{}", slug(project), slug(&short))
}

/// Branch URL label: `<project>-git-<branch>` — always points at the latest
/// deployment on that branch (Vercel's per-branch URL).
fn branch_alias(project: &str, branch: &str) -> String {
    format!("{}-git-{}", slug(project), slug(branch))
}

/// Point `key` at deployment `id` only if `id` is "newer" than whatever the alias
/// currently resolves to — ranked by (production, created_at). Keeps branch/commit
/// aliases tracking the right deployment even when records restore out of order.
fn set_alias_if_newer(st: &mut GwState, key: &str, id: &DeploymentId) {
    let Some(cand) = st
        .deployments
        .get(id)
        .map(|d| (d.production, d.created_at_ms))
    else {
        return;
    };
    let win = match st.aliases.get(key).and_then(|cur| st.deployments.get(cur)) {
        None => true,
        Some(ex) => cand > (ex.production, ex.created_at_ms),
    };
    if win {
        st.aliases.insert(key.to_string(), id.clone());
    }
}

/// Insert the immutable per-deployment alias plus the commit + branch URL aliases
/// for a deployment already present in `st.deployments`.
fn insert_deploy_aliases(st: &mut GwState, id: &DeploymentId) {
    st.aliases.insert(id.as_str().to_string(), id.clone());
    let meta = st
        .deployments
        .get(id)
        .map(|d| (d.project.clone(), d.git.clone()));
    if let Some((project, Some(g))) = meta {
        if !g.commit.is_empty() {
            set_alias_if_newer(st, &commit_alias(&project, &g.commit), id);
        }
        if !g.branch.is_empty() {
            set_alias_if_newer(st, &branch_alias(&project, &g.branch), id);
        }
    }
}

/// The public wildcard domain user deployments are actually reachable on
/// (`*.{apps_domain}`), published once at startup by the node.
///
/// The reported aliases used to be hardcoded `format!("{project}.localhost")`,
/// a local-dev default that outlived local dev: a PRODUCTION deployment
/// reported "Aliased to shoomoo.localhost" while genuinely serving on
/// shoomoo.shadw.app, sending anyone who trusted the deploy log to a dead
/// hostname.
///
/// This is display/reporting only and CANNOT affect routing — every host
/// lookup (`host_deployment_id`, `serves_host`, `attribution_for_host`,
/// `select`) keys on the FIRST LABEL of the host (`h.split('.').next()`), so
/// the alias map is keyed by bare subdomain and the suffix here is never
/// matched against anything. Changing it corrects what users are told without
/// touching what the router does.
static PUBLIC_APPS_DOMAIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Publish the node's apps domain for deployment-URL reporting. Idempotent;
/// the first non-empty value wins. Unset (local dev, tests) keeps the previous
/// `.localhost` behavior byte-for-byte.
pub fn set_public_apps_domain(domain: &str) {
    let d = domain.trim().trim_matches('.');
    if !d.is_empty() {
        let _ = PUBLIC_APPS_DOMAIN.set(d.to_ascii_lowercase());
    }
}

/// `<sub>.<apps domain>`, or `<sub>.localhost` when no domain is configured.
fn public_host(sub: &str) -> String {
    match PUBLIC_APPS_DOMAIN.get() {
        Some(d) => format!("{sub}.{d}"),
        None => format!("{sub}.localhost"),
    }
}

fn view_of(d: &Deployment) -> DeploymentInfo {
    let has_static = d.manifest.static_dir.is_some();
    let has_fn = !d.manifest.functions.is_empty();
    let kind = match (has_static, has_fn) {
        (true, true) => "fullstack",
        (false, true) => "function",
        _ => "static",
    };
    // Vercel's 3 URL types, surfaced so the dashboard can link each deployment to
    // its own immutable commit URL + branch URL (not just the production domain).
    let commit_alias = d
        .git
        .as_ref()
        .filter(|g| !g.commit.is_empty())
        .map(|g| public_host(&commit_alias(&d.project, &g.commit)))
        .unwrap_or_default();
    let branch_alias = d
        .git
        .as_ref()
        .filter(|g| !g.branch.is_empty())
        .map(|g| public_host(&branch_alias(&d.project, &g.branch)))
        .unwrap_or_default();
    DeploymentInfo {
        id: d.id.clone(),
        project: d.project.clone(),
        project_incarnation: d.project_incarnation,
        functions: d
            .manifest
            .functions
            .iter()
            .map(|f| f.name.clone())
            .collect(),
        created_at_ms: d.created_at_ms,
        alias: public_host(&d.project),
        commit_alias,
        branch_alias,
        id_alias: public_host(d.id.as_str()),
        // Immutable build environment (a superseded prod build stays "production").
        target: if d.target.is_empty() {
            if d.production {
                "production".into()
            } else {
                "preview".into()
            }
        } else {
            d.target.clone()
        },
        state: d.state,
        creator: d.creator.clone(),
        git: d.git.clone(),
        production: d.production,
        kind: kind.to_string(),
        framework: d.manifest.framework.clone(),
        features: fluid_core::DeploymentFeatures {
            redirects: d.manifest.redirects.len(),
            rewrites: d.manifest.rewrites.len(),
            middleware: d.manifest.middleware.is_some(),
            edge_functions: d.manifest.edge_function_count(),
            serverless_functions: d
                .manifest
                .functions
                .iter()
                .filter(|f| f.runtime != "edge")
                .count(),
        },
        tenant: d.tenant.clone(),
        // Stamped public raw-port bindings, so the fleet-deployments gossip
        // carries the `public_port` → deployment mapping to every edge node
        // (the generic raw proxy's routing table).
        raw_ports: d.manifest.raw_port_bindings(),
        // Dedicated public IPv4, hoisted the same way as `raw_ports` above.
        dedicated_ipv4: d.manifest.dedicated_ipv4_binding(),
        // Browser-eligible functions + their artifact descriptors, so the
        // admission-validating leader can tie a donor's digest to a real build
        // artifact for deployments hosted on OTHER nodes.
        browser_functions: d
            .manifest
            .functions
            .iter()
            .filter_map(|f| {
                f.browser_artifact
                    .clone()
                    .map(|artifact| fluid_core::BrowserFunctionRef {
                        name: f.name.clone(),
                        artifact,
                    })
            })
            .collect(),
        // …and the negative half: every function the build evaluated and
        // declined, with the reason. Without this a deployment that is ready
        // but unlisted is indistinguishable from one that was never evaluated,
        // which is exactly the "my opted-in function just isn't there" report.
        // Filtered on `browser_artifact.is_none()` so a function that later
        // became eligible can never report both.
        browser_ineligible: d
            .manifest
            .functions
            .iter()
            .filter(|f| f.browser_artifact.is_none())
            .filter_map(|f| {
                f.browser_ineligible_reason
                    .clone()
                    .map(|reason| fluid_core::BrowserIneligibility {
                        function: f.name.clone(),
                        reason,
                    })
            })
            .collect(),
        // The browser-database opt-in block, verbatim (raw policy, resolved at
        // the point of use) — same cross-node resolution reason as
        // `browser_functions` above.
        browser_db: d.manifest.browser_db.clone(),
    }
}

fn content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") | Some("mjs") => "text/javascript",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod route_policy_tests {
    use super::*;
    use fluid_core::{Manifest, RouteClass, RoutePolicy};

    fn dep_with(policies: Vec<RoutePolicy>) -> Deployment {
        Deployment {
            id: DeploymentId::from("dpl-test".to_string()),
            project: "p".into(),
            root: PathBuf::from("/tmp"),
            runtime_workdir: Some(PathBuf::from("/tmp")),
            manifest: Manifest {
                project: "p".into(),
                route_policies: policies,
                ..Default::default()
            },
            created_at_ms: 0,
            state: fluid_core::DeployState::Ready,
            project_incarnation: None,
            creator: String::new(),
            git: None,
            production: true,
            target: "production".into(),
            tenant: String::new(),
            marketplace_placement: None,
        }
    }

    fn resp(status: StatusCode, cache: Option<&str>) -> Response {
        let mut b = Response::builder().status(status);
        if let Some(c) = cache {
            b = b.header(header::CACHE_CONTROL, c);
        }
        b.body(Body::empty()).unwrap().into_response()
    }

    fn cc(r: &Response) -> Option<String> {
        r.headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn no_policies_is_byte_identical_noop() {
        let dep = dep_with(vec![]);
        let r = apply_route_policy(resp(StatusCode::OK, None), &dep, "/anything");
        assert!(cc(&r).is_none());
        assert!(!r.headers().contains_key("x-hive-route-class"));
    }

    #[test]
    fn isr_route_gets_synthesized_cache_and_class_header() {
        let dep = dep_with(vec![RoutePolicy {
            pattern: "/blog/[slug]".into(),
            class: RouteClass::Isr,
            revalidate: Some(120),
        }]);
        let r = apply_route_policy(resp(StatusCode::OK, None), &dep, "/blog/hello");
        assert_eq!(
            cc(&r).as_deref(),
            Some("public, s-maxage=120, stale-while-revalidate")
        );
        assert_eq!(r.headers().get("x-hive-route-class").unwrap(), "isr");
    }

    #[test]
    fn origin_cache_control_is_never_overridden() {
        let dep = dep_with(vec![RoutePolicy {
            pattern: "/blog/[slug]".into(),
            class: RouteClass::Isr,
            revalidate: Some(120),
        }]);
        let r = apply_route_policy(
            resp(StatusCode::OK, Some("private, no-store")),
            &dep,
            "/blog/hello",
        );
        assert_eq!(
            cc(&r).as_deref(),
            Some("private, no-store"),
            "app intent wins"
        );
        // class header is still tagged for observability.
        assert_eq!(r.headers().get("x-hive-route-class").unwrap(), "isr");
    }

    #[test]
    fn dynamic_route_tagged_but_no_synthetic_cache() {
        let dep = dep_with(vec![RoutePolicy {
            pattern: "/api/claw".into(),
            class: RouteClass::ApiNode,
            revalidate: None,
        }]);
        let r = apply_route_policy(resp(StatusCode::OK, None), &dep, "/api/claw");
        assert!(cc(&r).is_none(), "dynamic defers to origin");
        assert_eq!(r.headers().get("x-hive-route-class").unwrap(), "api_node");
    }

    #[test]
    fn non_success_status_not_cached() {
        let dep = dep_with(vec![RoutePolicy {
            pattern: "/blog/[slug]".into(),
            class: RouteClass::Isr,
            revalidate: Some(120),
        }]);
        let r = apply_route_policy(
            resp(StatusCode::INTERNAL_SERVER_ERROR, None),
            &dep,
            "/blog/hello",
        );
        assert!(cc(&r).is_none(), "errors are not cached");
    }
}

#[cfg(test)]
mod failure_class_tests {
    use super::*;
    use fluid_core::FailureClass;

    #[test]
    fn classify_lease_error_maps_quota_and_capacity() {
        // Quota breach -> 429 throttle.
        let q = classify_lease_error("function 'app:fn' saturated (TenantQuota)");
        assert_eq!(q, FailureClass::TenantThrottled);
        assert_eq!(q.status(), 429);
        assert_eq!(q.code(), "TENANT_THROTTLED");
        // Concurrency saturation / cold-start cap / anything else -> 503 capacity.
        for es in [
            "function 'app:fn' saturated (ConcurrencyLimit)",
            "function 'app:fn' saturated (ColdStartCap)",
            "cold-start coalesce timed out",
            "provision failed: backend stub",
        ] {
            let c = classify_lease_error(es);
            assert_eq!(c, FailureClass::CapacityExhausted, "for {es}");
            assert_eq!(c.status(), 503);
            assert_eq!(c.code(), "CAPACITY_EXHAUSTED");
        }
    }

    #[test]
    fn classify_lease_error_matches_fluid_bail_format() {
        // Lock the cross-crate contract: the string fluid-compute actually bails
        // with on a tenant-quota NACK must classify as TenantThrottled. If
        // fluid-compute changes its Debug/bail format, this fails loudly here
        // instead of silently downgrading throttles to 503 in production.
        let reason = fluid_compute::NackReason::TenantQuota;
        let bail = format!("function 'app:fn' saturated ({reason:?})");
        assert!(
            bail.contains("TenantQuota"),
            "fluid bail string was: {bail}"
        );
        assert_eq!(classify_lease_error(&bail), FailureClass::TenantThrottled);
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;

    #[test]
    fn pct_decode_and_query() {
        let q = parse_query("url=%2Fa%2Fb.png&w=640&q=75");
        assert_eq!(q[0], ("url".to_string(), "/a/b.png".to_string()));
        assert_eq!(q[1], ("w".to_string(), "640".to_string()));
    }

    #[test]
    fn host_and_pattern_matching() {
        assert!(host_matches("example.com", "example.com"));
        assert!(!host_matches("example.com", "evil.com"));
        assert!(host_matches("**.example.com", "cdn.images.example.com"));
        assert!(host_matches("*.example.com", "cdn.example.com"));
        assert!(!host_matches("*.example.com", "a.b.example.com"));
        assert!(pattern_matches("^/account123/.*$", "/account123/pic.png"));
        assert!(!pattern_matches("^/account123/.*$", "/other/pic.png"));
        assert!(pattern_matches("/imgs/**", "/imgs/a/b.png"));
        assert!(pattern_matches("/imgs/*", "/imgs/a.png"));
        assert!(!pattern_matches("/imgs/*", "/imgs/a/b.png"));
    }

    #[test]
    fn optimize_resizes_and_encodes() {
        // Build a 100x50 opaque RGB image, encode to PNG, then optimize to w=40.
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 50));
        let mut src = std::io::Cursor::new(Vec::new());
        img.write_to(&mut src, image::ImageFormat::Png).unwrap();
        let (out, ctype) = optimize_bytes(src.get_ref(), 40, 75, false, &[]).expect("optimized");
        // Opaque -> JPEG, and the decoded result is 40px wide.
        assert_eq!(ctype, "image/jpeg");
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.width(), 40);
        assert_eq!(decoded.height(), 20);
    }

    #[test]
    fn static_cache_classification() {
        assert_eq!(
            static_cache_control("/_next/static/chunks/main.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            static_cache_control("/assets/index-4f3a9c2b.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            static_cache_control("/main.1a2b3c4d.css"),
            "public, max-age=31536000, immutable"
        );
        // Non-hashed assets + HTML get the safe revalidating default.
        assert_eq!(
            static_cache_control("/index.html"),
            "public, max-age=0, must-revalidate"
        );
        assert_eq!(
            static_cache_control("/styles.css"),
            "public, max-age=0, must-revalidate"
        );
        assert_eq!(
            static_cache_control("/bootstrap5.css"),
            "public, max-age=0, must-revalidate"
        ); // not a hex hash
        assert_eq!(
            static_cache_control("/documentation.html"),
            "public, max-age=0, must-revalidate"
        );
        assert!(!is_hashed_asset("react-dom.js"));
        assert!(is_hashed_asset("index-4f3a9c2b.js"));
    }

    #[test]
    fn remote_allow_list() {
        let cfg = fluid_core::ImagesConfig {
            remote_patterns: vec![fluid_core::RemotePattern {
                protocol: Some("https".into()),
                hostname: "example.com".into(),
                port: None,
                pathname: Some("^/a/.*$".into()),
                search: None,
            }],
            ..Default::default()
        };
        assert!(remote_allowed(&cfg, "https://example.com/a/pic.png"));
        assert!(!remote_allowed(&cfg, "https://example.com/b/pic.png"));
        assert!(!remote_allowed(&cfg, "http://example.com/a/pic.png")); // wrong scheme
        assert!(!remote_allowed(&cfg, "https://evil.com/a/pic.png"));
    }
}
