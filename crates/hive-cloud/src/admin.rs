//! The node's admin/control API — everything the dashboard talks to.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
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
        .route("/v1/routing/rewrites", post(add_rewrite))
        .route("/v1/cron", get(cron_list).post(cron_add))
        .route("/v1/cron/:id", delete(cron_del))
        .route("/v1/workflows", get(wf_list).post(wf_define))
        .route("/v1/workflows/:id/run", post(wf_run))
        .route("/v1/workflows/runs", get(wf_runs))
        .route("/v1/sandbox", post(sandbox))
        .route("/deployments", get(dep_list).post(dep_create))
        .route("/v1/git/deploy", post(git_deploy))
        .route("/v1/builds/:id", get(build_get))
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
        .route("/v1/domains", get(domains_list))
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
    Json(req): Json<fluid_core::GitDeployRequest>,
) -> Json<Value> {
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

// ---- Deployments (previews) ----

async fn dep_list(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.gw.list()))
}

async fn dep_create(
    State(c): State<Arc<CloudState>>,
    Json(req): Json<fluid_core::DeployRequest>,
) -> Json<Value> {
    let info = c.gw.deploy(req.root, req.manifest);
    Json(json!(info))
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

async fn regions(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.registry.regions()))
}

async fn functions(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.fluid.stats()))
}

#[derive(Deserialize)]
struct LimitQ {
    limit: Option<usize>,
}

async fn logs(State(c): State<Arc<CloudState>>, Query(q): Query<LimitQ>) -> Json<Value> {
    Json(json!(c.recent_events(q.limit.unwrap_or(100))))
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
