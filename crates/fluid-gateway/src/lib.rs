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
use fluid_compute::{func_key, Fluid, FunctionStats};
use fluid_core::{Deployment, DeploymentId, DeploymentInfo, DeployRequest, Manifest, RouteTarget};
use fluid_tunnel::TunnelClient;
use hive_backend::connect_endpoint;
use hive_core::{now_ms, CellId};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
        })
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
        self.deploy_full(root, manifest, "you".into(), None, true, fluid_core::DeployState::Ready, String::new())
    }

    /// Name of the active isolation backend ("mock" | "firecracker").
    pub fn backend_name(&self) -> &'static str {
        self.fluid.backend_name()
    }

    /// Pack a built deployment's output so the serving cells can reach it (only
    /// meaningful for an isolated backend; a no-op for the same-host mock).
    pub async fn deliver_build(&self, image: &str, build_dir: &std::path::Path) -> anyhow::Result<()> {
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
        let tenant = if tenant.trim().is_empty() { "personal".to_string() } else { tenant };
        let id = DeploymentId::new();
        let workdir_root = root.clone();
        let cell_image = manifest.image.clone().unwrap_or_else(|| self.image.clone());
        for f in &manifest.functions {
            let key = func_key(id.as_str(), &f.name);
            self.fluid
                .register(key, f.clone(), cell_image.clone(), workdir_root.clone(), tenant.clone());
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
            target: if production { "production".into() } else { "preview".into() },
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

    /// The git source of a project's newest deployment (for "redeploy").
    pub fn git_for_project(&self, project: &str) -> Option<fluid_core::GitSource> {
        let st = self.state.lock();
        st.deployments
            .values()
            .filter(|d| d.project == project)
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
        let cell_image = rec.manifest.image.clone().unwrap_or_else(|| self.image.clone());
        for f in &rec.manifest.functions {
            let key = func_key(id.as_str(), &f.name);
            self.fluid.register(key, f.clone(), cell_image.clone(), rec.root.clone(), rec.tenant.clone());
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
                if rec.production { "production".into() } else { "preview".into() }
            } else {
                rec.target
            },
            tenant: rec.tenant,
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
    fn select(&self, host: Option<&str>) -> Option<Deployment> {
        let st = self.state.lock();
        if let Some(h) = host {
            let h = h.split(':').next().unwrap_or(h); // strip port
            let sub = h.split('.').next().unwrap_or(h);
            if let Some(id) = st.aliases.get(sub) {
                return st.deployments.get(id).cloned();
            }
        }
        st.default.as_ref().and_then(|id| st.deployments.get(id).cloned())
    }

    /// The deployment id a request `host` resolves to (its subdomain alias), if
    /// any. Exposes the same alias resolution `select` uses — handy for debugging
    /// and for asserting which deployment a project host points at.
    pub fn host_deployment_id(&self, host: &str) -> Option<String> {
        let h = host.split(':').next().unwrap_or(host);
        let sub = h.split('.').next().unwrap_or(h);
        self.state.lock().aliases.get(sub).map(|id| id.as_str().to_string())
    }

    /// Does THIS node actually have a deployment aliased for `host`'s subdomain?
    /// Exact alias match (no default fallback) — used by mesh routing to decide
    /// whether to serve locally or proxy to the peer that really hosts it.
    pub fn serves_host(&self, host: &str) -> bool {
        let h = host.split(':').next().unwrap_or(host);
        let sub = h.split('.').next().unwrap_or(h);
        self.state.lock().aliases.contains_key(sub)
    }

    /// All host subdomains this node serves (project aliases + deployment ids),
    /// published to peers so the mesh knows where each deployment lives.
    pub fn served_hosts(&self) -> Vec<String> {
        self.state.lock().aliases.keys().cloned().collect()
    }

    /// Projects this node hosts that are **container** deployments (a function with
    /// the `container` runtime) — these are the stateful workloads coordinated by a
    /// single-owner lease. Functions/static sites are excluded (stateless).
    pub fn container_projects(&self) -> Vec<String> {
        let st = self.state.lock();
        let mut out: Vec<String> = st
            .deployments
            .values()
            .filter(|d| d.manifest.functions.iter().any(|f| f.runtime == "container"))
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
            .map(|d| d.manifest.functions.iter().any(|f| f.runtime == "container"))
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

#[derive(Serialize)]
struct TunnelStats {
    tunnels_opened: u64,
    tunnels_reused: u64,
    reuse_pct: f64,
    live_tunnels: usize,
}

async fn admin_tunnels(State(gw): State<Arc<Gateway>>) -> Json<TunnelStats> {
    let opened = gw.tunnels_opened.load(Ordering::Relaxed);
    let reused = gw.tunnels_reused.load(Ordering::Relaxed);
    let total = opened + reused;
    let reuse_pct = if total > 0 { reused as f64 / total as f64 } else { 0.0 };
    Json(TunnelStats {
        tunnels_opened: opened,
        tunnels_reused: reused,
        reuse_pct,
        live_tunnels: gw.tunnels.lock().await.len(),
    })
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
        if let Some(resp) = vercel_insights(&parts.method, &path) {
            return resp;
        }
    }

    let dep = match gw.select(host.as_deref()) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no deployment").into_response(),
    };

    // 1) Redirects mapped from the framework build run first (respond immediately).
    if let Some((location, status)) = dep.manifest.redirect_for(&path) {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
        return Response::builder()
            .status(code)
            .header(header::LOCATION, location)
            .body(Body::empty())
            .unwrap()
            .into_response();
    }
    // 2) Rewrites map the public path to an internal one (client URL unchanged).
    let path = dep.manifest.rewrite_path(&path);

    match dep.manifest.resolve(&path) {
        RouteTarget::Static => serve_static(&dep, &path).await,
        RouteTarget::Function(name) => {
            let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "body too large").into_response(),
            };
            proxy_function(&gw, &dep, &name, &parts.method, &path_q, &parts.headers, body_bytes)
                .await
        }
    }
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
            .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(body))
            .unwrap()
            .into_response()
    };
    let accepted = || (StatusCode::ACCEPTED, [(header::CONTENT_TYPE, "text/plain")], "ok").into_response();

    match (method, path) {
        (&Method::GET, "/_vercel/insights/script.js") => Some(js(ANALYTICS_JS)),
        (&Method::GET, "/_vercel/speed-insights/script.js") => Some(js(SPEED_JS)),
        (&Method::POST, "/_vercel/insights/view")
        | (&Method::POST, "/_vercel/insights/event")
        | (&Method::POST, "/_vercel/speed-insights/vitals") => Some(accepted()),
        // Unknown _vercel path: 204 so the client never sees a hard 404.
        _ => Some((StatusCode::NO_CONTENT, "").into_response()),
    }
}

async fn serve_static(dep: &Deployment, path: &str) -> Response {
    let static_dir = dep.manifest.static_dir.clone().unwrap_or_else(|| ".".into());
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
            ([(header::CONTENT_TYPE, ctype)], bytes).into_response()
        }
        Err(_) => {
            // SPA-ish fallback: try index.html at the static root.
            let idx = base.join("index.html");
            if let Ok(bytes) = tokio::fs::read(&idx).await {
                ([(header::CONTENT_TYPE, "text/html")], bytes).into_response()
            } else {
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        }
    }
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
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
        .collect();

    // Per-function max duration (Vercel default 300s) — bounds the whole
    // invocation; on timeout we return 504 without affecting other requests
    // sharing the instance (error isolation).
    let max_dur = Duration::from_secs(gw.fluid.max_duration_secs(&key).unwrap_or(300).max(1));

    const MAX_REROUTES: usize = 3;
    let mut last_err = String::from("unknown");
    for attempt in 0..MAX_REROUTES {
        let lease = match gw.fluid.lease(&key).await {
            Ok(l) => l,
            Err(e) => {
                warn!(func = %key, error = %e, "lease failed");
                return (StatusCode::SERVICE_UNAVAILABLE, format!("no capacity: {e}"))
                    .into_response();
            }
        };
        let cell = lease.cell_id().clone();
        let ep = lease.endpoint.clone();

        let (client, reused) = match gw.tunnel_for(&cell, &ep).await {
            Ok(c) => c,
            Err(e) => {
                last_err = e.to_string();
                tracing::debug!(cell = %cell, attempt, error = %last_err, "tunnel connect failed");
                drop(lease);
                gw.fluid.mark_dead(&key, &cell).await;
                gw.drop_tunnel(&cell).await;
                continue;
            }
        };

        tracing::debug!(cell = %cell, reused, attempt, "dispatching request over tunnel");
        let req_fut = client.request(method.as_str(), path_q, hvec.clone(), &body);
        match tokio::time::timeout(max_dur, req_fut).await {
            Err(_) => {
                // Exceeded max duration — 504, do not reroute (the instance is
                // fine; only this invocation is over budget).
                drop(lease);
                return (StatusCode::GATEWAY_TIMEOUT, "FUNCTION_INVOCATION_TIMEOUT").into_response();
            }
            Ok(Ok(resp)) => {
                tracing::debug!(cell = %cell, status = resp.status, "got response head");
                return build_response(lease, cell, reused, attempt, resp).await;
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
                tracing::debug!(cell = %cell, reused, attempt, error = %last_err, "request failed");
                drop(lease);
                // Tunnel-level failure: if it's closed the instance is gone.
                if client.is_closed() {
                    gw.fluid.mark_dead(&key, &cell).await;
                    gw.drop_tunnel(&cell).await;
                }
                // else: nack/overload — just reroute to another instance.
            }
        }
    }
    (StatusCode::BAD_GATEWAY, format!("upstream failed after reroute: {last_err}")).into_response()
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
) -> Response {
    let mut hdrs = HeaderMap::new();
    let mut is_stream = false;
    for (k, v) in &resp.headers {
        let kl = k.to_ascii_lowercase();
        if kl == "content-length" || kl == "transfer-encoding" || kl == "connection" {
            continue;
        }
        if kl == "content-type" && v.to_ascii_lowercase().contains("event-stream") {
            is_stream = true;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            hdrs.append(name, val);
        }
    }
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
    // a max-duration so a stalled upstream becomes a 504 rather than hanging.
    let mut buf = Vec::new();
    let drained = tokio::time::timeout(Duration::from_secs(30), async {
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
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    mapped.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
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
    let Some(cand) = st.deployments.get(id).map(|d| (d.production, d.created_at_ms)) else {
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
    let meta = st.deployments.get(id).map(|d| (d.project.clone(), d.git.clone()));
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
        functions: d.manifest.functions.iter().map(|f| f.name.clone()).collect(),
        created_at_ms: d.created_at_ms,
        alias: format!("{}.localhost", d.project),
        commit_alias,
        branch_alias,
        id_alias: format!("{}.localhost", d.id.as_str()),
        // Immutable build environment (a superseded prod build stays "production").
        target: if d.target.is_empty() {
            if d.production { "production".into() } else { "preview".into() }
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
            serverless_functions: d.manifest.functions.iter().filter(|f| f.runtime != "edge").count(),
        },
        tenant: d.tenant.clone(),
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
