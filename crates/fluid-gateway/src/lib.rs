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
        self.deploy_full(root, manifest, "you".into(), None, true)
    }

    /// Full deploy with creator + git provenance + production flag.
    pub fn deploy_full(
        &self,
        root: String,
        manifest: Manifest,
        creator: String,
        git: Option<fluid_core::GitSource>,
        production: bool,
    ) -> DeploymentInfo {
        let id = DeploymentId::new();
        let workdir_root = root.clone();
        for f in &manifest.functions {
            let key = func_key(id.as_str(), &f.name);
            self.fluid
                .register(key, f.clone(), self.image.clone(), workdir_root.clone());
        }
        let dep = Deployment {
            id: id.clone(),
            project: manifest.project.clone(),
            root: PathBuf::from(root),
            manifest: manifest.clone(),
            created_at_ms: now_ms(),
            state: fluid_core::DeployState::Ready,
            creator,
            git,
            production,
        };
        let info = view_of(&dep);
        let mut st = self.state.lock();
        st.aliases.insert(dep.project.clone(), id.clone());
        // Per-deployment preview URL: <deployment-id>.localhost resolves to this
        // exact deployment even after newer ones become the project default.
        st.aliases.insert(id.as_str().to_string(), id.clone());
        st.deployments.insert(id.clone(), dep);
        st.default = Some(id);
        info
    }

    /// Resolve which project serves a given request host (the same way the
    /// public router selects), so events can be attributed to a project.
    pub fn project_for_host(&self, host: &str) -> Option<String> {
        self.select(Some(host)).map(|d| d.project)
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
            })
            .collect()
    }

    /// Restore a deployment from a persisted record (preserves its id), and
    /// re-register its functions with the Fluid pool. Used on boot.
    pub fn restore(&self, rec: fluid_core::DeployRecord) {
        let id = DeploymentId::from(rec.id.clone());
        for f in &rec.manifest.functions {
            let key = func_key(id.as_str(), &f.name);
            self.fluid.register(key, f.clone(), self.image.clone(), rec.root.clone());
        }
        let dep = Deployment {
            id: id.clone(),
            project: rec.project.clone(),
            root: PathBuf::from(&rec.root),
            manifest: rec.manifest,
            created_at_ms: rec.created_at_ms,
            state: fluid_core::DeployState::Ready,
            creator: rec.creator,
            git: rec.git,
            production: rec.production,
        };
        let mut st = self.state.lock();
        st.aliases.insert(dep.project.clone(), id.clone());
        st.aliases.insert(id.as_str().to_string(), id.clone());
        st.deployments.insert(id.clone(), dep);
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
fn view_of(d: &Deployment) -> DeploymentInfo {
    let has_static = d.manifest.static_dir.is_some();
    let has_fn = !d.manifest.functions.is_empty();
    let kind = match (has_static, has_fn) {
        (true, true) => "fullstack",
        (false, true) => "function",
        _ => "static",
    };
    DeploymentInfo {
        id: d.id.clone(),
        project: d.project.clone(),
        functions: d.manifest.functions.iter().map(|f| f.name.clone()).collect(),
        created_at_ms: d.created_at_ms,
        alias: format!("{}.localhost", d.project),
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
