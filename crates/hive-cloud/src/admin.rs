//! The node's admin/control API — everything the dashboard talks to.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put},
    Json, Router,
};
use base64::Engine;
use hive_core::{now_ms, BuildJob, JobState, ResourceSpec};
use hive_edge::{
    bot::BotPolicy, routing::{Redirect, Rewrite}, waf::WafRule, CronJob, WorkflowDef,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command;

use crate::state::CloudState;

pub fn router(cloud: Arc<CloudState>) -> Router {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // Distinct from /healthz: liveness ("is the process serving HTTP") tells
        // you nothing about mesh MEMBERSHIP — a node can be process-alive,
        // /healthz-green, and fully isolated from its fleet (the node-a/node-b
        // incident). Unauthenticated like /healthz, for the same reason: the
        // watchdog polling it has no JWT.
        .route("/v1/mesh", get(mesh_health))
        .route("/v1/overview", get(overview))
        .route("/v1/nodes", get(nodes))
        .route("/v1/serve-hosts", get(serve_hosts))
        .route("/v1/resources", get(resources_get))
        .route("/v1/leases", get(leases_get))
        .route("/v1/guardian/heads", get(guardian_heads))
        .route("/v1/cluster", get(cluster_status))
        .route("/v1/anycast", get(anycast_table))
        .route("/v1/ratelimit", get(ratelimit_get).put(ratelimit_put))
        .route("/v1/regions", get(regions))
        .route("/v1/wqueue/enqueue", post(wqueue_enqueue))
        .route("/v1/wqueue/stats", get(wqueue_stats))
        .route("/v1/logs", get(logs))
        .route("/v1/functions", get(functions))
        .route("/v1/tunnels", get(tunnels))
        .route("/v1/relay", get(relay_stats))
        .route("/v1/waf", get(waf_get))
        .route("/v1/waf/rules", post(waf_add_rule))
        .route("/v1/waf/rules/:id", delete(waf_del_rule))
        .route("/v1/waf/managed", put(waf_managed))
        .route("/v1/bot", get(bot_get).put(bot_put))
        .route("/v1/cdn", get(cdn_get))
        .route("/v1/cdn/purge", post(cdn_purge))
        // Runtime Cache (regional data cache for tenant functions). Loopback-only
        // (admin port), reached by local function cells via the injected
        // HIVE_RUNTIME_CACHE_URL. Scope = "<project>:<environment>".
        .route("/v1/runtime-cache", get(rc_stats))
        .route("/v1/runtime-cache/entry", get(rc_get).put(rc_put).delete(rc_delete))
        .route("/v1/runtime-cache/revalidate", post(rc_revalidate))
        .route("/v1/concurrency", get(concurrency_get))
        .route("/v1/routing", get(routing_get))
        .route("/v1/routing/redirects", post(add_redirect))
        .route("/v1/routing/redirects/delete", post(del_redirect))
        .route("/v1/routing/rewrites", post(add_rewrite))
        .route("/v1/routing/rewrites/delete", post(del_rewrite))
        .route("/v1/cron", get(cron_list).post(cron_add))
        .route("/v1/cron/:id", delete(cron_del))
        .route("/v1/cron/:id/run", post(cron_run))
        .route("/v1/workflows", get(wf_list).post(wf_define))
        .route("/v1/workflows/summary", get(wf_summary))
        .route("/v1/workflows/runs", get(wf_runs))
        .route("/v1/workflows/runs/:id", get(wf_run_detail))
        .route("/v1/workflows/:id/run", post(wf_run))
        .route("/v1/sandbox", post(sandbox))
        .route("/deployments", get(dep_list).post(dep_create))
        .route("/v1/deployments/:id", delete(dep_delete))
        .route("/v1/deployments/:id/resources", get(deployment_resources))
        .route("/v1/deployments/:id/build", get(deployment_build))
        .route("/v1/deployments/:id/service-graph", get(deployment_service_graph))
        .route("/v1/deployments/:id/promote", post(dep_promote))
        .route("/v1/projects/:project", delete(project_delete))
        .route("/v1/projects/:project/redeploy", post(project_redeploy))
        .route("/v1/git/deploy", post(git_deploy))
        // Zip upload: raw `.zip` body (default 2 MB axum limit raised to 12 MB; the
        // handler caps the archive at 10 MB and rejects non-zips).
        .route(
            "/v1/deploy/zip",
            post(deploy_zip).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        // Run a PRE-BUILT image from any registry (Docker Hub / Quay / arbitrary).
        .route("/v1/deploy/image", post(deploy_image))
        .route("/v1/fleet-deployments", get(fleet_deployments))
        .route("/v1/builds/:id", get(build_get))
        .route("/v1/buildcache/:key", get(buildcache_get))
        .route("/v1/build/frameworks", get(build_frameworks))
        .route("/v1/nodes/announce", post(node_announce))
        .route("/v1/mesh/roster", get(mesh_roster_get))
        .route("/v1/mesh/admit", post(mesh_admit))
        .route("/v1/token", post(mint_token))
        .route("/v1/whoami", get(whoami))
        .route("/v1/auth", get(auth_status))
        .route("/v1/regions/catalog", get(region_catalog))
        .route("/v1/projects/:project/settings", get(project_settings_get))
        .route("/v1/projects/:project/build", put(project_build_put))
        .route("/v1/projects/:project/functions", put(project_functions_put))
        .route("/v1/projects/:project/cron-enabled", put(project_cron_enabled_put))
        .route("/v1/projects/:project/env", post(project_env_put))
        .route("/v1/projects/:project/env/:key", delete(project_env_delete))
        .route("/v1/projects/:project/service-graph", get(project_service_graph))
        .route("/v1/projects/:project/domains", post(project_domain_add))
        .route("/v1/projects/:project/team", put(project_team_put))
        .route("/v1/domains", get(domains_list))
        .route("/v1/domains/:domain", get(domain_get))
        .route("/v1/domains/:domain/records", post(domain_add_record))
        .route("/v1/domains/:domain/records/:id", delete(domain_delete_record).put(domain_update_record))
        .route("/v1/domains/:domain/import", post(domain_import_records))
        .route("/v1/domains/:domain/scan", get(domain_scan_dns))
        .route("/v1/domains/:domain/nameservers", put(domain_set_nameservers))
        .route("/v1/domains/:domain/auto-renew", put(domain_set_auto_renew))
        .route("/v1/domains/:domain/ssl/renew", post(domain_renew_ssl))
        // ---- Teams ----
        .route("/v1/teams", get(teams_list).post(team_create))
        .route("/v1/teams/:slug", get(team_get))
        .route("/v1/teams/:slug/members", post(team_add_member))
        .route("/v1/teams/:slug/members/:email", delete(team_remove_member))
        .route("/v1/teams/:slug/plan", put(team_set_plan))
        .route("/v1/teams/:slug/sso", put(team_set_sso))
        // ---- GitOps (config repo link + inbound CI webhook) ----
        .route("/v1/gitops", get(gitops_get).put(gitops_put).delete(gitops_unlink))
        .route("/v1/gitops/synced", post(gitops_synced))
        .route("/v1/gitops/projects", get(gitops_projects))
        .route("/v1/git/webhook", post(git_webhook))
        // ---- API keys (tenant-scoped platform tokens) ----
        .route("/v1/apikeys", get(apikeys_list).post(apikey_create))
        .route("/v1/apikeys/:id", delete(apikey_revoke))
        // ---- Connected integrations (consumable via the Vercel-compatible SDK) ----
        .route("/v1/integrations", get(integrations_list).post(integration_upsert))
        .route("/v1/integrations/:id", get(integration_get).delete(integration_delete))
        .route("/v1/integrations/:id/credentials", get(integration_credentials))
        // ---- Webhooks ----
        .route("/v1/webhooks", get(webhooks_all).post(webhook_create_team))
        .route("/v1/webhooks/events", get(webhook_events))
        .route("/v1/webhooks/deliveries", get(webhook_deliveries))
        .route("/v1/webhooks/:id", delete(webhook_delete))
        .route("/v1/projects/:project/webhooks", get(webhooks_for_project).post(webhook_create))
        // ---- Databases / storage ----
        .route("/v1/databases", get(databases_list).post(database_create))
        .route("/v1/db-directory", get(db_directory))
        .route("/v1/admin/databases", get(admin_databases_all))
        .route("/v1/databases/:id", get(database_get).delete(database_delete))
        .route("/v1/databases/:id/credentials", get(database_credentials))
        .route("/v1/projects/:project/databases", get(databases_for_project))
        // Mesh-internal: register/remove a cross-region replica of a database.
        .route("/v1/databases/replica", post(database_replica))
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
        // ---- Notifications (inbox bell) ----
        .route("/v1/notifications", get(notifications_list))
        .route("/v1/notifications/read", post(notifications_read))
        .route("/v1/notifications/archive-all", post(notifications_archive_all))
        .route("/v1/notifications/:id/archive", post(notification_archive))
        // ---- Monitoring ----
        .route("/v1/metrics", get(metrics_get))
        // ---- Owner / ops dashboard ----
        .route("/v1/admin/overview", get(admin_overview))
        .route("/v1/admin/audit", get(admin_audit))
        .route("/v1/admin/data", get(data_collections))
        .route("/v1/admin/data/:collection", get(data_rows).post(data_create))
        .route("/v1/admin/data/:collection/:id", put(data_patch).delete(data_delete))
        .route("/v1/admin/namespaces", get(data_namespaces))
        .route("/v1/admin/sql/tables", get(sql_tables))
        .route("/v1/admin/sql/query", post(sql_query))
        .route("/v1/admin/guardian", get(guardian_status))
        .route("/v1/identity/sync", post(identity_sync))
        // ---- Billing & compute credits ----
        .route("/v1/billing", get(billing_get))
        .route("/v1/billing/ledger", get(billing_ledger))
        .route("/v1/billing/invoices", get(billing_invoices))
        .route("/v1/billing/checkout", post(billing_checkout))
        .route("/v1/billing/checkout/:id", get(billing_checkout_get))
        .route("/v1/billing/confirm", post(billing_confirm))
        .route("/v1/billing/charge", post(billing_charge))
        .route("/v1/billing/webhook", post(billing_webhook))
        // ---- Deployment preview / thumbnail ----
        .route("/v1/projects/:project/preview", get(project_preview))
        .route("/v1/projects/:project/thumbnail", get(project_thumbnail))
        .route("/v1/incidents", get(incidents_list).post(incident_open))
        .route("/v1/incidents/:id/updates", post(incident_update))
        // ---- Enterprise feature suite (IP blocking, SIEM, SAML, SCIM,
        //      deployment protection, microfrontends, conformance) ----
        .merge(crate::enterprise_api::routes())
        // ---- Platform-native Microfrontends (project-scoped groups/members/config) ----
        .merge(crate::microfrontends_api::routes())
        // ---- Platform-native Sandboxes (isolated on-demand Linux environments) ----
        .merge(crate::sandboxes_api::routes())
        .with_state(cloud);
    // EXPERIMENT: anonymous team/role membership (only with `--features zkauth`).
    #[cfg(feature = "zkauth")]
    let app = app.merge(crate::zkauth::routes());
    app
}

// ---- Auth (JWT) ----

#[derive(Deserialize)]
struct TokenReq {
    #[serde(default = "default_sub")]
    sub: String,
    #[serde(default = "default_tenant")]
    tenant: String,
    // No default: the caller must explicitly request the role it needs. A
    // silent "owner" default meant every mint (even one that forgot to specify
    // a role) came back with the highest tenant-scoped role.
    #[serde(default)]
    role: String,
    /// The verified account email (from the trusted dashboard server's own
    /// Clerk session, never client-supplied trust) — used ONLY to
    /// independently derive `platform_admin` against this backend's own
    /// `owner_email`. Never trust a client-supplied `platform_admin` boolean.
    #[serde(default)]
    email: String,
}
fn default_sub() -> String { "user".into() }
fn default_tenant() -> String { "default".into() }

/// Constant-time byte comparison for secret/token equality checks — ordinary
/// `==` on a `str`/`[u8]` short-circuits on the first mismatched byte, which is
/// a timing side-channel for comparing a caller-supplied value against a
/// stored secret (bearer tokens, shared internal secrets, password hashes).
pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth_status() -> Json<Value> {
    Json(json!({ "enforced": crate::auth::enforced() }))
}

/// Whether a token-mint request is permitted.
/// * Dev (no `HIVE_JWT_SECRET`): open — minted tokens aren't verified anyway, so
///   this preserves the current local flow.
/// * Enforced (`HIVE_JWT_SECRET` set): the mint is a privileged operation (it can
///   assert ANY tenant), so it MUST come from the trusted dashboard server,
///   proven by `x-hive-internal == HIVE_INTERNAL_TOKEN`. Fail CLOSED if the
///   internal token isn't configured — never allow open minting when enforced.
fn mint_allowed(headers: &HeaderMap) -> bool {
    if !crate::auth::enforced() {
        return true;
    }
    match std::env::var("HIVE_INTERNAL_TOKEN") {
        Ok(t) if !t.trim().is_empty() => headers
            .get("x-hive-internal")
            .and_then(|v| v.to_str().ok())
            .map(|v| ct_eq(v, &t))
            .unwrap_or(false),
        _ => false,
    }
}

/// Dedicated, tight rate limit on the token-mint endpoint — independent of the
/// tenant-facing edge pipeline's limiter (which doesn't even run in front of
/// the admin router, see the separate rate-limiting audit finding). `/v1/token`
/// is the platform's effective login/credential-mint endpoint, so repeated
/// guesses of `HIVE_INTERNAL_TOKEN` must be throttled independently of every
/// other route. 20 attempts / 60s per client IP, shared across the process.
fn mint_rate_limiter() -> &'static hive_edge::RateLimiter {
    static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| hive_edge::RateLimiter::new(20, 60_000))
}

/// Fleet-wide budget for the WHOLE admin/control-plane router — this surface
/// previously had NO rate limiting of its own at all (the tenant-facing edge
/// pipeline's WAF/limiter only wraps the deployment-serving router, never this
/// one), so a compromised dashboard session or a brute-force script could hit
/// every `/v1/*` admin route unbounded. 600 req/60s per client IP is generous
/// enough for legitimate dashboard polling (team switches, build/deployment
/// lists, live logs) while still bounding real abuse. Independent of
/// `mint_rate_limiter` (tighter, /v1/token-only) and the tenant edge limiter.
fn admin_rate_limiter() -> &'static hive_edge::RateLimiter {
    static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| hive_edge::RateLimiter::new(600, 60_000))
}

/// Outermost layer on the admin router (see `main.rs`): sheds excess requests
/// by real TCP peer address before any auth/handler work runs.
pub async fn admin_rate_limit(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !admin_rate_limiter().check(&peer.ip().to_string(), now_ms()) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    next.run(req).await
}

async fn mint_token(
    State(c): State<Arc<CloudState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<TokenReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Real TCP peer address — no reverse proxy sits in front of the admin
    // listener on any fleet node, so a client-supplied `x-forwarded-for` (the
    // previous source here) is pure attacker-controlled input that lets a
    // caller reset their own rate-limit bucket at will just by changing it.
    let ip = peer.ip().to_string();
    if !mint_rate_limiter().check(&ip, hive_core::now_ms()) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "too many token requests".into()));
    }
    if !mint_allowed(&headers) {
        return Err((StatusCode::FORBIDDEN, "token minting is server-only (x-hive-internal required)".into()));
    }
    // Independently derive platform-operator status from THIS backend's own
    // owner_email config — never trust a client-supplied claim for it. Mirrors
    // the identity/sync owner check (admin.rs `is_owner` above).
    let platform_admin = !c.owner_email.trim().is_empty()
        && !req.email.trim().is_empty()
        && req.email.trim().eq_ignore_ascii_case(c.owner_email.trim());
    // 1-hour tokens; the dashboard re-mints on load + periodically. Short-lived so
    // a leaked cookie expires quickly.
    let ttl = 3600i64;
    match crate::auth::issue(&req.sub, &req.tenant, &req.role, platform_admin, ttl) {
        Ok(token) => Ok(Json(json!({ "token": token, "expires_in": ttl }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

/// Echo the verified caller identity (tenant/role/sub) so the dashboard can
/// confirm its session mapping. Reads the JWT claims bound by `require_auth`
/// (from Bearer or the `hive_jwt` cookie); returns `authenticated:false` in dev.
async fn whoami(headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    if let Some(c) = claims.as_ref().map(|e| &e.0) {
        return Json(json!({ "authenticated": true, "sub": c.sub, "tenant": c.tenant, "role": c.role, "enforced": crate::auth::enforced() }));
    }
    // Not bound by middleware (e.g. dev pass-through) — try a best-effort verify so
    // the endpoint is useful when called directly.
    let tok = crate::auth::extract_token(&headers);
    match tok.and_then(|t| crate::auth::verify(&t).ok()) {
        Some(c) => Json(json!({ "authenticated": true, "sub": c.sub, "tenant": c.tenant, "role": c.role, "enforced": crate::auth::enforced() })),
        None => Json(json!({ "authenticated": false, "enforced": crate::auth::enforced() })),
    }
}

// ---- Project settings (env vars, build config, function settings) ----

/// The function-region catalog is built **from the live mesh** — the actual
/// regions in which P2P nodes report their longitude/latitude. Each region is
/// auto-assigned to its real continent (from lat/lon), so a node in Los Angeles
/// appears under "North America". No hard-coded region table.
async fn region_catalog(State(c): State<Arc<CloudState>>) -> Json<Value> {
    use std::collections::BTreeMap;
    // continent -> (region id -> entry). Dedupe a region across co-located nodes,
    // counting how many nodes back it.
    let mut by_continent: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for n in c.registry.nodes() {
        let continent = match (n.lat, n.lon) {
            (Some(lat), Some(lon)) => hive_edge::continent_of(lat, lon).to_string(),
            _ => "Unknown".to_string(),
        };
        let label = match (&n.city, &n.country) {
            (Some(city), Some(country)) => format!("{city}, {country}"),
            (Some(city), None) => city.clone(),
            _ => n.region.clone(),
        };
        let regions = by_continent.entry(continent).or_default();
        let entry = regions.entry(n.region.clone()).or_insert_with(|| {
            json!({ "id": n.region, "label": label, "aws": "", "lat": n.lat, "lon": n.lon, "nodes": 0 })
        });
        let count = entry.get("nodes").and_then(|v| v.as_u64()).unwrap_or(0);
        entry["nodes"] = json!(count + 1);
    }
    let out: serde_json::Map<String, Value> = by_continent
        .into_iter()
        .map(|(continent, regions)| (continent, json!(regions.into_values().collect::<Vec<_>>())))
        .collect();
    Json(json!(out))
}

async fn project_settings_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    // Settings rows are node-local. When THIS node has no row (reads are served
    // by whichever node DNS picked; the row lives on the node that deployed the
    // project), answering with the local defaults silently shows the user empty
    // settings. Proxy to the hosting node instead — it has the row, so it
    // answers directly (no re-proxy loop) — and fall back to local defaults
    // only if the host is unreachable.
    if crate::admin::record_tenant(&c.projects.team_of(&project)) == UNTAGGED_TENANT {
        if let Some(node) = host_node_for_project(&c, &project) {
            if let Some(v) =
                fetch_from_host(&c, &node, &format!("/v1/projects/{project}/settings"), &t).await
            {
                return Ok(Json(v));
            }
        }
    }
    Ok(Json(json!(c.projects.get_masked(&project))))
}

async fn project_build_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(build): Json<crate::project_settings::BuildConfig>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_build(&project, build);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// The tier (hobby/pro/enterprise) of the team owning a project.
fn team_plan(c: &Arc<CloudState>, project: &str) -> String {
    let team = norm(&c.projects.team_of(project)).to_string();
    c.teams.get(&team).map(|t| t.plan).unwrap_or_else(|| "hobby".into())
}

async fn project_functions_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(mut f): Json<crate::project_settings::FunctionSettings>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    // Enforce plan limits: runtime cap (Enterprise = 1h) and Enterprise-only
    // automatic multi-region fail-over.
    let plan = team_plan(&c, &project);
    let max_dur = crate::billing::plan_max_duration_secs(&plan);
    f.default_max_duration_secs = f.default_max_duration_secs.clamp(1, max_dur);
    // Persist the user's automatic-region-failover choice as set. (Previously this
    // was force-reset to false unless the plan was exactly "enterprise", so the
    // toggle silently never saved. The runtime `order_candidates` already honors
    // this flag; the setting should reflect what the operator selected.)
    c.projects.set_functions(&project, f);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

async fn project_env_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(v): Json<crate::project_settings::EnvVar>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    // Keep-secret + upsert-by-key semantics live in `put_env` (see there).
    c.projects.put_env(&project, v);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

async fn project_env_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((project, key)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.delete_env(&project, &key);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

// ---- Domains ----

#[derive(Deserialize)]
struct AddDomain {
    domain: String,
}

async fn project_domain_add(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<AddDomain>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
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

pub(crate) async fn domains_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Query(q): Query<LocalQ>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Custom domains live in each project's node-local ProjectSettings (never
    // gossiped), so a bare list shows ONLY locally-hosted projects' domains —
    // a member's remotely-placed project's domains were invisible on the node
    // that answered. Merge this node's domains with each peer hosting the
    // tenant's projects (the same fan-out /v1/workflows uses). `?local=true`
    // answers with local-only so a peer never re-fans (no loop).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    for (p, d) in c.projects.all_domains() {
        if norm(&c.projects.team_of(&p)) == t && seen.insert(format!("{p}\u{0}{d}")) {
            out.push(json!({ "project": p, "domain": d }));
        }
    }
    if !q.local.unwrap_or(false) {
        for node in peer_nodes_for_tenant(&c, &t) {
            if let Some(v) = fetch_from_host(&c, &node, "/v1/domains?local=true", &t).await {
                if let Some(arr) = v.as_array() {
                    for item in arr {
                        let p = item.get("project").and_then(|x| x.as_str()).unwrap_or("");
                        let d = item.get("domain").and_then(|x| x.as_str()).unwrap_or("");
                        if seen.insert(format!("{p}\u{0}{d}")) {
                            out.push(item.clone());
                        }
                    }
                }
            }
        }
    }
    Json(json!(out))
}

// ---- Deployment resources (functions + static assets, build artifacts) ----

fn asset_kind(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "html" | "htm" => "HTML",
        "js" | "mjs" | "cjs" => "JS",
        "css" => "CSS",
        "json" | "webmanifest" => "JSON",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "avif" => "Image",
        "woff" | "woff2" | "ttf" | "otf" => "Font",
        "txt" | "xml" | "map" => "Text",
        _ => "Misc",
    }
}

/// Walk a build output directory into a flat asset list (path, size, type),
/// skipping heavy/irrelevant dirs. Capped to keep the response light.
fn walk_assets(base: &std::path::Path, cap: usize) -> Vec<Value> {
    let mut out = Vec::new();
    fn rec(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<Value>, cap: usize) {
        if out.len() >= cap {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if out.len() >= cap {
                return;
            }
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            // Skip dependencies, VCS, and build caches/internals (incl. dotfiles)
            // so only real shipped assets show up.
            if name == "node_modules" || name.starts_with('.') || name == "cache" {
                continue;
            }
            if p.is_dir() {
                rec(base, &p, out, cap);
            } else if let Ok(rel) = p.strip_prefix(base) {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                let path = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
                out.push(json!({ "path": path, "size": size, "type": asset_kind(&path) }));
            }
        }
    }
    rec(base, base, &mut out, cap);
    out.sort_by(|a, b| a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or("")));
    out
}

/// Functions + static assets for a deployment — the build artifacts/resources.
pub(crate) async fn deployment_resources(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(rec) = c.gw.deployment_records().into_iter().find(|r| r.id == id) else {
        // Not hosted locally — the placement scheduler may have put this deployment
        // on a peer. Proxy to the hosting node so its build outputs (functions +
        // static assets, which live on that node's filesystem) are returned.
        if let Some(node) = host_node_for_deployment(&c, &id) {
            if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/resources"), &t).await {
                return Json(v);
            }
        }
        return Json(json!({ "functions": [], "static_assets": [], "total_static": 0 }));
    };
    // Ownership judged from the deployment record's OWN tenant tag (authoritative
    // and present — `rec` is a local record), NOT the node-local project row:
    // `team_of` is UNTAGGED on a node whose ProjectStore row was GC'd/never synced
    // (rows aren't gossiped), which used to hand a legit owner empty resources.
    // Fall back to the project row only when the record's tag was lost.
    let rec_owner = record_tenant(&rec.tenant);
    if rec_owner != t && norm(&c.projects.team_of(&rec.project)) != t {
        return Json(json!({ "functions": [], "static_assets": [], "total_static": 0 }));
    }
    let functions: Vec<Value> = rec
        .manifest
        .functions
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "runtime": if f.runtime == "edge" { "Edge" } else if f.runtime == "container" { "Container" } else { "Node" },
                "region": c.region,
                "memory_mib": f.memory_mib,
                "max_duration_secs": f.max_duration_secs,
                "edge": f.runtime == "edge",
            })
        })
        .collect();

    let root = std::path::PathBuf::from(&rec.root);
    // Walk the build OUTPUT, not the source tree: an explicit static_dir, else a
    // recognized framework output dir. Avoids listing source files + tool caches.
    let base = match rec.manifest.static_dir.as_deref() {
        Some(sd) if sd != "." && !sd.is_empty() => Some(root.join(sd)),
        _ => [".vercel/output/static", ".next/static", "dist", "build", "out", "public"]
            .iter()
            .map(|c| root.join(c))
            .find(|p| p.is_dir()),
    };
    let (mut assets, total_static) = match base {
        Some(b) => {
            let a = walk_assets(&b, 800);
            let n = a.len();
            (a, n)
        }
        None => (Vec::new(), 0),
    };
    assets.truncate(500);

    Json(json!({
        "functions": functions,
        "static_assets": assets,
        "total_static": total_static,
        "kind": if rec.manifest.functions.is_empty() { "static" } else { "functions" },
    }))
}

// ---- Domain detail: DNS records, nameservers, SSL ----

/// Full domain record (DNS records, nameservers, free SSL cert, metadata).
/// Creates a sensible default on first view.
async fn domain_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    // Connected projects: projects in this tenant whose domain ends with this one.
    let connected: Vec<Value> = c
        .projects
        .all_domains()
        .into_iter()
        .filter(|(p, d)| norm(&c.projects.team_of(p)) == t && (d == &domain || d.ends_with(&format!(".{domain}"))))
        .map(|(p, d)| json!({ "project": p, "domain": d }))
        .collect();
    Ok(Json(json!({ "domain": c.domains.ensure(&domain, &t), "connected": connected })))
}

#[derive(Deserialize)]
struct AddRecordReq {
    #[serde(default)]
    name: String,
    #[serde(rename = "type")]
    kind: String,
    value: String,
    #[serde(default = "default_dns_ttl")]
    ttl: u32,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default)]
    comment: String,
}
fn default_dns_ttl() -> u32 {
    60
}

async fn domain_add_record(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
    Json(r): Json<AddRecordReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let rec = crate::dns::DnsRecord {
        id: String::new(),
        name: r.name,
        kind: r.kind.to_uppercase(),
        value: r.value,
        ttl: r.ttl,
        priority: r.priority,
        comment: r.comment,
        created_ms: 0,
        system: false,
    };
    let added = c.domains.add_record(&domain, rec).ok_or((StatusCode::NOT_FOUND, "no such domain".into()))?;
    c.audit.record(&t, "user", "create", "dns_record", &added.id, &format!("{} {} → {} ({domain})", added.kind, added.name, added.value));
    crate::persist::persist(&c);
    Ok(Json(json!(added)))
}

async fn domain_delete_record(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((domain, id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let ok = c.domains.delete_record(&domain, &id);
    if ok {
        c.audit.record(&t, "user", "delete", "dns_record", &id, &domain);
        crate::persist::persist(&c);
    }
    Ok(Json(json!({ "deleted": ok })))
}

/// Edit an existing DNS record (system records are immutable).
async fn domain_update_record(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((domain, id)): Path<(String, String)>,
    Json(r): Json<AddRecordReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let updated = c
        .domains
        .update_record(&domain, &id, r.name, r.kind.to_uppercase(), r.value, r.ttl, r.priority, r.comment)
        .ok_or((StatusCode::NOT_FOUND, "no such record".into()))?;
    c.audit.record(&t, "user", "update", "dns_record", &updated.id, &format!("{} {} → {} ({domain})", updated.kind, updated.name, updated.value));
    crate::persist::persist(&c);
    Ok(Json(json!(updated)))
}

#[derive(Deserialize)]
struct ImportReq {
    /// Structured records (e.g. from the DNS scanner / record editor).
    #[serde(default)]
    records: Vec<AddRecordReq>,
    /// Raw BIND-style zone text to parse (the "paste your zone file" path).
    #[serde(default)]
    zone: String,
}

/// Bulk-import DNS records — the "migrate existing DNS" flow. Accepts a list of
/// structured records and/or a pasted BIND-style zone file. Idempotent: exact
/// duplicates (type+name+value) are skipped.
async fn domain_import_records(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
    Json(req): Json<ImportReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let mut recs: Vec<crate::dns::DnsRecord> = req
        .records
        .into_iter()
        .map(|r| crate::dns::DnsRecord {
            id: String::new(),
            name: r.name,
            kind: r.kind.to_uppercase(),
            value: r.value,
            ttl: r.ttl,
            priority: r.priority,
            comment: r.comment,
            created_ms: 0,
            system: false,
        })
        .collect();
    if !req.zone.trim().is_empty() {
        recs.extend(parse_zone(&req.zone, &domain));
    }
    let added = c.domains.import_records(&domain, recs);
    if !added.is_empty() {
        c.audit.record(&t, "user", "import", "dns_record", &domain, &format!("imported {} record(s)", added.len()));
        crate::persist::persist(&c);
    }
    Ok(Json(json!({ "imported": added.len(), "records": added })))
}

/// Detect a domain's CURRENT public DNS records (via DNS-over-HTTPS) so a user can
/// migrate them into the console with one click. Best-effort: returns whatever
/// resolves. Records are NOT added — the client imports the ones it wants.
async fn domain_scan_dns(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let mut found: Vec<Value> = Vec::new();
    // (query host suffix, record type) — apex + a few common subdomains.
    let queries: &[(&str, &str)] = &[
        ("", "A"), ("", "AAAA"), ("", "MX"), ("", "TXT"), ("", "NS"), ("", "CAA"),
        ("www", "CNAME"), ("www", "A"),
    ];
    for (sub, qtype) in queries {
        let qname = if sub.is_empty() { domain.clone() } else { format!("{sub}.{domain}") };
        let url = format!("https://cloudflare-dns.com/dns-query?name={qname}&type={qtype}");
        let resp = c
            .http
            .get(&url)
            .header("accept", "application/dns-json")
            .timeout(Duration::from_secs(4))
            .send()
            .await;
        let Ok(resp) = resp else { continue };
        let Ok(v) = resp.json::<Value>().await else { continue };
        let Some(ans) = v.get("Answer").and_then(|a| a.as_array()) else { continue };
        for a in ans {
            let rtype = a.get("type").and_then(|x| x.as_u64()).unwrap_or(0);
            let kind = dns_type_name(rtype);
            if kind != *qtype {
                continue; // skip e.g. CNAME chains returned for an A query
            }
            let raw = a.get("data").and_then(|x| x.as_str()).unwrap_or("").trim().trim_end_matches('.').to_string();
            if raw.is_empty() {
                continue;
            }
            let ttl = a.get("TTL").and_then(|x| x.as_u64()).unwrap_or(3600) as u32;
            // MX (and SRV) prefix a numeric priority in the data field.
            let (priority, value) = if (kind == "MX" || kind == "SRV") && raw.split_whitespace().count() >= 2 {
                let mut it = raw.splitn(2, char::is_whitespace);
                let p = it.next().unwrap_or("").parse::<u32>().ok();
                (p, it.next().unwrap_or("").trim().trim_end_matches('.').to_string())
            } else {
                (None, raw.clone())
            };
            // Don't suggest records we already have, or shadw's own anycast.
            found.push(json!({
                "name": *sub,
                "type": kind,
                "value": value,
                "ttl": ttl,
                "priority": priority,
            }));
        }
    }
    // De-dupe identical suggestions.
    found.dedup_by(|a, b| a.to_string() == b.to_string());
    Ok(Json(json!({ "domain": domain, "records": found })))
}

/// DoH numeric record type → name (the subset we surface).
fn dns_type_name(t: u64) -> &'static str {
    match t {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        33 => "SRV",
        257 => "CAA",
        _ => "",
    }
}

/// Tolerant BIND-style zone parser for the "paste your zone file" import. Handles
/// lines like `www 3600 IN A 76.76.21.21`, `@ IN MX 10 mail.example.com`, and the
/// minimal `www A 1.2.3.4`. Unknown/blank/comment lines are skipped.
fn parse_zone(text: &str, domain: &str) -> Vec<crate::dns::DnsRecord> {
    const TYPES: &[&str] = &["A", "AAAA", "CNAME", "ALIAS", "MX", "TXT", "CAA", "NS", "SRV"];
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim(); // strip ; comments
        if line.is_empty() {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        // Find the record TYPE token; everything before is name/ttl/class, after is value.
        let Some(ti) = toks.iter().position(|t| TYPES.contains(&t.to_uppercase().as_str())) else { continue };
        let kind = toks[ti].to_uppercase();
        // name = first token if it isn't a ttl/class keyword, else apex.
        let mut name = String::new();
        if ti > 0 {
            let first = toks[0];
            if !first.eq_ignore_ascii_case("IN") && first.parse::<u32>().is_err() {
                name = if first == "@" { String::new() } else { first.trim_end_matches(&format!(".{domain}")).trim_end_matches('.').to_string() };
            }
        }
        // ttl = a numeric token before the type, if any.
        let ttl = toks[..ti].iter().find_map(|t| t.parse::<u32>().ok()).unwrap_or(3600);
        let rest: Vec<&str> = toks[ti + 1..].to_vec();
        if rest.is_empty() {
            continue;
        }
        let (priority, value) = if (kind == "MX" || kind == "SRV") && rest.len() >= 2 && rest[0].parse::<u32>().is_ok() {
            (rest[0].parse::<u32>().ok(), rest[1..].join(" "))
        } else {
            (None, rest.join(" "))
        };
        let value = value.trim().trim_matches('"').trim_end_matches('.').to_string();
        if value.is_empty() {
            continue;
        }
        out.push(crate::dns::DnsRecord {
            id: String::new(),
            name,
            kind,
            value,
            ttl,
            priority,
            comment: "Imported".into(),
            created_ms: 0,
            system: false,
        });
    }
    out
}

#[derive(Deserialize)]
struct NameserversReq {
    nameservers: Vec<String>,
}

async fn domain_set_nameservers(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
    Json(r): Json<NameserversReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    c.domains.set_nameservers(&domain, r.nameservers);
    c.audit.record(&t, "user", "update", "nameservers", &domain, "");
    crate::persist::persist(&c);
    Ok(Json(json!(c.domains.get(&domain))))
}

#[derive(Deserialize)]
struct AutoRenewReq {
    on: bool,
}

async fn domain_set_auto_renew(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
    Json(r): Json<AutoRenewReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    c.domains.set_auto_renew(&domain, r.on);
    crate::persist::persist(&c);
    Ok(Json(json!(c.domains.get(&domain))))
}

async fn domain_renew_ssl(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let cert = c.domains.renew_ssl(&domain);
    c.audit.record(&t, "user", "update", "ssl_cert", &domain, "reissued free certificate");
    crate::persist::persist(&c);
    Ok(Json(json!(cert)))
}

// ---- Git deploy (Import Git Repository) ----

/// Pick a globally-unique project name. Deployment aliases are `<project>.localhost`
/// (global), so two projects can't share a name. A genuine redeploy (same name +
/// same repo + same tenant) keeps its name; anything else gets a `-N` suffix.
fn unique_project_name(c: &Arc<CloudState>, desired: &str, repo_url: &str, tenant: &str) -> String {
    let base = if desired.trim().is_empty() {
        crate::git::project_name_from_url(repo_url)
    } else {
        desired.trim().to_string()
    };
    // Case-insensitive collision check: `<project>.localhost` aliases are global
    // and host routing is case-insensitive, so "Foo" and "foo" must not coexist.
    // Uses find_key_ci (key-only, no full-map clone) on this hot deploy path.
    let existing_ci = |name: &str| c.projects.find_key_ci(name);
    let Some(hit) = existing_ci(&base) else {
        return base; // free — use it
    };
    // Redeploy of the same project (same repo + tenant) → keep the name.
    let same_tenant = norm(&c.projects.team_of(&hit)) == tenant;
    let same_repo = c
        .gw
        .git_for_project(&hit)
        .map(|g| crate::gitops::norm_repo(&g.repo_url) == crate::gitops::norm_repo(repo_url))
        .unwrap_or(false);
    if same_tenant && same_repo {
        return hit;
    }
    // Otherwise find the next free `-N`.
    for i in 2..1000 {
        let cand = format!("{base}-{i}");
        if existing_ci(&cand).is_none() {
            return cand;
        }
    }
    format!("{base}-{}", now_ms())
}

pub(crate) async fn git_deploy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<fluid_core::GitDeployRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    start_named_deploy(&c, &t, req).await
}

/// Shared deploy entry: assign the project to the tenant (unique name, conflict
/// check, root-dir persist) then kick off the async build. Used by BOTH the git
/// deploy (`/v1/git/deploy`) and the zip upload (`/v1/deploy/zip`) so naming +
/// placement behave identically regardless of source.
pub(crate) async fn start_named_deploy(
    c: &Arc<CloudState>,
    t: &str,
    mut req: fluid_core::GitDeployRequest,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Assign the (new) project to the requesting tenant so it shows under their
    // team only — with a globally-unique name (auto-generated when none given).
    let t = t.to_string();
    let requested = req.project.clone().unwrap_or_default();
    // A fanout / capability re-home dispatch (`no_fanout`) carries the coordinator's
    // already-resolved CANONICAL project name — use it verbatim, never uniquify.
    // Otherwise a target that happens to hold a stale same-named project would get
    // a `-N` suffix, so the deployment serves under the wrong host alias (the
    // container re-home → `container-dockerfile-3` bug).
    // A "New Deployment" from a project's own page (`redeploy`) targets an EXISTING
    // project the same tenant owns — keep its name verbatim regardless of source,
    // so a new deployment is never misread as a colliding new-project create.
    let redeploy_existing = req.redeploy
        && !requested.trim().is_empty()
        && c.projects
            .find_key_ci(requested.trim())
            .map(|k| norm(&c.projects.team_of(&k)) == t)
            .unwrap_or(false);
    let project = if (req.no_fanout || redeploy_existing) && !requested.trim().is_empty() {
        requested.trim().to_string()
    } else {
        unique_project_name(c, &requested, &req.repo_url, &t)
    };
    // Reject an EXPLICIT name the user typed that's already taken by a different
    // project (Issue #4) — don't silently rename to `<name>-2`. Fanout deploys
    // (no_fanout), redeploys of an existing project, and auto-named deploys (empty
    // requested) are exempt: those must resolve to a concrete name without erroring.
    if !req.no_fanout && !redeploy_existing && !requested.trim().is_empty() && project != requested.trim() {
        return Err((
            StatusCode::CONFLICT,
            format!("A project named \"{}\" already exists. Choose a different name.", requested.trim()),
        ));
    }
    // ---- Business locking (plan quotas + credit gate) ----
    // Only enforced on the COORDINATOR path (a `no_fanout` per-target dispatch has
    // already passed the gate on the coordinator; re-checking would double-reject or
    // block a legitimate placement). Redeploys of an existing project don't count
    // against the project cap.
    if !req.no_fanout {
        let is_new_project = !c
            .projects
            .snapshot()
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(&project));
        // Credit / included-compute lock: an exhausted Hobby account can't deploy.
        if let Err(e) = c.billing.can_deploy(&t) {
            return Err((StatusCode::PAYMENT_REQUIRED, e));
        }
        // Project-count quota for the plan.
        if is_new_project {
            let plan = c.billing.account(&t).plan;
            let max = crate::billing::plan_max_projects(&plan);
            if max > 0 {
                let count = c.projects.count_for_team(&t) as u32;
                if count >= max {
                    return Err((
                        StatusCode::PAYMENT_REQUIRED,
                        format!(
                            "Project limit reached ({count}/{max}) on the {plan} plan — upgrade to add more."
                        ),
                    ));
                }
            }
        }
    }
    // Ownership check: a project already owned by a DIFFERENT tenant must never be
    // silently reassigned here, regardless of no_fanout/redeploy_existing — closes
    // the no_fanout cross-tenant hijack (naming an existing victim project with
    // no_fanout=true used to skip straight to set_team below with no check at all).
    if let Some(existing_key) = c.projects.find_key_ci(&project) {
        if norm(&c.projects.team_of(&existing_key)) != t {
            return Err((StatusCode::FORBIDDEN, "project belongs to a different team".into()));
        }
    }
    req.project = Some(project.clone());
    c.projects.set_team(&project, &t);
    // Persist the subdirectory so future redeploys keep building it.
    if let Some(root) = req.root_dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        c.projects.set_root_dir(&project, root);
    }
    crate::persist::persist(c);
    // Start the build asynchronously; the dashboard streams logs via /v1/builds/:id.
    let build_id = crate::git::start_build(c.clone(), req);
    Ok(Json(json!({ "build_id": build_id, "project": project })))
}

/// Zip-upload deploy: an alternative SOURCE to a git URL. The raw request body is
/// the uploaded `.zip`; metadata (project name, env, production) rides as a base64
/// JSON `x-hive-deploy-meta` header. We base64 the archive into the GitDeployRequest
/// so the SAME placement + fanout path ships it to the chosen region node, where the
/// build extracts it instead of cloning. Capped well under the 16 MB gossip frame —
/// upload SOURCE only; dependencies install during the build.
pub(crate) async fn deploy_zip(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    const MAX_ZIP: usize = 10 * 1024 * 1024;
    if body.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Empty upload — choose a .zip file.".into()));
    }
    if body.len() > MAX_ZIP {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Zip too large (max 10 MB). Upload your SOURCE only — dependencies install during the build.".into(),
        ));
    }
    // Cheap sanity check that this is actually a zip (PK\x03\x04 / empty-archive magic).
    if !(body.starts_with(b"PK\x03\x04") || body.starts_with(b"PK\x05\x06")) {
        return Err((StatusCode::BAD_REQUEST, "That doesn't look like a .zip archive.".into()));
    }
    #[derive(serde::Deserialize, Default)]
    struct ZipMeta {
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(default)]
        production: Option<bool>,
    }
    let meta: ZipMeta = headers
        .get("x-hive-deploy-meta")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let zip_b64 = base64::engine::general_purpose::STANDARD.encode(body.as_ref());
    let filename = meta.filename.unwrap_or_else(|| "archive.zip".into());
    let req = fluid_core::GitDeployRequest {
        repo_url: format!("upload://{filename}"),
        branch: None,
        project: meta.project,
        creator: Some("you".into()),
        production: meta.production.unwrap_or(true),
        target: None,
        use_cache: true,
        root_dir: None,
        env: meta.env,
        no_fanout: false, // coordinator deploy → schedule + fanout (ships the zip to the region node)
        build_config: None,
        function_settings: None,
        redeploy: false,
        zip_b64: Some(zip_b64),
        image_ref: None,
        image_port: None,
    };
    start_named_deploy(&c, &t, req).await
}

#[derive(serde::Deserialize)]
pub(crate) struct ImageDeployReq {
    /// OCI image reference, e.g. `fruitbox12/simplifi:latest`, `quay.io/org/img:tag`.
    image: String,
    #[serde(default)]
    project: Option<String>,
    /// Explicit container port; auto-detected from the image when omitted.
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    env: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    production: Option<bool>,
    /// Fresh deployment of an EXISTING project (New Deployment modal) vs new project.
    #[serde(default)]
    redeploy: Option<bool>,
}

/// Deploy a PRE-BUILT container image from any registry. Skips clone + build: the
/// target node pulls the image, auto-detects its port, attaches a persistent volume,
/// and runs it with the project's env — riding the normal placement/fanout so it
/// lands on a container-capable region node.
pub(crate) async fn deploy_image(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(body): Json<ImageDeployReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let image = body.image.trim().to_string();
    if image.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Provide an image reference, e.g. fruitbox12/simplifi:latest".into()));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let req = fluid_core::GitDeployRequest {
        // A synthetic source URL so the deployment record reads sensibly; the actual
        // source is `image_ref` (no clone happens).
        repo_url: format!("image://{image}"),
        branch: None,
        project: body.project,
        creator: Some("you".into()),
        production: body.production.unwrap_or(true),
        target: None,
        use_cache: true,
        root_dir: None,
        env: body.env,
        no_fanout: false, // coordinator deploy → schedule + fanout to a container node
        build_config: None,
        function_settings: None,
        redeploy: body.redeploy.unwrap_or(false),
        zip_b64: None,
        image_ref: Some(image),
        image_port: body.port,
    };
    start_named_deploy(&c, &t, req).await
}

pub(crate) async fn build_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let b = c.builds.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    // Team-scoped — this route previously had no ownership check at all, so
    // any caller (even unauthenticated, since GET bypasses the JWT gate) who
    // knew/guessed a build id could read another tenant's full build log.
    // The project row is node-local (never gossiped) and Build carries no
    // tenant tag, so `team_of` alone 404'd a member's OWN build log on any node
    // whose row was GC'd/never synced. Fall back to the build's deployment
    // record tenant tag (local gw record or gossiped peer copy) — the same
    // authority `deployment_build`/`dep_list` trust.
    let owns = norm(&c.projects.team_of(&b.project)) == norm(&t)
        || b.deployment_id.as_deref().is_some_and(|did| {
            c.gw.deployment_records().iter().any(|r| r.id == did && record_tenant(&r.tenant) == norm(&t))
                || c.peer_deployments.read().values().flatten().any(|d| d.id.as_str() == did && record_tenant(&d.tenant) == norm(&t))
        });
    if !owns {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(b)))
}

/// The build behind a deployment — its status, timing, and full log lines — so the
/// deployment detail page can show build logs (incl. a failed build's error). Newest
/// matching build wins; scoped to the requesting team.
pub(crate) async fn deployment_build(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Tenant gate with the same fallback dep_list uses: `team_of` is NODE-LOCAL
    // (ProjectStore rows are never gossiped) and returns UNTAGGED for a project
    // this node has no row for — which used to 404 a build that IS in the local
    // map whenever the answering node lacked the project row (leader-forwarded
    // dashboard reads, post-restore nodes). Fall back to the DEPLOYMENT
    // record's own tenant tag (local gw record or the gossiped peer copy) —
    // the same authority dep_list trusts for exactly this reason.
    let dep_record_tenant: Option<String> = c
        .gw
        .deployment_records()
        .into_iter()
        .find(|r| r.id == id)
        .map(|r| record_tenant(&r.tenant).to_string())
        .or_else(|| {
            c.peer_deployments
                .read()
                .values()
                .flatten()
                .find(|d| d.id.as_str() == id)
                .map(|d| record_tenant(&d.tenant).to_string())
        });
    let team_ok = |b: &crate::git::Build| {
        norm(&c.projects.team_of(&b.project)) == norm(&t)
            || dep_record_tenant.as_deref() == Some(norm(&t))
    };
    // Local build record (newest matching, team-scoped).
    let mut builds: Vec<_> = c
        .builds
        .list()
        .into_iter()
        .filter(|b| b.deployment_id.as_deref() == Some(id.as_str()))
        .filter(team_ok)
        .collect();
    builds.sort_by_key(|b| b.started_ms);
    if let Some(b) = builds.pop() {
        return Ok(Json(json!(b)));
    }
    // Not built here — the build (and its logs) live on the node that hosts this
    // deployment. Proxy to it, exactly like deployment_resources does. The host has
    // the record locally, so it answers directly and there's no re-proxy loop.
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/build"), &t).await {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// Publish the host subdomains this node serves + its gateway URL, so peers can
/// build their cross-node routing tables (the mesh routes requests to wherever a
/// deployment actually lives).
pub(crate) async fn serve_hosts(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!({
        "node": c.node_name,
        "region": c.region,
        "gateway": c.public_base,
        "hosts": c.gw.served_hosts(),
        // Container deployments this node holds → feeds mesh lease election.
        "containers": c.gw.container_projects(),
    }))
}

/// Current container placement leases across the mesh (owner + fencing epoch +
/// expiry). The single owner of each stateful container, for visibility + the
/// edge router (container requests go to the owner).
pub(crate) async fn leases_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.leases.list()))
}

/// design-head-cid-exchange-rpc: `{namespace: [{key, hash, timestamp}, ...]}`
/// for every GuardianDB namespace this node's local replica currently holds —
/// content-addressed HEAD metadata only, never value bytes. The anti-entropy
/// loop (`spawn_anti_entropy_loop` in main.rs) fetches this from one random
/// healthy peer each round and diffs it against its own local
/// `guardian::namespace_heads()` to decide whether a real reconciliation sync
/// is needed. Unauthenticated like `/v1/serve-hosts`/`/v1/leases` (mesh
/// convergence machinery, not tenant data — no values are ever returned here).
pub(crate) async fn guardian_heads(State(_c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(crate::guardian::namespace_heads().await))
}

/// REAL cluster resource accounting: this node's live CPU/mem/disk/network usage
/// (sysinfo) plus cluster TOTALS = sum of every live node's capacity (gossiped via
/// NodeInfo). Answers "available compute / storage / bandwidth across the mesh".
async fn resources_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let nodes = c.registry.nodes();
    let cpu_cores: u64 = nodes.iter().map(|n| n.cpu_cores as u64).sum();
    let mem_total_mb: u64 = nodes.iter().map(|n| n.mem_total_mb).sum();
    let disk_total_gb: u64 = nodes.iter().map(|n| n.disk_total_gb).sum();
    let usage = crate::resources::live().await;
    Ok(Json(json!({
        "cluster": {
            "nodes": nodes.len(),
            "cpu_cores": cpu_cores,
            "mem_total_mb": mem_total_mb,
            "disk_total_gb": disk_total_gb,
        },
        "node": serde_json::to_value(&usage).unwrap_or_default(),
        "nodes": nodes.iter().map(|n| json!({
            "name": n.name, "region": n.region,
            "cpu_cores": n.cpu_cores, "mem_total_mb": n.mem_total_mb, "disk_total_gb": n.disk_total_gb,
            "city": n.city, "country": n.country, "healthy": n.healthy,
        })).collect::<Vec<_>>(),
    })))
}

/// Serve a build-cache blob to mesh peers (the P2P side of the build cache).
/// Read-only; peers pull `node_modules` tarballs by content-addressed key.
async fn buildcache_get(Path(key): Path<String>) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    // Key is a hex content hash — reject anything else (path-traversal guard).
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = crate::git::cache_root().join(format!("{key}.tar"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            // #22: attach a content digest (integrity) + an HMAC signature
            // (authenticity, when a fleet secret is set) so the puller can reject
            // corrupted or forged artifacts.
            let mut headers = vec![
                (axum::http::header::CONTENT_TYPE.as_str().to_string(), "application/x-tar".to_string()),
                (crate::git::ARTIFACT_SHA_HEADER.to_string(), crate::git::artifact_sha256(&bytes)),
            ];
            if let Some(secret) = crate::git::artifact_secret() {
                headers.push((crate::git::ARTIFACT_SIG_HEADER.to_string(), crate::git::artifact_sig(&secret, &bytes)));
            }
            let mut resp = bytes.into_response();
            for (k, v) in headers {
                if let (Ok(name), Ok(val)) = (
                    axum::http::HeaderName::from_bytes(k.as_bytes()),
                    axum::http::HeaderValue::from_str(&v),
                ) {
                    resp.headers_mut().insert(name, val);
                }
            }
            Ok(resp)
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// Framework-Defined Infrastructure: the catalog of frameworks the builder can
/// detect and compile into the Build Output API.
async fn build_frameworks() -> Json<Value> {
    Json(json!(fluid_build::PRESETS))
}

// ---- Deployments (previews) ----

/// The tenant (team slug) for a request, resolved with a strict trust order:
///
/// 1. **Verified JWT claims** (`claims`, injected into request extensions by
///    [`crate::auth::require_auth`]). The `tenant` claim is cryptographically
///    bound to the user's token, so when a JWT is present the spoofable
///    `x-hive-team` header is ignored entirely. This closes the tenant-isolation
///    bypass where any user could impersonate any team via the header.
/// 2. A platform **API key** (`Authorization: Bearer hive_…`), which returns the
///    key's bound team.
/// 3. **Dev mode only** (`!auth::enforced()`, i.e. `HIVE_JWT_SECRET` unset): the
///    dashboard's `x-hive-team` header, so unauthenticated local dev still works.
/// 4. Default: `"personal"`.
pub(crate) fn tenant(c: &Arc<CloudState>, h: &HeaderMap, claims: Option<&crate::auth::Claims>) -> String {
    let header_team = h
        .get("x-hive-team")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    resolve_tenant(claims, api_key_team(c, h), header_team, crate::auth::enforced())
}

/// Pure tenant-resolution priority (no request/state deps, so it's unit-testable).
/// JWT claim > API key > (dev-mode only) `x-hive-team` header > "personal".
fn resolve_tenant(
    claims: Option<&crate::auth::Claims>,
    api_team: Option<String>,
    header_team: Option<String>,
    enforced: bool,
) -> String {
    // 1. JWT claim wins — authoritative, cryptographically bound, not spoofable.
    if let Some(cl) = claims {
        return norm(&cl.tenant).to_string();
    }
    // 2. Platform API key → its bound team.
    if let Some(team) = api_team {
        return team;
    }
    // 3. Dev mode only: trust the x-hive-team header when no JWT auth is enforced.
    if !enforced {
        if let Some(team) = header_team {
            return team;
        }
        // Dev default: the single local user is the platform owner ("personal").
        return "personal".into();
    }
    // ENFORCED mode, no JWT claim and no API key: this request is UNAUTHENTICATED.
    // It must NOT fall back to "personal" (the owner's namespace) — that would
    // serve the owner's data to any unauthenticated read (the window before a
    // user's hive_jwt cookie is minted, or any tokenless caller). Scope it to an
    // anonymous namespace that owns NOTHING, so an unauthenticated read yields an
    // empty result instead of leaking another tenant's data.
    ANON_TENANT.into()
}

/// Namespace for unauthenticated requests under JWT enforcement — owns no data.
/// Deliberately not a valid team slug so nothing is ever created under it.
pub(crate) const ANON_TENANT: &str = "__anon__";

/// Namespace for a STORED RECORD whose tenant tag was lost or never set — a
/// gossiped `DeployRecord`/`DeploymentInfo` deserialized with `tenant: ""`
/// (`#[serde(default)]`, e.g. from a pre-tenancy snapshot or a stale-binary peer),
/// a restored snapshot record `Gateway::restore` never re-normalized, or a
/// project absent from a node's local (never-gossiped) `ProjectStore`.
///
/// `norm()` maps that same empty string to the LITERAL "personal" slug — which
/// is simultaneously the platform owner's real, live tenant — so every tag-loss
/// event anywhere in the gossip/persist/restore pipeline used to fail OPEN into
/// the owner's personal project list instead of failing closed (the multitenancy
/// leak: an org's projects appearing under the owner's personal view). This
/// mirrors `ANON_TENANT`'s fail-closed design for the read-request side; use it
/// (never `norm`) wherever a STORED tag is being interpreted, so a lost tag is
/// invisible to every tenant rather than adopted by the owner's.
pub(crate) const UNTAGGED_TENANT: &str = "__untagged__";

/// Interpret a STORED record's tenant tag: empty (tag-loss) resolves to the
/// fail-closed `UNTAGGED_TENANT` sentinel, never to the owner's "personal"
/// namespace. Use this instead of `norm()` for `DeployRecord.tenant`,
/// `ProjectSettings.team`, and any other persisted/gossiped ownership tag;
/// `norm()` remains correct for its existing callers that resolve a REQUEST's
/// tenant (where an empty resolved value legitimately means personal/dev-mode).
pub(crate) fn record_tenant(tag: &str) -> &str {
    if tag.trim().is_empty() { UNTAGGED_TENANT } else { tag }
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
pub(crate) fn norm(team: &str) -> &str {
    if team.is_empty() { "personal" } else { team }
}

/// Multi-tenant ownership guard: resolve the caller's tenant and verify it owns
/// `project`. Returns the tenant slug, or 403 when the project belongs to a
/// different team. EVERY handler that takes a project name from the path/body
/// and reads or mutates that project's state must call this — the path param is
/// attacker-controlled, the tenant is not.
pub(crate) fn require_project(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    project: &str,
) -> Result<String, (StatusCode, String)> {
    let t = tenant(c, h, claims);
    if project_owned_by(c, project, &t) {
        Ok(t)
    } else {
        Err((StatusCode::FORBIDDEN, "project belongs to a different team".into()))
    }
}

/// Whether `project` belongs to tenant `t`, judged fleet-aware. `ProjectStore`
/// rows are NODE-LOCAL (never gossiped): on any node that never ran this
/// project's deploy — including the control-plane leader for remotely-placed
/// projects — `team_of` is UNTAGGED, so a row-only comparison 403'd the
/// project's REAL team on every distributed read. Order of evidence:
/// 1. Local settings row matches — owned.
/// 2. Any deployment record (local gateway or gossiped peer copy) carries the
///    tenant tag — owned. Same authority `dep_list`/`deployment_build` trust.
/// 3. No row AND no deployment evidence anywhere in view: the project is
///    unknown to this node — optimistically trust the caller's verified tenant
///    (the sandboxes_api::require pattern). Deliberately narrower than that
///    one: any visible deployment record for the project keeps this strict, so
///    a wrong-team caller probing a real project still fails.
pub(crate) fn project_owned_by(c: &Arc<CloudState>, project: &str, t: &str) -> bool {
    let owner = c.projects.team_of(project);
    if norm(&owner) == t {
        return true;
    }
    let mut dep_seen = false;
    for r in c.gw.deployment_records() {
        if r.project.eq_ignore_ascii_case(project) {
            dep_seen = true;
            if record_tenant(&r.tenant) == t {
                return true;
            }
        }
    }
    for d in c.peer_deployments.read().values().flatten() {
        if d.project.eq_ignore_ascii_case(project) {
            dep_seen = true;
            if record_tenant(&d.tenant) == t {
                return true;
            }
        }
    }
    record_tenant(&owner) == UNTAGGED_TENANT && !dep_seen
}

/// Multi-tenant ownership guard for TEAM-level resources: resolve the caller's
/// tenant and verify it equals `slug` (a platform operator may act on any
/// team). When `min_role` is non-empty, additionally require the caller's
/// OWN role within that team (from the verified JWT claim — never
/// client-supplied) to be one of the listed values. Every `/v1/teams/:slug*`
/// handler must call this — the path `slug` is attacker-controlled, the
/// caller's tenant/role are not.
pub(crate) fn require_team(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    slug: &str,
    min_role: &[&str],
) -> Result<String, (StatusCode, String)> {
    if claims.map(|cl| cl.platform_admin).unwrap_or(false) {
        return Ok(tenant(c, h, claims));
    }
    let t = tenant(c, h, claims);
    if norm(&t) != norm(slug) {
        return Err((StatusCode::FORBIDDEN, "team belongs to a different tenant".into()));
    }
    if !min_role.is_empty() {
        let role = claims.map(|cl| cl.role.as_str()).unwrap_or("");
        if !min_role.contains(&role) {
            return Err((StatusCode::FORBIDDEN, "insufficient team role for this action".into()));
        }
    }
    Ok(t)
}

/// Multi-tenant ownership guard for DOMAIN resources: get-or-create the
/// domain record (first registrant claims it, matching `DomainStore::ensure`'s
/// existing semantics) and verify the caller's tenant owns it. Returns the
/// tenant, or 403 when a DIFFERENT tenant already registered this domain
/// string. `DomainStore` is keyed globally by the domain string with no
/// built-in ownership check, so every `/v1/domains/:domain*` handler that
/// creates-or-reads must call this.
fn require_domain_owner(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    domain: &str,
) -> Result<String, (StatusCode, String)> {
    let t = tenant(c, h, claims);
    let rec = c.domains.ensure(domain, &t);
    if norm(&rec.tenant) != norm(&t) {
        return Err((StatusCode::FORBIDDEN, "domain belongs to a different team".into()));
    }
    Ok(t)
}

/// Like [`require_domain_owner`] but does NOT create the domain record if it
/// doesn't exist — for record-level edit/delete operations, where `ensure()`'s
/// create-as-a-side-effect would be the wrong behavior for a domain nobody
/// has registered yet. A missing domain is left for the underlying
/// add/update/delete call to naturally report NOT_FOUND.
fn require_domain_owner_if_exists(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    domain: &str,
) -> Result<String, (StatusCode, String)> {
    let t = tenant(c, h, claims);
    if let Some(rec) = c.domains.get(domain) {
        if norm(&rec.tenant) != norm(&t) {
            return Err((StatusCode::FORBIDDEN, "domain belongs to a different team".into()));
        }
    }
    Ok(t)
}

/// Pure core of the platform-operator check (no env/global deps, so it's
/// unit-testable without touching the process-global `HIVE_JWT_SECRET` other
/// tests already own). Gate on `platform_admin`, NOT `role` — `role: "owner"`
/// only ever means "owner of `tenant`", and every user is the owner of their
/// own personal namespace. Using `role` here let any signed-up user reach
/// global WAF/CDN/routing mutations. `platform_admin` is independently
/// derived at mint time from this backend's own `owner_email`, never
/// client-asserted.
fn operator_allowed(claims: Option<&crate::auth::Claims>, enforced: bool) -> bool {
    if !enforced {
        return true;
    }
    matches!(claims, Some(cl) if cl.platform_admin)
}

/// Platform-operator guard for GLOBAL (non-tenant) infrastructure mutations —
/// WAF rules, bot policy, CDN purge, routing, runtime-cache. When JWT auth is
/// enforced, only a genuine platform operator may mutate; in dev (no
/// enforcement) it's open, matching the rest of the dev-mode API.
pub(crate) fn require_operator(claims: Option<&crate::auth::Claims>) -> Result<(), (StatusCode, String)> {
    if operator_allowed(claims, crate::auth::enforced()) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "platform-level change requires a platform operator".into()))
    }
}

/// Authenticated-READ guard for the topology views the dashboard shows every
/// signed-in user (network page: nodes/cluster/overview). Any verified claims
/// pass — the handler then decides between the full operator payload and the
/// sanitized tenant view. Missing claims while enforced is 401, not 403: the
/// dashboard's transparent cookie re-mint keys on 401, and "no session" is an
/// authentication failure, not an authorization decision.
pub(crate) fn require_auth_read(claims: Option<&crate::auth::Claims>) -> Result<(), (StatusCode, String)> {
    if !crate::auth::enforced() || claims.is_some() {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "sign-in required".into()))
    }
}

/// Public ingress region code for a region — the `<dep>.<code>.ngrok.pizza` label
/// (virginia→iad, bangkok→sin, san-jose→sfo, los-angeles→lax). Unknown regions
/// return "" so the dashboard falls back to the legacy region-agnostic zone URL.
pub fn region_code(region: &str) -> &'static str {
    match region {
        "virginia" => "iad",
        "bangkok" => "sin",
        "san-jose" => "sfo",
        "los-angeles" => "lax",
        "hong-kong" => "hkg",
        _ => "",
    }
}

async fn dep_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    // STRICT multi-tenant isolation: a request only ever sees the deployments for
    // its active tenant (the Clerk org slug / team via `x-hive-team`, or an API
    // key's team). Projects in other tenants are never returned — this is what
    // prevents data bleeding across accounts when switching teams.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // node name -> region, to resolve where each deployment ACTUALLY runs.
    let node_region: std::collections::HashMap<String, String> =
        c.registry.nodes().into_iter().map(|n| (n.name, n.region)).collect();
    // deployment id -> the SET of regions it's actually hosted in (multi-region aware).
    let mut host_regions: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();

    let mut list: Vec<_> = c
        .gw
        .list()
        .into_iter()
        .filter(|d| {
            // Trust the deployment's OWN tenant tag first — it's authoritative
            // for who actually built/owns it (matches the peer branch below).
            // `ProjectStore.team_of` is NODE-LOCAL and never gossiped, so on a
            // node other than the one that ran `set_team` it can miss even for
            // a correctly-tagged deployment; only consult it as a fallback when
            // the record itself was never tagged, and even then fail CLOSED
            // (never silently adopt into the owner's personal view).
            let own = record_tenant(&d.tenant);
            let effective =
                if own == UNTAGGED_TENANT { record_tenant(&c.projects.team_of(&d.project)).to_string() } else { own.to_string() };
            effective == t
        })
        .collect();
    // Locally-hosted deployments run in THIS node's region.
    for d in &list {
        host_regions.entry(d.id.to_string()).or_default().insert(c.region.clone());
    }
    // Merge in deployments the placement scheduler placed on OTHER mesh nodes, and
    // record each one's actual host region(s). Collect regions from EVERY peer that
    // hosts it (multi-region); push to the list only once (dedup by id).
    let mut seen: std::collections::HashSet<String> = list.iter().map(|d| d.id.to_string()).collect();
    for (node, deps) in c.peer_deployments.read().iter() {
        let nr = node_region.get(node);
        for d in deps {
            // Fail CLOSED on a lost/never-set gossiped tag: `record_tenant`
            // resolves empty to `UNTAGGED_TENANT`, which can never equal a real
            // tenant slug — unlike `norm`, which would collapse it into the
            // owner's literal "personal" namespace (the leak).
            if record_tenant(&d.tenant) != t {
                continue;
            }
            if let Some(r) = nr {
                host_regions.entry(d.id.to_string()).or_default().insert(r.clone());
            }
            if seen.insert(d.id.to_string()) {
                list.push(d.clone());
            }
        }
    }
    list.sort_by_key(|d| std::cmp::Reverse(d.created_at_ms));
    // Enrich each with its region + public ingress code for the region-encoded URL
    // (`<dep>.<code>.ngrok.pizza`). Use the region the deployment ACTUALLY runs in —
    // NOT the project's configured region, which can drift across nodes or be stale
    // until a redeploy (that mislabels the URL and makes the region ingress cross-hop).
    // Prefer the configured region only when the deployment genuinely runs there
    // (honors intent for multi-region); else any real host; else config, else default.
    let out: Vec<Value> = list
        .into_iter()
        .map(|d| {
            let hosts = host_regions.get(&d.id.to_string());
            let cfg = c.projects.get(&d.project).functions.regions.first().cloned();
            let region = match (hosts, &cfg) {
                (Some(set), Some(c)) if set.contains(c) => c.clone(),
                (Some(set), _) => set.iter().next().cloned().unwrap_or_else(|| "san-jose".into()),
                (None, Some(c)) => c.clone(),
                (None, None) => "san-jose".to_string(),
            };
            let mut v = serde_json::to_value(&d).unwrap_or_else(|_| json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert("region".to_string(), json!(region));
                o.insert("region_code".to_string(), json!(region_code(&region)));
            }
            v
        })
        .collect();
    Json(json!(out))
}

/// Node-to-node: this node's full deployment list (all tenants), for peers to
/// build a fleet-wide view. Consumed by the gossip loop into `peer_deployments`.
pub(crate) async fn fleet_deployments(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!({ "node": c.node_name, "deployments": c.gw.list() }))
}

async fn dep_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<fluid_core::DeployRequest>,
) -> Json<Value> {
    // Tag the deployment (and the cells it spawns) with the caller's tenant so
    // compute is partitioned per team — mirrors gw.deploy()'s defaults otherwise.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Honor the request's environment + git metadata (previously both were forced —
    // `production=true`, `git=None` — which made preview deployments and
    // commit-scoped routing, e.g. microfrontend fallback resolution, unreachable via
    // this route). A project's first deploy still claims the production alias inside
    // `deploy_full` even when `production=false`, so URLs resolve.
    let creator = req.creator.clone().unwrap_or_else(|| "you".into());
    let info = c.gw.deploy_full(
        req.root,
        req.manifest,
        creator,
        req.git,
        req.production,
        fluid_core::DeployState::Ready,
        t,
    );
    // Persist so the deployment survives a node restart (without this it lived
    // only in memory and was lost on reboot).
    crate::persist::persist(&c);
    Json(json!(info))
}

/// Roll back / promote: make an existing deployment the project's production.
pub(crate) async fn dep_promote(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    // Ownership check BEFORE the local-success path — this used to only be
    // resolved for the cross-node proxy fallback below, so any caller with a
    // valid JWT for ANY tenant could instantly repoint a DIFFERENT tenant's
    // production alias to an arbitrary deployment id hosted on this node.
    let t0 = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(d) = c.gw.list().into_iter().find(|d| d.id.0 == id) {
        // Judge ownership from the record's OWN tenant tag (authoritative +
        // present), falling back to the fleet-aware project predicate — the
        // node-local project row is UNTAGGED on nodes that never ran the deploy.
        if record_tenant(&d.tenant) != t0 && !project_owned_by(&c, &d.project, &t0) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    if let Some(info) = c.gw.promote(&id) {
        crate::persist::persist(&c);
        let ev = c.event(&c.region, "PROMOTE", &info.alias, "/", 200, "deploy", &format!("rolled back to {id}"));
        c.record(ev);
        crate::webhooks::dispatch(&c.webhooks, &info.project, "deployment.promoted",
            json!({ "id": id, "project": info.project, "url": c.deploy_url(&info.alias) }));
        return Ok(Json(json!(info)));
    }
    // Not hosted locally — the placement scheduler put this deployment on a peer.
    // Proxy the promote to its host NODE over iroh (FC nodes have no HTTP admin) so
    // instant rollback works cross-node.
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) = post_to_host(&c, &node, &format!("/v1/deployments/{id}/promote"), &t0).await {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// POST to a host node by NAME (no body): HTTP admin if we know its URL, else over
/// the iroh mesh. The mutation counterpart of `fetch_from_host` — proxies an action
/// (e.g. instant-rollback) to an FC-hosted deployment's node. Returns the response.
async fn post_to_host(c: &Arc<CloudState>, node: &str, path: &str, team: &str) -> Option<Value> {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        if let Ok(r) = c
            .http
            .post(format!("{admin}{path}"))
            .header("x-hive-team", team)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            if r.status().is_success() {
                if let Ok(v) = r.json::<Value>().await {
                    return Some(v);
                }
            }
        }
    }
    let target = c
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    if let Some((id, addr)) = target {
        let sep = if path.contains('?') { '&' } else { '?' };
        let p = format!("{path}{sep}{}", mesh_team_qs(team));
        // See `fetch_from_host`: bumped from 15s to give the discovery fallback in
        // `PeerPool::acquire` room to complete instead of being cut off here.
        if let Some(b) = crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &[], 20).await {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
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
        deployment: String::new(),
        request_id: String::new(),
    });
}

/// Delete a single deployment (unregisters its functions).
async fn dep_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Ownership: a deployment belongs to its project's team. If it exists but
    // isn't ours, 404 (don't disclose another tenant's resource). If it doesn't
    // exist, fall through to the idempotent no-op remove (unchanged behavior).
    if let Some(d) = c.gw.list().into_iter().find(|d| d.id.0 == id) {
        // Record's own tag first (authoritative), fleet-aware fallback second.
        if record_tenant(&d.tenant) != t && !project_owned_by(&c, &d.project, &t) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let project = c.gw.remove(&id).await;
    if let Some(p) = &project {
        record_event(&c, p, "delete", &format!("deleted deployment {id}"));
    }
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id, "project": project })))
}

/// Full teardown of a deleted project's resources BEYOND its deployment
/// records: provisioned databases (with queue/vector/blob payload purge, not
/// just the catalog entry), backing podman volumes, the on-disk source
/// checkout(s), and build history — plus a DURABLE audit record. Previously,
/// deleting a project left all of these behind indefinitely (a real GDPR
/// Art.17 gap: the only record of the deletion itself lived in a 500-entry,
/// non-persisted ring buffer that could rotate out within minutes and never
/// survived a restart). Called from both the local delete path and the
/// mesh-cascade receiving arm so a project deleted from ANY node gets the
/// same real cleanup.
async fn purge_project_resources(c: &Arc<CloudState>, project: &str, team: &str, n_deployments: usize) {
    c.audit.record(team, "user", "delete", "project", project, &format!("{n_deployments} deployment(s)"));

    for d in c.databases.list(Some(project)) {
        if !d.replicas.is_empty() {
            crate::db_replicate::remove_replicas(c.clone(), d.clone());
        }
        if let Some(container) = d.container.clone() {
            // DB containers are ALWAYS podman-created, even on macOS — see
            // `databases.rs::ensure_project_db_net`'s doc comment (static-IP
            // networking has no Apple `container` equivalent). Every
            // `podman rm`/DB-container teardown site in this file (this one,
            // plus the two below in `database_delete`/the "remove" op) stays
            // hardcoded to podman for that reason, unconditionally.
            let _ = tokio::process::Command::new("podman")
                .args(["rm", "-f", &container])
                .env(
                    "PATH",
                    format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}", std::env::var("PATH").unwrap_or_default()),
                )
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }
        c.databases.remove_db_and_purge_data(&d.id, &d.team);
    }

    purge_project_podman_volumes(project).await;
    c.builds.remove_for_project(project);
    purge_project_source_dirs(project).await;
}

/// Remove every volume named `hive-vol-<project>` or `hive-vol-<project>-<service>`
/// (compose/multi-service deployments each get their own suffixed volume) — these
/// are created for app/compose/Containerfile deployments' persistent `/data` and,
/// unlike the container itself, survive removal of whatever mounted them.
/// Best-effort: an unreachable/missing container CLI must never fail the broader
/// project-delete flow.
///
/// Checks BOTH backends on macOS: a project's volume could live in podman's store
/// (compose/multi-service deploys always use podman there — see
/// `hive_backend::container_cli`'s module doc) or in Apple `container`'s store
/// (single-container app/Containerfile deploys), and this has no record of which
/// one a given project actually used — only Linux fleet nodes ever have just one.
async fn purge_project_podman_volumes(project: &str) {
    let prefix = format!("hive-vol-{}", crate::git::sanitize_tag(project));
    let path_env = format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}", std::env::var("PATH").unwrap_or_default());
    let backends: &[bool] = if hive_backend::container_cli::is_apple_default() { &[false, true] } else { &[false] };
    for &apple in backends {
        let bin = hive_backend::container_cli::bin(apple);
        for name in hive_backend::container_cli::list_volume_names(apple, &path_env).await {
            // Exact match or a `-`-delimited suffix only (the per-service naming
            // shape from `container_volume_cfg`, e.g. "hive-vol-app-worker") — a
            // plain substring match would also reap an unrelated project's volume
            // whose name happens to CONTINUE the same characters with no
            // delimiter (e.g. "app" must not touch "appearance"'s volume). This
            // does NOT disambiguate a different project whose name is itself
            // `<target>-<anything>` (e.g. "app" vs a real project literally named
            // "app-v2") — project names are allocated to be globally unique (see
            // the project-name allocator's own comment in git.rs), so that
            // collision is not expected in practice, but is a known limit of this
            // prefix-based scheme, not a claim this check fully closes.
            if name.is_empty() || !(name == prefix || name.starts_with(&format!("{prefix}-"))) {
                continue;
            }
            let _ = tokio::process::Command::new(bin)
                .args(hive_backend::container_cli::volume_rm_args(apple, &name))
                .env("PATH", &path_env)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }
    }
}

/// Synchronously remove every on-disk source checkout for `project`, rather
/// than relying solely on the periodic `gc_build_dirs` timer (which could
/// leave a leaked secret or PII in the raw source tree readable on the host
/// disk for up to ~40 minutes after an explicit delete request — the timer
/// polls every 10 minutes and only reaps dirs untouched for 30+ minutes).
async fn purge_project_source_dirs(project: &str) {
    let base = crate::git::deploy_root();
    let prefix = format!("{project}-");
    let Ok(mut rd) = tokio::fs::read_dir(&base).await else { return };
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = tokio::fs::remove_dir_all(e.path()).await;
        }
    }
}

/// Delete an entire project: all its deployments + settings. By default this
/// cascades across the mesh (removing the project from any peer node the
/// placement scheduler put it on); `?cascade=false` deletes only on this node
/// (used by the scheduler's relocate cleanup, which must NOT cascade back and
/// wipe the freshly-placed copy on another node).
async fn project_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Query(q): Query<CascadeQ>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Authorize against the local store if we have it, else against the gossiped
    // fleet — a project placed by the scheduler may live ONLY on a peer, so it's
    // absent from this node's project store (team_of would default to "personal"
    // and wrongly 404). Allow the delete if the requester's tenant owns the
    // project anywhere in the mesh.
    let authorized = c.projects.get_if_set(&project).map(|s| norm(&s.team) == t).unwrap_or(false)
        // a deployment of this project hosted locally under the requester's tenant
        || c.gw.list().iter().any(|d| d.project == project && record_tenant(&d.tenant) == t)
        // …or hosted on a peer (project lives only on a scheduler-placed node)
        || c.peer_deployments.read().values().flatten().any(|d| d.project == project && record_tenant(&d.tenant) == t);
    // Not locally verifiable ≠ deniable: after a partial delete (or right after a
    // restart) this node may hold NO trace of a project that still serves on a
    // peer, and the gossip view can be sparse — a hard 404 here left ORPHANS
    // undeletable. Every receiving node re-checks ownership against ITS OWN store
    // before acting, so the safe behavior is: skip local teardown, but still
    // broadcast the single-hop cascade and let owners tear down their copies.
    if !authorized {
        if q.cascade.unwrap_or(true) {
            let c2 = c.clone();
            let project2 = project.clone();
            let team2 = t.clone();
            tokio::spawn(async move {
                let peers: Vec<String> = c2
                    .registry
                    .nodes()
                    .into_iter()
                    .filter(|n| !n.is_self && n.healthy)
                    .map(|n| n.name)
                    .collect();
                for node in peers {
                    dispatch_project_delete(&c2, &node, &project2, &team2).await;
                }
            });
            return Ok(Json(json!({ "project": project, "removed_deployments": [], "note": "not hosted here — cascade broadcast to peers" })));
        }
        return Err(StatusCode::NOT_FOUND);
    }
    // Remove locally + persist FIRST, then respond — so the dashboard gets an
    // immediate result. The cross-mesh teardown runs in the BACKGROUND; a slow or
    // unreachable peer must not make the request error after the delete already
    // succeeded (the "choppy: HTTP error, then it deletes" symptom).
    let ids = c.gw.remove_project(&project).await;
    record_event(&c, &project, "delete", &format!("deleted project {project} ({} deployment(s))", ids.len()));
    purge_project_resources(&c, &project, &t, ids.len()).await;
    c.projects.remove(&project);
    crate::persist::persist(&c);
    crate::webhooks::dispatch(&c.webhooks, &project, "project.removed", json!({ "project": project, "deployments": ids.len() }));

    // Background cascade (cascade=false → single hop, no loops): BROADCAST the
    // delete to EVERY healthy peer instead of a gossip-derived "hosting" set —
    // peer_deployments/peer_routes can be sparse right after restarts, which made
    // the cascade silently dispatch to NOBODY and leave remote copies serving
    // (the "deleting a project doesn't work" bug). The receiving arm is
    // team-checked and idempotent, and deletes are rare, so N-1 tiny messages is
    // the correct trade.
    if q.cascade.unwrap_or(true) {
        let c2 = c.clone();
        let project2 = project.clone();
        let team2 = t.clone();
        tokio::spawn(async move {
            let peers: Vec<String> = c2
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self && n.healthy)
                .map(|n| n.name)
                .collect();
            for node in peers {
                dispatch_project_delete(&c2, &node, &project2, &team2).await;
            }
        });
    }
    Ok(Json(json!({ "project": project, "removed_deployments": ids })))
}

/// Tear down a project on ONE peer node, over whatever control-plane transport
/// reaches it: the HTTP admin URL when known (local tunnel peers), else the iroh
/// mesh (Firecracker fleet nodes — `node_admins` is EMPTY for them since the SSH
/// tunnels were cut, which is exactly why HTTP-only cascade deletes silently
/// never reached the hosting nodes and "deleting a project didn't work").
/// `cascade=false` semantics on the receiving side: single hop, no loops.
pub(crate) async fn dispatch_project_delete(c: &Arc<CloudState>, node: &str, project: &str, team: &str) {
    if node == c.node_name {
        return; // local copy already handled by the caller
    }
    // Bind out of the lock FIRST so no parking_lot guard is held across the await
    // (the spawned cascade future must be `Send`).
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        let _ = c
            .http
            .delete(format!("{admin}/v1/projects/{project}?cascade=false"))
            .header("x-hive-team", team.to_string())
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        return;
    }
    // iroh mesh path: resolve the peer's cryptographic identity + address from the
    // registry and dispatch the delete as a gossip POST (team rides as `?team=`).
    let target = c
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    if let Some((id, addr)) = target {
        let path = format!("/v1/projects/{project}/delete?{}", mesh_team_qs(team));
        // See `fetch_from_host`: bumped from 15s for discovery-fallback headroom.
        if crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &path, &[], 20).await.is_none() {
            tracing::warn!(node, project, "project delete dispatch over iroh failed (peer will be retried on next delete)");
        }
    } else {
        tracing::warn!(node, project, "project delete: no route to hosting node (no admin URL, no iroh identity)");
    }
}

/// LOCAL-ONLY project teardown used by the mesh delete arm (the receiving side of
/// [`dispatch_project_delete`]): remove deployments + settings on THIS node,
/// persist, and record the event. Team-checked by the caller. `team` is used
/// only for the durable audit record and to resolve owned databases' team
/// (falls back to this node's own `team_of` if the caller has none, e.g. a
/// legacy single-hop dispatch that predates the `?team=` param).
pub(crate) async fn delete_project_local(c: &Arc<CloudState>, project: &str, team: &str) -> usize {
    let ids = c.gw.remove_project(project).await;
    record_event(c, project, "delete", &format!("deleted project {project} ({} deployment(s), mesh cascade)", ids.len()));
    let team = if team.trim().is_empty() { norm(&c.projects.team_of(project)).to_string() } else { team.to_string() };
    purge_project_resources(c, project, &team, ids.len()).await;
    c.projects.remove(project);
    crate::persist::persist(c);
    ids.len()
}

/// Query for project_delete: `cascade=false` deletes only on this node (no mesh
/// fan-out). Defaults to cascade when absent.
#[derive(Deserialize, Default)]
struct CascadeQ {
    #[serde(default, deserialize_with = "de_lenient_bool")]
    cascade: Option<bool>,
}

/// Body for a redeploy from the Redeploy modal (all fields optional/defaulted so
/// a bare `{}` still works). `target` chooses the environment (production /
/// preview); `use_cache` toggles the existing build cache.
#[derive(Deserialize, Default)]
struct RedeployBody {
    #[serde(default)]
    target: Option<String>,
    #[serde(default = "default_true_b")]
    use_cache: bool,
}

/// Redeploy a project's newest git source (create a fresh deployment).
/// Find the latest git source for a project — from this node's gateway, OR (when
/// the placement scheduler put the project on a peer) from the gossiped fleet
/// deployments. Lets redeploy work even when the project is hosted remotely.
fn git_for_project_fleet(c: &Arc<CloudState>, project: &str) -> Option<fluid_core::GitSource> {
    if let Some(g) = c.gw.git_for_project(project) {
        return Some(g);
    }
    c.peer_deployments
        .read()
        .values()
        .flatten()
        .filter(|d| d.project == project)
        .max_by_key(|d| d.created_at_ms)
        .and_then(|d| d.git.clone())
}


/// Peer node NAMES hosting `project` — addressed by node (not HTTP admin URL) so the
/// caller can reach them over the iroh mesh via `fetch_from_host` (FC nodes have no
/// HTTP admin URL in `node_admins`). Pairs with `peer_nodes_for_tenant` below.
fn host_nodes_for_project(c: &Arc<CloudState>, project: &str) -> Vec<String> {
    c.peer_deployments
        .read()
        .iter()
        .filter(|(_, deps)| deps.iter().any(|d| d.project == project))
        .map(|(node, _)| node.clone())
        .collect()
}

/// GET `path` from a peer admin, forwarding the team header; returns parsed JSON.
async fn proxy_get_json(c: &Arc<CloudState>, admin: &str, path: &str, team: &str) -> Option<Value> {
    let mut rb = c
        .http
        .get(format!("{admin}{path}"))
        .header("x-hive-team", team)
        .timeout(std::time::Duration::from_secs(10));
    // Under JWT enforcement `x-hive-team` alone resolves to ANON on the target
    // (headers are client-supplied, never trusted) — every tenant-gated read
    // proxied here silently 403'd. Attach the same short-lived signed service
    // delegation `fanout_remote` uses so this node-to-node read authenticates.
    if crate::auth::enforced() {
        if let Ok(tok) = crate::auth::issue("mesh-internal", team, "service", false, 60) {
            rb = rb.bearer_auth(tok);
        }
    }
    let resp = rb.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

// ---- node-addressed read-view proxying (HTTP admin OR iroh mesh) ----------------
// The NAT'd coordinator has NO HTTP admin path to the FC nodes (SSH tunnels cut), so
// `node_admins` is empty for them and the HTTP `host_admin_for_*` helpers return None
// — which silently emptied the dashboard's per-deployment read views (resources, and
// the workflows tab) for FC-hosted projects. These resolve the host NODE NAME instead
// and fetch over whichever transport works: HTTP admin if known, else the iroh mesh.

/// Host node NAME for deployment `id` (no HTTP-admin requirement).
fn host_node_for_deployment(c: &Arc<CloudState>, id: &str) -> Option<String> {
    c.peer_deployments
        .read()
        .iter()
        .find(|(_, deps)| deps.iter().any(|d| d.id.to_string() == id))
        .map(|(node, _)| node.clone())
}

/// Host node NAME for any deployment of `project` (no HTTP-admin requirement).
fn host_node_for_project(c: &Arc<CloudState>, project: &str) -> Option<String> {
    c.peer_deployments
        .read()
        .iter()
        .find(|(_, deps)| deps.iter().any(|d| d.project == project))
        .map(|(node, _)| node.clone())
}

/// Peer node NAMES hosting any of the tenant's projects (for fleet-wide aggregation).
fn peer_nodes_for_tenant(c: &Arc<CloudState>, team: &str) -> Vec<String> {
    let pd = c.peer_deployments.read();
    let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (node, deps) in pd.iter() {
        if deps.iter().any(|d| record_tenant(&d.tenant) == team) {
            out.insert(node.clone());
        }
    }
    out.into_iter().collect()
}

/// Every deployment across the WHOLE fleet, ALL tenants (operator scope): the
/// locally-hosted set plus every gossiped `peer_deployments` entry, deduped by id.
/// The ops overview + data browser run on one node, but the placement scheduler
/// hosts deployments on peers — a bare `c.gw.list()` returns only what this node
/// hosts (often nothing on a coordinator), which is why the ops console showed
/// "0 deployments" while peers served every project. This mirrors `dep_list`'s
/// aggregation but WITHOUT the tenant filter (operator-only callers).
fn fleet_deployments_all(c: &Arc<CloudState>) -> Vec<Value> {
    let mut list = c.gw.list();
    let mut seen: std::collections::HashSet<String> = list.iter().map(|d| d.id.to_string()).collect();
    for (_node, deps) in c.peer_deployments.read().iter() {
        for d in deps {
            if seen.insert(d.id.to_string()) {
                list.push(d.clone());
            }
        }
    }
    list.sort_by_key(|d| std::cmp::Reverse(d.created_at_ms));
    list.into_iter().map(|d| json!(d)).collect()
}

/// GET a read-view from a host node by NAME: HTTP admin if we have one, else over the
/// iroh mesh (resolve the node's id+addr from the gossip registry). Team rides as a
/// query param on the iroh path (no HTTP headers over that transport).
/// Aggregate per-function usage stats across the WHOLE fleet: the local node's
/// live stats plus each peer's `/v1/functions` (over HTTP admin or iroh gossip).
/// The billing meter loop uses this so a NAT'd coordinator still bills compute that
/// actually ran on the Firecracker nodes. Each `FunctionStats` carries its `tenant`.
pub async fn fleet_function_stats(c: &Arc<CloudState>) -> Vec<fluid_compute::FunctionStats> {
    let mut out: Vec<fluid_compute::FunctionStats> = c.fluid.stats();
    let self_name = c.node_name.clone();
    for n in c.registry.nodes() {
        if n.name == self_name || !n.healthy {
            continue;
        }
        if let Some(v) = fetch_from_host(c, &n.name, "/v1/functions", "").await {
            if let Ok(mut stats) = serde_json::from_value::<Vec<fluid_compute::FunctionStats>>(v) {
                out.append(&mut stats);
            }
        }
    }
    out
}

/// Build the mesh tenant query segment: `team=<t>` plus a signed, short-lived
/// (`&tok=<jwt>`) delegation token when JWT signing is configured. The host node
/// verifies the token as an authoritative, expiring tenant assertion instead of
/// trusting the raw `team=` param (see `gossip::team_claims`). Empty `team`
/// yields an empty segment (callers append nothing).
pub(crate) fn mesh_team_qs(team: &str) -> String {
    if team.is_empty() {
        return String::new();
    }
    if crate::auth::enforced() {
        if let Ok(tok) = crate::auth::issue("mesh-internal", team, "service", false, 60) {
            return format!("team={team}&tok={tok}");
        }
    }
    format!("team={team}")
}

pub(crate) async fn fetch_from_host(c: &Arc<CloudState>, node: &str, path: &str, team: &str) -> Option<Value> {
    // Bind out of the lock FIRST so no parking_lot guard is held across the await
    // (the dispatched future must be `Send`).
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        if let Some(v) = proxy_get_json(c, &admin, path, team).await {
            return Some(v);
        }
    }
    let target = c
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    if let Some((id, addr)) = target {
        let sep = if path.contains('?') { '&' } else { '?' };
        let p = format!("{path}{sep}{}", mesh_team_qs(team));
        // 20s (not the old flat 10s): `acquire`'s cached-hint attempt can now fall
        // back to a fresh-discovery dial (`PeerPool::dial_fresh`) when the hint is
        // stale, which needs headroom beyond `connect_budget()` alone to actually
        // complete instead of being cut off by this outer timeout.
        //
        // `addr` here is `n.iroh_addr` — a snapshot serialized at the HOST's own
        // boot/last-gossip time, so its relay (if any) can go stale exactly like
        // the direct addrs can. `request_to` re-hints the relay transport (this
        // node's live-gossiped registry view of `id`'s own/nearest relay_url —
        // see `gossip::relay_hinted_addr`) before dialing, so a call from e.g.
        // fc-hongkong to fc-sanjose hints sj's own live relay_url rather than
        // whatever was cached in `addr` at gossip time.
        if let Some(b) = crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_GET, &p, &[], 20).await {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

async fn project_redeploy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(body): Json<RedeployBody>,
) -> Result<Json<Value>, StatusCode> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project).map_err(|(code, _)| code)?;
    let git = git_for_project_fleet(&c, &project).ok_or(StatusCode::NOT_FOUND)?;
    let root_dir = Some(c.projects.root_dir_of(&project)).filter(|s| !s.is_empty());
    // Environment chosen in the modal: "production" | "preview". When absent the
    // branch decides (Vercel's classification).
    let target = body
        .target
        .map(|t| t.trim().to_lowercase())
        .filter(|t| t == "production" || t == "preview");
    let req = fluid_core::GitDeployRequest {
        repo_url: git.repo_url,
        branch: Some(git.branch).filter(|b| !b.is_empty()),
        project: Some(project),
        creator: Some("you".into()),
        production: true,
        target,
        use_cache: body.use_cache,
        root_dir,
        env: None, // redeploy: existing project env is read from the store at build time
        no_fanout: false, // dashboard redeploy is a coordinator deploy → schedule + fanout
        build_config: None, // coordinator reads its own store; fanout fills these per-target
        function_settings: None,
        redeploy: false, // goes straight to start_build (bypasses git_deploy naming)
        zip_b64: None,
        image_ref: None,
        image_port: None,
    };
    let build_id = crate::git::start_build(c.clone(), req);
    Ok(Json(json!({ "build_id": build_id })))
}

// ---- GitOps ----

/// All projects owned by a tenant plus their settings + git source — the data the
/// dashboard serializes into the committed `openedge.yaml`. FLEET-AWARE: iterating
/// only the local `ProjectStore` snapshot (as this handler did before) misses any
/// project this node never locally built/saw — `ProjectSettings` rows are
/// confirmed node-local-only, never gossiped (unlike `dep_list`/`admin_overview`,
/// which already merge `peer_deployments`). Union local settings with every
/// project name a fleet-aggregated deployment carries for this tenant, so a
/// project created (even with zero deployments yet) on ANOTHER node still
/// appears here — the exact class of bug that hid team Simpfi's `drugs-wtf`
/// project on 4 of 8 fleet nodes.
async fn gitops_projects(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (project, settings) in c.projects.snapshot() {
        if norm(&settings.team) != t {
            continue;
        }
        seen.insert(project.clone());
        let git = c.gw.git_for_project(&project);
        let prod = c.gw.list().into_iter().find(|d| d.project == project && d.production);
        // Enterprise deployment protection is part of the declarative config too —
        // include mode/scope (never the password) so protection changes commit.
        let prot = c.enterprise.protection(&project);
        out.push(json!({
            "project": project,
            "settings": c.projects.get_masked(&project),
            "git": git,
            "production": prod,
            "root_dir": settings.build.root_dir,
            "protection": { "mode": prot.mode, "scope": prot.scope },
        }));
    }
    // Fleet-visible fallback rows: a project this node has never locally seen a
    // ProjectSettings row for, but that the tenant owns per a gossiped deployment
    // record. Settings/git/protection are unavailable here (this node genuinely
    // has none) — surfaced with defaults rather than silently omitted, so the
    // project is at least visible/selectable instead of invisible.
    for (_node, deps) in c.peer_deployments.read().iter() {
        for d in deps {
            if record_tenant(&d.tenant) != t || !seen.insert(d.project.clone()) {
                continue;
            }
            out.push(json!({
                "project": d.project,
                "settings": crate::project_settings::ProjectSettings::default(),
                "git": Value::Null,
                "production": Value::Null,
                "root_dir": "",
                "protection": { "mode": "off", "scope": "preview" },
            }));
        }
    }
    // Second fallback tier: the fleet-replicated relational mirror (see
    // relational.rs) — the DURABLE fix. Unlike peer_deployments (which can
    // only know about a project that has at least one deployment somewhere),
    // this catches a project that was CREATED but never successfully
    // deployed anywhere in the fleet — `drugs-wtf`'s exact bug (zero
    // deployments fleet-wide, live-witnessed).
    for project in crate::relational::projects_for_team(&t).await {
        if !seen.insert(project.clone()) {
            continue;
        }
        out.push(json!({
            "project": project,
            "settings": crate::project_settings::ProjectSettings::default(),
            "git": Value::Null,
            "production": Value::Null,
            "root_dir": "",
            "protection": { "mode": "off", "scope": "preview" },
        }));
    }
    out.sort_by(|a, b| a["project"].as_str().unwrap_or("").cmp(b["project"].as_str().unwrap_or("")));
    Json(json!(out))
}

async fn gitops_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!(c.gitops.get(&t)))
}

#[derive(Deserialize)]
struct GitOpsLinkReq {
    repo: String,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    scope: String,
}

async fn gitops_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<GitOpsLinkReq>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let link = c.gitops.set_link(&t, &b.repo, &b.branch, &b.path, &b.scope);
    crate::persist::persist(&c);
    Json(json!(link))
}

async fn gitops_unlink(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.gitops.unlink(&t);
    crate::persist::persist(&c);
    Json(json!({ "unlinked": t }))
}

#[derive(Deserialize)]
struct GitOpsSynced {
    #[serde(default)]
    commit: String,
    #[serde(default)]
    hash: String,
}

async fn gitops_synced(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<GitOpsSynced>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let link = c.gitops.record_sync(&t, &b.commit, &b.hash);
    crate::persist::persist(&c);
    Json(json!(link))
}

/// Inbound GitHub webhook: on a push (or merged/updated PR) to a repo that backs
/// one or more existing projects, trigger a fresh production build+deploy from the
/// pushed commit — repos become deployable workflows (taubyte-style GitOps CI).
///
/// Auth: this route is in the `open` allowlist (GitHub can't present a platform
/// JWT). When `GITHUB_WEBHOOK_SECRET` is set the HMAC-SHA256 signature is verified;
/// with no secret configured it accepts unsigned deliveries (dev-open default).
async fn git_webhook(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Verify the GitHub signature when a secret is configured.
    if let Ok(secret) = std::env::var("GITHUB_WEBHOOK_SECRET") {
        if !secret.is_empty() {
            let sig = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !verify_github_sig(secret.as_bytes(), &body, sig) {
                return Err((StatusCode::UNAUTHORIZED, "bad signature".into()));
            }
        }
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("push")
        .to_string();
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("bad json: {e}")))?;

    // Extract the repo + branch + head commit, supporting `push` and
    // `pull_request` (opened/synchronize/reopened/closed) events. The deploy
    // TARGET follows Vercel's model: a PR always builds a PREVIEW; a push is
    // classified per-project from its branch (None => the production branch wins).
    let repo_full = payload["repository"]["full_name"].as_str().unwrap_or("").to_string();
    let mut target: Option<String> = None;
    let mut pr_number: Option<u64> = None;
    let (branch, commit) = match event.as_str() {
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or("");
            // A closed PR has nothing to (re)build. A merge fires a separate push
            // to the base branch, which is what produces the production deployment.
            if action == "closed" {
                return Ok(Json(json!({ "ignored": "pr closed" })));
            }
            // opened / synchronize / reopened / ready_for_review -> preview.
            target = Some("preview".into());
            pr_number = payload["number"].as_u64().or_else(|| payload["pull_request"]["number"].as_u64());
            let head_ref = payload["pull_request"]["head"]["ref"].as_str().unwrap_or("").to_string();
            let sha = payload["pull_request"]["head"]["sha"].as_str().unwrap_or("").to_string();
            (head_ref, sha)
        }
        "ping" => return Ok(Json(json!({ "pong": true }))),
        _ => {
            // push event -> classified from the branch at build time.
            if payload["deleted"].as_bool().unwrap_or(false) {
                return Ok(Json(json!({ "ignored": "branch deleted" })));
            }
            let r = payload["ref"].as_str().unwrap_or("");
            let branch = r.rsplit('/').next().unwrap_or("").to_string();
            let sha = payload["after"].as_str().unwrap_or("").to_string();
            (branch, sha)
        }
    };

    if repo_full.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "missing repository".into()));
    }
    let want = crate::gitops::norm_repo(&repo_full);

    // Deploy every project pointing at this repo for the pushed/PR branch. We do
    // NOT filter by branch: a push to a non-production branch (or a PR) is exactly
    // how preview deployments are created — the branch decides production vs
    // preview at build time (or `target` forces a preview for PRs).
    let mut triggered = Vec::new();
    for (project, _settings) in c.projects.snapshot() {
        // Fleet-aware git lookup: a project placed on a peer node (the common case —
        // most projects run on Firecracker nodes, not this coordinator) has no LOCAL
        // gateway git source, so `gw.git_for_project` returns None and the webhook
        // would silently trigger NOTHING. Use the gossiped fleet view so pushes/PRs
        // to remotely-placed projects still create deployments.
        let Some(git) = git_for_project_fleet(&c, &project) else { continue };
        if crate::gitops::norm_repo(&git.repo_url) != want {
            continue;
        }
        let deploy_branch = if branch.is_empty() { git.branch.clone() } else { branch.clone() };
        let root_dir = Some(c.projects.root_dir_of(&project)).filter(|s| !s.is_empty());
        let req = fluid_core::GitDeployRequest {
            repo_url: git.repo_url.clone(),
            branch: Some(deploy_branch).filter(|b| !b.is_empty()),
            project: Some(project.clone()),
            creator: Some("github".into()),
            production: true, // legacy field; classification uses `target`/branch
            target: target.clone(),
            use_cache: true, // git push redeploy: reuse the warm dependency cache
            root_dir,
            env: None, // git push redeploy: env comes from the project store
            no_fanout: false, // gitops redeploy is a coordinator deploy → schedule + fanout
            build_config: None,
            function_settings: None,
            redeploy: false, // webhook push → start_build directly (bypasses git_deploy naming)
            zip_b64: None,
            image_ref: None,
            image_port: None,
        };
        let build_id = crate::git::start_build(c.clone(), req);
        let ev = c.event(&c.region, "DEPLOY", &format!("{project}.localhost"), "/", 200, "gitops", &format!("github {} {} @ {}", event, want, &commit.chars().take(7).collect::<String>()));
        c.record(ev);
        triggered.push(json!({
            "project": project,
            "build_id": build_id,
            "target": target.clone().unwrap_or_else(|| "auto".into()),
            "branch": branch,
        }));
    }

    Ok(Json(json!({
        "repo": want,
        "branch": branch,
        "event": event,
        "pr": pr_number,
        "triggered": triggered.len(),
        "builds": triggered,
    })))
}

/// Constant-time-ish verification of GitHub's `sha256=<hex>` HMAC signature.
fn verify_github_sig(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex) = header.strip_prefix("sha256=") else { return false };
    let expected = hmac_sha256(secret, body);
    let expected_hex = hex_lower(&expected);
    // length-independent compare
    if hex.len() != expected_hex.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in hex.bytes().zip(expected_hex.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// HMAC-SHA256, hex-encoded — shared by every webhook verifier in this crate
/// (GitHub's `X-Hub-Signature-256`, Stripe's `Stripe-Signature`) so there's one
/// implementation of the actual crypto primitive.
pub(crate) fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    hex_lower(&hmac_sha256(key, msg))
}

/// Minimal HMAC-SHA256 (avoids pulling a new crate). SHA-256 from `sha2` which is
/// already in the dependency tree.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let h = Sha256::digest(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let out = outer.finalize();
    let mut res = [0u8; 32];
    res.copy_from_slice(&out);
    res
}

// ---- Mesh: a peer announces itself to us ----

pub(crate) async fn node_announce(
    State(c): State<Arc<CloudState>>,
    Json(node): Json<hive_edge::NodeInfo>,
) -> Json<Value> {
    // Converge the control-plane fencing epoch on the max witnessed anywhere.
    c.cluster.adopt_epoch(node.cp_epoch);
    c.registry.upsert_peer(node);
    Json(json!(c.registry.nodes()))
}

/// Observability for the hot-join mesh: the persisted key-addressed roster (this
/// node's dial map, `endpoint_id -> (endpoint_id, iroh_addr)` — no IPs) plus the
/// live trust-set size, so an operator can see who this node knows/trusts without
/// SSH. Read-only; requires an operator token when JWT is enforced.
async fn mesh_roster_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let roster: Vec<Value> = c
        .peer_iroh
        .read()
        .iter()
        .map(|(k, (nid, addr))| json!({ "key": k, "endpoint_id": nid, "iroh_addr": addr }))
        .collect();
    let trusted = c.trusted_peer_ids.read().map(|s| s.len()).unwrap_or(0);
    Ok(Json(json!({ "roster": roster, "trusted_peer_count": trusted })))
}

#[derive(Deserialize)]
struct MeshAdmitReq {
    endpoint_id: String,
}

/// Operator-initiated admission: pre-register an endpoint id into this node's
/// trust set before it ever connects (e.g. onboarding a node whose key you got
/// out-of-band). Redundant with — but not required by — the zero-touch join-proof
/// path (`STREAM_JOIN`, hive-p2p): a node holding `HIVE_JWT_SECRET` self-admits on
/// first contact without this call. This exists for HIVE_PEER_TRUST-enforced
/// fleets that also want an explicit, audited allowlist edit. Does NOT propagate
/// to peers directly; the admitted id becomes visible to them transitively the
/// next time this node gossips a NodeInfo carrying that iroh_addr (main.rs:1264).
async fn mesh_admit(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<MeshAdmitReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let id = req.endpoint_id.trim();
    if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "endpoint_id must be a 64-char hex iroh NodeId".into()));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Ok(mut set) = c.trusted_peer_ids.write() {
        set.insert(id.to_string());
    }
    c.audit.record(&t, "user", "admit", "mesh_peer", id, "manually admitted to the P2P trust set");
    Ok(Json(json!({ "admitted": id, "trusted_peer_count": c.trusted_peer_ids.read().map(|s| s.len()).unwrap_or(0) })))
}

/// Unauthenticated, liveness-adjacent mesh-membership probe (`/v1/mesh`) — no
/// JWT, matching `/healthz`, since the watchdog/monitoring polling it has none.
/// Deliberately separate from `/healthz`: liveness ("is the process serving
/// HTTP") tells you nothing about mesh membership. See `MeshHealth`'s doc.
async fn mesh_health(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.mesh_health()))
}

async fn overview(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cl = claims.as_ref().map(|e| &e.0);
    require_auth_read(cl)?;
    if !operator_allowed(cl, crate::auth::enforced()) {
        // Tenant-safe subset: topology shape only. Platform-wide counters
        // (requests/WAF/deployments/concurrency) stay operator-only; tenant
        // usage lives at the tenant-scoped /v1/metrics instead.
        return Ok(Json(json!({
            "node": c.node_name,
            "region": c.region,
            "regions": c.registry.regions(),
            "nodes": c.registry.nodes().len(),
            "control_plane": {
                "last_gossip_ms": c.last_gossip_ms(),
                "degraded": c.control_plane_degraded(crate::state::CP_DEGRADED_TTL_MS),
                "mesh": c.mesh_health(),
            },
        })));
    }
    let (reqs, blocked) = c.counters();
    let (hits, misses, stale, entries, ratio) = c.cdn.stats();
    let fstats = c.fluid.stats();
    let instances: usize = fstats.iter().map(|f| f.instances).sum();
    let cc = c.limiter.stats();
    Ok(Json(json!({
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
        "control_plane": {
            // #25: gossip freshness. degraded=true means peers are configured but
            // we haven't synced within the TTL — data plane still serves local
            // known-good state; this just makes the staleness observable.
            "last_gossip_ms": c.last_gossip_ms(),
            "degraded": c.control_plane_degraded(crate::state::CP_DEGRADED_TTL_MS),
            // Membership health, distinct from the staleness check above: a node
            // launched with no --peer args has zero configured peers, so
            // `degraded` above is unconditionally false for it even when it's
            // fully isolated from the fleet (see MeshHealth's doc).
            "mesh": c.mesh_health(),
        },
        "peer_trust": {
            // #20: P2P admission control. enforced=true rejects iroh peers whose
            // cryptographic identity isn't in `trusted` (env + gossip roster).
            "enforced": std::env::var("HIVE_PEER_TRUST").map(|v| v == "1" || v == "true").unwrap_or(false),
            "trusted_peers": c.trusted_peer_ids.read().map(|s| s.len()).unwrap_or(0),
        },
    })))
}

pub(crate) async fn nodes(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cl = claims.as_ref().map(|e| &e.0);
    require_auth_read(cl)?;
    let list = c.registry.nodes();
    // Operators (and the mesh-internal gossip path) get the full record —
    // peer sync depends on iroh_addr/peer_id being present.
    if operator_allowed(cl, crate::auth::enforced()) {
        return Ok(Json(json!(list)));
    }
    // Every other signed-in user gets the sanitized topology: enough to render
    // the network page (mesh map, health, capacity totals), none of the
    // mesh-internal addressing (iroh_addr/peer_id/public_ip/public_url).
    let sanitized: Vec<Value> = list
        .into_iter()
        .map(|n| {
            json!({
                "id": n.name,
                "name": n.name,
                "region": n.region,
                "city": n.city,
                "country": n.country,
                "lat": n.lat,
                "lon": n.lon,
                "healthy": n.healthy,
                "last_seen_ms": n.last_seen_ms,
                "is_self": n.is_self,
                "backend": n.backend,
                "cpu_cores": n.cpu_cores,
                "mem_total_mb": n.mem_total_mb,
                "disk_total_gb": n.disk_total_gb,
            })
        })
        .collect();
    Ok(Json(json!(sanitized)))
}

async fn cluster_status(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Leader/term/member NAMES are the coordination view the network page shows
    // every signed-in user; nothing mesh-internal is exposed here.
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    let members: Vec<String> = c.registry.nodes().into_iter().map(|n| n.id).collect();
    Ok(Json(json!(c.cluster.status(members))))
}

/// Anycast routing table: every node with latency/health, and the node this edge
/// would route a request to (lowest-latency healthy, region-preferred).
async fn anycast_table(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let selected = c.registry.anycast(Some(&c.region));
    // REAL placement: which nodes actually HOST deployments — this node's local
    // deployments plus peers' deployments learned via gossip (`peer_routes`). A
    // node is "serving" if it hosts >= 1 deployment, independent of the single
    // anycast pick. This is what the Network page reads to mark serving vs standby.
    use std::collections::{HashMap, HashSet};
    let mut serving: HashMap<String, HashSet<String>> = HashMap::new();
    let self_subs: HashSet<String> = c
        .gw
        .served_hosts()
        .into_iter()
        .filter_map(|h| h.split('.').next().map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if !self_subs.is_empty() {
        serving.insert(c.node_name.clone(), self_subs);
    }
    for (sub, routes) in c.peer_routes.read().iter() {
        for r in routes {
            serving.entry(r.node_id.clone()).or_default().insert(sub.clone());
        }
    }
    let serving_counts: HashMap<String, usize> =
        serving.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    Json(json!({
        "region": c.region,
        "selected": selected.as_ref().map(|n| n.name.clone()),
        "table": c.registry.routing_table(),
        // node name -> count of deployments it actually hosts (0/absent = standby).
        "serving": serving_counts,
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

async fn ratelimit_put(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<RateLimitBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // This mutates the NODE's single shared DDoS rate limiter — every other
    // tenant hosted on this node relies on it. Unlike a per-tenant setting,
    // any authenticated caller (of any role) could previously disable it
    // fleet-wide, matching the operator-only gate already used for WAF/bot
    // policy.
    require_operator(claims.as_ref().map(|e| &e.0))?;
    c.ratelimit.set(b.enabled, b.limit, b.window_ms);
    Ok(Json(json!(c.ratelimit.stats())))
}

async fn regions(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.registry.regions())))
}

pub async fn functions(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.fluid.stats())))
}

/// Tunnel reuse + #14 byte/backpressure metering for this node's gateway.
async fn tunnels(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.gw.tunnel_stats().await)))
}

/// Relay cost accounting (#23): relay-vs-direct connection + byte breakdown for
/// this node's mesh trunks. `relayed_*` bytes transit a relay server (a real cost
/// + a holepunch-failure signal). Empty when P2P (iroh) isn't enabled on the node.
async fn relay_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let pool = c.mesh.read().clone();
    Ok(match pool {
        Some(pool) => {
            let s = pool.relay_stats().await;
            let total = s.relayed_bytes_tx + s.relayed_bytes_rx + s.direct_bytes_tx + s.direct_bytes_rx;
            let relayed = s.relayed_bytes_tx + s.relayed_bytes_rx;
            let relayed_pct = if total > 0 { relayed as f64 / total as f64 } else { 0.0 };
            Json(json!({
                "enabled": true,
                "relayed_conns": s.relayed_conns,
                "direct_conns": s.direct_conns,
                "relayed_bytes_tx": s.relayed_bytes_tx,
                "relayed_bytes_rx": s.relayed_bytes_rx,
                "direct_bytes_tx": s.direct_bytes_tx,
                "direct_bytes_rx": s.direct_bytes_rx,
                "relayed_bytes_pct": relayed_pct,
                // Per-peer, per-phase iroh timeout counters (#H4) — p2p_timeout{phase,node_id}.
                "timeouts": s.timeouts.iter().map(|t| json!({
                    "node_id": t.node_id,
                    "phase": t.phase,
                    "count": t.count,
                })).collect::<Vec<_>>(),
                // Vercel DNS reconciler (ngrok retirement) — leader-elected publish loop.
                "dns_reconciler": {
                    "passes": crate::vercel_dns::STATS.passes.load(std::sync::atomic::Ordering::Relaxed),
                    "creates": crate::vercel_dns::STATS.creates.load(std::sync::atomic::Ordering::Relaxed),
                    "deletes": crate::vercel_dns::STATS.deletes.load(std::sync::atomic::Ordering::Relaxed),
                    "api_errors": crate::vercel_dns::STATS.api_errors.load(std::sync::atomic::Ordering::Relaxed),
                    "empty_set_blocks": crate::vercel_dns::STATS.empty_set_blocks.load(std::sync::atomic::Ordering::Relaxed),
                    "last_pass_ms": crate::vercel_dns::STATS.last_pass_ms.load(std::sync::atomic::Ordering::Relaxed),
                },
                // ACME/TLS state: which zones have an installed certificate.
                "tls_zones": crate::acme::installed_zones(),
                // Web3 gossip signature verification (staged log→enforce rollout).
                "gossip_verify": {
                    "mode": format!("{:?}", hive_p2p::verify_mode()).to_lowercase(),
                    "sign_outbound": hive_p2p::gossip_sign_enabled(),
                    "signed_ok": hive_p2p::verify_stats().0,
                    "unsigned": hive_p2p::verify_stats().1,
                    "bad_sig": hive_p2p::verify_stats().2,
                    "stale_ts": hive_p2p::verify_stats().3,
                    "signer_mismatch": hive_p2p::verify_stats().4,
                    "rejected": hive_p2p::verify_stats().5,
                },
            }))
        }
        None => Json(json!({ "enabled": false })),
    })
}

#[derive(Deserialize)]
pub(crate) struct LimitQ {
    pub(crate) limit: Option<usize>,
    /// Filter events to a project (matches the deployment host subdomain).
    pub(crate) project: Option<String>,
    /// Filter events to ONE deployment: events tagged with this deployment id
    /// at record time, plus untagged events on that deployment's own alias
    /// hosts. Composes with `project` (the deployment-detail view sends both).
    #[serde(default)]
    pub(crate) deployment: Option<String>,
    /// Free-text search across path/host/detail.
    pub(crate) q: Option<String>,
    /// Internal: when set by a fleet-aggregation proxy, return ONLY this node's
    /// local events (no fan-out) — prevents proxy recursion.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
}

pub(crate) async fn logs(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Query(q): Query<LimitQ>) -> Json<Value> {
    let limit = q.limit.unwrap_or(100);
    let cl = claims.as_ref().map(|e| &e.0);
    let is_operator = operator_allowed(cl, crate::auth::enforced());
    let t = tenant(&c, &headers, cl);
    let mut evs = c.recent_events(2000);
    // Tenant scope: only events for projects owned by this team. UNATTRIBUTED
    // events (empty project: platform-apex bot probes, unresolved hosts,
    // control-plane noise) are OPERATOR-ONLY in the unfiltered view — they are
    // nobody's tenant traffic, and showing them to every signed-in team both
    // leaks cross-tenant request metadata (hosts/paths) and buries the team's
    // own logs in scanner noise. A project-scoped view below still re-admits
    // the project's OWN untagged host traffic via the alias-host branch.
    evs.retain(|e| {
        if e.project.is_empty() {
            return is_operator;
        }
        // Fleet-aware: an event's project is owned per the deployment tenant
        // tags, not the node-local (row-missing on non-hosting nodes) project row.
        project_owned_by(&c, &e.project, &t)
    });
    if let Some(p) = q.project.as_ref().filter(|p| !p.is_empty()) {
        let pl = p.to_lowercase();
        // EXACT project scoping. The old filter also kept any event whose `host`
        // or `detail` merely CONTAINED the project name as a substring — so
        // `/projects/ctest/logs` leaked in every other project's events that
        // happened to mention "ctest" anywhere (and a short name like "css"
        // matched "cssdemosite", "processor", …). Scope instead to: events
        // tagged with this exact project, OR untagged/infra events whose `host`
        // is one of THIS project's real deployment hostnames (exact label or a
        // subdomain of it) — never a blind substring on free-text detail.
        // The deployment host set is TENANT-CHECKED (team_of == t): a
        // same-named project belonging to another tenant must not contribute
        // hosts that would pull its traffic into this view.
        let hosts: std::collections::HashSet<String> = c
            .gw
            .list()
            .into_iter()
            .filter(|d| d.project.eq_ignore_ascii_case(&pl) && record_tenant(&d.tenant) == t)
            .flat_map(|d| {
                [d.alias, d.commit_alias, d.branch_alias, d.id_alias]
                    .into_iter()
                    .filter(|h| !h.is_empty())
                    .map(|h| h.to_lowercase())
            })
            .collect();
        evs.retain(|e| {
            if e.project.to_lowercase() == pl {
                return true;
            }
            if !e.project.is_empty() {
                return false; // tagged for a DIFFERENT project — never leak it.
            }
            // Untagged/infra event: keep only if its host is one of this
            // project's deployment hosts (exact or a subdomain label boundary).
            let h = e.host.to_lowercase();
            hosts
                .iter()
                .any(|ph| h == *ph || h.starts_with(&format!("{ph}.")) || ph.starts_with(&format!("{h}.")))
        });
    }
    if let Some(d) = q.deployment.as_ref().filter(|d| !d.is_empty()) {
        let dl = d.to_lowercase();
        // Deployment scope: events exactly tagged with this deployment id at
        // record time (see Event.deployment), plus — for events recorded before
        // the tag existed — untagged events whose host FIRST LABEL is the
        // deployment id itself (the id_alias URL is unambiguous over time;
        // project/branch aliases move between deployments on promote, so those
        // are only trusted via the record-time tag).
        evs.retain(|e| {
            if e.deployment.to_lowercase() == dl {
                return true;
            }
            if !e.deployment.is_empty() {
                return false;
            }
            let h = e.host.to_lowercase();
            let label = h.split(':').next().unwrap_or(&h).split('.').next().unwrap_or(&h);
            label == dl
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
    // Fleet aggregation: requests are recorded on the node that SERVES them, so a
    // project placed on a peer logs there, not here. Merge in the relevant peers'
    // events (scoped to the same project/search, `local=true` to avoid recursion),
    // then sort newest-first and truncate.
    let mut out: Vec<Value> = evs.into_iter().map(|e| json!(e)).collect();
    if !q.local.unwrap_or(false) {
        let qs = {
            let mut s = format!("limit={}&local=true", limit);
            if let Some(p) = q.project.as_ref().filter(|p| !p.is_empty()) {
                s.push_str(&format!("&project={}", urlencode(p)));
            }
            if let Some(d) = q.deployment.as_ref().filter(|d| !d.is_empty()) {
                s.push_str(&format!("&deployment={}", urlencode(d)));
            }
            if let Some(qq) = q.q.as_ref().filter(|s| !s.is_empty()) {
                s.push_str(&format!("&q={}", urlencode(qq)));
            }
            s
        };
        // A project filter targets just its host node(s); otherwise pull from every
        // peer hosting one of this tenant's projects. Address by NODE (not HTTP admin
        // URL) and go through `fetch_from_host`, which falls back to the iroh mesh —
        // FC nodes have no HTTP admin URL, so an admin-URL-only path returns nothing.
        // (The old `git_for_project` gate skipped the proxy entirely for git-sourced
        // projects, but the placement scheduler still hosts those on peers → the
        // coordinator logged nothing for them. This is the "empty project logs" bug.)
        let nodes: Vec<String> = match q.project.as_deref() {
            Some(p) if !p.is_empty() => host_nodes_for_project(&c, p),
            _ => peer_nodes_for_tenant(&c, &t),
        };
        for node in nodes {
            if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/logs?{qs}"), &t).await {
                if let Some(arr) = v.as_array() {
                    out.extend(arr.iter().cloned());
                }
            }
        }
        out.sort_by(|a, b| {
            b.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0).cmp(&a.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0))
        });
    }
    out.truncate(limit);
    Json(json!(out))
}

// ---- WAF ----

async fn waf_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!({ "managed": c.waf.managed_enabled(), "rules": c.waf.rules() })))
}

async fn waf_add_rule(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(rule): Json<WafRule>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // WAF rules are GLOBAL edge infrastructure — operator-only when auth is on.
    require_operator(claims.as_ref().map(|e| &e.0))?;
    c.waf.add_rule(rule);
    crate::persist::persist(&c);
    Ok(Json(json!({ "rules": c.waf.rules() })))
}

async fn waf_del_rule(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let kept: Vec<WafRule> = c.waf.rules().into_iter().filter(|r| r.id != id).collect();
    c.waf.set_rules(kept);
    crate::persist::persist(&c);
    Ok(Json(json!({ "rules": c.waf.rules() })))
}

#[derive(Deserialize)]
struct ManagedBody {
    enabled: bool,
}

async fn waf_managed(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<ManagedBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    c.waf.set_managed(b.enabled);
    Ok(Json(json!({ "managed": c.waf.managed_enabled() })))
}

// ---- Bot management ----

async fn bot_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(*c.bot_policy.read())))
}

async fn bot_put(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(p): Json<BotPolicy>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    *c.bot_policy.write() = p;
    Ok(Json(json!(*c.bot_policy.read())))
}

// ---- CDN ----

async fn cdn_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let (hits, misses, stale, entries, ratio) = c.cdn.stats();
    Ok(Json(json!({ "hits": hits, "misses": misses, "stale": stale, "entries": entries, "hit_ratio": ratio })))
}

// ---- Runtime Cache (Vercel-style regional data cache) ----

#[derive(Deserialize)]
struct RcKey {
    scope: String,
    key: String,
    #[serde(default)]
    ttl: u64,
    /// Comma-separated tags.
    #[serde(default)]
    tags: String,
}

#[derive(Deserialize)]
struct RcTag {
    scope: String,
    tag: String,
}

async fn rc_stats(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let (entries, reads, writes, hits, revalidations) = c.runtime_cache.stats();
    Json(json!({
        "entries": entries, "reads": reads, "writes": writes,
        "hits": hits, "revalidations": revalidations,
    }))
}

/// Runtime-cache scope authorization. The scope is `{project}:{env}`; project
/// names are globally unique, so ownership is checked via the project's team.
/// Requests carrying NO tenant context (no JWT / API key / x-hive-team) are the
/// in-cell loopback data plane (HIVE_RUNTIME_CACHE_URL) — the admin port is
/// loopback-bound + firewalled, so those pass through unchanged.
fn rc_authorize(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    scope: &str,
) -> Result<(), (StatusCode, String)> {
    let has_ctx = claims.is_some()
        || headers.contains_key("x-hive-team")
        || headers.contains_key(axum::http::header::AUTHORIZATION);
    if !has_ctx {
        return Ok(());
    }
    let project = scope.split(':').next().unwrap_or("");
    let t = tenant(c, headers, claims);
    if project_owned_by(c, project, &t) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "runtime-cache scope belongs to a different team".into()))
    }
}

async fn rc_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<RcKey>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(e) = rc_authorize(&c, &headers, claims.as_ref().map(|x| &x.0), &q.scope) {
        return e.into_response();
    }
    match c.runtime_cache.get(&q.scope, &q.key) {
        Some(v) => ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn rc_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<RcKey>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(e) = rc_authorize(&c, &headers, claims.as_ref().map(|x| &x.0), &q.scope) {
        return e.into_response();
    }
    let tags: Vec<String> = q.tags.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect();
    match c.runtime_cache.set(&q.scope, &q.key, body.to_vec(), q.ttl, tags) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn rc_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<RcKey>,
) -> StatusCode {
    if rc_authorize(&c, &headers, claims.as_ref().map(|x| &x.0), &q.scope).is_err() {
        return StatusCode::FORBIDDEN;
    }
    c.runtime_cache.delete(&q.scope, &q.key);
    StatusCode::NO_CONTENT
}

async fn rc_revalidate(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<RcTag>,
) -> Result<Json<Value>, (StatusCode, String)> {
    rc_authorize(&c, &headers, claims.as_ref().map(|x| &x.0), &q.scope)?;
    let removed = c.runtime_cache.revalidate_tag(&q.scope, &q.tag);
    Ok(Json(json!({ "removed": removed })))
}

async fn cdn_purge(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    c.cdn.purge();
    Ok(Json(json!({ "purged": true })))
}

// ---- Concurrency scaling ----

async fn concurrency_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.limiter.stats()))
}

// ---- Routing layer ----

async fn routing_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!({ "redirects": c.router.redirects(), "rewrites": c.router.rewrites() })))
}

async fn add_redirect(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(r): Json<Redirect>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    c.router.add_redirect(r);
    crate::persist::persist(&c);
    Ok(Json(json!({ "redirects": c.router.redirects() })))
}

async fn add_rewrite(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(r): Json<Rewrite>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    c.router.add_rewrite(r);
    crate::persist::persist(&c);
    Ok(Json(json!({ "rewrites": c.router.rewrites() })))
}

#[derive(Deserialize)]
struct BySource {
    source: String,
}

async fn del_redirect(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<BySource>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let kept: Vec<Redirect> = c.router.redirects().into_iter().filter(|r| r.source != b.source).collect();
    c.router.set_redirects(kept);
    crate::persist::persist(&c);
    Ok(Json(json!({ "redirects": c.router.redirects() })))
}

async fn del_rewrite(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<BySource>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let kept: Vec<Rewrite> = c.router.rewrites().into_iter().filter(|r| r.source != b.source).collect();
    c.router.set_rewrites(kept);
    crate::persist::persist(&c);
    Ok(Json(json!({ "rewrites": c.router.rewrites() })))
}

// ---- Cron ----

async fn cron_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Tenant-scoped: only this team's jobs. Legacy jobs (no tenant recorded) are
    // attributed by their target project's owning team.
    let jobs: Vec<CronJob> = c
        .cron
        .list()
        .into_iter()
        .filter(|j| {
            let owner = if j.tenant.is_empty() { norm(&c.projects.team_of(&j.deployment)).to_string() } else { j.tenant.clone() };
            owner == t
        })
        .collect();
    Json(json!(jobs))
}

async fn cron_add(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut job): Json<CronJob>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Stamp the caller's tenant (never trust a tenant in the body) and require
    // the target project to belong to them.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &job.deployment)?;
    job.tenant = t;
    match c.cron.add(job) {
        Ok(j) => Ok(Json(json!(j))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

async fn cron_del(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let owned = c.cron.list().into_iter().any(|j| {
        j.id == id && {
            let owner = if j.tenant.is_empty() { norm(&c.projects.team_of(&j.deployment)).to_string() } else { j.tenant.clone() };
            owner == t
        }
    });
    if !owned {
        return Err((StatusCode::FORBIDDEN, "cron job belongs to a different team".into()));
    }
    c.cron.remove(&id);
    Ok(Json(json!({ "removed": id })))
}

/// A cron job's owning team — legacy jobs (no tenant recorded) are attributed
/// by their target project's owning team, matching `cron_list`/`cron_del`.
fn cron_job_owner(c: &Arc<CloudState>, job: &CronJob) -> String {
    if job.tenant.is_empty() { norm(&c.projects.team_of(&job.deployment)).to_string() } else { job.tenant.clone() }
}

/// Manually trigger a cron job right now (the dashboard's "Run" button) — the
/// SAME invocation path the scheduler's own tick loop uses
/// (`spawn_cron_loop`/`invoke`), so a manual run behaves identically to a
/// real firing, but stamped as a manual run in the recorded event and without
/// touching the job's own `next_run_ms` (its schedule is unaffected).
async fn cron_run(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let job = c.cron.get(&id).ok_or((StatusCode::NOT_FOUND, "no such cron job".into()))?;
    if cron_job_owner(&c, &job) != t {
        return Err((StatusCode::FORBIDDEN, "cron job belongs to a different team".into()));
    }
    let (status, detail) = match crate::invoke(&c, &job.deployment, &job.path).await {
        Ok((s, _)) => (s, format!("cron {} -> {s} (manual run)", job.name)),
        Err(e) => (0, format!("cron {} error: {e} (manual run)", job.name)),
    };
    let ev = c.event(&c.region, "CRON", &job.deployment, &job.path, status, "cron", &detail);
    c.record(ev);
    let updated = c.cron.record_manual_run(&id, now_ms());
    Ok(Json(json!({ "status": status, "job": updated })))
}

async fn project_cron_enabled_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<CronToggle>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_cron_enabled(&project, b.enabled);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

#[derive(Deserialize)]
struct CronToggle {
    enabled: bool,
}

// ---- Workflows ----

/// Lenient `Option<bool>` query-param deserializer: axum's default (via
/// `serde_urlencoded`) only accepts the literal strings `"true"`/`"false"`
/// and 400s on anything else, but callers (the dashboard, `?summary=1`) send
/// `"1"`/`"0"` — the same convention `gossip.rs`'s internal `wf_query`/
/// `logs_query` already special-case (`v == "true" || v == "1"`) for the
/// mesh-RPC path. This brings the public HTTP path up to that convention.
fn de_lenient_bool<'de, D>(d: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) => match s.as_str() {
            "true" | "1" | "yes" => Ok(Some(true)),
            "false" | "0" | "no" => Ok(Some(false)),
            _ => Ok(None),
        },
    }
}

/// Query for endpoints that fan out to peers: `?local=true` answers with this
/// node's local data only, so a proxied call never re-fans (loop guard).
#[derive(Deserialize)]
pub(crate) struct LocalQ {
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct WfQuery {
    /// Restrict to a single project.
    pub(crate) project: Option<String>,
    /// Internal: when set by a fleet-aggregation proxy call, return ONLY this
    /// node's local workflows (no further fan-out) — prevents proxy recursion.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
    /// List-shape response: strip per-step `output` payloads (the runs TABLE
    /// renders none of them; full detail lives on `/v1/workflows/runs/:id`).
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) summary: Option<bool>,
}

/// Does this workflow's project belong to the requesting team? Fleet-aware —
/// the node-local project row is UNTAGGED on a node that never ran the deploy.
fn wf_in_team(c: &Arc<CloudState>, project: &str, team: &str) -> bool {
    project_owned_by(c, project, norm(team))
}

pub(crate) async fn wf_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Query(q): Query<WfQuery>) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Workflows are ingested on the node that HOSTS a deployment. If this project
    // was placed on a peer, proxy to that node so its workflows show up.
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() {
            if let Some(node) = host_node_for_project(&c, project) {
                if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/workflows?project={project}&local=true"), &team).await {
                    return Json(v);
                }
            }
        }
    }
    let mut defs: Vec<Value> = c
        .workflows
        .defs()
        .into_iter()
        .filter(|d| wf_in_team(&c, &d.project, &team))
        .filter(|d| q.project.as_deref().map(|p| p == d.project).unwrap_or(true))
        .map(|d| json!(d))
        .collect();
    // Global view (no project filter): merge in workflows registered on peer nodes
    // that host this tenant's projects — they're ingested on the HOST, so the
    // coordinator otherwise shows none for scheduler-placed projects.
    if q.project.is_none() && !q.local.unwrap_or(false) {
        let mut seen: std::collections::HashSet<String> = defs
            .iter()
            .map(|d| format!("{}\u{0}{}", d.get("project").and_then(|x| x.as_str()).unwrap_or(""), d.get("id").and_then(|x| x.as_str()).unwrap_or("")))
            .collect();
        for node in peer_nodes_for_tenant(&c, &team) {
            if let Some(v) = fetch_from_host(&c, &node, "/v1/workflows?local=true", &team).await {
                if let Some(arr) = v.as_array() {
                    for d in arr {
                        let key = format!("{}\u{0}{}", d.get("project").and_then(|x| x.as_str()).unwrap_or(""), d.get("id").and_then(|x| x.as_str()).unwrap_or(""));
                        if seen.insert(key) {
                            defs.push(d.clone());
                        }
                    }
                }
            }
        }
    }
    Json(json!(defs))
}

async fn wf_define(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(def): Json<WorkflowDef>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // The workflow's project must belong to the caller's team.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &def.project)?;
    c.workflows.define(def);
    let defs: Vec<WorkflowDef> = c.workflows.defs().into_iter().filter(|d| wf_in_team(&c, &d.project, &t)).collect();
    Ok(Json(json!(defs)))
}

pub(crate) async fn wf_runs(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Query(q): Query<WfQuery>) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() {
            if let Some(node) = host_node_for_project(&c, project) {
                if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/workflows/runs?project={project}&local=true"), &team).await {
                    return Json(v);
                }
            }
        }
    }
    let mut runs: Vec<Value> = c
        .workflows
        .runs()
        .into_iter()
        .filter(|r| wf_in_team(&c, &r.project, &team))
        .filter(|r| q.project.as_deref().map(|p| p == r.project).unwrap_or(true))
        .map(|r| json!(r))
        .collect();
    // Append the REAL runs the deployed app executed, read from its WDK "world"
    // store. Only for projects HOSTED on THIS node (env_map decrypts locally); the
    // coordinator gets these via the per-project / `local=true` proxy paths above.
    {
        let mut locals: Vec<String> = match q.project.as_deref() {
            Some(p) if c.gw.git_for_project(p).is_some() => vec![p.to_string()],
            Some(_) => vec![],
            None => {
                let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
                for d in c.gw.list() {
                    // The deployment record's own tenant tag is authoritative and
                    // present (local record); the project row is node-local and
                    // UNTAGGED on nodes that never ran this deploy.
                    if record_tenant(&d.tenant) == team {
                        s.insert(d.project);
                    }
                }
                s.into_iter().collect()
            }
        };
        locals.retain(|p| crate::world::has_world(&c, p));
        for proj in locals {
            if let Some(wruns) = crate::world::list_runs(&c, &proj, 100).await {
                runs.extend(wruns);
            }
        }
    }
    let run_key = |r: &Value| -> Option<String> {
        r.get("runId").or_else(|| r.get("id")).and_then(|x| x.as_str()).map(String::from)
    };
    if q.project.is_none() && !q.local.unwrap_or(false) {
        let mut seen: std::collections::HashSet<String> = runs.iter().filter_map(run_key).collect();
        for node in peer_nodes_for_tenant(&c, &team) {
            if let Some(v) = fetch_from_host(&c, &node, "/v1/workflows/runs?local=true", &team).await {
                if let Some(arr) = v.as_array() {
                    for r in arr {
                        if let Some(id) = run_key(r) {
                            if seen.insert(id) {
                                runs.push(r.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    // LIST-shape trim (`?summary=1`, sent by the dashboard's runs table): strip
    // each step's `output` payload — the table renders name/status/duration
    // only, but every poll was shipping full per-step output text for every
    // run (the single biggest bytes item on the workflows page). The full
    // detail stays on `/v1/workflows/runs/:id`.
    if q.summary.unwrap_or(false) {
        for r in runs.iter_mut() {
            if let Some(steps) = r.get_mut("steps").and_then(|s| s.as_array_mut()) {
                for s in steps.iter_mut() {
                    if let Some(o) = s.as_object_mut() {
                        o.remove("output");
                    }
                }
            }
        }
    }
    Json(json!(runs))
}

/// One run with full step detail (for the trace timeline / Gantt). Resolves from
/// our engine first, else from the project's WDK "world" store — proxied to the
/// hosting node when the project lives on a peer (the world env is decrypted
/// there). `?project=` is required to locate a world run.
pub(crate) async fn wf_run_detail(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    Query(q): Query<WfQuery>,
) -> Result<Json<Value>, StatusCode> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(r) = c.workflows.run(&id) {
        // This fast path (a run hosted on THIS node) used to skip the team
        // check every other lookup branch below performs — any authenticated
        // tenant who learned/guessed a run id could read a different
        // tenant's full workflow step output.
        if wf_in_team(&c, &r.project, &team) {
            return Ok(Json(json!(r)));
        }
        return Err(StatusCode::NOT_FOUND);
    }
    let found = |v: &Value| v.get("run").map(|r| !r.is_null()).unwrap_or(false);
    // 1) Explicit project (proxy to its host node over iroh if remote, else local).
    if let Some(project) = q.project.as_deref() {
        if !q.local.unwrap_or(false) {
            for node in host_nodes_for_project(&c, project) {
                if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/workflows/runs/{id}?project={project}&local=true"), &team).await {
                    if found(&v) {
                        return Ok(Json(v));
                    }
                }
            }
        }
        if let Some(detail) = crate::world::run_detail(&c, project, &id).await {
            if found(&detail) {
                return Ok(Json(detail));
            }
        }
    }
    // 2) Auto-resolve: no (or wrong) project given — scan this node's world
    // projects for the run (lets a bare /workflows/runs/<id> URL resolve).
    let locals: Vec<String> = {
        let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
        for d in c.gw.list() {
            // Authoritative record tag, not the node-local project row.
            if record_tenant(&d.tenant) == team {
                s.insert(d.project);
            }
        }
        s.into_iter().collect()
    };
    for p in locals {
        if crate::world::has_world(&c, &p) {
            if let Some(detail) = crate::world::run_detail(&c, &p, &id).await {
                if found(&detail) {
                    return Ok(Json(detail));
                }
            }
        }
    }
    // 3) Fleet: ask peers hosting this tenant's projects to resolve it (over iroh).
    if !q.local.unwrap_or(false) {
        for node in peer_nodes_for_tenant(&c, &team) {
            if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/workflows/runs/{id}?local=true"), &team).await {
                if found(&v) {
                    return Ok(Json(v));
                }
            }
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// Per-project rollup for the global "All Projects" workflows view.
pub(crate) async fn wf_summary(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Query(q): Query<WfQuery>) -> Json<Value> {
    use std::collections::BTreeMap;
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // project -> (created, completed, failed, active)
    let mut agg: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for r in c.workflows.runs() {
        if !wf_in_team(&c, &r.project, &team) {
            continue;
        }
        let proj = if r.project.is_empty() { "default".to_string() } else { r.project.clone() };
        let e = agg.entry(proj).or_insert((0, 0, 0, 0));
        e.0 += 1; // created
        match r.status {
            hive_edge::workflows::RunStatus::Succeeded => e.1 += 1,
            hive_edge::workflows::RunStatus::Failed => e.2 += 1,
            hive_edge::workflows::RunStatus::Running | hive_edge::workflows::RunStatus::Pending => e.3 += 1,
            _ => {}
        }
    }
    // Merge peer rollups (the tenant's projects placed on other nodes). A project
    // lives on one host, so per-project rows don't overlap; sum defensively.
    if !q.local.unwrap_or(false) {
      for node in peer_nodes_for_tenant(&c, &team) {
        if let Some(v) = fetch_from_host(&c, &node, "/v1/workflows/summary?local=true", &team).await {
            if let Some(arr) = v.as_array() {
                for r in arr {
                    let proj = r.get("project").and_then(|x| x.as_str()).unwrap_or("default").to_string();
                    let e = agg.entry(proj).or_insert((0, 0, 0, 0));
                    let g = |k: &str| r.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    e.0 += g("created");
                    e.1 += g("completed");
                    e.2 += g("failed");
                    e.3 += g("active");
                }
            }
        }
      }
    }
    let rows: Vec<Value> = agg
        .into_iter()
        .map(|(project, (created, completed, failed, active))| {
            json!({ "project": project, "created": created, "completed": completed, "failed": failed, "active": active })
        })
        .collect();
    Json(json!(rows))
}

async fn wf_run(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Only the owning team may trigger a workflow run.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(def) = c.workflows.defs().into_iter().find(|d| d.id == id) {
        if !wf_in_team(&c, &def.project, &t) {
            return Err((StatusCode::FORBIDDEN, "workflow belongs to a different team".into()));
        }
    }
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
    /// Optional EXACT tenant slug (e.g. a Clerk-org-derived id/slug already in use
    /// by existing tenant-scoped data — projects/deployments/billing accounts
    /// that predate this team row and are tagged with a specific string). When
    /// absent/empty, falls back to the original slugify(name)+uniqueness-suffix
    /// behavior (unchanged for every existing caller). When present, the team is
    /// created with exactly this slug so it lines up with data that already
    /// exists under that tenant id — never silently re-derived.
    #[serde(default)]
    slug: String,
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

/// Scoped to the caller's OWN team, matching a personal-namespace-style "list
/// my teams" — never the global roster. A platform operator sees every team.
async fn teams_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let cl = claims.as_ref().map(|e| &e.0);
    if cl.map(|c| c.platform_admin).unwrap_or(false) {
        return Json(json!(c.teams.list()));
    }
    let t = tenant(&c, &headers, cl);
    Json(json!(c.teams.get(&t).into_iter().collect::<Vec<_>>()))
}

async fn team_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(&c, &headers, claims.as_ref().map(|e| &e.0), &slug, &[])?;
    c.teams.get(&slug).map(|t| Json(json!(t))).ok_or((StatusCode::NOT_FOUND, "no such team".into()))
}

async fn team_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<CreateTeam>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // An exact-slug creation targets a specific pre-existing tenant id (e.g. one
    // already used by real projects/billing) — that's an operator-level action,
    // not the normal self-serve "create my own org" flow every signed-in user can
    // do, so gate it the same way every other cross-tenant admin write is gated.
    if !b.slug.trim().is_empty() {
        require_operator(claims.as_ref().map(|e| &e.0))?;
    }
    let slug = b.slug.trim();
    let t = if slug.is_empty() {
        c.teams.create(&b.name, &b.plan, &c.owner_email)
    } else {
        c.teams
            .create_with_slug(slug, &b.name, &b.plan, &c.owner_email)
            .ok_or((StatusCode::CONFLICT, format!("team slug '{slug}' already exists")))?
    };
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

async fn team_add_member(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(slug): Path<String>,
    Json(b): Json<AddMember>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(&c, &headers, claims.as_ref().map(|e| &e.0), &slug, &["owner", "admin"])?;
    // Only an Owner-rank caller may grant Owner to someone else — an Admin
    // adding/promoting a member to Owner would be a lateral privilege
    // escalation within the team.
    if matches!(b.role, crate::teams::Role::Owner) {
        let is_owner_caller = claims.as_ref().map(|e| e.0.platform_admin || e.0.role == "owner").unwrap_or(false);
        if !is_owner_caller {
            return Err((StatusCode::FORBIDDEN, "only an Owner may grant the Owner role".into()));
        }
    }
    // Seat quota (business locking): block adding a NEW member past the plan's seat
    // limit. Updating an existing member's role is always allowed.
    let team = c.teams.get(&slug).ok_or((StatusCode::NOT_FOUND, "team not found".into()))?;
    let is_new = team.member(&b.email).is_none();
    if is_new {
        let plan = c.billing.account(&slug).plan;
        let max = crate::billing::plan_max_members(&plan);
        if max > 0 && team.members.len() as u32 >= max {
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                format!(
                    "Seat limit reached ({}/{max}) on the {plan} plan — upgrade to add more members.",
                    team.members.len()
                ),
            ));
        }
    }
    let t = c
        .teams
        .add_member(&slug, &b.email, b.role)
        .ok_or((StatusCode::NOT_FOUND, "team not found".into()))?;
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

async fn team_remove_member(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((slug, email)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(&c, &headers, claims.as_ref().map(|e| &e.0), &slug, &["owner", "admin"])?;
    let t = c.teams.remove_member(&slug, &email).ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

#[derive(Deserialize)]
struct SetPlan {
    plan: String,
}

/// Change a team's tier (hobby | pro | enterprise). Keeps the billing account in
/// sync so the compute allowance + plan label update together.
async fn team_set_plan(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(slug): Path<String>,
    Json(b): Json<SetPlan>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(&c, &headers, claims.as_ref().map(|e| &e.0), &slug, &["owner", "admin"])?;
    let plan = b.plan.to_lowercase();
    if !matches!(plan.as_str(), "hobby" | "pro" | "enterprise") {
        return Err((StatusCode::BAD_REQUEST, "unknown plan".into()));
    }
    c.teams.set_plan(&slug, &plan).ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    c.billing.set_plan(&slug, &plan);
    // Downgrades drop Enterprise-only SSO.
    if !crate::billing::plan_allows_sso(&plan) {
        c.teams.set_sso(&slug, false);
    }
    crate::persist::persist(&c);
    Ok(Json(json!(c.teams.get(&slug))))
}

#[derive(Deserialize)]
struct SetSso {
    enabled: bool,
}

/// Toggle team/org SSO — Enterprise only.
async fn team_set_sso(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(slug): Path<String>,
    Json(b): Json<SetSso>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(&c, &headers, claims.as_ref().map(|e| &e.0), &slug, &["owner", "admin"])?;
    let team = c.teams.get(&slug).ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    if !crate::billing::plan_allows_sso(&team.plan) {
        return Err((StatusCode::FORBIDDEN, "SSO requires the Enterprise plan".into()));
    }
    let t = c.teams.set_sso(&slug, b.enabled).ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
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
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<ProjectTeam>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Only the CURRENT owning team may move a project (or change its protection).
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &b.team);
    c.projects.set_preview_protection(&project, b.preview_protection);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

// ============================ API keys ============================

#[derive(Deserialize)]
struct CreateApiKey {
    name: String,
    #[serde(default)]
    role: String,
}

async fn apikeys_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!(c.apikeys.list(&t).iter().map(|k| k.public()).collect::<Vec<_>>()))
}

async fn apikey_create(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Json(b): Json<CreateApiKey>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let (key, token) = c.apikeys.create(&b.name, &t, &b.role);
    crate::persist::persist(&c);
    // The plaintext token is returned exactly once.
    let mut v = key.public();
    v["token"] = json!(token);
    Json(v)
}

async fn apikey_revoke(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ok = c.apikeys.revoke(&id, &t);
    crate::persist::persist(&c);
    Json(json!({ "revoked": ok, "id": id }))
}

// ---- Connected integrations ----
//
// The team's linked third-party integrations as consumable resources. Every
// endpoint is tenant-scoped, so a `hive_…` platform key (Authorization: Bearer)
// transparently scopes the Vercel-compatible SDK to that key's team. The list/get
// views are redacted; `/credentials` returns the secret token/connection.

async fn integrations_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!(c.integrations.list(&t).iter().map(|i| i.public()).collect::<Vec<_>>()))
}

async fn integration_upsert(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Json(b): Json<crate::integrations::UpsertReq>) -> Result<Json<Value>, (StatusCode, String)> {
    if b.provider.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "provider is required".into()));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let rec = c.integrations.upsert(&t, b);
    crate::persist::persist(&c);
    let ev = c.event(&c.region, "INTEGRATION", &rec.provider, "/", 200, "link",
        &format!("integration {} linked for {} ({} env var(s))", rec.provider, t, rec.env.len()));
    c.record(ev);
    Ok(Json(rec.public()))
}

async fn integration_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.integrations.get(&t, &id).map(|i| Json(i.public())).ok_or(StatusCode::NOT_FOUND)
}

async fn integration_credentials(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.integrations.get(&t, &id).map(|i| Json(i.full())).ok_or(StatusCode::NOT_FOUND)
}

async fn integration_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ok = c.integrations.delete(&t, &id);
    crate::persist::persist(&c);
    Json(json!({ "deleted": ok, "id": id }))
}

// ---- Intelligent service graph (Issue #2) ----
//
// The algorithmically-derived graph of services actually consumed by a deployment
// (app/framework, bundled front+back nodes, workspace packages, databases). Scanned
// async on build AND lazily on-demand (below) so EXISTING + FAILED deployments —
// which predate the async scan — are backfilled the first time they're viewed.

/// Scan a hosted deployment's on-disk source tree and store its service graph.
/// Works for existing/failed deployments (the checkout is kept for live ones).
async fn scan_root_graph(c: &Arc<CloudState>, project: &str, dep_id: &str, root_str: &str) -> Option<crate::svcgraph::ServiceGraph> {
    let root = std::path::PathBuf::from(root_str);
    if root_str.is_empty() || !root.exists() {
        return None;
    }
    let root2 = root.clone();
    let scan = tokio::task::spawn_blocking(move || {
        let fw = fluid_build::detect(&root2);
        let is_container = root2.join("Dockerfile").exists()
            || root2.join("Containerfile").exists()
            || crate::compose::compose_file(&root2).is_some();
        crate::svcgraph::scan_dir(&root2, fw.slug, fw.name, is_container)
    })
    .await
    .ok()?;
    let env_keys: Vec<String> = c.projects.get_masked(project).env.iter().map(|e| e.key.clone()).collect();
    let g = crate::svcgraph::build_graph(project, dep_id, &scan, &env_keys);
    c.svcgraph.put(g.clone());
    crate::persist::persist(c);
    Some(g)
}

pub(crate) async fn scan_record_graph(c: &Arc<CloudState>, rec: &fluid_core::DeployRecord) -> Option<crate::svcgraph::ServiceGraph> {
    scan_root_graph(c, &rec.project, &rec.id, &rec.root).await
}

/// The LOCAL service graph for a project: the stored one, else scanned on-demand
/// from the newest locally-hosted deployment record. No fan-out (caller does that).
pub(crate) async fn local_project_graph(c: &Arc<CloudState>, project: &str) -> Option<crate::svcgraph::ServiceGraph> {
    if let Some(g) = c.svcgraph.latest_for_project(project) {
        return Some(g);
    }
    // Try records newest-first, scanning the first whose checkout still exists on
    // disk (a failed/newer redeploy may have a GC'd root while an older LIVE one
    // is still present + serving).
    let mut recs: Vec<_> = c.gw.deployment_records().into_iter().filter(|r| r.project == project).collect();
    recs.sort_by_key(|r| r.created_at_ms);
    while let Some(rec) = recs.pop() {
        if let Some(g) = scan_record_graph(c, &rec).await {
            return Some(g);
        }
    }
    // No usable deployment RECORD here (e.g. a CONTAINER deploy: the record lives on
    // the coordinator, but the container + source run on THIS lease-owner node). Fall
    // back to the newest on-disk source checkout for the project.
    if let Some(dir) = crate::git::newest_deploy_dir(project) {
        return scan_root_graph(c, project, project, &dir.to_string_lossy()).await;
    }
    None
}

/// The LOCAL service graph for a deployment id: stored, else scanned on-demand from
/// its locally-hosted record. No proxy (caller does that).
pub(crate) async fn local_deployment_graph(c: &Arc<CloudState>, id: &str) -> Option<crate::svcgraph::ServiceGraph> {
    if let Some(g) = c.svcgraph.get(id) {
        return Some(g);
    }
    let rec = c.gw.deployment_records().into_iter().find(|r| r.id == id)?;
    scan_record_graph(c, &rec).await
}

pub(crate) async fn deployment_service_graph(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Locally hosted? Return the stored graph, or scan its source on-demand (backfill
    // for existing/failed deployments), team-checked.
    if c.gw.deployment_records().iter().any(|r| r.id == id) {
        if let Some(g) = local_deployment_graph(&c, &id).await {
            if project_owned_by(&c, &g.project, norm(&t)) {
                return Ok(Json(json!(g)));
            }
        }
    }
    // Not here — proxy to the node that hosts the deployment.
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/service-graph"), &t).await {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn project_service_graph(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>, Path(project): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Fleet-aware ownership: `team_of` is UNTAGGED (never empty) on a node that
    // doesn't host this project, so the old `!known.is_empty()` guard never
    // relaxed and 404'd a member's own service graph everywhere but the host.
    // project_owned_by trusts the deployment tenant tags and stays permissive
    // for a fleet-unknown project (so the on-demand scan / host proxy below run).
    if !project_owned_by(&c, &project, &t) {
        return Err(StatusCode::NOT_FOUND);
    }
    // Stored, or scanned on-demand from a locally-hosted deployment (backfills
    // existing/failed projects deployed here before the async scan existed).
    if let Some(g) = local_project_graph(&c, &project).await {
        return Ok(Json(json!(g)));
    }
    // Not hosted here — the graph (or the source to scan) lives on whichever node
    // built the project. Fan out to every OTHER healthy peer and return the first
    // that has it (each answers from its LOCAL store/scan + team-checks → no re-proxy
    // loop, no cross-tenant leak). Registry is the reliable peer set (trunk-warmed).
    let self_name = c.node_name.clone();
    for n in c.registry.nodes() {
        if n.name == self_name || n.iroh_addr.is_none() {
            continue;
        }
        if let Some(v) = fetch_from_host(&c, &n.name, &format!("/v1/projects/{project}/service-graph"), &t).await {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

// ============================ Webhooks ============================

async fn webhook_events() -> Json<Value> {
    Json(json!(crate::webhooks::ALL_EVENTS))
}

async fn webhooks_all(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    // Only the caller's own webhooks (the payload includes signing secrets, so
    // this must be tenant-scoped).
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let list: Vec<_> = c.webhooks.snapshot().into_iter().filter(|w| norm(&w.team) == t).map(|w| w.decrypted()).collect();
    Json(json!(list))
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

async fn webhook_create_team(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Json(b): Json<CreateTeamWebhook>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let targets: Vec<String> = if b.projects.is_empty() || b.projects.iter().any(|p| p == "*") {
        vec!["*".into()]
    } else {
        b.projects.clone()
    };
    let mut created = Vec::new();
    for p in targets {
        created.push(c.webhooks.add(crate::webhooks::Webhook {
            id: String::new(),
            team: t.clone(),
            project: p,
            url: b.url.clone(),
            events: b.events.clone(),
            secret: String::new(),
            enabled: true,
            created_ms: 0,
        }));
    }
    crate::persist::persist(&c);
    let created: Vec<_> = created.into_iter().map(|w| w.decrypted()).collect();
    Json(json!(created))
}

async fn webhooks_for_project(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(project): Path<String>) -> Json<Value> {
    // Don't expose another tenant's project webhooks (incl. their secrets).
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!([]));
    }
    let list: Vec<_> = c.webhooks.list(Some(&project)).into_iter().map(|w| w.decrypted()).collect();
    Json(json!(list))
}

#[derive(Deserialize)]
struct CreateWebhook {
    url: String,
    #[serde(default)]
    events: Vec<String>,
}

async fn webhook_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<CreateWebhook>,
) -> Result<Json<Value>, StatusCode> {
    // A webhook may only be attached to a project the caller owns.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Err(StatusCode::NOT_FOUND);
    }
    let wh = c.webhooks.add(crate::webhooks::Webhook {
        id: String::new(),
        team: t,
        project,
        url: b.url,
        events: b.events,
        secret: String::new(),
        enabled: true,
        created_ms: 0,
    });
    crate::persist::persist(&c);
    Ok(Json(json!(wh.decrypted())))
}

async fn webhook_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(w) = c.webhooks.snapshot().into_iter().find(|w| w.id == id) {
        if norm(&w.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    c.webhooks.remove(&id);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
}

async fn webhook_deliveries(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.webhooks.deliveries(100))))
}

// ============================ Databases ============================

async fn databases_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let list: Vec<_> = c
        .databases
        .list(None)
        .into_iter()
        .filter(|d| norm(&d.team) == t)
        .collect();
    Json(json!(list))
}

/// Platform-owner view: ALL databases across every tenant (the ops Database Fleet
/// is global, unlike the tenant-scoped `/v1/databases`).
async fn admin_databases_all(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.databases.list(None))))
}

async fn databases_for_project(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(project): Path<String>) -> Json<Value> {
    // Only expose databases for a project the caller's tenant actually owns.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!([]));
    }
    Json(json!(c.databases.list(Some(&project))))
}

async fn database_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let d = c.databases.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    if norm(&d.team) != t {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(d)))
}

async fn database_credentials(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    // Returns unmasked connection secrets — must be strictly tenant-scoped.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let d = c.databases.get_raw(&id).ok_or(StatusCode::NOT_FOUND)?;
    if norm(&d.team) != t {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(d)))
}

/// The env-var (KEY, VALUE, sensitive) triples a database injects into its owning
/// project so deployments reach it automatically — canonical names (DATABASE_URL,
/// REDIS_URL, …) for the common single-DB case PLUS name-prefixed copies that are
/// collision-free when a project has several DBs of the same kind. Pure — no side
/// effects — so create (apply) and delete (remove by key) agree on the key set.
fn db_egress_pairs(c: &Arc<CloudState>, d: &crate::databases::Database) -> Vec<(String, String, bool)> {
    let api_base = c.api_base();
    let prefix = crate::databases::env_prefix(&d.name);
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for (k, v) in crate::databases::env_exports(d, &api_base) {
        let kl = k.to_lowercase();
        let sensitive = kl.contains("url") || kl.contains("token") || kl.contains("password") || kl.contains("key") || kl.contains("secret");
        out.push((k.clone(), v.clone(), sensitive));
        if !prefix.is_empty() {
            out.push((format!("{prefix}_{k}"), v, sensitive));
        }
    }
    out
}

pub(crate) fn apply_db_egress(c: &Arc<CloudState>, d: &crate::databases::Database) {
    for (k, v, sensitive) in db_egress_pairs(c, d) {
        c.projects.put_env(&d.project, crate::project_settings::EnvVar {
            key: k, value: v, target: "all".into(), sensitive, updated_ms: now_ms(),
        });
    }
}

/// NON-SECRET directory of gateway-addressable DBs hosted on this node — the
/// same payload as the `/v1/db-directory` gossip arm (routing metadata only:
/// {id, db_host, host_node, kind}). The DNS leader fans this out to publish
/// per-DB A records for DBs provisioned on other nodes.
async fn db_directory(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Gating the HTTP path only — the iroh gossip fan-out this feeds
    // (vercel_dns.rs's DNS reconciler, via `fetch_from_host`) has its own
    // separate inline arm in gossip.rs that never calls this handler, so this
    // does not break that legitimate mesh-internal use; it only stops a direct
    // internet client from using this as cross-tenant DB-id recon (a stepping
    // stone toward the `database_replica` finding).
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.databases.directory())))
}

async fn database_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut req): Json<crate::databases::ProvisionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cloud = c.clone();
    // Always the server-resolved tenant — a client-supplied `team` used to be
    // trusted outright whenever non-empty, letting any caller register a
    // database (and auto-inject its connection env) under a DIFFERENT tenant.
    req.team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Only enforce project ownership when the project is ALREADY registered
    // (has a team of record) — provisioning a database for a brand-new
    // project name before its first deploy is a normal flow and must keep
    // working. An EXISTING project owned by a different tenant is rejected.
    if !req.project.is_empty() && c.projects.snapshot().contains_key(&req.project) {
        require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &req.project)?;
    }
    let project = req.project.clone();
    let db = crate::databases::provision(c.databases.clone(), c.region.clone(), req, c.db_domain.clone(), c.node_name.clone(), move |d| {
        // EGRESS: auto-inject this DB's connection env into the owning project so
        // its deployments can reach it with zero manual copy-paste. Only when the
        // DB is actually ready (connection populated).
        if !d.project.is_empty() && matches!(d.status, crate::databases::DbStatus::Ready) {
            apply_db_egress(&cloud, &d);
        }
        // REPLICATION: if replica regions were configured, fan the DB out to them.
        if !d.replicas.is_empty() {
            crate::db_replicate::ensure_replicas(cloud.clone(), d.clone());
        }
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
    Ok(Json(json!(db)))
}

async fn database_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(d) = c.databases.get_raw(&id) {
        if norm(&d.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
        // Remove the auto-injected egress env vars from the owning project.
        if !d.project.is_empty() {
            for (k, _, _) in db_egress_pairs(&c, &d) {
                c.projects.delete_env(&d.project, &k);
            }
        }
        // Tear down any cross-region replicas (before consuming `d.container`).
        if !d.replicas.is_empty() {
            crate::db_replicate::remove_replicas(c.clone(), d.clone());
        }
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
    // Purges the actual queue/vector/blob PAYLOAD data too (keyed by a
    // team-prefixed connection name, not the db id) — `remove_db` alone only
    // ever removed the catalog entry, leaving customer data (which may
    // include PII in vector metadata or queue message bodies) orphaned in
    // memory/on-disk forever, re-persisted in every future snapshot.
    c.databases.remove_db_and_purge_data(&id, &t);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
}

/// Mesh-internal: register or remove a cross-region REPLICA of a database on this
/// node. Called by the primary node's replication fanout (peer-authenticated over
/// the mesh, or via the loopback+firewalled admin API). On register we record the
/// database locally as a `replica` in THIS node's region, auto-inject its egress
/// env into the owning project (so in-region deployments reach it), and — for
/// Postgres/Redis — provision a real in-region backing container.
pub(crate) async fn database_replica(
    State(c): State<Arc<CloudState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let op = body.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let mut db: crate::databases::Database = serde_json::from_value(body.get("db").cloned().unwrap_or(Value::Null))
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("bad db payload: {e}")))?;
    // This RPC is reachable both over the (currently unauthenticated) mesh
    // path and directly on the public HTTP admin router, and it used to fully
    // trust a client-supplied `db` body — so a caller with no relationship to
    // the target database could force-destroy or re-register it. Real
    // mesh-peer authentication for this path is tracked separately (the
    // gossip/peer-trust hardening work); as an immediate, scoped fix: when
    // `db.id` already names an EXISTING record, require the request's team
    // to match the REAL owning team on record before mutating it. A brand-new
    // id (the normal "replicate to a new peer" case) has nothing to compare
    // against yet and is unaffected.
    if !db.id.trim().is_empty() {
        if let Some(existing) = c.databases.get_raw(&db.id) {
            if norm(&existing.team) != norm(&db.team) {
                return Err((StatusCode::FORBIDDEN, "db belongs to a different team".into()));
            }
        }
    }
    match op {
        "register" => {
            db.role = "replica".into();
            db.region = c.region.clone();
            // Platform-native kinds serve from THIS node's in-process store (data
            // arrives via write-mirroring); rewrite endpoint host to our API base.
            // Postgres/Redis get a real local backing container so reads are local.
            let backed = crate::databases::provision_replica_backing(c.databases.clone(), &db).await;
            db.connection = backed.0;
            db.container = backed.1;
            db.mode = backed.2;
            c.databases.upsert_replica(db.clone());
            if !db.project.is_empty() {
                apply_db_egress(&c, &db);
            }
            crate::persist::persist(&c);
            Ok(Json(json!({ "registered": db.id, "region": db.region })))
        }
        "remove" => {
            if !db.project.is_empty() {
                for (k, _, _) in db_egress_pairs(&c, &db) {
                    c.projects.delete_env(&db.project, &k);
                }
            }
            if let Some(existing) = c.databases.get_raw(&db.id) {
                if let Some(container) = existing.container {
                    let _ = tokio::process::Command::new("podman")
                        .args(["rm", "-f", &container])
                        .env("PATH", format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}", std::env::var("PATH").unwrap_or_default()))
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                }
            }
            c.databases.remove_db(&db.id);
            crate::persist::persist(&c);
            Ok(Json(json!({ "removed": db.id })))
        }
        _ => Err((StatusCode::BAD_REQUEST, "op must be register|remove".into())),
    }
}

/// Apply a MIRRORED storage write received over the iroh mesh (the HTTP admin
/// path goes through the normal handlers). Parses `<team>` from `?team=` and the
/// kind/name/key from the path, applying to the SAME tenant namespace so the
/// replica holds identical data. Never re-mirrors (already a replicated write).
pub(crate) async fn apply_mirrored_write(c: &Arc<CloudState>, path: &str, body: &[u8]) {
    let team = qs(path, "team").unwrap_or_else(|| "personal".into());
    let no_q = path.split('?').next().unwrap_or(path);
    let segs: Vec<&str> = no_q.trim_start_matches('/').split('/').collect();
    // segs: ["v1","storage",<kind>, ...]
    let kind = segs.get(2).copied().unwrap_or("");
    match kind {
        "blob" => {
            if let (Some(bucket), Some(key)) = (segs.get(3), segs.get(4)) {
                c.databases.blob_put(&format!("{team}::{bucket}"), key, body.to_vec());
            }
        }
        "queue" => {
            if let Some(queue) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<Value>(body) {
                    if let Some(msg) = v.get("message") {
                        c.databases.queue_push(&format!("{team}::{queue}"), msg.to_string());
                    }
                }
            }
        }
        "vector" => {
            if let Some(index) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<VectorUpsert>(body) {
                    c.databases.vector_upsert(&format!("{team}::{index}"), &v.id, v.vector, v.metadata);
                }
            }
        }
        "pubsub" => {
            if let Some(topic) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<Value>(body) {
                    if let Some(msg) = v.get("message") {
                        c.databases.publish(&format!("{team}::{topic}"), msg.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

fn qs(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

// ---- Functional storage REST (Blob / Queue / Vector) ----
//
// MULTI-TENANT: every namespace name (bucket/queue/index/topic/room) coming from
// the request path is tenant-prefixed (`<tenant>::<name>`) before touching the
// store, so two teams using the same name get DISJOINT storage. Responses echo
// the caller's un-prefixed name.

fn ns(c: &Arc<CloudState>, h: &HeaderMap, claims: Option<&crate::auth::Claims>, name: &str) -> String {
    format!("{}::{}", tenant(c, h, claims), name)
}

/// Resolve the (team, is_mirror) scope for a storage WRITE. A mesh-internal
/// mirrored write is a trusted replication call from a primary node — its team
/// is authoritative, so replicated data lands in the SAME tenant namespace on the
/// replica. This MUST be bound to a verified identity, not header presence: this
/// admin router is bound to the public API host (main.rs), so an arbitrary
/// internet caller can set `x-hive-mirror`/`x-hive-team` themselves. `x-hive-mirror-tok`
/// carries a short-lived signed token (minted the same way as `mesh_team_qs`,
/// `crate::auth::issue("mesh-internal", team, "service", false, 60)`) — only a
/// caller holding `HIVE_JWT_SECRET` can produce one, closing the cross-tenant
/// write bypass. When JWT enforcement is off (dev/single-node), the raw header
/// is trusted as before (nothing to forge against). Normal writes resolve the
/// tenant as usual.
fn write_scope(c: &Arc<CloudState>, h: &HeaderMap, claims: Option<&crate::auth::Claims>) -> (String, bool) {
    let is_mirror = h.get("x-hive-mirror").is_some() || h.get("x-mirror").is_some();
    if is_mirror {
        if crate::auth::enforced() {
            let team = h
                .get("x-hive-mirror-tok")
                .and_then(|v| v.to_str().ok())
                .and_then(|tok| crate::auth::verify(tok).ok())
                .map(|claims| claims.tenant);
            return match team {
                Some(team) if !team.is_empty() => (team, true),
                _ => {
                    tracing::warn!("mirror write rejected: missing/invalid x-hive-mirror-tok under enforced auth");
                    (tenant(c, h, claims), false)
                }
            };
        }
        let team = h.get("x-hive-team").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "personal".into());
        (team, true)
    } else {
        (tenant(c, h, claims), false)
    }
}

async fn blob_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((bucket, key)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Json<Value> {
    let (team, is_mirror) = write_scope(&c, &headers, claims.as_ref().map(|e| &e.0));
    let nsb = format!("{team}::{bucket}");
    let size = body.len();
    c.databases.blob_put(&nsb, &key, body.to_vec());
    crate::db_replicate::on_write(&c, is_mirror, &team, &bucket, "PUT", format!("/v1/storage/blob/{bucket}/{key}"), "application/octet-stream", body.to_vec());
    Json(json!({ "bucket": bucket, "key": key, "size": size, "url": format!("/v1/storage/blob/{bucket}/{key}") }))
}

async fn blob_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    let nsb = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &bucket);
    match c.databases.blob_get(&nsb, &key) {
        Some(data) => Ok(data.into_response()),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn blob_list_keys(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(bucket): Path<String>,
) -> Json<Value> {
    let nsb = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &bucket);
    Json(json!({ "bucket": bucket, "keys": c.databases.blob_list(&nsb) }))
}

#[derive(Deserialize)]
struct QueueMsg {
    message: Value,
}

async fn queue_push(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(queue): Path<String>,
    Json(b): Json<QueueMsg>,
) -> Json<Value> {
    let (team, is_mirror) = write_scope(&c, &headers, claims.as_ref().map(|e| &e.0));
    let nsq = format!("{team}::{queue}");
    let depth = c.databases.queue_push(&nsq, b.message.to_string());
    let mirror_body = serde_json::to_vec(&json!({ "message": b.message })).unwrap_or_default();
    crate::db_replicate::on_write(&c, is_mirror, &team, &queue, "POST", format!("/v1/storage/queue/{queue}"), "application/json", mirror_body);
    Json(json!({ "queue": queue, "depth": depth }))
}

async fn queue_pop(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(queue): Path<String>,
) -> Json<Value> {
    let nsq = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &queue);
    let msg = c.databases.queue_pop(&nsq);
    let parsed = msg.as_ref().and_then(|m| serde_json::from_str::<Value>(m).ok());
    Json(json!({ "queue": queue, "message": parsed.or(msg.map(Value::String)), "depth": c.databases.queue_depth(&nsq) }))
}

async fn queue_depth(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(queue): Path<String>,
) -> Json<Value> {
    let nsq = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &queue);
    Json(json!({ "queue": queue, "depth": c.databases.queue_depth(&nsq) }))
}

#[derive(Deserialize)]
struct VectorUpsert {
    id: String,
    vector: Vec<f32>,
    #[serde(default)]
    metadata: Value,
}

async fn vector_upsert(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(index): Path<String>,
    Json(b): Json<VectorUpsert>,
) -> Json<Value> {
    let (team, is_mirror) = write_scope(&c, &headers, claims.as_ref().map(|e| &e.0));
    let nsi = format!("{team}::{index}");
    let mirror_body = serde_json::to_vec(&json!({ "id": b.id, "vector": b.vector, "metadata": b.metadata })).unwrap_or_default();
    c.databases.vector_upsert(&nsi, &b.id, b.vector, b.metadata);
    crate::db_replicate::on_write(&c, is_mirror, &team, &index, "POST", format!("/v1/storage/vector/{index}"), "application/json", mirror_body);
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

async fn vector_query(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(index): Path<String>,
    Json(b): Json<VectorQuery>,
) -> Json<Value> {
    let nsi = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &index);
    Json(json!({ "index": index, "matches": c.databases.vector_query(&nsi, &b.vector, b.top_k) }))
}

// ---- Managed World Queue (Vercel WDK, hive-native, no external queue dep) ----

#[derive(Deserialize)]
struct WqueueEnqueueReq {
    target_url: String,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    payload: Value,
    #[serde(default)]
    delay_seconds: u64,
    #[serde(default = "default_wqueue_max_attempts")]
    max_attempts: u32,
}
fn default_wqueue_max_attempts() -> u32 {
    10
}

async fn wqueue_enqueue(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<WqueueEnqueueReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Tenant-scoped like every other /v1/storage/*-style mutation; the target
    // URL is caller-supplied so it must resolve to a real, authenticated caller
    // (never let an anonymous/ANON_TENANT caller schedule arbitrary HTTP callbacks).
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if t == ANON_TENANT {
        return Err((StatusCode::UNAUTHORIZED, "authentication required".into()));
    }
    let id = c.world_queue.enqueue(t, b.target_url, b.headers, b.payload, b.delay_seconds, b.max_attempts);
    Ok(Json(json!({ "message_id": id })))
}

async fn wqueue_stats(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(c.world_queue.stats())
}

// ---- Pub/Sub + Realtime (WebSocket secure streaming) ----

async fn pubsub_info(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(topic): Path<String>,
) -> Json<Value> {
    let nst = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &topic);
    Json(json!({
        "topic": topic,
        "subscribers": c.databases.subscriber_count(&nst),
        "published": c.databases.published_count(&nst),
    }))
}

async fn pubsub_publish(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(topic): Path<String>,
    Json(b): Json<QueueMsg>,
) -> Json<Value> {
    let (team, is_mirror) = write_scope(&c, &headers, claims.as_ref().map(|e| &e.0));
    let nst = format!("{team}::{topic}");
    let delivered = c.databases.publish(&nst, b.message.to_string());
    let mirror_body = serde_json::to_vec(&json!({ "message": b.message })).unwrap_or_default();
    crate::db_replicate::on_write(&c, is_mirror, &team, &topic, "POST", format!("/v1/storage/pubsub/{topic}/publish"), "application/json", mirror_body);
    Json(json!({ "topic": topic, "delivered": delivered }))
}

async fn ws_pubsub(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(topic): Path<String>,
) -> axum::response::Response {
    let mut rx = c.databases.subscribe(&ns(&c, &headers, claims.as_ref().map(|e| &e.0), &topic));
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
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(room): Path<String>,
) -> axum::response::Response {
    let room = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &room);
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

async fn securelinks_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    Json(json!(c.securelinks.list(&tenant(&c, &headers, claims.as_ref().map(|e| &e.0)))))
}

async fn securelink_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut req): Json<crate::securelink::ProvisionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Always the server-resolved tenant — see the identical fix on
    // database_create for why a client-supplied `team` must never be trusted.
    req.team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Only enforce ownership when wiring into an ALREADY-registered project
    // (an unset/new project name is a normal "create the tunnel first" flow).
    if let Some(p) = req.project.as_deref().filter(|p| !p.is_empty()) {
        if c.projects.snapshot().contains_key(p) {
            require_project(&c, &headers, claims.as_ref().map(|e| &e.0), p)?;
        }
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

async fn securelink_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(l) = c.securelinks.all().into_iter().find(|l| l.id == id) {
        if norm(&l.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    c.securelinks.remove(&id);
    Ok(Json(json!({ "removed": id })))
}

// ============================ Monitoring ============================

#[derive(Deserialize)]
struct MetricsQ {
    minutes: Option<usize>,
    project: Option<String>,
    /// "minute" (default) | "hour" | "day" — the consumption-breakdown chart's
    /// Daily/Weekly/Monthly toggle (ui/app/usage/page.tsx). Unrecognized/absent
    /// falls back to "minute" (today's pre-existing behavior) via
    /// `Granularity::parse` — never a 400, this is a display parameter.
    gran: Option<String>,
    /// Set by the fleet fan-out's own inner call (`metrics_get` -> peer) to
    /// request ONLY this node's local view, with no further fan-out — stops
    /// the recursion at one hop. Absent/false = the top-level call, which fans
    /// out to every other healthy node (see `metrics_get`'s doc comment).
    #[serde(default, deserialize_with = "de_lenient_bool")]
    local: Option<bool>,
}

/// Merge one peer's `/v1/metrics?local=true` JSON response into the running
/// accumulators (series by `t_ms`, status/top-paths/projects by key). Silently
/// no-ops on a malformed/unreachable peer response — a single unreachable node
/// must not fail the whole tenant's usage view (matches `fleet_function_stats`'s
/// best-effort fan-out).
fn merge_peer_metrics(
    v: &Value,
    series: &mut Vec<crate::metrics::Bucket>,
    status_distribution: &mut std::collections::HashMap<String, u64>,
    top_paths: &mut std::collections::HashMap<String, u64>,
    projects: &mut std::collections::HashMap<String, u64>,
) {
    if let Some(peer_series) = v.get("series").and_then(|s| serde_json::from_value::<Vec<crate::metrics::Bucket>>(s.clone()).ok()) {
        let mut by_t: std::collections::HashMap<u64, usize> = series.iter().enumerate().map(|(i, b)| (b.t_ms, i)).collect();
        for pb in peer_series {
            if let Some(&i) = by_t.get(&pb.t_ms) {
                series[i].add(&pb);
            } else {
                by_t.insert(pb.t_ms, series.len());
                series.push(pb);
            }
        }
        series.sort_by_key(|b| b.t_ms);
    }
    if let Some(peer_status) = v.get("status_distribution").and_then(|s| serde_json::from_value::<std::collections::HashMap<String, u64>>(s.clone()).ok()) {
        for (k, n) in peer_status {
            *status_distribution.entry(k).or_insert(0) += n;
        }
    }
    for (dst, key) in [(&mut *top_paths, "top_paths"), (&mut *projects, "projects")] {
        let (name_key, count_key) = if key == "top_paths" { ("path", "count") } else { ("project", "requests") };
        if let Some(rows) = v.get(key).and_then(|s| s.as_array()) {
            for row in rows {
                if let (Some(name), Some(n)) = (row.get(name_key).and_then(|x| x.as_str()), row.get(count_key).and_then(|x| x.as_u64())) {
                    *dst.entry(name.to_string()).or_insert(0) += n;
                }
            }
        }
    }
}

/// TENANT-SCOPED usage series + breakdowns for the Usage page. FLEET-AGGREGATE:
/// `MetricsStore` is confirmed node-local with zero cross-node merge (never
/// gossiped, and deliberately NOT adopted from a peer on restore — see
/// `guardian::strip_node_local`'s "would double-count its traffic" comment,
/// unlike billing which has a single elected metering owner). A tenant's
/// traffic can land on ANY healthy node the routing mesh sends it to, so
/// without fanning out, `/v1/metrics` answers depend entirely on which node
/// happens to serve the dashboard's request — live-witnessed: the SAME tenant
/// showed 0 requests on 7 of 8 fleet nodes and real traffic only on the 8th.
/// `q.local=true` (set only on the inner fan-out call below) stops a peer from
/// ALSO fanning out — single-hop only, no N-way amplification.
async fn metrics_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Query(q): Query<MetricsQ>) -> Json<Value> {
    let gran = crate::metrics::Granularity::parse(q.gran.as_deref());
    // Clamp to what this resolution's ring buffer actually retains (Minute:
    // 24h, Hour: 30d, Day: ~13mo — see metrics.rs's MAX_*_BUCKETS) so a caller
    // can never request a span longer than the data that exists to answer it.
    let minutes = q.minutes.unwrap_or(60).min(gran.max_span_minutes());
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // TENANT-SCOPED: every series/status/path read is confined to the caller's
    // tenant — no cross-tenant telemetry leak. (The owner ops console reads global
    // metrics through the separate operator-gated `admin_overview`.)
    let scope = Some(t.as_str());
    let project = q.project.as_deref().filter(|p| !p.is_empty());
    let mut series = c.metrics.series(gran, minutes, now_ms(), scope, project);
    let mut status_distribution = c.metrics.status_distribution(scope);
    let mut top_paths: std::collections::HashMap<String, u64> = c.metrics.top_paths(scope, 200).into_iter().collect();
    let mut projects: std::collections::HashMap<String, u64> = c.metrics.project_totals(gran, minutes, now_ms(), scope).into_iter().collect();

    if !q.local.unwrap_or(false) {
        let self_name = c.node_name.clone();
        for n in c.registry.nodes() {
            if n.name == self_name || !n.healthy {
                continue;
            }
            let mut path = format!("/v1/metrics?local=true&minutes={minutes}&gran={}", q.gran.as_deref().unwrap_or("minute"));
            if let Some(p) = project {
                path.push_str(&format!("&project={}", urlencode(p)));
            }
            if let Some(v) = fetch_from_host(&c, &n.name, &path, &t).await {
                merge_peer_metrics(&v, &mut series, &mut status_distribution, &mut top_paths, &mut projects);
            }
        }
    }

    let total_req: u64 = series.iter().map(|b| b.requests).sum();
    let total_err: u64 = series.iter().map(|b| b.errors + b.client_err).sum();
    let total_blocked: u64 = series.iter().map(|b| b.blocked).sum();
    let hits: u64 = series.iter().map(|b| b.cache_hits).sum();
    let miss: u64 = series.iter().map(|b| b.cache_miss).sum();
    let cache_ratio = if hits + miss == 0 { 0.0 } else { hits as f64 / (hits + miss) as f64 };
    let err_rate = if total_req == 0 { 0.0 } else { total_err as f64 / total_req as f64 };
    let mut top_paths_v: Vec<(String, u64)> = top_paths.into_iter().collect();
    top_paths_v.sort_by(|a, b| b.1.cmp(&a.1));
    top_paths_v.truncate(10);
    let mut projects_v: Vec<(String, u64)> = projects.into_iter().collect();
    projects_v.sort_by(|a, b| b.1.cmp(&a.1));
    Json(json!({
        "series": series,
        "totals": {
            "requests": total_req,
            "errors": total_err,
            "blocked": total_blocked,
            "error_rate": err_rate,
            "cache_hit_ratio": cache_ratio,
        },
        "status_distribution": status_distribution,
        "top_paths": top_paths_v.into_iter().map(|(p, n)| json!({ "path": p, "count": n })).collect::<Vec<_>>(),
        "projects": projects_v.into_iter().map(|(p, n)| json!({ "project": p, "requests": n })).collect::<Vec<_>>(),
    }))
}

// ============================ Owner / ops dashboard ============================

#[derive(Deserialize, Default)]
struct OverviewQ {
    /// Set by the fleet fan-out's own inner call — see `metrics_get`'s doc
    /// comment for the identical single-hop-fan-out rationale (`c.counters()`
    /// and `c.metrics` are both confirmed node-local with no live cross-node
    /// merge, same defect class).
    #[serde(default, deserialize_with = "de_lenient_bool")]
    local: Option<bool>,
}

async fn admin_overview(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    q: Option<Query<OverviewQ>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let (mut reqs, mut blocked) = c.counters();
    let fstats = c.fluid.stats();
    let instances: usize = fstats.iter().map(|f| f.instances).sum();
    let nodes = c.registry.nodes();
    let dbs = c.databases.list(None);
    let live_dbs = dbs.iter().filter(|d| d.mode == "live").count();
    // Recent error rate from the metrics buckets (last 30m), GLOBAL across all
    // tenants — this is the operator ops console (owner-only), not tenant-facing.
    let series = c.metrics.series(crate::metrics::Granularity::Minute, 30, now_ms(), None, None);
    let mut req30: u64 = series.iter().map(|b| b.requests).sum();
    let mut err30: u64 = series.iter().map(|b| b.errors).sum();
    // FLEET-AGGREGATE: `c.counters()`/`c.metrics` are node-local (same confirmed
    // defect as `metrics_get` — see its doc comment). Without this, the ops
    // console's headline request/blocked/error-rate tiles reflect only whichever
    // node happens to serve the request, understating real fleet traffic by up
    // to 8x. `local=true` on the fan-out call stops peer recursion (one hop).
    if !q.map(|Query(q)| q.local.unwrap_or(false)).unwrap_or(false) {
        let self_name = c.node_name.clone();
        for n in &nodes {
            if n.name == self_name || !n.healthy {
                continue;
            }
            let Some(v) = fetch_from_host(&c, &n.name, "/v1/admin/overview?local=true", "").await else { continue };
            reqs += v.get("requests").and_then(|x| x.as_u64()).unwrap_or(0);
            blocked += v.get("blocked").and_then(|x| x.as_u64()).unwrap_or(0);
            req30 += v.get("req30").and_then(|x| x.as_u64()).unwrap_or(0);
            err30 += v.get("err30").and_then(|x| x.as_u64()).unwrap_or(0);
        }
    }
    let err_rate = if req30 == 0 { 0.0 } else { err30 as f64 / req30 as f64 };
    // Fleet-aggregated counts (operator, all tenants): the ops overview runs on
    // ONE node while the placement scheduler hosts deployments on peers, so a
    // bare `c.gw.list()` under-counts to zero here. Deployments = local + gossiped
    // peer_deployments (deduped by id). Projects = the tenant-tagged projects
    // store (authoritative, gossiped) UNION any project that only shows up as a
    // hosted deployment — so a project is counted even before its record replicates.
    let fleet_deps = fleet_deployments_all(&c);
    let deployments_count = fleet_deps.len();
    let projects_count = {
        let mut names: std::collections::BTreeSet<String> =
            c.projects.snapshot().into_iter().map(|(k, _)| k).collect();
        for d in &fleet_deps {
            if let Some(p) = d.get("project").and_then(|v| v.as_str()) {
                names.insert(p.to_string());
            }
        }
        names.len()
    };
    Ok(Json(json!({
        "owner": c.owner_email,
        "teams": c.teams.count(),
        "projects": projects_count,
        "deployments": deployments_count,
        "databases": { "total": dbs.len(), "live": live_dbs },
        "nodes": nodes.len(),
        "regions": c.registry.regions(),
        "instances": instances,
        "requests": reqs,
        "blocked": blocked,
        "error_rate_30m": err_rate,
        // Raw 30m request/error counts (not just the ratio) so a fleet fan-out
        // call can correctly weight-average across nodes rather than averaging
        // already-averaged per-node ratios.
        "req30": req30,
        "err30": err30,
        "incidents_open": c.incidents.open_count(),
        "cluster": c.cluster.status(nodes.iter().map(|n| n.id.clone()).collect()),
        "webhooks": c.webhooks.list(None).len(),
        // Real cluster capacity = sum of every live node's host resources.
        "resources": {
            "cpu_cores": nodes.iter().map(|n| n.cpu_cores as u64).sum::<u64>(),
            "mem_total_mb": nodes.iter().map(|n| n.mem_total_mb).sum::<u64>(),
            "disk_total_gb": nodes.iter().map(|n| n.disk_total_gb).sum::<u64>(),
        },
    })))
}

async fn admin_audit(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    // The durable, append-only audit log of every state mutation (newest first).
    Ok(Json(json!(c.audit.recent(300, None))))
}

async fn incidents_list(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.incidents.list())))
}

async fn incident_open(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<crate::incidents::OpenReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let inc = c.incidents.open(req);
    crate::persist::persist(&c);
    crate::webhooks::dispatch(&c.webhooks, "*", "incident.opened", json!({ "id": inc.id, "title": inc.title, "severity": inc.severity }));
    Ok(Json(json!(inc)))
}

async fn incident_update(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    Json(req): Json<crate::incidents::UpdateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let resolved = matches!(req.status, crate::incidents::IncidentStatus::Resolved);
    let inc = c.incidents.update(&id, req).ok_or((StatusCode::NOT_FOUND, "no such incident".into()))?;
    crate::persist::persist(&c);
    if resolved {
        crate::webhooks::dispatch(&c.webhooks, "*", "incident.resolved", json!({ "id": inc.id, "title": inc.title }));
    }
    Ok(Json(json!(inc)))
}

// ---- Notifications (inbox bell) ----

/// Compute the live notification list for a team from real platform signals
/// (failed deploys, 5xx error anomalies, blocked-traffic usage anomalies),
/// applying the user's read/archived state. Archived items keep their `archived`
/// flag so the client can render an Archive tab.
fn build_notifications(c: &Arc<CloudState>, team: &str) -> Vec<crate::notifications::Notification> {
    use crate::notifications::Notification;
    use std::collections::HashMap;
    let team = norm(team).to_string();
    let mut out: Vec<Notification> = Vec::new();

    // 1) Failed deployments.
    for d in c.gw.list() {
        // Authoritative record tag (present on the local record), not the
        // node-local project row which is UNTAGGED on a non-hosting node.
        if record_tenant(&d.tenant) != team {
            continue;
        }
        if d.state == fluid_core::DeployState::Error {
            let env = if d.production { "Production" } else { "Preview" };
            out.push(Notification {
                id: format!("deploy-{}", d.id.0),
                severity: "warning".into(),
                category: "deploy".into(),
                project: d.project.clone(),
                environment: env.into(),
                message: format!("{} failed to deploy in the {} environment", d.project, env),
                ts_ms: d.created_at_ms,
                read: false,
                archived: false,
            });
        }
    }

    // 1b) Failed builds (git deploys that errored before going live).
    for b in c.builds.list() {
        // Fleet-aware ownership (project rows are node-local): a failed-build
        // alert would otherwise silently drop on any node whose project row is
        // absent, hiding the failure from the owner.
        if !project_owned_by(&c, &b.project, &team) {
            continue;
        }
        if b.state == fluid_core::DeployState::Error {
            out.push(Notification {
                id: format!("build-{}", b.id),
                severity: "warning".into(),
                category: "deploy".into(),
                project: b.project.clone(),
                environment: "Production".into(),
                message: format!("{} failed to deploy in the Production environment", b.project),
                ts_ms: b.started_ms,
                read: false,
                archived: false,
            });
        }
    }

    // 2) Error / usage anomalies from recent edge events, grouped per project.
    let mut err5xx: HashMap<String, u64> = HashMap::new();
    let mut blocked: HashMap<String, u64> = HashMap::new();
    for ev in c.recent_events(300) {
        if ev.project.is_empty() {
            continue;
        }
        // Edge events carry no tenant tag — judge via the fleet-aware predicate
        // (deployment tenant tags) so a member's error/usage alerts aren't
        // dropped on a node whose project row is absent.
        if !project_owned_by(c, &ev.project, &team) {
            continue;
        }
        if ev.status >= 500 {
            let e = err5xx.entry(ev.project.clone()).or_insert(0);
            *e = (*e).max(ev.ts_ms);
        }
        if ev.action == "waf-deny" || ev.action == "bot-block" {
            let e = blocked.entry(ev.project.clone()).or_insert(0);
            *e = (*e).max(ev.ts_ms);
        }
    }
    for (proj, ts) in err5xx {
        out.push(Notification {
            id: format!("anom-5xx-{proj}"),
            severity: "error".into(),
            category: "anomaly".into(),
            environment: "Production".into(),
            message: format!("error anomaly detected for {proj}: 5xx status codes"),
            project: proj,
            ts_ms: ts,
            read: false,
            archived: false,
        });
    }
    for (proj, ts) in blocked {
        out.push(Notification {
            id: format!("usage-blocked-{proj}"),
            severity: "error".into(),
            category: "usage".into(),
            environment: "Production".into(),
            message: format!("usage anomaly detected for {proj}: blocked requests"),
            project: proj,
            ts_ms: ts,
            read: false,
            archived: false,
        });
    }

    for n in out.iter_mut() {
        n.read = c.notifications.is_read(&n.id);
        n.archived = c.notifications.is_archived(&n.id);
    }
    out.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    out
}

async fn notifications_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let items = build_notifications(&c, &team);
    let inbox = items.iter().filter(|n| !n.archived).count();
    let unread = items.iter().filter(|n| !n.archived && !n.read).count();
    Json(json!({ "unread": unread, "inbox": inbox, "items": items }))
}

async fn notification_archive(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Only archive an id that actually belongs to the caller's own team's
    // notification feed — this used to archive ANY id globally, letting one
    // team hide another team's alert (e.g. a failed-deploy/error-rate
    // notification) by guessing its predictable `deploy-<id>`/`anom-*` format.
    if !build_notifications(&c, &team).iter().any(|n| n.id == id) {
        return Err((StatusCode::NOT_FOUND, "no such notification".into()));
    }
    c.notifications.archive(&id);
    Ok(Json(json!({ "archived": id })))
}

async fn notifications_archive_all(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ids: Vec<String> = build_notifications(&c, &team)
        .into_iter()
        .filter(|n| !n.archived)
        .map(|n| n.id)
        .collect();
    c.notifications.archive_all(&ids);
    Json(json!({ "archived": ids.len() }))
}

async fn notifications_read(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ids: Vec<String> = build_notifications(&c, &team).into_iter().map(|n| n.id).collect();
    c.notifications.mark_read(&ids);
    Json(json!({ "read": ids.len() }))
}

// ============================ Identity sync (orgs & users) ============================

#[derive(Deserialize)]
struct SyncOrg {
    id: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    image_url: String,
}

#[derive(Deserialize)]
struct SyncUser {
    id: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    image_url: String,
}

#[derive(Deserialize)]
struct IdentitySyncReq {
    user: SyncUser,
    #[serde(default)]
    org: Option<SyncOrg>,
}

/// The dashboard syncs the signed-in user + active org from the identity provider
/// (Clerk). We index them into the store, scoped to the active tenant namespace.
///
/// SECURITY (fixes a confirmed cross-tenant data leak): `is_owner` and the
/// indexed tenant are now derived from the caller's OWN verified JWT claims
/// (`platform_admin`/`tenant`) when auth is enforced — NEVER from the request
/// body's `user.email`/`org`. The previous implementation trusted the body
/// outright: this route requires SOME authenticated caller (any mutation not
/// in `auth::require_auth`'s open list), but nothing checked that the caller
/// actually WAS the identity/org they claimed — so any authenticated user,
/// including one from a completely different, unprivileged tenant, could POST
/// `{"user":{"email":"<owner's email>"},...}` and get back `is_owner: true`.
/// The dashboard's own client then persisted that as `hive_is_owner=1` in
/// their OWN browser's localStorage, and `currentTeam()` (ui/lib/api.ts) reads
/// that flag to route EVERY subsequent request from that browser to the
/// owner's "personal" tenant — exposing the owner's real projects,
/// deployments, and billing data to that caller. Likewise, an org claimed in
/// `req.org` is only honored if it matches the caller's own verified tenant
/// (already Clerk-membership-checked at JWT-mint time in `/api/token`), so a
/// caller can't index themselves into (or read as) an org they don't belong
/// to. Unenforced (dev/local, no `HIVE_JWT_SECRET`) mode keeps the old
/// body-trusting behavior — there is no tenant boundary to protect there.
/// Pure decision core of `identity_sync` (no request/state deps, unit-testable):
/// given the caller's OWN verified claims (if any), the enforcement mode, the
/// org they're claiming (if any), and their raw user id, resolve `(tenant,
/// org_slug_to_index, is_owner)`. `body_email_is_owner` is ONLY consulted in
/// the unenforced-dev fallback (`auth_is_owner: None, enforced: false`) — it
/// must never be trusted when `enforced` is true.
fn resolve_identity_sync(
    enforced: bool,
    auth_tenant: Option<&str>,
    auth_is_owner: Option<bool>,
    claimed_org_slug: Option<&str>,
    user_id: &str,
    body_email_is_owner: bool,
) -> (String, Option<String>, bool) {
    let (tenant, org_slug) = match claimed_org_slug {
        Some(slug) => {
            if enforced && auth_tenant != Some(slug) {
                // Authenticated as a DIFFERENT tenant than the org claimed here —
                // never index (or report) an org the caller isn't verified to
                // belong to. Fall back to their own real tenant.
                (auth_tenant.map(str::to_string).unwrap_or_else(|| ANON_TENANT.to_string()), None)
            } else {
                (slug.to_string(), Some(slug.to_string()))
            }
        }
        None => match auth_is_owner {
            // Personal scope: the OWNER keeps the legacy "personal" namespace;
            // every other verified caller is isolated under their own `u_<uid>`
            // — never the shared literal "personal".
            Some(true) => ("personal".to_string(), None),
            Some(false) => (format!("u_{user_id}"), None),
            None if !enforced => ("personal".to_string(), None), // dev/unenforced fallback
            None => (ANON_TENANT.to_string(), None),
        },
    };
    let is_owner = match auth_is_owner {
        Some(b) => b,
        None if !enforced => body_email_is_owner,
        None => false,
    };
    (tenant, org_slug, is_owner)
}

async fn identity_sync(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<IdentitySyncReq>,
) -> Json<Value> {
    let enforced = crate::auth::enforced();
    let verified = claims.as_ref().map(|e| &e.0);
    let auth_tenant: Option<String> = verified.map(|cl| norm(&cl.tenant).to_string());
    let auth_is_owner: Option<bool> = verified.map(|cl| cl.platform_admin);
    let claimed_org_slug = req.org.as_ref().map(|o| if o.slug.is_empty() { o.id.clone() } else { o.slug.clone() });
    let body_email_is_owner = !c.owner_email.trim().is_empty() && req.user.email.eq_ignore_ascii_case(c.owner_email.trim());

    let (tenant, org_slug, is_owner) = resolve_identity_sync(
        enforced,
        auth_tenant.as_deref(),
        auth_is_owner,
        claimed_org_slug.as_deref(),
        &req.user.id,
        body_email_is_owner,
    );
    if let (Some(slug), Some(o)) = (&org_slug, &req.org) {
        c.identity.upsert_org(&o.id, slug, &o.name, &o.image_url);
    }
    // Mirror VERIFIED org membership into the teams roster. Previously nothing
    // ever did: `team_create` seeds only the platform owner, so every Clerk org
    // member who signed in and used the team daily was invisible to
    // GET /v1/teams. `org_slug` is only Some when the caller's verified JWT
    // tenant equals the claimed org (Clerk-membership-checked at mint). Gated
    // on `x-hive-internal` so the email is exactly the Clerk-verified one the
    // server-side mint route posts — a browser-originated sync (no internal
    // header) can't write an arbitrary body email into its org's roster.
    if let Some(slug) = &org_slug {
        let email = req.user.email.trim();
        if !email.is_empty()
            && mint_allowed(&headers)
            && !email.eq_ignore_ascii_case(c.owner_email.trim())
        {
            if c.teams.get(slug).is_none() {
                let name = req
                    .org
                    .as_ref()
                    .map(|o| o.name.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(slug.as_str());
                let _ = c.teams.create_with_slug(slug, name, "pro", &c.owner_email);
            }
            // Add-if-absent only: `add_member` overwrites an existing member's
            // role, and a manually granted Owner/Admin must never be demoted
            // by a routine sync.
            let already = c.teams.get(slug).map(|t| t.member(email).is_some()).unwrap_or(false);
            if !already {
                let role = if verified.map(|cl| cl.role == "admin").unwrap_or(false) {
                    crate::teams::Role::Admin
                } else {
                    crate::teams::Role::Member
                };
                c.teams.add_member(slug, email, role);
            }
        }
    }
    c.identity.upsert_user(
        &req.user.id,
        &req.user.email,
        &req.user.name,
        &req.user.image_url,
        &tenant,
        org_slug.as_deref(),
    );
    crate::persist::persist(&c);
    Json(json!({ "ok": true, "tenant": tenant, "is_owner": is_owner }))
}

// ============================ Billing & compute credits ============================

/// The single node currently metering usage into `BillingStore` (mirrors
/// `spawn_billing_meter_loop`'s own election EXACTLY: the manual
/// `HIVE_BILLING_COORDINATOR_NODE` pin if set, else the control-plane leader) —
/// every OTHER node's local `BillingStore` is stale/empty for live reads (only
/// ever bootstrapped from a peer snapshot at boot, never kept live-current).
fn billing_authority_node(c: &Arc<CloudState>) -> String {
    std::env::var("HIVE_BILLING_COORDINATOR_NODE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| c.control_plane_leader())
}

/// Proxy a billing GET to the authority node when this node isn't it — fixes the
/// confirmed bug where `/v1/billing` answers diverge by node (live-witnessed: 5
/// distinct billing states, including a `plan` disagreement, across 8 nodes for
/// the SAME tenant). Falls back to serving this node's own (possibly stale)
/// local value if the proxy is unreachable, rather than erroring the page.
async fn proxy_billing_read(c: &Arc<CloudState>, path: &str, team: &str) -> Option<Value> {
    let authority = billing_authority_node(c);
    if authority == c.node_name {
        return None; // we ARE authoritative; caller serves its own local read
    }
    fetch_from_host(c, &authority, path, team).await
}

pub(crate) async fn billing_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(v) = proxy_billing_read(&c, "/v1/billing", &t).await {
        return Json(v);
    }
    let acc = c.billing.account(&t);
    let plan = acc.plan.clone();
    // Live quota usage (business-locking gauges) + the metered rate card + this
    // period's draft invoice, so the UI shows limits/usage/invoicing in one place.
    let projects_used = c.projects.count_for_team(&t) as u32;
    let members_used = c.teams.get(&t).map(|team| team.members.len() as u32).unwrap_or(0);
    Json(json!({
        "account": acc,
        "plans": crate::billing::PLANS,
        "stripe": crate::billing::stripe_configured(),
        "rate_card": crate::billing::RATE_CARD,
        "limits": {
            "max_projects": crate::billing::plan_max_projects(&plan),
            "max_members": crate::billing::plan_max_members(&plan),
            "max_duration_secs": crate::billing::plan_max_duration_secs(&plan),
            "allows_failover": crate::billing::plan_allows_failover(&plan),
            "allows_sso": crate::billing::plan_allows_sso(&plan),
            "projects_used": projects_used,
            "members_used": members_used,
            "can_deploy": c.billing.can_deploy(&t).is_ok(),
        },
        "current_invoice": c.billing.current_invoice(&t),
    }))
}

/// Invoices for the tenant (finalized periods + current draft, newest first).
/// Prefers the LOCAL fleet-replicated relational mirror (fastest — no network
/// hop, and correct even if the billing leader is briefly unreachable),
/// falling back to the HTTP proxy-to-leader, falling back to this node's own
/// (possibly stale) local BillingStore.
pub(crate) async fn billing_invoices(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some((_, _, Some(invoices_json))) = crate::relational::billing_snapshot(&t).await {
        if let Ok(v) = serde_json::from_str::<Value>(&invoices_json) {
            return Json(v);
        }
    }
    if let Some(v) = proxy_billing_read(&c, "/v1/billing/invoices", &t).await {
        return Json(v);
    }
    Json(json!(c.billing.invoices(&t)))
}

pub(crate) async fn billing_ledger(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some((_, Some(ledger_json), _)) = crate::relational::billing_snapshot(&t).await {
        if let Ok(v) = serde_json::from_str::<Value>(&ledger_json) {
            return Json(v);
        }
    }
    if let Some(v) = proxy_billing_read(&c, "/v1/billing/ledger", &t).await {
        return Json(v);
    }
    Json(json!(c.billing.ledger(&t)))
}

#[derive(Deserialize)]
struct CheckoutReq {
    /// "plan" | "credits"
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    plan: Option<String>,
    /// For credit top-ups, the amount in cents.
    #[serde(default)]
    amount_cents: Option<u64>,
}
fn default_kind() -> String {
    "plan".into()
}

async fn billing_checkout(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Json(req): Json<CheckoutReq>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let (plan, amount, label, price_id) = if req.kind == "credits" {
        let amt = req.amount_cents.unwrap_or(1000);
        ("".to_string(), amt, format!("OpenEdge credits (${:.2})", amt as f64 / 100.0), None)
    } else {
        let plan = req.plan.unwrap_or_else(|| "pro".into());
        let spec = crate::billing::plan_spec(&plan);
        (plan, spec.price_cents, format!("OpenEdge {} plan", spec.name), spec.stripe_price_id)
    };
    // A free plan (Hobby, or any future $0 tier) has nothing to charge —
    // Stripe Checkout rejects a $0 payment/subscription outright, and there's
    // no reason to round-trip it at all. Apply immediately, same as the mock
    // path always did for this case.
    if amount == 0 {
        let acc = c.billing.set_plan(&t, &plan);
        c.audit.record(&t, "user", "plan_change", "billing", &plan, "switched to a free plan (no checkout needed)");
        crate::persist::persist(&c);
        return Json(json!({ "url": "", "mock": false, "applied": true, "account": acc }));
    }
    let co = c.billing.open_checkout(&t, &req.kind, &plan, amount);

    // Real Stripe Checkout when configured; otherwise the local mock checkout.
    if crate::billing::stripe_configured() {
        let base = std::env::var("PUBLIC_URL").unwrap_or_else(|_| "http://localhost:3000".into());
        let success = format!("{base}/billing?success={}", co.id);
        let cancel = format!("{base}/billing?canceled=1");
        match crate::billing::stripe_checkout(&c.http, price_id, amount, &label, &success, &cancel, &co.id).await {
            Ok((url, stripe_session_id)) => {
                c.billing.attach_stripe_session(&co.id, &stripe_session_id);
                return Json(json!({ "url": url, "mock": false, "session": co.id }));
            }
            Err(e) => tracing::warn!(error=%e, "stripe checkout failed; falling back to mock"),
        }
    }
    Json(json!({ "url": format!("/billing/checkout?session={}", co.id), "mock": true, "session": co.id }))
}

async fn billing_checkout_get(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    c.billing.get_checkout(&id).map(|co| Json(json!(co))).ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct ConfirmReq {
    session: String,
}

async fn billing_confirm(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Json(req): Json<ConfirmReq>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let co = c.billing.get_checkout(&req.session).ok_or(StatusCode::NOT_FOUND)?;
    // A checkout may only be confirmed by the SAME tenant that opened it —
    // without this, any authenticated caller who learns/guesses another
    // tenant's checkout id could force-complete or grief it.
    if norm(&co.tenant) != t {
        return Err(StatusCode::FORBIDDEN);
    }
    // When this was a REAL Stripe checkout, verify actual payment with Stripe
    // before applying anything — the client hitting this endpoint (a redirect
    // back from Stripe, or a direct call) is NOT proof of payment on its own;
    // without this check, anyone could open a checkout and immediately call
    // confirm without ever paying, and receive the plan/credits for free.
    let mut stripe_customer = String::new();
    let mut stripe_subscription = String::new();
    if !co.stripe_session_id.is_empty() {
        let status = crate::billing::stripe_verify_session(&c.http, &co.stripe_session_id)
            .await
            .map_err(|e| { tracing::warn!(error=%e, session=%co.stripe_session_id, "stripe session verification failed"); StatusCode::BAD_GATEWAY })?;
        if !status.paid {
            return Err(StatusCode::PAYMENT_REQUIRED);
        }
        stripe_customer = status.customer.unwrap_or_default();
        stripe_subscription = status.subscription.unwrap_or_default();
    }
    let (co, acc) = c.billing.confirm_checkout(&req.session).ok_or(StatusCode::NOT_FOUND)?;
    if !stripe_customer.is_empty() || !stripe_subscription.is_empty() {
        c.billing.set_stripe_ids(&co.tenant, &stripe_customer, &stripe_subscription);
    }
    c.audit.record(&t, "user", "charge", "billing", &co.id, &format!("checkout {} {} ${:.2}", co.kind, co.plan, co.amount_cents as f64 / 100.0));
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true, "account": acc })))
}

/// Stripe's server-to-server notification of billing events — the
/// authoritative sync path (unlike `billing_confirm`, which only fires when a
/// user's browser happens to be present for the redirect back). Handles the
/// checkout completing (in case the user closes the tab before that redirect)
/// and subscription cancellation/renewal so a tenant's plan stays correct even
/// when nobody is watching.
///
/// Auth: this route is in `auth::require_auth`'s `open` allowlist (Stripe
/// can't present a platform JWT — it's authenticated by its own HMAC
/// signature, verified below). Deliberately FAILS CLOSED when
/// `STRIPE_WEBHOOK_SECRET` is unset — unlike `git_webhook`'s dev-open
/// default, an unverifiable delivery must never be allowed to mutate a
/// tenant's billing state.
async fn billing_webhook(State(c): State<Arc<CloudState>>, headers: HeaderMap, body: axum::body::Bytes) -> Result<StatusCode, StatusCode> {
    let secret = std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default();
    if secret.is_empty() {
        tracing::warn!("stripe webhook received but STRIPE_WEBHOOK_SECRET is not configured — rejecting (fail closed)");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let sig = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !crate::billing::verify_webhook_signature(&body, sig, &secret) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let obj = v.get("data").and_then(|d| d.get("object")).cloned().unwrap_or_else(|| json!({}));

    match event_type {
        // Primary confirmation path — mirrors `billing_confirm`'s apply logic,
        // keyed by the `checkout_id` we stamped into the session's metadata
        // (see `stripe_checkout`) rather than a tenant header, since Stripe is
        // the caller here, not an authenticated user.
        "checkout.session.completed" => {
            let checkout_id = obj.get("metadata").and_then(|m| m.get("checkout_id")).and_then(|s| s.as_str()).unwrap_or("");
            if !checkout_id.is_empty() {
                if let Some((co, _acc)) = c.billing.confirm_checkout(checkout_id) {
                    let customer = obj.get("customer").and_then(|s| s.as_str()).unwrap_or("");
                    let subscription = obj.get("subscription").and_then(|s| s.as_str()).unwrap_or("");
                    if !customer.is_empty() || !subscription.is_empty() {
                        c.billing.set_stripe_ids(&co.tenant, customer, subscription);
                    }
                    c.audit.record(&co.tenant, "system", "charge", "billing", &co.id, "stripe webhook: checkout.session.completed");
                    crate::persist::persist(&c);
                }
                // Already confirmed via `billing_confirm` (the common case,
                // since the user's browser is usually still present) —
                // `confirm_checkout` is idempotent (removes on first success),
                // so a `None` here just means nothing left to do.
            }
        }
        // Cancellation (immediate or end-of-period, depending on Stripe
        // dashboard/API settings) — downgrade to Hobby so quotas/business-locks
        // re-engage even if nobody visits the dashboard again.
        "customer.subscription.deleted" => {
            let sub_id = obj.get("id").and_then(|s| s.as_str()).unwrap_or("");
            if let Some(tenant) = (!sub_id.is_empty()).then(|| c.billing.tenant_for_subscription(sub_id)).flatten() {
                c.billing.set_plan(&tenant, "hobby");
                c.audit.record(&tenant, "system", "plan_change", "billing", sub_id, "stripe webhook: subscription canceled -> downgraded to hobby");
                crate::persist::persist(&c);
            }
        }
        // A renewal failure (card declined etc.) surfaces as the subscription
        // moving to "past_due"/"unpaid" — logged for now (no automatic
        // downgrade on a transient failure; Stripe's own retry schedule and
        // dunning emails run first, and `customer.subscription.deleted` is
        // still the terminal signal once retries are exhausted).
        "customer.subscription.updated" => {
            let sub_id = obj.get("id").and_then(|s| s.as_str()).unwrap_or("");
            let status = obj.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if let Some(tenant) = (!sub_id.is_empty()).then(|| c.billing.tenant_for_subscription(sub_id)).flatten() {
                tracing::info!(%tenant, %sub_id, %status, "stripe webhook: subscription updated");
            }
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct ChargeReq {
    cents: u64,
    #[serde(default)]
    note: String,
}

async fn billing_charge(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Option<axum::Extension<crate::auth::Claims>>,Json(req): Json<ChargeReq>) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let note = if req.note.is_empty() { "Compute usage".to_string() } else { req.note.clone() };
    match c.billing.charge(&t, req.cents, &note) {
        Ok(acc) => {
            c.audit.record(&t, "system", "charge", "billing", "compute", &format!("{} ¢ — {}", req.cents, note));
            crate::persist::persist(&c);
            Ok(Json(json!({ "ok": true, "account": acc })))
        }
        Err(e) => Err((StatusCode::PAYMENT_REQUIRED, e)),
    }
}

// ============================ Ops data browser ============================
//
// Exposes the platform's own data — the persisted PlatformSnapshot collections
// plus live in-memory stores — so the owner can query/view/explore the state
// that rides the iroh/guardian-db replication mesh.

/// All collections the platform stores, with row data. Ordered for the UI.
fn all_collections(c: &Arc<CloudState>) -> Vec<(&'static str, Vec<Value>)> {
    let mut projects: Vec<Value> = c
        .projects
        .snapshot()
        .into_iter()
        .map(|(k, v)| json!({ "project": k, "team": v.team, "domains": v.domains, "env_count": v.env.len(), "build": v.build, "functions": v.functions, "preview_protection": v.preview_protection }))
        .collect();
    projects.sort_by(|a, b| a["project"].as_str().unwrap_or("").cmp(b["project"].as_str().unwrap_or("")));

    let wf_runs: Vec<Value> = c.workflows.runs().into_iter().map(|r| json!(r)).collect();
    let wf_defs: Vec<Value> = c.workflows.defs().into_iter().map(|d| json!(d)).collect();

    vec![
        // Fleet-aggregated: this operator data browser runs on ONE node, but the
        // placement scheduler hosts a project's deployments on peer nodes, so a
        // bare `c.gw.list()` (locally-hosted only) showed "0 deployments" here
        // while every deployment lived on a peer. Merge the gossiped
        // peer_deployments (deduped by id) exactly as the tenant-facing
        // `dep_list` does — the ops view spans ALL tenants (operator-only).
        ("deployments", fleet_deployments_all(c)),
        ("projects", projects),
        ("orgs", c.identity.orgs().into_iter().map(|o| json!(o)).collect()),
        ("users", c.identity.users().into_iter().map(|u| json!(u)).collect()),
        // MASKED — this feeds the ops-console data browser; even an operator
        // should not see raw credentials here (a real "reveal" affordance, if
        // ever added, should be its own explicit, individually-audited action).
        ("databases", c.databases.list(None).into_iter().map(|d| json!(d)).collect()),
        ("domains", c.domains.snapshot().into_iter().map(|d| json!(d)).collect()),
        ("secure_links", c.securelinks.all().into_iter().map(|l| json!(l)).collect()),
        ("api_keys", c.apikeys.snapshot().into_iter().map(|k| json!(k)).collect()),
        ("builds", c.builds.list().into_iter().map(|b| json!({ "id": b.id, "project": b.project, "state": b.state, "branch": b.branch, "commit": b.commit, "commit_message": b.commit_message, "alias": b.alias, "started_ms": b.started_ms, "finished_ms": b.finished_ms, "log_lines": b.lines.len() })).collect()),
        ("workflow_defs", wf_defs),
        ("workflow_runs", wf_runs),
        ("incidents", c.incidents.snapshot().into_iter().map(|i| json!(i)).collect()),
        ("webhooks", c.webhooks.snapshot().into_iter().map(|w| json!(w)).collect()),
        ("billing", c.billing.all_accounts().into_iter().map(|a| json!(a)).collect()),
        ("billing_ledger", c.billing.snapshot().1.into_iter().map(|l| json!(l)).collect()),
        ("audit_log", c.audit.recent(2000, None).into_iter().map(|a| json!(a)).collect()),
        ("events", c.recent_events(200).into_iter().map(|e| json!(e)).collect()),
    ]
}

/// The tenant namespaces the platform store is partitioned into, with per-record
/// counts — the multi-tenant schema of the guardian-db / snapshot.
async fn data_namespaces(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let snap = crate::persist::capture(&c);
    let docs = crate::persist::namespaced(&snap);
    let rows: Vec<Value> = docs
        .into_iter()
        .map(|(ns, doc)| {
            let count = |k: &str| doc.get(k).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            json!({
                "namespace": ns,
                "projects": count("projects"),
                "deployments": count("deployments"),
                "databases": count("databases"),
                "api_keys": count("api_keys"),
                "webhooks": count("webhooks"),
            })
        })
        .collect();
    Ok(Json(json!({ "namespaces": rows })))
}

/// Live status of the always-on GuardianDB durable store: the keys currently
/// held in the iroh-docs `hive-state` KV (one per tenant namespace) plus a
/// content sample proving data round-trips through the replicated store — not a
/// mock. `online` is true once the iroh-backed store has opened.
async fn guardian_status(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let mut keys = crate::guardian::keys().await;
    keys.sort();
    // Round-trip the first key to prove reads come back from GuardianDB itself.
    let sample = match keys.first() {
        Some(k) => crate::guardian::get(k)
            .await
            .map(|b| json!({ "key": k, "bytes": b.len() }))
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    Ok(Json(json!({
        "store": "guardian-db",
        "engine": "iroh-docs (BLAKE3 · QUIC · Willow reconciliation)",
        "kv": "hive-state",
        "online": !keys.is_empty(),
        "key_count": keys.len(),
        "keys": keys,
        "sample": sample,
    })))
}

async fn data_collections(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let mut cols: Vec<Value> = all_collections(&c)
        .into_iter()
        .map(|(name, rows)| json!({ "name": name, "count": rows.len(), "editable": false }))
        .collect();
    // Editable document collections (full CRUD).
    for (name, count) in c.docs.collections() {
        cols.push(json!({ "name": name, "count": count, "editable": true }));
    }
    Ok(Json(json!({ "collections": cols, "store": "guardian-db (iroh) · local snapshot" })))
}

#[derive(Deserialize)]
struct DataQ {
    q: Option<String>,
    limit: Option<usize>,
}

fn doc_rows(c: &Arc<CloudState>, collection: &str) -> Option<Vec<Value>> {
    let docs = c.docs.list(collection);
    if docs.is_empty() {
        return None;
    }
    Some(docs.into_iter().map(|d| json!(d)).collect())
}

async fn data_rows(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(collection): Path<String>,
    Query(q): Query<DataQ>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let typed = all_collections(&c).into_iter().find(|(name, _)| *name == collection).map(|(_, r)| r);
    let editable = typed.is_none();
    let rows = typed.or_else(|| doc_rows(&c, &collection)).ok_or((StatusCode::NOT_FOUND, "no such collection".into()))?;
    let needle = q.q.unwrap_or_default().to_lowercase();
    let total = rows.len();
    let mut filtered: Vec<Value> = rows
        .into_iter()
        .filter(|r| needle.is_empty() || r.to_string().to_lowercase().contains(&needle))
        .collect();
    let matched = filtered.len();
    let limit = q.limit.unwrap_or(200).min(2000);
    filtered.truncate(limit);
    Ok(Json(json!({ "collection": collection, "total": total, "matched": matched, "rows": filtered, "editable": editable })))
}

/// Create a document in an editable collection (DocStore).
async fn data_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(collection): Path<String>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Don't let custom docs shadow a typed platform collection.
    if all_collections(&c).iter().any(|(n, _)| *n == collection) {
        return Err((StatusCode::CONFLICT, format!("'{collection}' is a managed collection — create custom docs in a new collection name")));
    }
    let doc = c.docs.create(&collection, &t, body);
    c.audit.record(&t, "user", "create", "document", &doc.id, &collection);
    crate::persist::persist(&c);
    Ok(Json(json!(doc)))
}

/// Patch a document by id (editable collections only).
async fn data_patch(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((collection, id)): Path<(String, String)>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let doc = c.docs.patch(&id, body).ok_or((StatusCode::NOT_FOUND, "no such document".into()))?;
    c.audit.record(&t, "user", "update", "document", &id, &collection);
    crate::persist::persist(&c);
    Ok(Json(json!(doc)))
}

/// Delete a row: a custom document, or a typed entry via its owning store.
async fn data_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((collection, id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.docs.delete(&id) {
        c.audit.record(&t, "user", "delete", "document", &id, &collection);
        crate::persist::persist(&c);
        return Ok(Json(json!({ "deleted": id })));
    }
    let ok = match collection.as_str() {
        "deployments" => c.gw.remove(&id).await.is_some(),
        "databases" => {
            // Use the RECORD's own owning team (not the operator's `t`) so the
            // queue/vector namespace purge targets the correct tenant.
            let owner = c.databases.get_raw(&id).map(|d| d.team).unwrap_or_else(|| t.clone());
            c.databases.remove_db_and_purge_data(&id, &owner);
            true
        }
        "secure_links" => { c.securelinks.remove(&id); true }
        "webhooks" => { c.webhooks.remove(&id); true }
        _ => return Err((StatusCode::BAD_REQUEST, format!("'{collection}' rows are managed and can't be deleted here"))),
    };
    if !ok {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    c.audit.record(&t, "user", "delete", &collection, &id, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "deleted": id })))
}

// ============================ Data Browser: "view as PostgreSQL" ============================
// An alternative, READ-ONLY view of the relational mirror built on guardian-db's
// native SQL layer (crates/hive-cloud/src/relational.rs) — real typed tables
// (project_teams, billing_*) instead of the document-collection JSON browsing
// above. Deliberately a separate, narrower surface: no PUT/POST/DELETE here at
// all (see relational::reject_unless_readonly) — mutations to this data still
// only ever happen through the normal typed stores (ProjectStore/BillingStore),
// never through this admin query box.

/// List the known relational tables + their columns.
async fn sql_tables(claims: Option<axum::Extension<crate::auth::Claims>>) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let tables: Vec<Value> = crate::relational::known_tables()
        .into_iter()
        .map(|t| json!({ "name": t.name, "columns": t.columns.into_iter().map(|c| json!({ "name": c.name, "type": c.ty })).collect::<Vec<_>>() }))
        .collect();
    Ok(Json(json!({ "tables": tables })))
}

#[derive(Deserialize)]
struct SqlQueryBody {
    sql: Option<String>,
    table: Option<String>,
}

/// Run a read-only query against the relational mirror: either a caller-typed
/// `sql` (SELECT/read-only WITH only — enforced in relational.rs, not just
/// here), or a `table` name for the default `SELECT * FROM <table> LIMIT 200`
/// the table-browser view uses. Exactly one of the two is expected.
async fn sql_query(
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(body): Json<SqlQueryBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let sql = match (body.sql.as_deref().map(str::trim), body.table.as_deref().map(str::trim)) {
        (Some(s), _) if !s.is_empty() => s.to_string(),
        (_, Some(t)) if !t.is_empty() => {
            if !crate::relational::known_tables().iter().any(|k| k.name == t) {
                return Err((StatusCode::BAD_REQUEST, format!("unknown table '{t}'")));
            }
            format!("SELECT * FROM {t} LIMIT 200")
        }
        _ => return Err((StatusCode::BAD_REQUEST, "provide 'sql' or 'table'".into())),
    };
    crate::relational::run_readonly_query(&sql).await.map(Json).map_err(|e| (StatusCode::BAD_REQUEST, e))
}

// ============================ Deployment preview / thumbnail ============================

/// Preview metadata for a project's production deployment: a site thumbnail for
/// frontends, or the JSON/text body for backend services.
async fn project_preview(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Json<Value> {
    // Don't render another tenant's app content as a preview.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!({ "kind": "none" }));
    }
    let dep = c.gw.list().into_iter().find(|d| d.project == project && d.production)
        .or_else(|| c.gw.list().into_iter().find(|d| d.project == project));
    let Some(dep) = dep else {
        return Json(json!({ "kind": "none" }));
    };
    let alias = dep.alias.clone();
    let is_frontend = dep.kind == "static" || dep.kind == "fullstack";
    if is_frontend {
        return Json(json!({
            "kind": "image",
            "url": format!("/v1/projects/{}/thumbnail", urlencode(&project)),
            "alias": alias,
        }));
    }
    // Backend service: fetch "/" through the gateway and return the body.
    let base = c.public_base.clone();
    let resp = c.http.get(format!("{base}/")).header("host", &alias).send().await;
    match resp {
        Ok(r) => {
            let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            let trimmed: String = body.chars().take(4000).collect();
            let kind = if ct.contains("json") || trimmed.trim_start().starts_with('{') || trimmed.trim_start().starts_with('[') {
                "json"
            } else {
                "text"
            };
            Json(json!({ "kind": kind, "status": status, "content_type": ct, "body": trimmed, "alias": alias }))
        }
        Err(e) => Json(json!({ "kind": "text", "body": format!("(no response: {e})"), "alias": alias })),
    }
}

pub(crate) fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') { c.to_string() } else { format!("%{:02X}", c as u32) })
        .collect()
}

/// A PNG thumbnail of the deployed site, captured with headless Chrome and
/// cached per deployment. Falls back to a generated SVG card if capture fails.
async fn project_thumbnail(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Don't render another tenant's app content as a preview — mirrors the
    // check `project_preview` already performs; this handler independently
    // captures/serves the actual image bytes and previously had no check at
    // all (also bypassing `preview_protection` for a password-gated project).
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return (StatusCode::NOT_FOUND, "no deployment").into_response();
    }
    let dep = c.gw.list().into_iter().find(|d| d.project == project && d.production)
        .or_else(|| c.gw.list().into_iter().find(|d| d.project == project));
    let Some(dep) = dep else {
        return (StatusCode::NOT_FOUND, "no deployment").into_response();
    };
    let cache = crate::persist::data_dir().join("thumbnails");
    let _ = tokio::fs::create_dir_all(&cache).await;
    let png = cache.join(format!("{}.png", dep.id.as_str()));

    if !png.exists() {
        let _ = capture_thumbnail(&dep.alias, &png).await;
    }
    if let Ok(bytes) = tokio::fs::read(&png).await {
        return ([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "public, max-age=60")], bytes).into_response();
    }
    // Fallback: a simple SVG placeholder card.
    let svg = thumbnail_placeholder(&project);
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
}

/// Capture a screenshot of `http://<alias>:<gateway-port>/` with headless Chrome.
async fn capture_thumbnail(alias: &str, out: &std::path::Path) -> anyhow::Result<()> {
    // The gateway serves on the public port; derive it from public_base later if
    // needed — default 8787 for the local mesh.
    let port = std::env::var("HIVE_PUBLIC_PORT").unwrap_or_else(|_| "8787".into());
    let url = format!("http://{alias}:{port}/");
    let chrome = chrome_path();
    let status = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(&chrome)
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-sandbox")
            .arg("--hide-scrollbars")
            .arg("--window-size=1280,800")
            .arg(format!("--screenshot={}", out.to_string_lossy()))
            .arg(&url)
            .output(),
    )
    .await??;
    anyhow::ensure!(status.status.success() && out.exists(), "chrome screenshot failed");
    Ok(())
}

fn chrome_path() -> String {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        return p;
    }
    for p in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
    ] {
        if std::path::Path::new(p).exists() {
            return p.to_string();
        }
    }
    "google-chrome".into()
}

fn thumbnail_placeholder(project: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="800" viewBox="0 0 1280 800">
  <rect width="1280" height="800" fill="#0a0a0a"/>
  <polygon points="640,330 700,430 580,430" fill="#ededed"/>
  <text x="640" y="500" fill="#ededed" font-family="sans-serif" font-size="40" text-anchor="middle">{project}</text>
  <text x="640" y="548" fill="#888" font-family="sans-serif" font-size="22" text-anchor="middle">Deployed on OpenEdge</text>
</svg>"##
    )
}

#[cfg(test)]
mod identity_sync_tests {
    use super::resolve_identity_sync;

    /// REGRESSION TEST for a real, confirmed cross-tenant data leak: the
    /// original `identity_sync` derived `is_owner` from the REQUEST BODY's
    /// `user.email`, trusted outright regardless of who the caller was
    /// actually authenticated as. Any authenticated user — including one from
    /// a completely different, unprivileged tenant — could claim the owner's
    /// email in the body and get `is_owner: true` back; the dashboard's own
    /// client then persisted that in ITS OWN browser's localStorage, routing
    /// every subsequent request from that browser to the owner's "personal"
    /// tenant. This is the confirmed root cause of "dylanwong007@gmail.com's
    /// hobby-plan projects appearing under other accounts/orgs" — the fix
    /// derives `is_owner`/tenant from the caller's OWN verified JWT claim,
    /// never the body, once enforced.
    #[test]
    fn enforced_mode_never_trusts_a_forged_owner_email_in_the_body() {
        // The attack: authenticated as "thoth-division" (a real, unprivileged
        // tenant), claiming the owner's email in the body, no org claimed
        // (personal-scope request) — exactly the shape the exploit sends.
        let (tenant, org_slug, is_owner) = resolve_identity_sync(
            /* enforced */ true,
            /* auth_tenant */ Some("thoth-division"),
            /* auth_is_owner (the REAL, server-verified value for this caller) */ Some(false),
            /* claimed_org_slug */ None,
            /* user_id */ "user_thoth_attacker",
            /* body_email_is_owner (forged — irrelevant once enforced) */ true,
        );
        assert!(!is_owner, "a non-owner caller must never be granted is_owner, no matter what the body claims");
        assert_eq!(tenant, "u_user_thoth_attacker", "must be isolated under their own per-user namespace");
        assert_eq!(org_slug, None);
        assert_ne!(tenant, "personal", "must never land in the owner's shared namespace");
    }

    #[test]
    fn enforced_mode_grants_owner_only_via_the_verified_claim() {
        let (tenant, _org, is_owner) = resolve_identity_sync(true, Some("personal"), Some(true), None, "user_real_owner", false);
        assert!(is_owner);
        assert_eq!(tenant, "personal");
    }

    #[test]
    fn enforced_mode_rejects_claiming_an_org_the_caller_is_not_verified_for() {
        // Authenticated as "acme" but the request body claims org "thoth-division"
        // (e.g. a stale/forged client payload) — must fall back to the caller's
        // OWN real tenant, never index or report the claimed org.
        let (tenant, org_slug, _owner) = resolve_identity_sync(true, Some("acme"), Some(false), Some("thoth-division"), "user_x", false);
        assert_eq!(tenant, "acme");
        assert_eq!(org_slug, None, "an unverified org claim must never be indexed");
    }

    #[test]
    fn enforced_mode_honors_an_org_claim_matching_the_verified_tenant() {
        let (tenant, org_slug, _owner) = resolve_identity_sync(true, Some("thoth-division"), Some(false), Some("thoth-division"), "user_x", false);
        assert_eq!(tenant, "thoth-division");
        assert_eq!(org_slug.as_deref(), Some("thoth-division"));
    }

    #[test]
    fn enforced_mode_with_no_claims_never_falls_back_to_personal() {
        // Should be unreachable in practice (require_auth already rejects an
        // unauthenticated mutation before this function runs), but the pure
        // function must fail safe on its own regardless.
        let (tenant, org_slug, is_owner) = resolve_identity_sync(true, None, None, None, "user_x", true);
        assert_ne!(tenant, "personal");
        assert!(!is_owner);
        assert_eq!(org_slug, None);
    }

    #[test]
    fn unenforced_dev_mode_keeps_the_legacy_body_trusting_fallback() {
        // No JWT auth configured at all (local/dev) — no tenant boundary exists
        // to protect, so the historical behavior (trust the body) is fine and
        // intentionally preserved for zero-friction local development.
        let (tenant, _org, is_owner) = resolve_identity_sync(false, None, None, None, "user_x", true);
        assert!(is_owner);
        assert_eq!(tenant, "personal");

        let (tenant2, _org2, is_owner2) = resolve_identity_sync(false, None, None, None, "user_x", false);
        assert!(!is_owner2);
        assert_eq!(tenant2, "personal", "unenforced personal-scope default is still literal \"personal\" (pre-existing dev behavior)");
    }
}

#[cfg(test)]
mod dns_import_tests {
    use super::parse_zone;

    #[test]
    fn parses_bind_style_records() {
        let zone = r#"
; example zone
@        3600  IN  A      76.76.21.21
www      3600  IN  CNAME  app.example.com.
@              IN  MX     10 mail.example.com.
mail           IN  A      1.2.3.4
@        3600  IN  TXT    "v=spf1 include:_spf.example.com ~all"
sub      A      9.9.9.9
"#;
        let recs = parse_zone(zone, "example.com");
        // apex A
        assert!(recs.iter().any(|r| r.kind == "A" && r.name.is_empty() && r.value == "76.76.21.21"));
        // www CNAME (trailing dot stripped)
        assert!(recs.iter().any(|r| r.kind == "CNAME" && r.name == "www" && r.value == "app.example.com"));
        // MX with priority
        let mx = recs.iter().find(|r| r.kind == "MX").expect("mx");
        assert_eq!(mx.priority, Some(10));
        assert_eq!(mx.value, "mail.example.com");
        // TXT keeps content (quotes stripped)
        assert!(recs.iter().any(|r| r.kind == "TXT" && r.value.contains("v=spf1")));
        // minimal "name TYPE value" form
        assert!(recs.iter().any(|r| r.kind == "A" && r.name == "sub" && r.value == "9.9.9.9"));
    }

    #[test]
    fn skips_blank_and_comment_and_unknown_lines() {
        let zone = "; just a comment\n\nfoo bar baz\n@ IN A 1.1.1.1\n";
        let recs = parse_zone(zone, "x.com");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].value, "1.1.1.1");
    }
}

#[cfg(test)]
mod tenant_isolation_tests {
    use super::resolve_tenant;
    use crate::auth::Claims;

    // The exact struct `auth::verify()` returns for a valid JWT. We build it
    // directly rather than via issue()/verify() so the test stays deterministic:
    // those touch the process-global `HIVE_JWT_SECRET`, which one git.rs test
    // already "owns" — a second env-mutating test would race it. The JWT *plumbing*
    // (require_auth -> extensions -> tenant()) is what wires this into requests;
    // here we assert the resolution PRIORITY that closes the spoof bypass.
    fn claims_for(tenant: &str) -> Claims {
        Claims {
            sub: "user-1".into(),
            tenant: tenant.into(),
            role: "owner".into(),
            iat: 0,
            exp: 0,
            platform_admin: false,
        }
    }

    #[test]
    fn jwt_tenant_claim_beats_spoofed_header() {
        // Valid JWT for team-a; request carries a spoofed `x-hive-team: team-b`.
        let claims = claims_for("team-a");
        let resolved = resolve_tenant(
            Some(&claims),
            None,                       // no API key
            Some("team-b".to_string()), // spoofed header — must be ignored
            true,                       // auth enforced
        );
        assert_eq!(resolved, "team-a", "JWT claim must win over x-hive-team header");
    }

    #[test]
    fn dev_mode_falls_back_to_header_without_jwt() {
        // No JWT + not enforced (HIVE_JWT_SECRET unset) → the dashboard's header
        // is honored so unauthenticated local dev still works.
        let resolved = resolve_tenant(
            None,                       // no JWT claims
            None,                       // no API key
            Some("team-b".to_string()),
            false, // dev mode (not enforced)
        );
        assert_eq!(resolved, "team-b", "dev mode trusts x-hive-team");
    }

    #[test]
    fn enforced_without_jwt_is_anon_not_owner() {
        // Enforced + no JWT + no API key: the spoofable header is NOT trusted, AND
        // it does NOT fall back to "personal" (the owner's namespace) — an
        // unauthenticated read must own NOTHING, never leak the owner's data.
        let resolved = resolve_tenant(None, None, Some("team-b".to_string()), true);
        assert_eq!(resolved, super::ANON_TENANT, "unauthenticated enforced request is anonymous, not the owner");
        assert_ne!(resolved, "personal", "must not default to the owner tenant");
    }

    #[test]
    fn dev_mode_default_is_personal_when_no_header() {
        // Dev (unenforced), no header → the single local user is the owner.
        assert_eq!(resolve_tenant(None, None, None, false), "personal");
    }

    #[test]
    fn api_key_team_used_when_no_jwt() {
        let resolved = resolve_tenant(None, Some("key-team".to_string()), Some("team-b".to_string()), true);
        assert_eq!(resolved, "key-team", "API key team wins over header");
    }

    // --- require_operator / platform_admin (privilege-escalation fix) ---

    #[test]
    fn tenant_owner_role_no_longer_grants_platform_operator_access() {
        // REGRESSION TEST for a real, confirmed vulnerability: every signed-up
        // user was minted `role: "owner"` for their OWN personal tenant, and
        // `require_operator` used to accept that role directly — letting any
        // customer reach global WAF/CDN/routing mutation endpoints. A tenant
        // "owner" (role="owner", platform_admin=false, the ordinary case for
        // every non-owner user) must now be REJECTED.
        let claims = Claims {
            sub: "user-1".into(),
            tenant: "some-customer-team".into(),
            role: "owner".into(),
            iat: 0,
            exp: 0,
            platform_admin: false,
        };
        assert!(
            !super::operator_allowed(Some(&claims), true),
            "a tenant-scoped \"owner\" role must NOT satisfy the platform-operator check"
        );
    }

    #[test]
    fn genuine_platform_admin_claim_grants_operator_access() {
        let claims = Claims {
            sub: "real-owner".into(),
            tenant: "personal".into(),
            role: "owner".into(),
            iat: 0,
            exp: 0,
            platform_admin: true,
        };
        assert!(super::operator_allowed(Some(&claims), true));
    }

    #[test]
    fn no_claims_denied_when_enforced_but_open_in_dev_mode() {
        assert!(!super::operator_allowed(None, true), "no claims + enforced must be denied");
        assert!(super::operator_allowed(None, false), "dev mode (unenforced) stays open");
    }

    #[test]
    fn ct_eq_matches_ordinary_equality_semantics() {
        assert!(super::ct_eq("same-secret", "same-secret"));
        assert!(!super::ct_eq("same-secret", "different"));
        assert!(!super::ct_eq("short", "shorter-value"));
        assert!(super::ct_eq("", ""));
    }
}

#[cfg(test)]
mod project_purge_tests {
    use super::*;

    /// REGRESSION TEST for a real, confirmed GDPR Art.17 gap: nothing ever
    /// removed a project's backing container volume — removing the container
    /// that mounted it leaves the NAMED volume (and the customer data inside
    /// it) on host disk forever. Real container-CLI calls (no mocking):
    /// creates two projects' volumes (one with a per-service suffix, matching
    /// a compose deployment), purges one, and confirms the OTHER project's
    /// volume — including one whose name is a superstring of the deleted
    /// project's — survives untouched. On macOS, creates the target's volumes
    /// in BOTH backends (podman + Apple `container`) to prove the dual-store
    /// sweep in `purge_project_podman_volumes` actually cleans both.
    #[tokio::test]
    async fn purge_project_podman_volumes_removes_only_the_target_project() {
        let path_env = std::env::var("PATH").unwrap_or_default();
        let backends: &[bool] = if hive_backend::container_cli::is_apple_default() { &[false, true] } else { &[false] };
        let mut usable: Vec<bool> = Vec::new();
        for &apple in backends {
            if hive_backend::container_cli::available(apple).await {
                usable.push(apple);
            } else {
                eprintln!("skipping backend apple={apple}: CLI not found/usable");
            }
        }
        if usable.is_empty() {
            eprintln!("skipping: no container CLI available");
            return;
        }

        let suffix = std::process::id();
        let target = format!("purgetest-{suffix}");
        // A different project whose name CONTINUES the same characters with
        // no delimiter — the exact false-positive case the exact-or-`-`-
        // suffix check exists to exclude (see the function's doc comment for
        // the case it does NOT cover: a different project literally named
        // `<target>-<anything>`).
        let other = format!("purgetest-{suffix}other");

        for &apple in &usable {
            let bin = hive_backend::container_cli::bin(apple);
            for name in [format!("hive-vol-{target}"), format!("hive-vol-{target}-worker"), format!("hive-vol-{other}")] {
                let _ = tokio::process::Command::new(bin).args(["volume", "create", &name]).output().await;
            }
        }

        purge_project_podman_volumes(&target).await;

        for &apple in &usable {
            let names = hive_backend::container_cli::list_volume_names(apple, &path_env).await;
            assert!(!names.contains(&format!("hive-vol-{target}")), "target project's base volume must be removed (apple={apple})");
            assert!(!names.contains(&format!("hive-vol-{target}-worker")), "target project's per-service volume must be removed (apple={apple})");
            assert!(names.contains(&format!("hive-vol-{other}")), "a DIFFERENT project whose name is a superstring must survive (apple={apple})");

            // Cleanup whatever's left (the `other` volume this test created).
            let bin = hive_backend::container_cli::bin(apple);
            let _ = tokio::process::Command::new(bin).args(hive_backend::container_cli::volume_rm_args(apple, &format!("hive-vol-{other}"))).output().await;
        }
    }

    #[tokio::test]
    async fn purge_project_source_dirs_removes_only_matching_checkouts() {
        // Real filesystem, real `deploy_root()` — confirms the synchronous
        // cleanup (added so a deleted project's source, which can carry
        // committed secrets/PII, doesn't linger for up to ~40 minutes waiting
        // on the periodic gc_build_dirs timer) removes the target project's
        // checkout dir(s) — including a `-building-<ms>` in-progress checkout,
        // the OTHER real naming shape this repo uses (git.rs:3391) — while
        // leaving an unrelated project's checkout untouched. Uses the same
        // `<project>-<stamp>` prefix convention as `newest_deploy_dir`
        // (git.rs) — like that existing function, this is a prefix match, not
        // a delimiter-exact one, which is a pre-existing, accepted property
        // of this convention (project names are allocated to be globally
        // unique, per the "Pick a globally-unique project name" comment on
        // git.rs's project-name allocator), not a new gap introduced here.
        let base = crate::git::deploy_root();
        tokio::fs::create_dir_all(&base).await.unwrap();
        let suffix = std::process::id();
        let project = format!("purge-src-{suffix}");
        let target_dir = base.join(format!("{project}-abc123"));
        let target_building_dir = base.join(format!("{project}-building-999999"));
        let unrelated_dir = base.join(format!("purge-src-unrelated-{suffix}-xyz789"));
        tokio::fs::create_dir_all(&target_dir).await.unwrap();
        tokio::fs::create_dir_all(&target_building_dir).await.unwrap();
        tokio::fs::create_dir_all(&unrelated_dir).await.unwrap();
        tokio::fs::write(target_dir.join("secret.env"), b"DATABASE_URL=leaked").await.unwrap();

        purge_project_source_dirs(&project).await;

        assert!(!target_dir.exists(), "target project's checkout must be removed");
        assert!(!target_building_dir.exists(), "target project's in-progress -building- checkout must also be removed");
        assert!(unrelated_dir.exists(), "an unrelated project's checkout must survive");

        let _ = tokio::fs::remove_dir_all(&unrelated_dir).await;
    }
}
