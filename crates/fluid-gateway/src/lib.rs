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

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use fluid_compute::{func_key, Fluid, FunctionStats, Lease};
use fluid_core::{DeployRequest, Deployment, DeploymentId, DeploymentInfo, Manifest, RouteTarget};
use fluid_tunnel::TunnelClient;
use hive_backend::connect_endpoint;
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
    /// project name -> current deployment id.
    aliases: HashMap<String, DeploymentId>,
    default: Option<DeploymentId>,
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
                aliases: HashMap::new(),
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

    /// Where the active backend expects a DELIVERED build inside a cell, or
    /// `None` when its cells read the host build dir directly. Branch on THIS,
    /// not on `backend_name()`, to decide whether `deliver_build` must run — a
    /// name comparison silently excludes any backend added later.
    pub fn delivered_workdir(&self) -> Option<&'static str> {
        self.fluid.delivered_workdir()
    }

    /// Pack a built deployment's output so the serving cells can reach it (only
    /// meaningful for an isolated backend; a no-op for the same-host mock).
    pub async fn deliver_build(
        &self,
        image: &str,
        build_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        self.fluid.deliver_build(image, build_dir).await
    }

    /// Full deploy with creator + git provenance + production flag + owning tenant.
    /// `tenant` (empty = "personal") tags the deployment and every function pool /
    /// cell it spawns, so compute is partitioned and quota'd per team.
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
        // Normalize the owner once at the boundary so the stored record, the
        // function pools, and every cell agree on the tenant (empty => "personal").
        let tenant = if tenant.trim().is_empty() {
            "personal".to_string()
        } else {
            tenant
        };
        let id = DeploymentId::new();
        let workdir_root = root.clone();
        let cell_image = manifest.image.clone().unwrap_or_else(|| self.image.clone());
        for f in &manifest.functions {
            let key = func_key(id.as_str(), &f.name);
            self.fluid.register(
                key,
                f.clone(),
                cell_image.clone(),
                workdir_root.clone(),
                tenant.clone(),
            );
        }
        let dep = Deployment {
            id: id.clone(),
            project: manifest.project.clone(),
            root: PathBuf::from(root),
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
        };
        let info = view_of(&dep);
        let project = dep.project.clone();
        let mut st = self.state.lock();
        // Does this project already have a (different) production deployment? If
        // not, this deploy claims the bare production domain even when it isn't
        // itself a production deploy — so the very first deploy and the "Building…"
        // placeholder are reachable at <project>.<host> right away.
        let has_production = st
            .deployments
            .values()
            .any(|d| d.project == project && d.production && d.id != id);
        // Invariant: at most ONE production deployment per project. Promoting this
        // one to production demotes any prior production deployment of the same
        // project, so the production alias can't later resolve to a stale deployment
        // after a restart (which would serve the OLD build).
        if production {
            for d in st.deployments.values_mut() {
                if d.project == project {
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
            st.aliases.insert(project, id.clone());
            st.default = Some(id.clone());
        }
        info
    }

    /// Resolve which project serves a given request host (the same way the
    /// public router selects), so events can be attributed to a project.
    pub fn project_for_host(&self, host: &str) -> Option<String> {
        self.select(Some(host)).map(|d| d.project)
    }

    /// The full deployment a request `host` resolves to (same alias resolution the
    /// public router uses). Exposes `target`/`production` so the preview gate can
    /// decide protection by the deployment's ACTUAL environment — not by guessing
    /// from the subdomain (which wrongly flags a production deployment's commit/id
    /// URLs as previews).
    pub fn deployment_for_host(&self, host: &str) -> Option<DeploymentInfo> {
        self.select(Some(host)).map(|d| view_of(&d))
    }

    /// Attach a custom domain to a project (its first label aliases to the
    /// project's current deployment). Returns true if the project exists.
    pub fn add_alias(&self, domain: &str, project: &str) -> bool {
        let label = domain.split('.').next().unwrap_or(domain).to_string();
        let mut st = self.state.lock();
        let target = st.aliases.get(project).cloned();
        if let Some(id) = target {
            st.aliases.insert(label, id);
            true
        } else {
            false
        }
    }

    /// Promote an existing deployment to be its project's production (rollback /
    /// instant promote). Re-points the project alias + default to it.
    pub fn promote(&self, id: &str) -> Option<DeploymentInfo> {
        let did = DeploymentId::from(id.to_string());
        let mut st = self.state.lock();
        let project = st.deployments.get(&did)?.project.clone();
        // Flip production flags within the project.
        for d in st.deployments.values_mut() {
            if d.project == project {
                d.production = d.id == did;
            }
        }
        st.aliases.insert(project, did.clone());
        st.default = Some(did.clone());
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
        for f in &dep.manifest.functions {
            self.fluid
                .update_config(&func_key(did.as_str(), &f.name), f.clone());
        }
        Some(view_of(dep))
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
        // Drop any aliases that pointed at this deployment.
        st.aliases.retain(|_, v| *v != did);
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

    /// Delete every deployment for a project. Returns the removed deployment ids.
    pub async fn remove_project(&self, project: &str) -> Vec<String> {
        let ids: Vec<String> = {
            let st = self.state.lock();
            st.deployments
                .values()
                .filter(|d| d.project == project)
                .map(|d| d.id.to_string())
                .collect()
        };
        for id in &ids {
            self.remove(id).await;
        }
        ids
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
                root: d.root.to_string_lossy().into_owned(),
                manifest: d.manifest.clone(),
                created_at_ms: d.created_at_ms,
                creator: d.creator.clone(),
                git: d.git.clone(),
                production: d.production,
                target: d.target.clone(),
                state: d.state,
                tenant: d.tenant.clone(),
            })
            .collect()
    }

    /// Restore a deployment from a persisted record (preserves its id), and
    /// re-register its functions with the Fluid pool. Used on boot.
    pub fn restore(&self, rec: fluid_core::DeployRecord) {
        let id = DeploymentId::from(rec.id.clone());
        let cell_image = rec
            .manifest
            .image
            .clone()
            .unwrap_or_else(|| self.image.clone());
        for f in &rec.manifest.functions {
            let key = func_key(id.as_str(), &f.name);
            self.fluid.register(
                key,
                f.clone(),
                cell_image.clone(),
                rec.root.clone(),
                rec.tenant.clone(),
            );
        }
        let dep = Deployment {
            id: id.clone(),
            project: rec.project.clone(),
            root: PathBuf::from(&rec.root),
            manifest: rec.manifest,
            created_at_ms: rec.created_at_ms,
            state: rec.state,
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
            // binary. Collapsing that into the literal "personal" slug handed
            // another tenant's deployment to the platform owner's real
            // namespace on every restart of a node holding a stale snapshot.
            // Fail closed: never adopt an untagged record into a live tenant.
            tenant: if rec.tenant.trim().is_empty() {
                "__untagged__".to_string()
            } else {
                rec.tenant
            },
        };
        let project = dep.project.clone();
        let mut st = self.state.lock();
        st.deployments.insert(id.clone(), dep);
        // The project (production) alias must resolve to the project's CURRENT
        // deployment, not whichever record happens to restore last. Prefer the
        // production deployment, and among equals the newest — so a stale prior
        // deployment (e.g. a superseded build) never wins the alias after a reboot.
        set_alias_if_newer(&mut st, &project, &id);
        // The per-deployment preview URL + commit/branch URLs (branch tracks the
        // newest deployment on that branch — set_alias_if_newer handles ordering).
        insert_deploy_aliases(&mut st, &id);
        st.default.get_or_insert(id);
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
        let RouteTarget::Function(name) = dep.manifest.resolve(path) else {
            return None;
        };
        let key = func_key(dep.id.as_str(), &name);
        self.fluid.lease(&key).await.ok()
    }

    fn select(&self, host: Option<&str>) -> Option<Deployment> {
        let st = self.state.lock();
        if let Some(h) = host {
            let h = h.split(':').next().unwrap_or(h); // strip port
            let sub = h.split('.').next().unwrap_or(h);
            if let Some(id) = st.aliases.get(sub) {
                return st.deployments.get(id).cloned();
            }
        }
        st.default
            .as_ref()
            .and_then(|id| st.deployments.get(id).cloned())
    }

    /// The deployment id a request `host` resolves to (its subdomain alias), if
    /// any. Exposes the same alias resolution `select` uses — handy for debugging
    /// and for asserting which deployment a project host points at.
    pub fn host_deployment_id(&self, host: &str) -> Option<String> {
        let h = host.split(':').next().unwrap_or(host);
        let sub = h.split('.').next().unwrap_or(h);
        self.state
            .lock()
            .aliases
            .get(sub)
            .map(|id| id.as_str().to_string())
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
        let sub = h.split('.').next().unwrap_or(h);
        let st = self.state.lock();
        let id = st.aliases.get(sub)?.clone();
        let project = st.deployments.get(&id)?.project.clone();
        Some((id.as_str().to_string(), project))
    }

    /// Does THIS node actually have a deployment aliased for `host`'s subdomain?
    /// Exact alias match (no default fallback) — used by mesh routing to decide
    /// whether to serve locally or proxy to the peer that really hosts it.
    pub fn serves_host(&self, host: &str) -> bool {
        let h = host.split(':').next().unwrap_or(host);
        let sub = h.split('.').next().unwrap_or(h);
        self.state.lock().aliases.contains_key(sub)
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
        let sub = h.split('.').next().unwrap_or(h);
        let st = self.state.lock();
        let id = st.aliases.get(sub)?;
        st.deployments.get(id).map(|d| d.state)
    }

    /// All host subdomains this node serves (project aliases + deployment ids),
    /// published to peers so the mesh knows where each deployment lives.
    pub fn served_hosts(&self) -> Vec<String> {
        self.state.lock().aliases.keys().cloned().collect()
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
        let sub = h.split('.').next().unwrap_or(h);
        let st = self.state.lock();
        st.aliases
            .get(sub)
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

    let resp = match dep.manifest.resolve(&path) {
        RouteTarget::Static => {
            // Adapter frameworks (OpenNext / vinext): immutable assets serve from
            // `static_dir`; on a MISS the request falls through to the origin/SSR
            // function (the CDN→function model) so dynamic routes still render.
            // When no `origin_function` is set (the common case) this is exactly
            // the previous behavior — `serve_static` with its SPA/404 fallback.
            let enc = accepted_encodings(&parts.headers);
            match dep.manifest.origin_function.clone() {
                Some(origin) => match read_static_file(&dep, &path, enc).await {
                    Some(r) => r,
                    None => {
                        let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
                            Ok(b) => b,
                            Err(_) => {
                                return (StatusCode::BAD_REQUEST, "body too large").into_response()
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
                },
                None => serve_static(&dep, &path, enc).await,
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
/// compress; the build-time precompression path (which has no request waiting
/// on it) is what covers very large assets.
const MAX_INLINE_COMPRESS_BYTES: usize = 8 * 1024 * 1024;

/// Is `sibling` a usable precompressed copy of `src` — i.e. present and not
/// older than the source? An asset rebuilt in place without its sibling being
/// refreshed must never be served as stale bytes under a fresh URL.
async fn fresh_sibling(src: &std::path::Path, sibling: &std::path::Path) -> Option<Vec<u8>> {
    let (sm, bm) = (
        tokio::fs::metadata(src).await.ok()?,
        tokio::fs::metadata(sibling).await.ok()?,
    );
    let (st, bt) = (sm.modified().ok()?, bm.modified().ok()?);
    if bt < st {
        return None;
    }
    tokio::fs::read(sibling).await.ok()
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
/// Order: an on-disk precompressed sibling (`<file>.br`, then `<file>.gz`) is
/// preferred — zero CPU per request, which is the whole point of Vercel's
/// precompress-at-build-time model. Failing that, the bytes are compressed
/// ONCE inline and the sibling is written next to the source (tmp+rename, so a
/// concurrent request never observes a partial file), making every subsequent
/// request of that asset free. `Vary: Accept-Encoding` is always set, including
/// on identity responses, because the same URL genuinely varies by request
/// header and a shared cache must not reuse one client's answer for another.
async fn static_asset_response(
    file: &std::path::Path,
    bytes: Vec<u8>,
    ctype: &str,
    cache_control: &str,
    enc: AcceptedEncodings,
) -> Response {
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
    if (want_br || want_gz) && is_compressible(ctype) && bytes.len() >= MIN_COMPRESS_BYTES {
        // 1) Precompressed sibling.
        for (want, ext, label) in [(want_br, "br", "br"), (want_gz, "gz", "gzip")] {
            if !want {
                continue;
            }
            let sib = std::path::PathBuf::from(format!("{}.{ext}", file.display()));
            if let Some(pre) = fresh_sibling(file, &sib).await {
                headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(label));
                return (headers, pre).into_response();
            }
        }
        // 2) Compress once inline, then persist for next time.
        if bytes.len() <= MAX_INLINE_COMPRESS_BYTES {
            let use_br = want_br;
            let src = bytes.clone();
            if let Ok(Some(out)) =
                tokio::task::spawn_blocking(move || compress_bytes(&src, use_br)).await
            {
                // Only worth serving if it actually got smaller.
                if out.len() < bytes.len() {
                    let ext = if use_br { "br" } else { "gz" };
                    let sib = std::path::PathBuf::from(format!("{}.{ext}", file.display()));
                    let tmp = std::path::PathBuf::from(format!(
                        "{}.{}.tmp",
                        sib.display(),
                        std::process::id()
                    ));
                    let payload = out.clone();
                    tokio::spawn(async move {
                        if tokio::fs::write(&tmp, &payload).await.is_ok() {
                            let _ = tokio::fs::rename(&tmp, &sib).await;
                        }
                    });
                    headers.insert(
                        header::CONTENT_ENCODING,
                        HeaderValue::from_static(if use_br { "br" } else { "gzip" }),
                    );
                    return (headers, out).into_response();
                }
            }
        }
    }
    (headers, bytes).into_response()
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

/// Try to read a concrete static asset for `path` from the deployment's
/// `static_dir`. Returns `Some(response)` only when an actual file (or its
/// cleanUrls `.html` sibling) exists; returns `None` on a miss WITHOUT the
/// SPA-index/404 fallback, so the caller can fall through to an origin function.
async fn read_static_file(
    dep: &Deployment,
    path: &str,
    enc: AcceptedEncodings,
) -> Option<Response> {
    let static_dir = dep
        .manifest
        .static_dir
        .clone()
        .unwrap_or_else(|| ".".into());
    let base = dep.root.join(static_dir);
    let rel = path.trim_start_matches('/');
    let mut file = base.join(rel);
    if path.ends_with('/') || rel.is_empty() {
        file = file.join("index.html");
    }
    if !is_within(&base, &file) {
        return None;
    }
    if let Ok(bytes) = tokio::fs::read(&file).await {
        let ctype = content_type(&file);
        return Some(
            static_asset_response(&file, bytes, ctype, static_cache_control(path), enc).await,
        );
    }
    // cleanUrls: `/about` -> `about.html`.
    if dep.manifest.clean_urls && !rel.is_empty() && !path.ends_with('/') {
        let html = base.join(format!("{rel}.html"));
        if is_within(&base, &html) {
            if let Ok(bytes) = tokio::fs::read(&html).await {
                return Some(
                    static_asset_response(
                        &html,
                        bytes,
                        "text/html; charset=utf-8",
                        "public, max-age=0, must-revalidate",
                        enc,
                    )
                    .await,
                );
            }
        }
    }
    None
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

async fn serve_static(dep: &Deployment, path: &str, enc: AcceptedEncodings) -> Response {
    let static_dir = dep
        .manifest
        .static_dir
        .clone()
        .unwrap_or_else(|| ".".into());
    let base = dep.root.join(static_dir);
    let rel = path.trim_start_matches('/');
    let mut file = base.join(rel);
    // Directory or root -> index.html.
    if path.ends_with('/') || rel.is_empty() {
        file = file.join("index.html");
    }
    // Path-traversal guard: resolved file must stay under base.
    if !is_within(&base, &file) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let ctype = content_type(&file);
            static_asset_response(&file, bytes, ctype, static_cache_control(path), enc).await
        }
        Err(_) => {
            // cleanUrls: serve `about.html` for a request to `/about`.
            if dep.manifest.clean_urls && !rel.is_empty() && !path.ends_with('/') {
                let html = base.join(format!("{rel}.html"));
                if is_within(&base, &html) {
                    if let Ok(bytes) = tokio::fs::read(&html).await {
                        return static_asset_response(
                            &html,
                            bytes,
                            "text/html; charset=utf-8",
                            "public, max-age=0, must-revalidate",
                            enc,
                        )
                        .await;
                    }
                }
            }
            // SPA-ish fallback: try index.html at the static root.
            let idx = base.join("index.html");
            if let Ok(bytes) = tokio::fs::read(&idx).await {
                static_asset_response(
                    &idx,
                    bytes,
                    "text/html",
                    "public, max-age=0, must-revalidate",
                    enc,
                )
                .await
            } else {
                no_static_file(dep, path)
            }
        }
    }
}

/// Vercel Image Optimization API (`/_vercel/image`, also `/_next/image`).
/// Validates the request against the deployment's `images` config, fetches the
/// source (local asset or allow-listed remote), and re-encodes it at the
/// requested width/quality. Resizing uses the pure-Rust `image` crate.
async fn serve_optimized_image(dep: &Deployment, query: &str, req_headers: &HeaderMap) -> Response {
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
        // Local asset: validate localPatterns (when set), read from static dir.
        if let Some(c) = cfg {
            if !c.local_patterns.is_empty() && !local_allowed(c, &url) {
                return bad("local url not allowed by images.localPatterns");
            }
        }
        let static_dir = dep
            .manifest
            .static_dir
            .clone()
            .unwrap_or_else(|| ".".into());
        let base = dep.root.join(static_dir);
        let rel = url
            .split('?')
            .next()
            .unwrap_or(&url)
            .trim_start_matches('/');
        let file = base.join(rel);
        if !is_within(&base, &file) {
            return bad("forbidden");
        }
        match tokio::fs::read(&file).await {
            Ok(b) => (b, rel.ends_with(".svg")),
            Err(_) => return (StatusCode::NOT_FOUND, "image not found").into_response(),
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
            let quota_ok = browser
                .invoke_quota
                .get(&target.endpoint_id)
                .is_none_or(|(window_start, count)| {
                    now.saturating_sub(*window_start) >= quota_window_ms || *count < quota_max
                });
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
    let hvec: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| {
            let n = k.as_str();
            n != "connection" && n != "content-length" && n != "host"
        })
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();

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
        .map(|g| format!("{}.localhost", commit_alias(&d.project, &g.commit)))
        .unwrap_or_default();
    let branch_alias = d
        .git
        .as_ref()
        .filter(|g| !g.branch.is_empty())
        .map(|g| format!("{}.localhost", branch_alias(&d.project, &g.branch)))
        .unwrap_or_default();
    DeploymentInfo {
        id: d.id.clone(),
        project: d.project.clone(),
        functions: d
            .manifest
            .functions
            .iter()
            .map(|f| f.name.clone())
            .collect(),
        created_at_ms: d.created_at_ms,
        alias: format!("{}.localhost", d.project),
        commit_alias,
        branch_alias,
        id_alias: format!("{}.localhost", d.id.as_str()),
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

fn is_within(base: &Path, candidate: &Path) -> bool {
    // `candidate` is `base.join(rel)`; if it stays under `base` with no `..`
    // components, it's safe.
    match candidate.strip_prefix(base) {
        Ok(rest) => !rest
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        Err(_) => false,
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
            manifest: Manifest {
                project: "p".into(),
                route_policies: policies,
                ..Default::default()
            },
            created_at_ms: 0,
            state: fluid_core::DeployState::Ready,
            creator: String::new(),
            git: None,
            production: true,
            target: "production".into(),
            tenant: String::new(),
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
