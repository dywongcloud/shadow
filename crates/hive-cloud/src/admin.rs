//! The node's admin/control API — everything the dashboard talks to.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put},
    Json, Router,
};
use hive_core::{now_ms, BuildJob, JobState, ResourceSpec};
use hive_edge::{
    bot::BotPolicy, routing::{Redirect, Rewrite}, waf::WafRule, CronJob, WorkflowDef,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::CloudState;

pub fn router(cloud: Arc<CloudState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/overview", get(overview))
        .route("/v1/nodes", get(nodes))
        .route("/v1/cluster", get(cluster_status))
        .route("/v1/anycast", get(anycast_table))
        .route("/v1/ratelimit", get(ratelimit_get).put(ratelimit_put))
        .route("/v1/regions", get(regions))
        .route("/v1/logs", get(logs))
        .route("/v1/functions", get(functions))
        .route("/v1/waf", get(waf_get))
        .route("/v1/waf/rules", post(waf_add_rule))
        .route("/v1/waf/rules/:id", delete(waf_del_rule))
        .route("/v1/waf/managed", put(waf_managed))
        .route("/v1/bot", get(bot_get).put(bot_put))
        .route("/v1/cdn", get(cdn_get))
        .route("/v1/cdn/purge", post(cdn_purge))
        .route("/v1/concurrency", get(concurrency_get))
        .route("/v1/routing", get(routing_get))
        .route("/v1/routing/redirects", post(add_redirect))
        .route("/v1/routing/redirects/delete", post(del_redirect))
        .route("/v1/routing/rewrites", post(add_rewrite))
        .route("/v1/routing/rewrites/delete", post(del_rewrite))
        .route("/v1/cron", get(cron_list).post(cron_add))
        .route("/v1/cron/:id", delete(cron_del))
        .route("/v1/workflows", get(wf_list).post(wf_define))
        .route("/v1/workflows/:id/run", post(wf_run))
        .route("/v1/workflows/runs", get(wf_runs))
        .route("/v1/sandbox", post(sandbox))
        .route("/deployments", get(dep_list).post(dep_create))
        .route("/v1/deployments/:id", delete(dep_delete))
        .route("/v1/deployments/:id/promote", post(dep_promote))
        .route("/v1/projects/:project", delete(project_delete))
        .route("/v1/projects/:project/redeploy", post(project_redeploy))
        .route("/v1/git/deploy", post(git_deploy))
        .route("/v1/builds/:id", get(build_get))
        .route("/v1/build/frameworks", get(build_frameworks))
        .route("/v1/nodes/announce", post(node_announce))
        .route("/v1/token", post(mint_token))
        .route("/v1/auth", get(auth_status))
        .route("/v1/regions/catalog", get(region_catalog))
        .route("/v1/projects/:project/settings", get(project_settings_get))
        .route("/v1/projects/:project/build", put(project_build_put))
        .route("/v1/projects/:project/functions", put(project_functions_put))
        .route("/v1/projects/:project/env", post(project_env_put))
        .route("/v1/projects/:project/env/:key", delete(project_env_delete))
        .route("/v1/projects/:project/domains", post(project_domain_add))
        .route("/v1/projects/:project/team", put(project_team_put))
        .route("/v1/domains", get(domains_list))
        // ---- Teams ----
        .route("/v1/teams", get(teams_list).post(team_create))
        .route("/v1/teams/:slug", get(team_get))
        .route("/v1/teams/:slug/members", post(team_add_member))
        .route("/v1/teams/:slug/members/:email", delete(team_remove_member))
        // ---- API keys (tenant-scoped platform tokens) ----
        .route("/v1/apikeys", get(apikeys_list).post(apikey_create))
        .route("/v1/apikeys/:id", delete(apikey_revoke))
        // ---- Webhooks ----
        .route("/v1/webhooks", get(webhooks_all).post(webhook_create_team))
        .route("/v1/webhooks/events", get(webhook_events))
        .route("/v1/webhooks/deliveries", get(webhook_deliveries))
        .route("/v1/webhooks/:id", delete(webhook_delete))
        .route("/v1/projects/:project/webhooks", get(webhooks_for_project).post(webhook_create))
        // ---- Databases / storage ----
        .route("/v1/databases", get(databases_list).post(database_create))
        .route("/v1/databases/:id", get(database_get).delete(database_delete))
        .route("/v1/databases/:id/credentials", get(database_credentials))
        .route("/v1/projects/:project/databases", get(databases_for_project))
        // Functional storage REST surface (Blob / Queue / Vector).
        .route("/v1/storage/blob/:bucket", get(blob_list_keys))
        .route("/v1/storage/blob/:bucket/:key", get(blob_get).put(blob_put))
        .route("/v1/storage/queue/:queue", get(queue_depth).post(queue_push))
        .route("/v1/storage/queue/:queue/pop", post(queue_pop))
        .route("/v1/storage/vector/:index", post(vector_upsert))
        .route("/v1/storage/vector/:index/query", post(vector_query))
        // Pub/Sub + Realtime (WebSocket secure streaming)
        .route("/v1/storage/pubsub/:topic", get(pubsub_info))
        .route("/v1/storage/pubsub/:topic/publish", post(pubsub_publish))
        .route("/v1/ws/pubsub/:topic", get(ws_pubsub))
        .route("/v1/ws/realtime/:room", get(ws_realtime))
        .route("/v1/ws/echo", get(ws_echo))
        // ---- Secure compute (private backend tunnels) ----
        .route("/v1/securelinks", get(securelinks_list).post(securelink_create))
        .route("/v1/securelinks/:id", delete(securelink_delete))
        // ---- Monitoring ----
        .route("/v1/metrics", get(metrics_get))
        // ---- Owner / ops dashboard ----
        .route("/v1/admin/overview", get(admin_overview))
        .route("/v1/admin/audit", get(admin_audit))
        .route("/v1/incidents", get(incidents_list).post(incident_open))
        .route("/v1/incidents/:id/updates", post(incident_update))
        .with_state(cloud)
}

// ---- Auth (JWT) ----

#[derive(Deserialize)]
struct TokenReq {
    #[serde(default = "default_sub")]
    sub: String,
    #[serde(default = "default_tenant")]
    tenant: String,
    #[serde(default = "default_role")]
    role: String,
}
fn default_sub() -> String { "user".into() }
fn default_tenant() -> String { "default".into() }
fn default_role() -> String { "owner".into() }

async fn auth_status() -> Json<Value> {
    Json(json!({ "enforced": crate::auth::enforced() }))
}

async fn mint_token(Json(req): Json<TokenReq>) -> Result<Json<Value>, (StatusCode, String)> {
    match crate::auth::issue(&req.sub, &req.tenant, &req.role, 8 * 3600) {
        Ok(token) => Ok(Json(json!({ "token": token, "expires_in": 8 * 3600 }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

// ---- Project settings (env vars, build config, function settings) ----

async fn region_catalog() -> Json<Value> {
    Json(crate::project_settings::region_catalog())
}

async fn project_settings_get(State(c): State<Arc<CloudState>>, Path(project): Path<String>) -> Json<Value> {
    Json(json!(c.projects.get_masked(&project)))
}

async fn project_build_put(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(build): Json<crate::project_settings::BuildConfig>,
) -> Json<Value> {
    c.projects.set_build(&project, build);
    crate::persist::persist(&c);
    Json(json!(c.projects.get_masked(&project)))
}

async fn project_functions_put(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(f): Json<crate::project_settings::FunctionSettings>,
) -> Json<Value> {
    c.projects.set_functions(&project, f);
    crate::persist::persist(&c);
    Json(json!(c.projects.get_masked(&project)))
}

async fn project_env_put(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(v): Json<crate::project_settings::EnvVar>,
) -> Json<Value> {
    c.projects.put_env(&project, v);
    crate::persist::persist(&c);
    Json(json!(c.projects.get_masked(&project)))
}

async fn project_env_delete(
    State(c): State<Arc<CloudState>>,
    Path((project, key)): Path<(String, String)>,
) -> Json<Value> {
    c.projects.delete_env(&project, &key);
    crate::persist::persist(&c);
    Json(json!(c.projects.get_masked(&project)))
}

// ---- Domains ----

#[derive(Deserialize)]
struct AddDomain {
    domain: String,
}

async fn project_domain_add(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(b): Json<AddDomain>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let ok = c.gw.add_alias(&b.domain, &project);
    if !ok {
        return Err((StatusCode::NOT_FOUND, format!("no deployment for project '{project}'")));
    }
    c.projects.add_domain(&project, b.domain.clone());
    crate::persist::persist(&c);
    let ev = c.event(&c.region, "DOMAIN", &b.domain, "/", 200, "domain-add", &project);
    c.record(ev);
    Ok(Json(json!({ "domain": b.domain, "project": project, "attached": true })))
}

async fn domains_list(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let pairs = c.projects.all_domains();
    Json(json!(pairs.into_iter().map(|(p, d)| json!({ "project": p, "domain": d })).collect::<Vec<_>>()))
}

// ---- Git deploy (Import Git Repository) ----

async fn git_deploy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(req): Json<fluid_core::GitDeployRequest>,
) -> Json<Value> {
    // Assign the (new) project to the requesting tenant so it shows under their
    // team only.
    let t = tenant(&c, &headers);
    let project = req.project.clone().unwrap_or_else(|| crate::git::project_name_from_url(&req.repo_url));
    c.projects.set_team(&project, &t);
    crate::persist::persist(&c);
    // Start the build asynchronously; the dashboard streams logs via /v1/builds/:id.
    let build_id = crate::git::start_build(c.clone(), req);
    Json(json!({ "build_id": build_id }))
}

async fn build_get(
    State(c): State<Arc<CloudState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    c.builds.get(&id).map(|b| Json(json!(b))).ok_or(StatusCode::NOT_FOUND)
}

/// Framework-Defined Infrastructure: the catalog of frameworks the builder can
/// detect and compile into the Build Output API.
async fn build_frameworks() -> Json<Value> {
    Json(json!(fluid_build::PRESETS))
}

// ---- Deployments (previews) ----

/// The tenant (team slug) for a request. A platform **API key**
/// (`Authorization: Bearer hive_…`) scopes the request to the key's team; the
/// dashboard's `x-hive-team` header is the fallback; default "personal".
fn tenant(c: &Arc<CloudState>, h: &HeaderMap) -> String {
    if let Some(team) = api_key_team(c, h) {
        return team;
    }
    h.get("x-hive-team")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "personal".into())
}

/// If a valid platform API key is presented, return its team.
fn api_key_team(c: &Arc<CloudState>, h: &HeaderMap) -> Option<String> {
    let auth = h.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let tok = auth.strip_prefix("Bearer ")?;
    if !tok.starts_with("hive_") {
        return None;
    }
    c.apikeys.verify(tok).map(|k| k.team)
}

/// Normalize an owner slug: empty/absent => "personal".
fn norm(team: &str) -> &str {
    if team.is_empty() { "personal" } else { team }
}

async fn dep_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
    let list: Vec<_> = c
        .gw
        .list()
        .into_iter()
        .filter(|d| norm(&c.projects.team_of(&d.project)) == t)
        .collect();
    Json(json!(list))
}

async fn dep_create(
    State(c): State<Arc<CloudState>>,
    Json(req): Json<fluid_core::DeployRequest>,
) -> Json<Value> {
    let info = c.gw.deploy(req.root, req.manifest);
    Json(json!(info))
}

/// Roll back / promote: make an existing deployment the project's production.
async fn dep_promote(
    State(c): State<Arc<CloudState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let info = c.gw.promote(&id).ok_or(StatusCode::NOT_FOUND)?;
    crate::persist::persist(&c);
    let ev = c.event(&c.region, "PROMOTE", &info.alias, "/", 200, "deploy", &format!("rolled back to {id}"));
    c.record(ev);
    crate::webhooks::dispatch(&c.webhooks, &info.project, "deployment.promoted",
        json!({ "id": id, "project": info.project, "url": format!("https://{}", info.alias) }));
    Ok(Json(json!(info)))
}

/// Record a control-plane event (shows in Recent Activity / Activity / Audit),
/// scoped to a project so it respects tenant filtering.
fn record_event(c: &Arc<CloudState>, project: &str, action: &str, detail: &str) {
    c.record(crate::state::Event {
        ts_ms: now_ms(),
        region: c.region.clone(),
        method: "DELETE".into(),
        host: format!("{project}.localhost"),
        path: "/".into(),
        status: 200,
        action: action.to_string(),
        detail: detail.to_string(),
        project: project.to_string(),
    });
}

/// Delete a single deployment (unregisters its functions).
async fn dep_delete(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    let project = c.gw.remove(&id).await;
    if let Some(p) = &project {
        record_event(&c, p, "delete", &format!("deleted deployment {id}"));
    }
    crate::persist::persist(&c);
    Json(json!({ "removed": id, "project": project }))
}

/// Delete an entire project: all its deployments + settings.
async fn project_delete(State(c): State<Arc<CloudState>>, Path(project): Path<String>) -> Json<Value> {
    let ids = c.gw.remove_project(&project).await;
    record_event(&c, &project, "delete", &format!("deleted project {project} ({} deployment(s))", ids.len()));
    c.projects.remove(&project);
    crate::persist::persist(&c);
    crate::webhooks::dispatch(&c.webhooks, &project, "project.removed", json!({ "project": project, "deployments": ids.len() }));
    Json(json!({ "project": project, "removed_deployments": ids }))
}

/// Redeploy a project's newest git source (create a fresh deployment).
async fn project_redeploy(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let git = c.gw.git_for_project(&project).ok_or(StatusCode::NOT_FOUND)?;
    let req = fluid_core::GitDeployRequest {
        repo_url: git.repo_url,
        branch: Some(git.branch).filter(|b| !b.is_empty()),
        project: Some(project),
        creator: Some("you".into()),
        production: true,
    };
    let build_id = crate::git::start_build(c.clone(), req);
    Ok(Json(json!({ "build_id": build_id })))
}

// ---- Mesh: a peer announces itself to us ----

async fn node_announce(
    State(c): State<Arc<CloudState>>,
    Json(node): Json<hive_edge::NodeInfo>,
) -> Json<Value> {
    c.registry.upsert_peer(node);
    Json(json!(c.registry.nodes()))
}

async fn overview(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let (reqs, blocked) = c.counters();
    let (hits, misses, stale, entries, ratio) = c.cdn.stats();
    let fstats = c.fluid.stats();
    let instances: usize = fstats.iter().map(|f| f.instances).sum();
    let cc = c.limiter.stats();
    Json(json!({
        "node": c.node_name,
        "region": c.region,
        "regions": c.registry.regions(),
        "nodes": c.registry.nodes().len(),
        "deployments": c.gw.list().len(),
        "functions": fstats.len(),
        "instances": instances,
        "requests": reqs,
        "blocked": blocked,
        "cdn": { "hits": hits, "misses": misses, "stale": stale, "entries": entries, "hit_ratio": ratio },
        "concurrency": cc,
        "waf_rules": c.waf.rules().len(),
        "waf_managed": c.waf.managed_enabled(),
        "cron_jobs": c.cron.list().len(),
        "workflows": c.workflows.defs().len(),
    }))
}

async fn nodes(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.registry.nodes()))
}

async fn cluster_status(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let members: Vec<String> = c.registry.nodes().into_iter().map(|n| n.id).collect();
    Json(json!(c.cluster.status(members)))
}

/// Anycast routing table: every node with latency/health, and the node this edge
/// would route a request to (lowest-latency healthy, region-preferred).
async fn anycast_table(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let selected = c.registry.anycast(Some(&c.region));
    Json(json!({
        "region": c.region,
        "selected": selected.as_ref().map(|n| n.name.clone()),
        "table": c.registry.routing_table(),
    }))
}

async fn ratelimit_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.ratelimit.stats()))
}

#[derive(Deserialize)]
struct RateLimitBody {
    enabled: bool,
    #[serde(default)]
    limit: u32,
    #[serde(default)]
    window_ms: u64,
}

async fn ratelimit_put(State(c): State<Arc<CloudState>>, Json(b): Json<RateLimitBody>) -> Json<Value> {
    c.ratelimit.set(b.enabled, b.limit, b.window_ms);
    Json(json!(c.ratelimit.stats()))
}

async fn regions(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.registry.regions()))
}

async fn functions(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.fluid.stats()))
}

#[derive(Deserialize)]
struct LimitQ {
    limit: Option<usize>,
    /// Filter events to a project (matches the deployment host subdomain).
    project: Option<String>,
    /// Free-text search across path/host/detail.
    q: Option<String>,
}

async fn logs(State(c): State<Arc<CloudState>>, headers: HeaderMap, Query(q): Query<LimitQ>) -> Json<Value> {
    let limit = q.limit.unwrap_or(100);
    let t = tenant(&c, &headers);
    let mut evs = c.recent_events(2000);
    // Tenant scope: only events for projects owned by this team. Infra events
    // (empty project) are shown to everyone.
    evs.retain(|e| e.project.is_empty() || norm(&c.projects.team_of(&e.project)) == t);
    if let Some(p) = q.project.as_ref().filter(|p| !p.is_empty()) {
        let pl = p.to_lowercase();
        evs.retain(|e| {
            e.project.to_lowercase() == pl
                || e.host.to_lowercase().contains(&pl)
                || e.detail.to_lowercase().contains(&pl)
        });
    }
    if let Some(s) = q.q.as_ref().filter(|s| !s.is_empty()) {
        let sl = s.to_lowercase();
        evs.retain(|e| {
            e.path.to_lowercase().contains(&sl)
                || e.host.to_lowercase().contains(&sl)
                || e.detail.to_lowercase().contains(&sl)
                || e.action.to_lowercase().contains(&sl)
        });
    }
    evs.truncate(limit);
    Json(json!(evs))
}

// ---- WAF ----

async fn waf_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!({ "managed": c.waf.managed_enabled(), "rules": c.waf.rules() }))
}

async fn waf_add_rule(State(c): State<Arc<CloudState>>, Json(rule): Json<WafRule>) -> Json<Value> {
    c.waf.add_rule(rule);
    crate::persist::persist(&c);
    Json(json!({ "rules": c.waf.rules() }))
}

async fn waf_del_rule(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    let kept: Vec<WafRule> = c.waf.rules().into_iter().filter(|r| r.id != id).collect();
    c.waf.set_rules(kept);
    crate::persist::persist(&c);
    Json(json!({ "rules": c.waf.rules() }))
}

#[derive(Deserialize)]
struct ManagedBody {
    enabled: bool,
}

async fn waf_managed(State(c): State<Arc<CloudState>>, Json(b): Json<ManagedBody>) -> Json<Value> {
    c.waf.set_managed(b.enabled);
    Json(json!({ "managed": c.waf.managed_enabled() }))
}

// ---- Bot management ----

async fn bot_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(*c.bot_policy.read()))
}

async fn bot_put(State(c): State<Arc<CloudState>>, Json(p): Json<BotPolicy>) -> Json<Value> {
    *c.bot_policy.write() = p;
    Json(json!(*c.bot_policy.read()))
}

// ---- CDN ----

async fn cdn_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let (hits, misses, stale, entries, ratio) = c.cdn.stats();
    Json(json!({ "hits": hits, "misses": misses, "stale": stale, "entries": entries, "hit_ratio": ratio }))
}

async fn cdn_purge(State(c): State<Arc<CloudState>>) -> Json<Value> {
    c.cdn.purge();
    Json(json!({ "purged": true }))
}

// ---- Concurrency scaling ----

async fn concurrency_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.limiter.stats()))
}

// ---- Routing layer ----

async fn routing_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!({ "redirects": c.router.redirects(), "rewrites": c.router.rewrites() }))
}

async fn add_redirect(State(c): State<Arc<CloudState>>, Json(r): Json<Redirect>) -> Json<Value> {
    c.router.add_redirect(r);
    crate::persist::persist(&c);
    Json(json!({ "redirects": c.router.redirects() }))
}

async fn add_rewrite(State(c): State<Arc<CloudState>>, Json(r): Json<Rewrite>) -> Json<Value> {
    c.router.add_rewrite(r);
    crate::persist::persist(&c);
    Json(json!({ "rewrites": c.router.rewrites() }))
}

#[derive(Deserialize)]
struct BySource {
    source: String,
}

async fn del_redirect(State(c): State<Arc<CloudState>>, Json(b): Json<BySource>) -> Json<Value> {
    let kept: Vec<Redirect> = c.router.redirects().into_iter().filter(|r| r.source != b.source).collect();
    c.router.set_redirects(kept);
    crate::persist::persist(&c);
    Json(json!({ "redirects": c.router.redirects() }))
}

async fn del_rewrite(State(c): State<Arc<CloudState>>, Json(b): Json<BySource>) -> Json<Value> {
    let kept: Vec<Rewrite> = c.router.rewrites().into_iter().filter(|r| r.source != b.source).collect();
    c.router.set_rewrites(kept);
    crate::persist::persist(&c);
    Json(json!({ "rewrites": c.router.rewrites() }))
}

// ---- Cron ----

async fn cron_list(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.cron.list()))
}

async fn cron_add(State(c): State<Arc<CloudState>>, Json(job): Json<CronJob>) -> Result<Json<Value>, (StatusCode, String)> {
    match c.cron.add(job) {
        Ok(j) => Ok(Json(json!(j))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

async fn cron_del(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    c.cron.remove(&id);
    Json(json!({ "removed": id }))
}

// ---- Workflows ----

async fn wf_list(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.workflows.defs()))
}

async fn wf_define(State(c): State<Arc<CloudState>>, Json(def): Json<WorkflowDef>) -> Json<Value> {
    c.workflows.define(def);
    Json(json!(c.workflows.defs()))
}

async fn wf_runs(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.workflows.runs()))
}

async fn wf_run(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let invoker = crate::wf_invoker(c.clone());
    match c.workflows.start(&id, invoker) {
        Ok(run_id) => Ok(Json(json!({ "run_id": run_id }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

// ---- Sandbox (run arbitrary code in an isolated cell) ----

#[derive(Deserialize)]
struct SandboxReq {
    #[serde(default = "default_image")]
    image: String,
    commands: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}
fn default_image() -> String {
    "default".into()
}
fn default_timeout() -> u64 {
    60
}

#[derive(Serialize)]
struct SandboxResp {
    job_id: String,
    state: String,
    exit_code: Option<i32>,
    logs: Vec<String>,
    duration_ms: u64,
}

async fn sandbox(State(c): State<Arc<CloudState>>, Json(req): Json<SandboxReq>) -> Json<SandboxResp> {
    let started = now_ms();
    let job = BuildJob::builder(req.image)
        .commands(req.commands)
        .resources(ResourceSpec {
            vcpus: 1,
            mem_mib: 512,
            disk_mib: 1024,
            timeout_secs: req.timeout_secs,
        })
        .build();
    let id = c.hive.submit(job);

    // Wait for completion (bounded).
    let mut state = JobState::Queued;
    for _ in 0..(req.timeout_secs.max(5) * 20) {
        if let Some(v) = c.hive.job_view(&id) {
            state = v.state;
            if v.state.is_terminal() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let view = c.hive.job_view(&id);
    let logs = c
        .hive
        .subscribe_logs(&id)
        .map(|(backlog, _rx)| backlog.into_iter().map(|l| l.line).collect())
        .unwrap_or_default();
    Json(SandboxResp {
        job_id: id.to_string(),
        state: format!("{state:?}"),
        exit_code: view.and_then(|v| v.exit_code),
        logs,
        duration_ms: now_ms().saturating_sub(started),
    })
}

// ============================ Teams ============================

#[derive(Deserialize)]
struct CreateTeam {
    name: String,
    #[serde(default = "default_plan")]
    plan: String,
}
fn default_plan() -> String {
    "pro".into()
}

#[derive(Deserialize)]
struct AddMember {
    email: String,
    #[serde(default = "default_member_role")]
    role: crate::teams::Role,
}
fn default_member_role() -> crate::teams::Role {
    crate::teams::Role::Member
}

async fn teams_list(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.teams.list()))
}

async fn team_get(State(c): State<Arc<CloudState>>, Path(slug): Path<String>) -> Result<Json<Value>, StatusCode> {
    c.teams.get(&slug).map(|t| Json(json!(t))).ok_or(StatusCode::NOT_FOUND)
}

async fn team_create(State(c): State<Arc<CloudState>>, Json(b): Json<CreateTeam>) -> Json<Value> {
    let t = c.teams.create(&b.name, &b.plan, &c.owner_email);
    crate::persist::persist(&c);
    Json(json!(t))
}

async fn team_add_member(
    State(c): State<Arc<CloudState>>,
    Path(slug): Path<String>,
    Json(b): Json<AddMember>,
) -> Result<Json<Value>, StatusCode> {
    let t = c.teams.add_member(&slug, &b.email, b.role).ok_or(StatusCode::NOT_FOUND)?;
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

async fn team_remove_member(
    State(c): State<Arc<CloudState>>,
    Path((slug, email)): Path<(String, String)>,
) -> Result<Json<Value>, StatusCode> {
    let t = c.teams.remove_member(&slug, &email).ok_or(StatusCode::NOT_FOUND)?;
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

#[derive(Deserialize)]
struct ProjectTeam {
    team: String,
    #[serde(default = "default_true_b")]
    preview_protection: bool,
}
fn default_true_b() -> bool {
    true
}

async fn project_team_put(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(b): Json<ProjectTeam>,
) -> Json<Value> {
    c.projects.set_team(&project, &b.team);
    c.projects.set_preview_protection(&project, b.preview_protection);
    crate::persist::persist(&c);
    Json(json!(c.projects.get_masked(&project)))
}

// ============================ API keys ============================

#[derive(Deserialize)]
struct CreateApiKey {
    name: String,
    #[serde(default)]
    role: String,
}

async fn apikeys_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
    Json(json!(c.apikeys.list(&t).iter().map(|k| k.public()).collect::<Vec<_>>()))
}

async fn apikey_create(State(c): State<Arc<CloudState>>, headers: HeaderMap, Json(b): Json<CreateApiKey>) -> Json<Value> {
    let t = tenant(&c, &headers);
    let (key, token) = c.apikeys.create(&b.name, &t, &b.role);
    crate::persist::persist(&c);
    // The plaintext token is returned exactly once.
    let mut v = key.public();
    v["token"] = json!(token);
    Json(v)
}

async fn apikey_revoke(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers);
    let ok = c.apikeys.revoke(&id, &t);
    crate::persist::persist(&c);
    Json(json!({ "revoked": ok, "id": id }))
}

// ============================ Webhooks ============================

async fn webhook_events() -> Json<Value> {
    Json(json!(crate::webhooks::ALL_EVENTS))
}

async fn webhooks_all(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.webhooks.list(None)))
}

#[derive(Deserialize)]
struct CreateTeamWebhook {
    url: String,
    #[serde(default)]
    events: Vec<String>,
    /// Empty or ["*"] = all projects; otherwise one webhook per project.
    #[serde(default)]
    projects: Vec<String>,
}

async fn webhook_create_team(State(c): State<Arc<CloudState>>, Json(b): Json<CreateTeamWebhook>) -> Json<Value> {
    let targets: Vec<String> = if b.projects.is_empty() || b.projects.iter().any(|p| p == "*") {
        vec!["*".into()]
    } else {
        b.projects.clone()
    };
    let mut created = Vec::new();
    for p in targets {
        created.push(c.webhooks.add(crate::webhooks::Webhook {
            id: String::new(),
            project: p,
            url: b.url.clone(),
            events: b.events.clone(),
            secret: String::new(),
            enabled: true,
            created_ms: 0,
        }));
    }
    crate::persist::persist(&c);
    Json(json!(created))
}

async fn webhooks_for_project(State(c): State<Arc<CloudState>>, Path(project): Path<String>) -> Json<Value> {
    Json(json!(c.webhooks.list(Some(&project))))
}

#[derive(Deserialize)]
struct CreateWebhook {
    url: String,
    #[serde(default)]
    events: Vec<String>,
}

async fn webhook_create(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(b): Json<CreateWebhook>,
) -> Json<Value> {
    let wh = c.webhooks.add(crate::webhooks::Webhook {
        id: String::new(),
        project,
        url: b.url,
        events: b.events,
        secret: String::new(),
        enabled: true,
        created_ms: 0,
    });
    crate::persist::persist(&c);
    Json(json!(wh))
}

async fn webhook_delete(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    c.webhooks.remove(&id);
    crate::persist::persist(&c);
    Json(json!({ "removed": id }))
}

async fn webhook_deliveries(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.webhooks.deliveries(100)))
}

// ============================ Databases ============================

async fn databases_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
    let list: Vec<_> = c
        .databases
        .list(None)
        .into_iter()
        .filter(|d| norm(&d.team) == t)
        .collect();
    Json(json!(list))
}

async fn databases_for_project(State(c): State<Arc<CloudState>>, Path(project): Path<String>) -> Json<Value> {
    Json(json!(c.databases.list(Some(&project))))
}

async fn database_get(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    c.databases.get(&id).map(|d| Json(json!(d))).ok_or(StatusCode::NOT_FOUND)
}

async fn database_credentials(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    c.databases.get_raw(&id).map(|d| Json(json!(d))).ok_or(StatusCode::NOT_FOUND)
}

async fn database_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(mut req): Json<crate::databases::ProvisionReq>,
) -> Json<Value> {
    let cloud = c.clone();
    if req.team.trim().is_empty() {
        req.team = tenant(&c, &headers);
    }
    let project = req.project.clone();
    let db = crate::databases::provision(c.databases.clone(), c.region.clone(), req, move |d| {
        crate::persist::persist(&cloud);
        crate::webhooks::dispatch(
            &cloud.webhooks,
            &project,
            "database.ready",
            json!({ "id": d.id, "name": d.name, "kind": d.kind, "status": d.status }),
        );
    });
    crate::persist::persist(&c);
    crate::webhooks::dispatch(
        &c.webhooks,
        &db.project,
        "database.created",
        json!({ "id": db.id, "name": db.name, "kind": db.kind }),
    );
    Json(json!(db))
}

async fn database_delete(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    if let Some(d) = c.databases.get_raw(&id) {
        if let Some(container) = d.container {
            // Best-effort teardown of the backing container.
            let _ = tokio::process::Command::new("podman")
                .args(["rm", "-f", &container])
                .env("PATH", format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}", std::env::var("PATH").unwrap_or_default()))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }
    }
    c.databases.remove_db(&id);
    crate::persist::persist(&c);
    Json(json!({ "removed": id }))
}

// ---- Functional storage REST (Blob / Queue / Vector) ----

async fn blob_put(
    State(c): State<Arc<CloudState>>,
    Path((bucket, key)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Json<Value> {
    let size = body.len();
    c.databases.blob_put(&bucket, &key, body.to_vec());
    Json(json!({ "bucket": bucket, "key": key, "size": size, "url": format!("/v1/storage/blob/{bucket}/{key}") }))
}

async fn blob_get(
    State(c): State<Arc<CloudState>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    match c.databases.blob_get(&bucket, &key) {
        Some(data) => Ok(data.into_response()),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn blob_list_keys(State(c): State<Arc<CloudState>>, Path(bucket): Path<String>) -> Json<Value> {
    Json(json!({ "bucket": bucket, "keys": c.databases.blob_list(&bucket) }))
}

#[derive(Deserialize)]
struct QueueMsg {
    message: Value,
}

async fn queue_push(State(c): State<Arc<CloudState>>, Path(queue): Path<String>, Json(b): Json<QueueMsg>) -> Json<Value> {
    let depth = c.databases.queue_push(&queue, b.message.to_string());
    Json(json!({ "queue": queue, "depth": depth }))
}

async fn queue_pop(State(c): State<Arc<CloudState>>, Path(queue): Path<String>) -> Json<Value> {
    let msg = c.databases.queue_pop(&queue);
    let parsed = msg.as_ref().and_then(|m| serde_json::from_str::<Value>(m).ok());
    Json(json!({ "queue": queue, "message": parsed.or(msg.map(Value::String)), "depth": c.databases.queue_depth(&queue) }))
}

async fn queue_depth(State(c): State<Arc<CloudState>>, Path(queue): Path<String>) -> Json<Value> {
    Json(json!({ "queue": queue, "depth": c.databases.queue_depth(&queue) }))
}

#[derive(Deserialize)]
struct VectorUpsert {
    id: String,
    vector: Vec<f32>,
    #[serde(default)]
    metadata: Value,
}

async fn vector_upsert(State(c): State<Arc<CloudState>>, Path(index): Path<String>, Json(b): Json<VectorUpsert>) -> Json<Value> {
    c.databases.vector_upsert(&index, &b.id, b.vector, b.metadata);
    Json(json!({ "index": index, "id": b.id, "upserted": true }))
}

#[derive(Deserialize)]
struct VectorQuery {
    vector: Vec<f32>,
    #[serde(default = "default_topk")]
    top_k: usize,
}
fn default_topk() -> usize {
    5
}

async fn vector_query(State(c): State<Arc<CloudState>>, Path(index): Path<String>, Json(b): Json<VectorQuery>) -> Json<Value> {
    Json(json!({ "index": index, "matches": c.databases.vector_query(&index, &b.vector, b.top_k) }))
}

// ---- Pub/Sub + Realtime (WebSocket secure streaming) ----

async fn pubsub_info(State(c): State<Arc<CloudState>>, Path(topic): Path<String>) -> Json<Value> {
    Json(json!({
        "topic": topic,
        "subscribers": c.databases.subscriber_count(&topic),
        "published": c.databases.published_count(&topic),
    }))
}

async fn pubsub_publish(State(c): State<Arc<CloudState>>, Path(topic): Path<String>, Json(b): Json<QueueMsg>) -> Json<Value> {
    let delivered = c.databases.publish(&topic, b.message.to_string());
    Json(json!({ "topic": topic, "delivered": delivered }))
}

async fn ws_pubsub(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    Path(topic): Path<String>,
) -> axum::response::Response {
    let mut rx = c.databases.subscribe(&topic);
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let _ = socket
            .send(Message::Text(json!({ "type": "subscribed", "topic": topic }).to_string()))
            .await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(m) => { if socket.send(Message::Text(m)).await.is_err() { break; } }
                    Err(_) => continue, // lagged: skip
                },
                client = socket.recv() => match client {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    })
}

async fn ws_realtime(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    Path(room): Path<String>,
) -> axum::response::Response {
    let mut rx = c.databases.subscribe(&room);
    let db = c.databases.clone();
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let _ = socket
            .send(Message::Text(json!({ "type": "joined", "room": room }).to_string()))
            .await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(m) => { if socket.send(Message::Text(m)).await.is_err() { break; } }
                    Err(_) => continue,
                },
                client = socket.recv() => match client {
                    // Bidirectional: a client message is broadcast to the whole room.
                    Some(Ok(Message::Text(t))) => { db.publish(&room, t); }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    })
}

async fn ws_echo(ws: axum::extract::ws::WebSocketUpgrade) -> axum::response::Response {
    ws.on_upgrade(|mut socket| async move {
        use axum::extract::ws::Message;
        while let Some(Ok(msg)) = socket.recv().await {
            match msg {
                Message::Text(t) => { if socket.send(Message::Text(format!("echo: {t}"))).await.is_err() { break; } }
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

// ====================== Secure compute (WireGuard) ======================

async fn securelinks_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    Json(json!(c.securelinks.list(&tenant(&c, &headers))))
}

async fn securelink_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(mut req): Json<crate::securelink::ProvisionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if req.team.trim().is_empty() {
        req.team = tenant(&c, &headers);
    }
    let region = c.region.clone();
    let rec = c.securelinks.provision(req, &region).await.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Wire the function datapath: inject the connector's local address as a
    // project env var, so deployed functions reach the private backend through
    // the encrypted tunnel transparently.
    if !rec.project.is_empty() && !rec.env_var.is_empty() {
        c.projects.put_env(&rec.project, crate::project_settings::EnvVar {
            key: rec.env_var.clone(),
            value: rec.local_addr.clone(),
            target: "all".into(),
            sensitive: false,
            updated_ms: now_ms(),
        });
        crate::persist::persist(&c);
        let ev = c.event(&c.region, "SECURE", &rec.target, "/", 200, "deploy",
            &format!("secure tunnel {} → {} wired to {}.{}", rec.local_addr, rec.target, rec.project, rec.env_var));
        c.record(ev);
    }
    Ok(Json(json!(rec)))
}

async fn securelink_delete(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    c.securelinks.remove(&id);
    Json(json!({ "removed": id }))
}

// ============================ Monitoring ============================

#[derive(Deserialize)]
struct MetricsQ {
    minutes: Option<usize>,
    project: Option<String>,
}

async fn metrics_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, Query(q): Query<MetricsQ>) -> Json<Value> {
    let minutes = q.minutes.unwrap_or(60).min(180);
    let t = tenant(&c, &headers);
    let project = q.project.as_deref().filter(|p| !p.is_empty());
    let series = c.metrics.series(minutes, now_ms(), project);
    let total_req: u64 = series.iter().map(|b| b.requests).sum();
    let total_err: u64 = series.iter().map(|b| b.errors + b.client_err).sum();
    let total_blocked: u64 = series.iter().map(|b| b.blocked).sum();
    let hits: u64 = series.iter().map(|b| b.cache_hits).sum();
    let miss: u64 = series.iter().map(|b| b.cache_miss).sum();
    let cache_ratio = if hits + miss == 0 { 0.0 } else { hits as f64 / (hits + miss) as f64 };
    let err_rate = if total_req == 0 { 0.0 } else { total_err as f64 / total_req as f64 };
    Json(json!({
        "series": series,
        "totals": {
            "requests": total_req,
            "errors": total_err,
            "blocked": total_blocked,
            "error_rate": err_rate,
            "cache_hit_ratio": cache_ratio,
        },
        "status_distribution": c.metrics.status_distribution(),
        "top_paths": c.metrics.top_paths(10).into_iter().map(|(p, n)| json!({ "path": p, "count": n })).collect::<Vec<_>>(),
        "projects": c.metrics.project_totals(minutes, now_ms()).into_iter()
            .filter(|(p, _)| norm(&c.projects.team_of(p)) == t)
            .map(|(p, n)| json!({ "project": p, "requests": n })).collect::<Vec<_>>(),
    }))
}

// ============================ Owner / ops dashboard ============================

async fn admin_overview(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let (reqs, blocked) = c.counters();
    let fstats = c.fluid.stats();
    let instances: usize = fstats.iter().map(|f| f.instances).sum();
    let nodes = c.registry.nodes();
    let dbs = c.databases.list(None);
    let live_dbs = dbs.iter().filter(|d| d.mode == "live").count();
    // Recent error rate from the metrics buckets (last 30m).
    let series = c.metrics.series(30, now_ms(), None);
    let req30: u64 = series.iter().map(|b| b.requests).sum();
    let err30: u64 = series.iter().map(|b| b.errors).sum();
    let err_rate = if req30 == 0 { 0.0 } else { err30 as f64 / req30 as f64 };
    Json(json!({
        "owner": c.owner_email,
        "teams": c.teams.count(),
        "projects": c.gw.list().iter().map(|d| d.project.clone()).collect::<std::collections::BTreeSet<_>>().len(),
        "deployments": c.gw.list().len(),
        "databases": { "total": dbs.len(), "live": live_dbs },
        "nodes": nodes.len(),
        "regions": c.registry.regions(),
        "instances": instances,
        "requests": reqs,
        "blocked": blocked,
        "error_rate_30m": err_rate,
        "incidents_open": c.incidents.open_count(),
        "cluster": c.cluster.status(nodes.iter().map(|n| n.id.clone()).collect()),
        "webhooks": c.webhooks.list(None).len(),
    }))
}

async fn admin_audit(State(c): State<Arc<CloudState>>) -> Json<Value> {
    // Operational/audit feed = control-plane actions (deploys, domains, cron,
    // WAF, incidents) drawn from the event stream.
    let evs = c.recent_events(2000);
    let audit: Vec<_> = evs
        .into_iter()
        .filter(|e| {
            matches!(
                e.action.as_str(),
                "deploy" | "delete" | "domain-add" | "cron" | "waf-deny" | "throttled" | "redirect" | "rewrite"
            )
        })
        .take(200)
        .collect();
    Json(json!(audit))
}

async fn incidents_list(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.incidents.list()))
}

async fn incident_open(State(c): State<Arc<CloudState>>, Json(req): Json<crate::incidents::OpenReq>) -> Json<Value> {
    let inc = c.incidents.open(req);
    crate::persist::persist(&c);
    crate::webhooks::dispatch(&c.webhooks, "*", "incident.opened", json!({ "id": inc.id, "title": inc.title, "severity": inc.severity }));
    Json(json!(inc))
}

async fn incident_update(
    State(c): State<Arc<CloudState>>,
    Path(id): Path<String>,
    Json(req): Json<crate::incidents::UpdateReq>,
) -> Result<Json<Value>, StatusCode> {
    let resolved = matches!(req.status, crate::incidents::IncidentStatus::Resolved);
    let inc = c.incidents.update(&id, req).ok_or(StatusCode::NOT_FOUND)?;
    crate::persist::persist(&c);
    if resolved {
        crate::webhooks::dispatch(&c.webhooks, "*", "incident.resolved", json!({ "id": inc.id, "title": inc.title }));
    }
    Ok(Json(json!(inc)))
}
