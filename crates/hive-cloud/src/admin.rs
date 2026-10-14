//! The node's admin/control API — everything the dashboard talks to.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use base64::Engine;
use fluid_gateway::{RumDevice, RumRaw};
use hive_core::{now_ms, BuildJob, JobState, ResourceSpec};
use hive_edge::{
    bot::BotPolicy,
    routing::{Redirect, Rewrite},
    waf::WafRule,
    CronJob, WorkflowDef,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command;

use crate::state::CloudState;

const DEPLOY_ZIP_MAX_BYTES: usize = 10 * 1024 * 1024;
const DEPLOY_ZIP_BODY_LIMIT_BYTES: usize = DEPLOY_ZIP_MAX_BYTES + 2 * 1024 * 1024;

const MINT_ATTEMPTS_PER_WINDOW: u32 = 20;
const ADMIN_REQUESTS_PER_WINDOW: u32 = 600;
const RATE_LIMIT_WINDOW_MS: u64 = 60_000;
const MINTED_TOKEN_TTL_SECS: i64 = 60 * 60;
const DOMAIN_VERIFY_DEFAULT_SECS: u64 = 45;
const MESH_FORWARD_TIMEOUT_SECS: u64 = 20;
const PROJECT_DELETE_QUICK_PASS_PEER_BUDGET: Duration = Duration::from_secs(28);
const FRAMEWORK_OUTPUT_DIRS: [&str; 6] = [
    ".vercel/output/static",
    ".next/static",
    "dist",
    "build",
    "out",
    "public",
];

pub fn router(cloud: Arc<CloudState>) -> Router {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/mesh", get(mesh_health))
        .route("/v1/overview", get(overview))
        .route("/v1/tasks/health", get(tasks_health))
        .route("/v1/nodes", get(nodes))
        .route("/v1/serve-hosts", get(serve_hosts_public))
        .route("/v1/resources", get(resources_get))
        .route("/v1/leases", get(leases_public))
        .route("/v1/guardian/heads", get(guardian_heads_public))
        .route("/v1/cluster", get(cluster_status))
        .route("/v1/anycast", get(anycast_public))
        .route("/v1/ratelimit", get(ratelimit_get).put(ratelimit_put))
        .route("/v1/regions", get(regions))
        .route("/v1/wqueue/enqueue", post(wqueue_enqueue))
        .route("/v1/wqueue/stats", get(wqueue_stats))
        .route("/v1/logs", get(logs))
        .route("/v1/functions", get(functions_public))
        .route("/v1/tunnels", get(tunnels))
        .route("/v1/relay", get(relay_stats))
        .route("/v1/gpu-pools", get(gpu_pools))
        .route("/v1/inference", get(inference_endpoints))
        .route("/v1/dns/stats", get(dns_stats))
        .route("/v1/host/listeners", get(host_listeners))
        .route("/v1/mesh/discovery", get(mesh_discovery))
        .route("/v1/mesh/establish", get(mesh_establish))
        .route("/v1/admin/runtime", get(admin_runtime))
        .route("/v1/admin/content-hash", get(admin_content_hash))
        .route("/v1/node/restarts", get(node_restarts))
        .route("/v1/mesh/health-guard", get(mesh_health_guard))
        .route("/v1/debug/heap", get(heap_profile))
        .route("/v1/debug/memory", get(memory_stats))
        .route("/v1/waf", get(waf_get))
        .route("/v1/waf/rules", post(waf_add_rule))
        .route("/v1/waf/rules/:id", delete(waf_del_rule))
        .route("/v1/waf/managed", put(waf_managed))
        .route("/v1/bot", get(bot_get).put(bot_put))
        .route("/v1/cdn", get(cdn_get))
        .route("/v1/cdn/purge", post(cdn_purge))
        .route("/v1/runtime-cache", get(rc_stats))
        .route(
            "/v1/runtime-cache/entry",
            get(rc_get).put(rc_put).delete(rc_delete),
        )
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
        .route("/v1/workflows/hooks", get(wf_hooks))
        .route("/v1/workflows/runs", get(wf_runs))
        .route("/v1/workflows/runs/:id", get(wf_run_detail))
        .route("/v1/workflows/runs/:id/cancel", post(wf_run_cancel))
        .route("/v1/workflows/runs/:id/replay", post(wf_run_replay))
        .route("/v1/workflows/runs/:id/reenqueue", post(wf_run_reenqueue))
        .route("/v1/workflows/runs/:id/wakeup", post(wf_run_wakeup))
        .route("/v1/workflows/runs/:id/events", post(wf_run_add_event))
        .route(
            "/v1/workflows/runs/:id/attributes",
            patch(wf_run_set_attributes).post(wf_run_set_attributes),
        )
        .route("/v1/workflows/:id/run", post(wf_run))
        .route("/v1/sandbox", post(sandbox))
        .route("/deployments", get(dep_list).post(dep_create))
        .route("/v1/deployments/:id", delete(dep_delete))
        .route("/v1/deployments/:id/resources", get(deployment_resources))
        .route("/v1/deployments/:id/build", get(deployment_build))
        .route("/v1/deployments/:id/integrity", get(deployment_integrity))
        .route(
            "/v1/deployments/:id/service-graph",
            get(deployment_service_graph),
        )
        .route("/v1/deployments/:id/promote", post(dep_promote))
        .route("/v1/projects/:project", delete(project_delete))
        .route("/v1/projects/:project/rename", post(project_rename))
        .route(
            "/v1/project-delete/capabilities",
            get(project_delete_capabilities),
        )
        .route("/v1/projects/:project/redeploy", post(project_redeploy))
        .route("/v1/runtime-artifact/v1/git/deploy", post(git_deploy))
        .route("/v1/git/deploy", post(git_deploy_public))
        .route(
            "/v1/deploy/zip",
            post(deploy_zip).layer(DefaultBodyLimit::max(DEPLOY_ZIP_BODY_LIMIT_BYTES)),
        )
        .route("/v1/deploy/image", post(deploy_image))
        .route("/v1/fleet-deployments", get(fleet_deployments_public))
        .route("/v1/builds/:id", get(build_get))
        .route("/v1/builds/:id/cancel", post(build_cancel))
        .route("/v1/buildcache/:key", get(buildcache_get))
        .route("/v1/build/frameworks", get(build_frameworks))
        .route("/v1/nodes/announce", post(node_announce_public))
        .route("/v1/mesh/roster", get(mesh_roster_get))
        .route("/v1/mesh/admit", post(mesh_admit))
        .route("/v1/token", post(mint_token))
        .route("/v1/tls/bundle-mesh", get(tls_bundle_mesh))
        .route("/v1/whoami", get(whoami))
        .route("/v1/auth", get(auth_status))
        .route("/v1/regions/catalog", get(region_catalog))
        .route("/v1/projects/:project/settings", get(project_settings_get))
        .route("/v1/projects/:project/build", put(project_build_put))
        .route(
            "/v1/projects/:project/functions",
            put(project_functions_put),
        )
        .route("/v1/projects/:project/network", put(project_network_put))
        .route(
            "/v1/projects/:project/cron-enabled",
            put(project_cron_enabled_put),
        )
        .route("/v1/projects/:project/git-ci", put(project_git_ci_put))
        .route(
            "/v1/projects/:project/browser-db",
            put(project_browser_db_put).delete(project_browser_db_delete),
        )
        .route(
            "/v1/projects/:project/container",
            put(project_container_put).delete(project_container_delete),
        )
        .route("/v1/projects/:project/env", post(project_env_put))
        .route("/v1/projects/:project/env/:key", delete(project_env_delete))
        .route(
            "/v1/projects/:project/service-graph",
            get(project_service_graph),
        )
        .route("/v1/projects/:project/domains", post(project_domain_add))
        .route(
            "/v1/projects/:project/domains/:domain/activate",
            post(domain_activate),
        )
        .route(
            "/v1/projects/:project/domains/:domain/detach",
            post(domain_detach_owner),
        )
        .route(
            "/v1/projects/:project/domains/:domain",
            delete(project_domain_detach),
        )
        .route("/v1/projects/:project/team", put(project_team_put))
        .route("/v1/domains", get(domains_list))
        .route("/v1/domains/:domain", get(domain_get))
        .route("/v1/domains/:domain/verify", post(domain_verify_now))
        .route("/v1/domains-nameserver-roster", get(domain_ns_roster))
        .route("/v1/domains/:domain/records", post(domain_add_record))
        .route(
            "/v1/domains/:domain/records/:id",
            delete(domain_delete_record).put(domain_update_record),
        )
        .route("/v1/domains/:domain/import", post(domain_import_records))
        .route("/v1/domains/:domain/scan", get(domain_scan_dns))
        .route(
            "/v1/domains/:domain/nameservers",
            put(domain_set_nameservers),
        )
        .route("/v1/domains/:domain/auto-renew", put(domain_set_auto_renew))
        .route("/v1/domains/:domain/ssl/renew", post(domain_renew_ssl))
        .route("/v1/teams", get(teams_list).post(team_create))
        .route("/v1/teams/:slug", get(team_get).delete(team_delete))
        .route("/v1/teams/:slug/members", post(team_add_member))
        .route("/v1/teams/:slug/members/:email", delete(team_remove_member))
        .route("/v1/teams/:slug/plan", put(team_set_plan))
        .route("/v1/teams/:slug/sso", put(team_set_sso))
        .route(
            "/v1/gitops",
            get(gitops_get).put(gitops_put).delete(gitops_unlink),
        )
        .route("/v1/gitops/synced", post(gitops_synced))
        .route("/v1/gitops/projects", get(gitops_projects))
        .route("/v1/git/webhook", post(git_webhook))
        .route("/v1/apikeys", get(apikeys_list).post(apikey_create))
        .route("/v1/apikeys/:id", delete(apikey_revoke))
        .route(
            "/v1/integrations",
            get(integrations_list).post(integration_upsert),
        )
        .route(
            "/v1/integrations/:id",
            get(integration_get).delete(integration_delete),
        )
        .route(
            "/v1/integrations/:id/credentials",
            get(integration_credentials),
        )
        .route("/v1/webhooks", get(webhooks_all).post(webhook_create_team))
        .route("/v1/webhooks/events", get(webhook_events))
        .route("/v1/webhooks/deliveries", get(webhook_deliveries))
        .route("/v1/webhooks/:id", delete(webhook_delete))
        .route(
            "/v1/projects/:project/webhooks",
            get(webhooks_for_project).post(webhook_create),
        )
        .route("/v1/databases", get(databases_list).post(database_create))
        .route(
            "/v1/internal/databases/provision-local",
            post(database_provision_local),
        )
        .route(
            "/v1/internal/databases/teardown-local",
            post(database_teardown_local),
        )
        .route("/v1/db-directory", get(db_directory))
        .route("/v1/admin/databases", get(admin_databases_all))
        .route(
            "/v1/databases/:id",
            get(database_get).delete(database_delete),
        )
        .route("/v1/databases/:id/credentials", get(database_credentials))
        .route("/v1/databases/:id/studio-open", post(database_studio_open))
        .route("/v1/databases/:id/rotate", post(database_rotate))
        .route(
            "/v1/projects/:project/databases",
            get(databases_for_project),
        )
        .route("/v1/databases/replica", post(database_replica_public))
        .route("/v1/storage/blob/:bucket", get(blob_list_keys))
        .route("/v1/storage/blob/:bucket/:key", get(blob_get).put(blob_put))
        .route(
            "/v1/storage/queue/:queue",
            get(queue_depth).post(queue_push),
        )
        .route("/v1/storage/queue/:queue/pop", post(queue_pop))
        .route("/v1/storage/vector/:index", post(vector_upsert))
        .route("/v1/storage/vector/:index/query", post(vector_query))
        .route("/v1/storage/pubsub/:topic", get(pubsub_info))
        .route("/v1/storage/pubsub/:topic/publish", post(pubsub_publish))
        .route("/v1/ws/pubsub/:topic", get(ws_pubsub))
        .route("/v1/ws/realtime/:room", get(ws_realtime))
        .route("/v1/ws/echo", get(ws_echo))
        .route(
            "/v1/securelinks",
            get(securelinks_list).post(securelink_create),
        )
        .route("/v1/securelinks/:id", delete(securelink_delete))
        .route("/v1/notifications", get(notifications_list))
        .route("/v1/notifications/read", post(notifications_read))
        .route(
            "/v1/notifications/archive-all",
            post(notifications_archive_all),
        )
        .route("/v1/notifications/:id/archive", post(notification_archive))
        .route("/v1/push/vapid-key", get(push_vapid_key))
        .route(
            "/v1/push/subscribe",
            post(push_subscribe).delete(push_unsubscribe),
        )
        .route("/v1/push/settings", get(push_settings))
        .route("/v1/push/sms", axum::routing::put(push_sms_put))
        .route("/v1/push/sms/verify", post(push_sms_verify))
        .route("/v1/push/sms-relay", put(push_sms_relay))
        .route("/v1/push/sms-direct-mx", put(push_sms_direct_mx))
        .route("/v1/push/sms-key", post(push_sms_key_put))
        .route("/v1/push/test", post(push_test))
        .route("/v1/metrics", get(metrics_get))
        .route("/v1/speed-insights", get(speed_insights_get))
        .route("/v1/admin/overview", get(admin_overview))
        .route("/v1/admin/placement", get(admin_placement))
        .route(
            "/v1/admin/audit",
            get(admin_audit).layer(tower_http::compression::CompressionLayer::new()),
        )
        .route("/v1/admin/data", get(data_collections))
        .route(
            "/v1/admin/data/:collection",
            get(data_rows).post(data_create),
        )
        .route(
            "/v1/admin/data/:collection/:id",
            put(data_patch).delete(data_delete),
        )
        .route("/v1/admin/namespaces", get(data_namespaces))
        .route("/v1/admin/sql/tables", get(sql_tables))
        .route("/v1/admin/sql/query", post(sql_query))
        .route("/v1/admin/guardian", get(guardian_status))
        .route("/v1/identity/sync", post(identity_sync))
        .route("/v1/billing", get(billing_get))
        .route("/v1/billing/ledger", get(billing_ledger))
        .route("/v1/billing/invoices", get(billing_invoices))
        .route("/v1/billing/checkout", post(billing_checkout))
        .route("/v1/billing/addons", get(billing_addons))
        .route("/v1/billing/checkout/:id", get(billing_checkout_get))
        .route("/v1/billing/confirm", post(billing_confirm))
        .route("/v1/billing/charge", post(billing_charge))
        .route("/v1/billing/webhook", post(billing_webhook))
        .route("/v1/admin/billing/grant", post(billing_grant))
        .route("/v1/admin/billing/backfill", post(billing_backfill_run))
        .route(
            "/v1/admin/projects/:project/team",
            post(admin_set_project_team),
        )
        .route("/v1/projects/:project/preview", get(project_preview))
        .route("/v1/projects/:project/thumbnail", get(project_thumbnail))
        .route(
            "/v1/incidents",
            get(incidents_list)
                .post(incident_open)
                .layer(tower_http::compression::CompressionLayer::new()),
        )
        .route("/v1/status/incidents", get(status_incidents))
        .route("/v1/incidents/:id", axum::routing::delete(incident_delete))
        .route("/v1/incidents/:id/updates", post(incident_update))
        .merge(crate::browser_admission::routes())
        .merge(crate::browser_artifacts::routes())
        .merge(crate::runtime_artifact_transfer::routes())
        .merge(crate::browser_db::routes())
        .merge(crate::browser_db_rest::routes())
        .merge(crate::browser_presence::routes())
        .merge(crate::browser_metering::routes())
        .merge(crate::enterprise_api::routes())
        .merge(crate::microfrontends_api::routes())
        .merge(crate::sandboxes_api::routes())
        .merge(crate::storage_api::routes())
        .merge(crate::queues_api::routes())
        .merge(crate::hrana::routes())
        .merge(crate::db_rest::routes())
        .merge(crate::drive_api::routes())
        .merge(crate::drive_webdav::routes())
        .with_state(cloud.clone());
    #[cfg(feature = "zkauth")]
    let app = app.merge(crate::zkauth::routes(cloud.clone()));
    let _ = &cloud;
    app
}

#[derive(Deserialize)]
struct TokenReq {
    #[serde(default = "default_sub")]
    sub: String,
    #[serde(default = "default_tenant")]
    tenant: String,
    /// No default: a mint carries only the role the caller names.
    #[serde(default)]
    role: String,
    /// The verified account email from the dashboard's Clerk session: the only input to
    /// `platform_admin`.
    #[serde(default)]
    email: String,
}
fn default_sub() -> String {
    "user".into()
}
fn default_tenant() -> String {
    "default".into()
}

/// Constant-time equality for secrets and tokens (`==` short-circuits on the first differing byte).
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

/// Dev (no `HIVE_JWT_SECRET`) mints freely; when enforced a mint can assert ANY tenant, so it must
/// present `x-hive-internal == HIVE_INTERNAL_TOKEN` and fails CLOSED when that token is unset.
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

/// Throttles `/v1/token` guesses at `HIVE_INTERNAL_TOKEN` independently of every other route (the
/// tenant edge limiter never fronts the admin router).
fn mint_rate_limiter() -> &'static hive_edge::RateLimiter {
    static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> = std::sync::OnceLock::new();
    LIMITER
        .get_or_init(|| hive_edge::RateLimiter::new(MINT_ATTEMPTS_PER_WINDOW, RATE_LIMIT_WINDOW_MS))
}

/// Per-client-IP budget for the whole admin router, which the tenant edge limiter never fronts;
/// sized for dashboard polling.
fn admin_rate_limiter() -> &'static hive_edge::RateLimiter {
    static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| {
        hive_edge::RateLimiter::new(ADMIN_REQUESTS_PER_WINDOW, RATE_LIMIT_WINDOW_MS)
    })
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

/// HTTPS fallback for the cert bundle `acme::bundle_for_mesh` serves over the mesh: a peer with a
/// stale relay cert cannot reach the leader to fetch the cert that fixes it. Serves a DECRYPTED
/// private key, so it fails CLOSED: a non-empty `HIVE_INTERNAL_TOKEN` presented via
/// `x-hive-internal` (no dev-mode open case), behind the mint rate limiter.
async fn tls_bundle_mesh(
    State(c): State<Arc<CloudState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;
    if !mint_rate_limiter().check(&peer.ip().to_string(), hive_core::now_ms()) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "too many requests".into()));
    }
    let allowed = match std::env::var("HIVE_INTERNAL_TOKEN") {
        Ok(t) if !t.trim().is_empty() => headers
            .get("x-hive-internal")
            .and_then(|v| v.to_str().ok())
            .map(|v| ct_eq(v, &t))
            .unwrap_or(false),
        _ => false,
    };
    if !allowed {
        return Err((StatusCode::FORBIDDEN, "internal token required".into()));
    }
    let name = q.get("name").map(String::as_str).unwrap_or("");
    let bytes = crate::acme::bundle_for_mesh(&c, name);
    if bytes.is_empty() {
        return Err((StatusCode::NOT_FOUND, "no such bundle".into()));
    }
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response())
}

async fn mint_token(
    State(c): State<Arc<CloudState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<TokenReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let peer_ip = peer.ip().to_string();
    if !mint_rate_limiter().check(&peer_ip, hive_core::now_ms()) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many token requests".into(),
        ));
    }
    if !mint_allowed(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "token minting is server-only (x-hive-internal required)".into(),
        ));
    }
    let email = req.email.trim();
    let platform_admin =
        !email.is_empty() && c.admin_emails.iter().any(|a| email.eq_ignore_ascii_case(a));
    let admin_tenant = req.tenant.trim();
    let is_billing_writer = || crate::leadership::may_act(&c, crate::leadership::Job::BillingMeter);
    if platform_admin && !admin_tenant.is_empty() && is_billing_writer() {
        let floor_already_enterprise =
            c.billing.account(admin_tenant).tier_locked_plan.as_deref() == Some("enterprise");
        if !floor_already_enterprise {
            apply_admin_enterprise(&c, admin_tenant);
        }
    }
    let ttl = MINTED_TOKEN_TTL_SECS;
    match crate::auth::issue(&req.sub, &req.tenant, &req.role, platform_admin, ttl) {
        Ok(token) => Ok(Json(json!({ "token": token, "expires_in": ttl }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

/// Echoes the verified caller identity (tenant/role/sub) from the `require_auth` claims;
/// `authenticated:false` in dev.
async fn whoami(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    if let Some(cl) = claims.as_ref().map(|e| &e.0) {
        return Json(
            json!({ "authenticated": true, "sub": cl.sub, "tenant": cl.tenant, "role": cl.role, "enforced": crate::auth::enforced() }),
        );
    }
    let tok = crate::auth::extract_token(&headers);
    match tok.and_then(|t| {
        crate::auth::verify(&t)
            .ok()
            .or_else(|| crate::auth::api_key_claims(&c, &t))
    }) {
        Some(cl) => Json(
            json!({ "authenticated": true, "sub": cl.sub, "tenant": cl.tenant, "role": cl.role, "enforced": crate::auth::enforced() }),
        ),
        None => Json(json!({ "authenticated": false, "enforced": crate::auth::enforced() })),
    }
}

/// The function-region catalog built from the live mesh: each node's region under its real
/// continent (from lat/lon), deduped across co-located nodes with a node count.
fn build_region_catalog(c: &Arc<CloudState>) -> Value {
    use std::collections::BTreeMap;
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
    json!(out)
}

async fn region_catalog(State(c): State<Arc<CloudState>>) -> Json<Value> {
    if let Some(leader) = c.leader_forward_target() {
        if let Some(v) = fetch_from_host(&c, &leader, "/v1/regions/catalog", "").await {
            return Json(v);
        }
    }
    Json(build_region_catalog(&c))
}

async fn project_settings_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let path = format!("/v1/projects/{project}/settings");
    if !c.is_control_plane_leader() {
        if let Some(leader) = c.leader_forward_target() {
            if let Some(v) = fetch_from_host(&c, &leader, &path, &t).await {
                return Ok(Json(v));
            }
        }
        let never_hosted_here =
            crate::admin::record_tenant(&c.projects.team_of(&project)) == UNTAGGED_TENANT;
        if never_hosted_here {
            if let Some(node) = host_node_for_project(&c, &project) {
                if let Some(v) = fetch_from_host(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
        }
    }
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// A save keeps `framework_auto` only when the dashboard round-trips the whole generated framework + command
/// tuple unchanged; editing any member makes it an explicit user configuration.
fn is_unchanged_generated_tuple(
    current: &crate::project_settings::BuildConfig,
    saved: &crate::project_settings::BuildConfig,
) -> bool {
    current.framework_auto
        && saved.framework.trim() == current.framework.trim()
        && saved.install_command.trim() == current.install_command.trim()
        && saved.build_command.trim() == current.build_command.trim()
        && saved.output_dir.trim() == current.output_dir.trim()
}

async fn project_build_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(build): Json<crate::project_settings::BuildConfig>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let mut build = build;
    let round_trips_generated_tuple = c
        .projects
        .get_if_set(&project)
        .map(|s| s.build)
        .is_some_and(|current| is_unchanged_generated_tuple(&current, &build));
    build.framework_auto = round_trips_generated_tuple;
    c.projects.set_build(&project, build);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// The tier (hobby/pro/enterprise) of the team owning a project; a personal namespace has no team row,
/// so its billing plan answers.
pub(crate) fn team_plan(c: &Arc<CloudState>, project: &str) -> String {
    let team = norm(&c.projects.team_of(project)).to_string();
    c.teams
        .get(&team)
        .map(|t| t.plan)
        .unwrap_or_else(|| c.billing.account(&team).plan)
}

async fn project_functions_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(mut f): Json<crate::project_settings::FunctionSettings>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let plan = team_plan(&c, &project);
    let max_dur = crate::billing::plan_max_duration_secs(&plan);
    f.default_max_duration_secs = f.default_max_duration_secs.clamp(1, max_dur);
    c.projects.set_functions(&project, f);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// `PUT /v1/projects/:project/browser-db`: saves only the dashboard-managed
/// `ProjectSettings::browser_db` mirror (no fluid.json, no rebuild), validating table names at save
/// time because `BrowserDbPolicy::resolve` silently drops invalid entries; follow with `/redeploy`
/// so the block lands in a Ready manifest.
async fn project_browser_db_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(policy): Json<fluid_core::BrowserDbPolicy>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    for t in &policy.schema {
        let valid_name = {
            let mut chars = t.name.chars();
            matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
                && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        };
        if !valid_name {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("table name {:?} must match [A-Za-z_][A-Za-z0-9_]*", t.name),
            ));
        }
        if t.ddl.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("table {:?} needs a CREATE TABLE IF NOT EXISTS ddl", t.name),
            ));
        }
    }
    c.projects.set_browser_db(&project, Some(policy));
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// `DELETE /v1/projects/:project/browser-db`: the NEXT deploy stops carrying the dashboard-managed
/// block (a fluid.json declaration still wins); nothing deployed changes.
async fn project_browser_db_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_browser_db(&project, None);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// `PUT /v1/projects/:project/container`: dashboard-managed container config, applied from the NEXT
/// deploy (`git.rs` produce_manifest merges it, fluid.json wins per field) or a `/redeploy`;
/// nothing already deployed changes.
async fn project_container_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(spec): Json<crate::project_settings::ContainerSettings>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    if let Some(p) = &spec.protocol {
        if p.parse::<fluid_core::ServiceProtocol>().is_err() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("protocol {p:?} must be one of: http, tcp, udp, grpc"),
            ));
        }
    }
    c.projects.set_container(&project, Some(spec));
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// `DELETE /v1/projects/:project/container`: the NEXT deploy stops carrying the dashboard-managed
/// config (a fluid.json `container` block still wins per field).
async fn project_container_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_container(&project, None);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// One user-editable port row of `PUT /v1/projects/:project/network`; `protocol` is parsed with the
/// STRICT `FromStr` (400 on a bad value), never the lenient serde impl reserved for already-stored
/// state.
#[derive(Deserialize)]
pub(crate) struct NetworkPortIn {
    container_port: u16,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct NetworkPutBody {
    #[serde(default)]
    ports: Vec<NetworkPortIn>,
}

/// A Settings->Network save rebuilds the specs from a form that has no notion of compose publish
/// requests, so `preferred_public_port` is carried over from the current record's matching spec;
/// otherwise every compose-published port is unpublished fleet-wide.
fn carry_over_compose_publish_requests(
    specs: &mut [fluid_core::PortSpec],
    recs: &[fluid_core::DeployRecord],
    project: &str,
) {
    let current: Vec<fluid_core::PortSpec> = recs
        .iter()
        .filter(|r| r.project == project)
        .max_by_key(|r| (r.production, r.created_at_ms))
        .and_then(|r| r.manifest.functions.first())
        .map(|f| f.ports.clone())
        .unwrap_or_default();
    for spec in specs.iter_mut() {
        if let Some(cur) = current
            .iter()
            .find(|cs| cs.container_port == spec.container_port && cs.protocol == spec.protocol)
        {
            spec.preferred_public_port = cur.preferred_public_port;
        }
    }
}

/// Edits a project's exposed ports/protocol without a redeploy: rewrites the PRIMARY (first)
/// function's `ports` on the production (else newest Ready) record in place
/// (`Gateway::update_manifest`) and stamps raw ports through the leader allocator. Claims are
/// released only when the edit leaves NO ports declared and NO stamped binding on any function; a
/// project hosted on a peer has no local record, so the validated edit is FORWARDED to its host
/// (`put_to_host`, `gossip::dispatch`).
pub(crate) async fn project_network_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<NetworkPutBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let mut specs: Vec<fluid_core::PortSpec> = Vec::with_capacity(b.ports.len());
    for p in &b.ports {
        if p.container_port == 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                "container_port must be 1-65535".into(),
            ));
        }
        let protocol: fluid_core::ServiceProtocol = p
            .protocol
            .parse()
            .map_err(|e: fluid_core::InvalidProtocol| (StatusCode::BAD_REQUEST, e.to_string()))?;
        if specs
            .iter()
            .any(|s| s.container_port == p.container_port && s.protocol == protocol)
        {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "duplicate port spec {}/{}",
                    p.container_port,
                    protocol.as_str()
                ),
            ));
        }
        specs.push(fluid_core::PortSpec {
            container_port: p.container_port,
            protocol,
            label: p
                .label
                .clone()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
            public_port: None,
            preferred_public_port: None,
        });
    }
    let recs = c.gw.deployment_records();
    carry_over_compose_publish_requests(&mut specs, &recs, &project);
    let rec = recs
        .iter()
        .filter(|r| r.project == project && r.production)
        .max_by_key(|r| r.created_at_ms)
        .or_else(|| {
            recs.iter()
                .filter(|r| r.project == project && r.state == fluid_core::DeployState::Ready)
                .max_by_key(|r| r.created_at_ms)
        });
    let Some(rec) = rec else {
        if let Some(node) = host_node_for_project(&c, &project) {
            let fwd_body = json!({
                "ports": specs.iter().map(|s| json!({
                    "container_port": s.container_port,
                    "protocol": s.protocol.as_str(),
                    "label": s.label,
                })).collect::<Vec<_>>(),
            });
            if let Some(v) = put_to_host(
                &c,
                &node,
                &format!("/v1/projects/{project}/network"),
                &team,
                &fwd_body,
            )
            .await
            {
                return Ok(Json(v));
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("project '{project}' is hosted on node '{node}' but the network-edit forward failed"),
            ));
        }
        return Err((
            StatusCode::NOT_FOUND,
            format!("no deployment record for project '{project}' on this node"),
        ));
    };
    if rec.manifest.functions.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "deployment '{}' has no functions to expose ports on",
                rec.id
            ),
        ));
    }
    let mut manifest = rec.manifest.clone();
    {
        let f = &mut manifest.functions[0];
        f.ports = specs.clone();
        f.protocol = specs.first().map(|s| s.protocol).unwrap_or_default();
    }
    if let Err(e) =
        crate::raw_ports::allocate_raw_ports_coordinated(&c, &project, &mut manifest).await
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("raw public-port allocation failed: {e}"),
        ));
    }
    if b.ports.is_empty() && manifest.raw_port_bindings().is_empty() {
        crate::raw_ports::release_raw_ports_coordinated(&c, &project).await;
    }
    let dep_id = rec.id.clone();
    let bindings = manifest.raw_port_bindings();
    let ports_out = manifest.functions[0].ports.clone();
    if c.gw
        .update_manifest(&dep_id, move |m| *m = manifest)
        .is_none()
    {
        return Err((
            StatusCode::NOT_FOUND,
            format!("deployment '{dep_id}' vanished mid-update"),
        ));
    }
    crate::persist::persist(&c);
    let detail = if bindings.is_empty() {
        "cleared exposed ports".to_string()
    } else {
        bindings
            .iter()
            .map(|bd| {
                format!(
                    "{} {}→{}",
                    bd.protocol.as_str(),
                    bd.public_port,
                    bd.container_port
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ev = c.event(
        &c.region,
        "PUT",
        &format!("{project}.localhost"),
        "/settings/network",
        200,
        "network-edit",
        &detail,
    );
    c.record(ev);
    Ok(Json(json!({
        "project": project,
        "deployment": dep_id,
        "ports": ports_out,
        "raw_ports": bindings,
    })))
}

/// Records the outcome of the auto-CI install the dashboard attempts right after a project's first
/// git import ([`crate::project_settings::GitCiStatus`]); a project without a completed GitHub
/// OAuth connection otherwise silently never auto-deploys.
async fn project_git_ci_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(mut status): Json<crate::project_settings::GitCiStatus>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    status.checked_ms = hive_core::now_ms();
    c.projects.set_git_ci(&project, status);
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
    let t = require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    // `require_project*` only proves TENANCY (the caller is some member of the owning team); env
    // vars can hold secrets and change what a deployment does, so writing/removing one needs at
    // least `member` — a bare `viewer` could otherwise set or clear them (confirmed live by an
    // adversarial review).
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &t,
        &["owner", "admin", "member"],
    )?;
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
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &t,
        &["owner", "admin", "member"],
    )?;
    c.projects.delete_env(&project, &key);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

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
    let team = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let domain = b.domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || !domain.contains('.') {
        return Err((StatusCode::BAD_REQUEST, "not a valid domain".into()));
    }
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let verify = match c.domains.ensure_verify(&domain, &team, &project) {
        Ok(v) => v,
        Err(msg) => return Err((StatusCode::CONFLICT, msg)),
    };
    c.projects.add_domain(&project, domain.clone());
    crate::persist::persist(&c);
    if verify.status == "verified" {
        return activate_domain_alias(&c, &domain, &project, &team).await;
    }
    Ok(Json(json!({
        "domain": domain,
        "project": project,
        "attached": false,
        "status": "pending",
        "verify": verify,
        "instructions": format!(
            "Add a TXT record named {} with the value {} at your DNS provider, then verification completes automatically (usually within a few minutes).",
            verify.txt_name, verify.txt_value
        ),
    })))
}

/// DoH TXT lookup: does `_hive-verify.<domain>` currently answer `want`? Returns (matched,
/// human-readable outcome for the record's last_probe).
async fn doh_txt_match(c: &Arc<CloudState>, qname: &str, want: &str) -> (bool, String) {
    let url = format!("https://cloudflare-dns.com/dns-query?name={qname}&type=TXT");
    let resp = c
        .http
        .get(&url)
        .header("accept", "application/dns-json")
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    let Ok(resp) = resp else {
        return (false, "resolver error (no answer from public DoH)".into());
    };
    let Ok(v) = resp.json::<Value>().await else {
        return (false, "resolver error (bad DoH response)".into());
    };
    let status = v.get("Status").and_then(|s| s.as_u64()).unwrap_or(0);
    let Some(ans) = v.get("Answer").and_then(|a| a.as_array()) else {
        return (
            false,
            if status == 3 {
                "NXDOMAIN".into()
            } else {
                "no TXT answer yet".into()
            },
        );
    };
    for a in ans {
        if a.get("type").and_then(|t| t.as_u64()) != Some(16) {
            continue;
        }
        let data = a
            .get("data")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim_matches('"')
            .trim();
        if data == want {
            return (true, "TXT matched".into());
        }
    }
    (false, "TXT present but value does not match".into())
}

/// Runs the custom-cert pass now (internally leader-gated), so TLS lands within seconds rather than
/// at the next loop tick.
fn kick_custom_cert_pass(c: &Arc<CloudState>) {
    let kick = c.clone();
    tokio::spawn(async move { crate::acme::custom_cert_pass(&kick).await });
}

/// Pins a delegated zone's apex to the CURRENT healthy edge set; true when the set changed
/// (`set_system_address_records` damps no-op refreshes).
fn pin_zone_addresses(c: &Arc<CloudState>, domain: &str) -> bool {
    let nodes = c.registry.nodes();
    c.domains.set_system_address_records(
        domain,
        crate::dnsserver::lb_records_strings(&nodes, 1),
        crate::dnsserver::lb_records_strings(&nodes, 28),
    )
}

/// Marks the domain verified (rebinding the record to the proving team) AND pins the delegated
/// zone's apex: the one helper for every activation path, since a verified domain whose zone serves
/// no address records resolves nowhere.
fn mark_domain_verified(c: &Arc<CloudState>, domain: &str, project: &str) {
    c.domains
        .mark_verified_as(domain, Some(&verify_project_team(c, project)));
    pin_zone_addresses(c, domain);
}

/// Activates an attached domain's alias, self-verifying: the DoH proof is re-checked HERE (a caller
/// can never assert it), and the verified mark lands only after the apply is confirmed (local
/// `add_alias` or the owner arm's ack), so a failed apply stays pending for the watcher. When
/// another node hosts the project it forwards to the owner's activate arm and marks the leader's
/// wholesale-replicated DomainStore itself.
async fn activate_domain_alias(
    c: &Arc<CloudState>,
    domain: &str,
    project: &str,
    team: &str,
) -> Result<Json<Value>, (StatusCode, String)> {
    let Some(verify) = c.domains.verify_of(domain) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "no verification challenge exists for '{domain}' — attach it to the project first"
            ),
        ));
    };
    let (matched, outcome) = doh_txt_match(c, &verify.txt_name, &verify.txt_value).await;
    c.domains.mark_verify_probe(domain, &outcome);
    if !matched {
        return Ok(Json(json!({
            "domain": domain,
            "project": project,
            "attached": false,
            "status": "pending",
            "verify": c.domains.verify_of(domain),
            "probe": outcome,
        })));
    }
    if c.gw.add_alias(domain, project) {
        mark_domain_verified(c, domain, project);
        crate::persist::persist(&c);
        let ev = c.event(
            c.region.clone().as_str(),
            "DOMAIN",
            domain,
            "/",
            200,
            "domain-verified",
            project,
        );
        c.record(ev);
        kick_custom_cert_pass(c);
        return Ok(Json(json!({
            "domain": domain,
            "project": project,
            "attached": true,
            "status": "verified",
        })));
    }
    let Some(node) = host_node_for_project(c, project) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no deployment for project '{project}'"),
        ));
    };
    let body = json!({ "project": project });
    if let Some(v) = post_to_host_json(
        c,
        &node,
        &format!("/v1/projects/{project}/domains/{domain}/activate"),
        team,
        &body,
    )
    .await
    {
        if v.get("status").and_then(|s| s.as_str()) == Some("verified") {
            mark_domain_verified(c, domain, project);
            crate::persist::persist(&c);
            kick_custom_cert_pass(c);
        }
        return Ok(Json(v));
    }
    Err((
        StatusCode::BAD_GATEWAY,
        format!("project '{project}' is hosted on node '{node}' but the domain activation forward failed"),
    ))
}

/// Owner-side activation (`POST /v1/projects/:p/domains/:domain/activate` and the gossip arm):
/// re-proves the TXT at THIS node (a forwarded activation carries no authority), applies the alias,
/// and only then marks verified so a failed apply stays pending. An already-verified domain only
/// has its alias re-applied: the TXT may be gone by then (path-A tenants remove it), so the proof
/// is never re-demanded.
pub(crate) async fn domain_activate_local(
    c: &Arc<CloudState>,
    project: &str,
    domain: &str,
) -> Value {
    let Some(verify) = c.domains.verify_of(domain) else {
        return json!({
            "domain": domain,
            "project": project,
            "attached": false,
            "status": "error",
            "error": format!("no verification challenge exists for '{domain}'"),
        });
    };
    let already_proven = verify.status == "verified";
    if already_proven {
        if c.gw.add_alias(domain, project) {
            return json!({
                "domain": domain,
                "project": project,
                "attached": true,
                "status": "verified",
            });
        }
        return json!({
            "domain": domain,
            "project": project,
            "attached": false,
            "status": "pending",
            "reason": "no_local_deployment",
            "probe": "verified, but the project has no production deployment on this node yet",
        });
    }
    let (matched, outcome) = doh_txt_match(c, &verify.txt_name, &verify.txt_value).await;
    c.domains.mark_verify_probe(domain, &outcome);
    if !matched {
        return json!({
            "domain": domain,
            "project": project,
            "attached": false,
            "status": "pending",
            "probe": outcome,
        });
    }
    if !c.gw.add_alias(domain, project) {
        return json!({
            "domain": domain,
            "project": project,
            "attached": false,
            "status": "pending",
            "reason": "no_local_deployment",
            "probe": "TXT matched, but the project has no deployment on this node yet",
        });
    }
    mark_domain_verified(c, domain, project);
    crate::persist::persist(c);
    kick_custom_cert_pass(c);
    let ev = c.event(
        c.region.clone().as_str(),
        "DOMAIN",
        domain,
        "/",
        200,
        "domain-verified",
        project,
    );
    c.record(ev);
    json!({
        "domain": domain,
        "project": project,
        "attached": true,
        "status": "verified",
    })
}

/// The proving attach's team for the tenant rebind: the project's tag when known, never invented
/// (personal namespaces have no team row).
fn verify_project_team(c: &Arc<CloudState>, project: &str) -> String {
    let t = c.projects.team_of(project);
    if !t.is_empty() {
        return t;
    }
    String::new()
}

/// Owner-side activation arm (`POST /v1/projects/:p/domains/:domain/activate`): re-proves the TXT
/// itself, since a forwarded activation carries no authority.
async fn domain_activate(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((project, domain)): Path<(String, String)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    if !c
        .domains
        .verify_of(&domain)
        .is_some_and(|v| v.project == project)
    {
        return Err((
            StatusCode::CONFLICT,
            format!("no verification challenge links '{domain}' to project '{project}'"),
        ));
    }
    let team = verify_project_team(&c, &project);
    let local = domain_activate_local(&c, &project, &domain).await;
    let parked_no_deployment = local.get("attached").and_then(|a| a.as_bool()) == Some(false)
        && local.get("reason").and_then(|r| r.as_str()) == Some("no_local_deployment");
    let is_forwarded_hop = q.get("local").map(|s| s.as_str()) == Some("true");
    if parked_no_deployment && !is_forwarded_hop {
        if let Some(node) = host_node_for_project(&c, &project) {
            if node != c.node_name {
                let body = json!({ "project": project });
                if let Some(v) = post_to_host_json(
                    &c,
                    &node,
                    &format!("/v1/projects/{project}/domains/{domain}/activate?local=true"),
                    &team,
                    &body,
                )
                .await
                {
                    if v.get("status").and_then(|s| s.as_str()) == Some("verified") {
                        mark_domain_verified(&c, &domain, &project);
                        crate::persist::persist(&c);
                        kick_custom_cert_pass(&c);
                    }
                    return Ok(Json(v));
                }
            }
        }
    }
    Ok(Json(local))
}

/// `POST /v1/domains/:domain/verify` — synchronous re-check for the UI's
/// "verify now" button. Same self-proving path as the background watcher.
async fn domain_verify_now(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let Some(verify) = c.domains.verify_of(&domain) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "no verification challenge exists for '{domain}' — attach it to a project first"
            ),
        ));
    };
    let team = crate::admin::tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    activate_domain_alias(&c, &domain, &verify.project, &team).await
}

/// `GET /v1/domains-nameserver-roster`: the nameserver hostnames a tenant points their registrar
/// at, ONLY peer-ATTESTED ones under the platform's >=2 floor (below it the response says so
/// instead of inventing a pair), plus the edge A/AAAA set for path A.
async fn domain_ns_roster(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if crate::auth::enforced() && claims.is_none() {
        return Err((StatusCode::UNAUTHORIZED, "authentication required".into()));
    }
    crate::admin::tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let nodes = c.registry.nodes();
    let verdicts = crate::dns_probe::validate_nameservers(&nodes);
    let proven: Vec<String> = verdicts
        .iter()
        .filter(|v| v.validated)
        .map(|v| {
            format!(
                "{}.{}",
                crate::vercel_dns::ns_label(&v.node),
                c.apps_domain.trim().trim_matches('.').to_lowercase()
            )
        })
        .collect();
    let nodes = c.registry.nodes();
    let (rr4, _) = crate::dnsserver::lb_records_pub(&nodes, 1);
    let (rr6, _) = crate::dnsserver::lb_records_pub(&nodes, 28);
    let fmt = |rd: &[u8]| -> String {
        if rd.len() == 4 {
            format!("{}.{}.{}.{}", rd[0], rd[1], rd[2], rd[3])
        } else {
            rd.chunks(2)
                .map(|c| format!("{:x}", u16::from_be_bytes([c[0], c[1]])))
                .collect::<Vec<_>>()
                .join(":")
        }
    };
    let v4: Vec<String> = rr4.iter().map(|rd| fmt(rd)).collect();
    let v6: Vec<String> = rr6.iter().map(|rd| fmt(rd)).collect();
    Ok(Json(json!({
        "nameservers": proven,
        "delegation_ready": proven.len() >= 2,
        "edge_ipv4": v4,
        "edge_ipv6": v6,
    })))
}

/// `DELETE /v1/projects/:p/domains/:domain`: detach. The alias dies when no project still lists the
/// domain OR it names a deployment of the DETACHED project (the rebind case) and every hosting peer
/// is told; the DomainStore record stays and its verification clears so a re-attach proves again.
async fn project_domain_detach(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((project, domain)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let attached_to_this_project = c
        .projects
        .all_domains()
        .into_iter()
        .any(|(p, d)| p == project && d == domain);
    if !attached_to_this_project {
        return Err((
            StatusCode::NOT_FOUND,
            format!("'{domain}' is not attached to project '{project}'"),
        ));
    }
    c.projects.remove_domain(&project, &domain);
    let still_attached = c
        .projects
        .all_domains()
        .into_iter()
        .any(|(_, d)| d == domain);
    let alias_revoked = !still_attached || c.gw.alias_points_at_project(&domain, &project);
    if alias_revoked {
        c.gw.remove_alias(&domain);
    }
    if !still_attached {
        if c.domains
            .verify_of(&domain)
            .is_some_and(|v| v.project == project)
        {
            c.domains.clear_verify(&domain);
        }
        c.domains
            .set_system_address_records(&domain, vec![], vec![]);
    }
    let hosts: Vec<String> = {
        let pd = c.peer_deployments.read();
        pd.iter()
            .filter(|(node, deps)| {
                **node != c.node_name && deps.iter().any(|d| d.project == project)
            })
            .map(|(node, _)| node.clone())
            .collect()
    };
    if alias_revoked {
        for node in hosts {
            let body = json!({});
            let _ = post_to_host_json(
                &c,
                &node,
                &format!("/v1/projects/{project}/domains/{domain}/detach"),
                &team,
                &body,
            )
            .await;
        }
    }
    crate::persist::persist(&c);
    Ok(Json(
        json!({ "domain": domain, "project": project, "detached": true }),
    ))
}

/// Owner-side detach arm (`POST /v1/projects/:p/domains/:domain/detach`).
async fn domain_detach_owner(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((project, domain)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let attached_to_this_project = c
        .projects
        .all_domains()
        .into_iter()
        .any(|(p, d)| p == project && d == domain);
    if !attached_to_this_project {
        return Err((
            StatusCode::NOT_FOUND,
            format!("'{domain}' is not attached to project '{project}'"),
        ));
    }
    c.projects.remove_domain(&project, &domain);
    let still_attached = c
        .projects
        .all_domains()
        .into_iter()
        .any(|(_, d)| d == domain);
    if !still_attached || c.gw.alias_points_at_project(&domain, &project) {
        c.gw.remove_alias(&domain);
    }
    if !still_attached {
        if c.domains
            .verify_of(&domain)
            .is_some_and(|v| v.project == project)
        {
            c.domains.clear_verify(&domain);
        }
        c.domains
            .set_system_address_records(&domain, vec![], vec![]);
    }
    crate::persist::persist(&c);
    Ok(Json(
        json!({ "domain": domain, "project": project, "detached": true }),
    ))
}

/// Leader-only watcher (`HIVE_DOMAIN_VERIFY_SECS`, default 45 s, `0` disables): asks each pending
/// challenge's OWNING node to activate (the owner arm re-proves the TXT; this loop never asserts a
/// proof), re-pins verified apex sets, and re-asserts routing a cross-node relocation drops.
pub fn spawn_domain_verify_loop(cloud: Arc<CloudState>) {
    let secs = std::env::var("HIVE_DOMAIN_VERIFY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DOMAIN_VERIFY_DEFAULT_SECS);
    if secs == 0 {
        return;
    }
    crate::supervise::spawn_supervised("domain-verify", move || {
        let cloud = cloud.clone();
        async move {
            loop {
                crate::supervise::beat("domain-verify");
                tokio::time::sleep(Duration::from_secs(secs)).await;
                if !crate::leadership::may_act(&cloud, crate::leadership::Job::DomainVerifyPin) {
                    continue;
                }
                let verified = cloud.domains.verified_attachments();
                let mut addrs_changed = false;
                for (domain, _) in &verified {
                    addrs_changed |= pin_zone_addresses(&cloud, domain);
                }
                if addrs_changed {
                    crate::persist::persist(&cloud);
                }
                for (domain, project) in &verified {
                    let Some(node) = host_node_for_project(&cloud, project) else {
                        continue;
                    };
                    if node == cloud.node_name {
                        cloud.gw.add_alias(domain, project);
                        continue;
                    }
                    let tenant = cloud
                        .domains
                        .get(domain)
                        .map(|d| d.tenant.clone())
                        .unwrap_or_default();
                    let body = json!({ "project": project });
                    let _ = post_to_host_json(
                        &cloud,
                        &node,
                        &format!("/v1/projects/{project}/domains/{domain}/activate"),
                        &tenant,
                        &body,
                    )
                    .await;
                }
                for (domain, verify) in cloud.domains.pending_verifications() {
                    let Some(node) = host_node_for_project(&cloud, &verify.project) else {
                        continue;
                    };
                    let tenant = cloud
                        .domains
                        .get(&domain)
                        .map(|d| d.tenant.clone())
                        .unwrap_or_default();
                    let body = json!({ "project": verify.project });
                    let v = post_to_host_json(
                        &cloud,
                        &node,
                        &format!(
                            "/v1/projects/{}/domains/{}/activate",
                            verify.project, domain
                        ),
                        &tenant,
                        &body,
                    )
                    .await;
                    if let Some(v) = v {
                        if v.get("status").and_then(|s| s.as_str()) == Some("verified") {
                            mark_domain_verified(&cloud, &domain, &verify.project);
                            crate::persist::persist(&cloud);
                            tracing::info!(domain, project = %verify.project, "domain-verify: TXT proof observed, domain attached");
                        }
                    }
                }
            }
        }
    });
}

/// Body-carrying POST to a node's admin surface: the POST counterpart of `put_to_host`
/// (`node_admins` first, then the iroh mesh).
pub(crate) async fn post_to_host_json(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
    body: &Value,
) -> Option<Value> {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        let mut request = c
            .http
            .post(format!("{admin}{path}"))
            .header("x-hive-team", team);
        if let Ok(token) = std::env::var("HIVE_INTERNAL_TOKEN") {
            if !token.trim().is_empty() {
                request = request.header("x-hive-internal", token);
            }
        }
        if crate::auth::enforced() {
            if let Ok(token) = crate::auth::issue(
                "mesh-internal",
                team,
                "service",
                false,
                crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
            ) {
                request = request.bearer_auth(token);
            }
        }
        if let Ok(r) = request
            .timeout(std::time::Duration::from_secs(15))
            .json(body)
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
        let body_bytes = serde_json::to_vec(body).unwrap_or_default();
        if let Some(b) =
            crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &body_bytes, 20)
                .await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

pub(crate) async fn domains_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<LocalQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    for (p, d) in c.projects.all_domains() {
        if norm(&c.projects.team_of(&p)) == t && seen.insert(format!("{p}\u{0}{d}")) {
            let vs = c
                .domains
                .verify_of(&d)
                .map(|v| v.status)
                .unwrap_or_default();
            out.push(json!({ "project": p, "domain": d, "verify_status": vs }));
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

fn asset_kind(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
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
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            if out.len() >= cap {
                return;
            }
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let not_a_shipped_asset =
                name == "node_modules" || name.starts_with('.') || name == "cache";
            if not_a_shipped_asset {
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
    out.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });
    out
}

/// Functions + static assets for a deployment — the build artifacts/resources.
pub(crate) async fn deployment_resources(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(rec) = c.gw.deployment_records().into_iter().find(|r| r.id == id) else {
        if let Some(node) = host_node_for_deployment(&c, &id) {
            if let Some(v) =
                fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/resources"), &t).await
            {
                return Json(v);
            }
        }
        return Json(json!({ "functions": [], "static_assets": [], "total_static": 0 }));
    };
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
                "runtime": match hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) {
                    _ if f.runtime == "edge" => "Edge",
                    hive_core::Runtime::Container => "Container",
                    hive_core::Runtime::Bun => "Bun",
                    hive_core::Runtime::Python => "Python",
                    hive_core::Runtime::Wasmer => "Wasmer",
                    hive_core::Runtime::Node | hive_core::Runtime::Command => "Node",
                },
                "region": c.region,
                "memory_mib": f.memory_mib,
                "max_duration_secs": f.max_duration_secs,
                "edge": f.runtime == "edge",
            })
        })
        .collect();

    let root = std::path::PathBuf::from(&rec.root);
    let build_output_dir = match rec.manifest.static_dir.as_deref() {
        Some(sd) if sd != "." && !sd.is_empty() => Some(root.join(sd)),
        _ => FRAMEWORK_OUTPUT_DIRS
            .iter()
            .map(|dir| root.join(dir))
            .find(|p| p.is_dir()),
    };
    let (mut assets, total_static) = match build_output_dir {
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

/// Full domain record (DNS records, nameservers, SSL, metadata) of one that already exists; a GET never creates it.
async fn domain_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(rec) = c.domains.get(&domain) else {
        return Err((StatusCode::NOT_FOUND, "no such domain".into()));
    };
    if norm(&rec.tenant) != norm(&t) {
        return Err((
            StatusCode::FORBIDDEN,
            "domain belongs to a different team".into(),
        ));
    }
    let connected: Vec<Value> = c
        .projects
        .all_domains()
        .into_iter()
        .filter(|(p, d)| {
            norm(&c.projects.team_of(p)) == t
                && (d == &domain || d.ends_with(&format!(".{domain}")))
        })
        .map(|(p, d)| json!({ "project": p, "domain": d }))
        .collect();
    Ok(Json(json!({ "domain": rec, "connected": connected })))
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

/// Value length bound for any zone record: an unbounded TXT made a tenant zone a UDP amplifier.
const MAX_RECORD_VALUE_BYTES: usize = 4096;
/// Per-(name, kind) RR bound — the amplifier needs many answers at one name.
const MAX_RECORDS_PER_NAME_KIND: usize = 32;

/// Platform-managed names (`_hive-verify`, `_acme-challenge`): the proof channel must not be
/// tenant-writable, or a zone holder could answer their own challenge.
fn reserved_record_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n == "_hive-verify"
        || n.starts_with("_hive-verify.")
        || n == "_acme-challenge"
        || n.starts_with("_acme-challenge.")
}

/// Shared write-side validation for zone records (add/update/import): reserved names, MX/SRV/CAA
/// shapes, value length and the per-(name, kind) count; `replacing_id` excludes the record being
/// updated from the count.
fn validate_record_write(
    c: &Arc<CloudState>,
    domain: &str,
    name: &str,
    kind: &str,
    value: &str,
    priority: Option<u32>,
    replacing_id: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    if reserved_record_name(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "'{}' is a platform-managed name — it cannot be set as a zone record",
                name.trim().to_ascii_lowercase()
            ),
        ));
    }
    let kind_u = kind.to_uppercase();
    if value.len() > MAX_RECORD_VALUE_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("record value exceeds {MAX_RECORD_VALUE_BYTES} bytes"),
        ));
    }
    if (kind_u == "MX" || kind_u == "SRV") && priority.unwrap_or(0) > 65_535 {
        return Err((
            StatusCode::BAD_REQUEST,
            "priority must fit in 16 bits (0–65535)".into(),
        ));
    }
    if kind_u == "SRV" {
        let mut it = value.split_whitespace();
        let ok = it.next().and_then(|w| w.parse::<u16>().ok()).is_some()
            && it.next().and_then(|p| p.parse::<u16>().ok()).is_some()
            && it.next().is_some_and(|t| !t.is_empty());
        if !ok {
            return Err((
                StatusCode::BAD_REQUEST,
                "SRV value must be '<weight> <port> <target>' (e.g. '1 443 sip.example.com')"
                    .into(),
            ));
        }
    }
    if kind_u == "CAA" {
        let mut it = value.splitn(3, char::is_whitespace);
        let ok = it.next().and_then(|f| f.parse::<u8>().ok()).is_some()
            && it.next().is_some_and(|t| !t.is_empty() && t.len() <= 255);
        if !ok {
            return Err((
                StatusCode::BAD_REQUEST,
                "CAA value must be '<flags> <tag> \"<value>\"' (e.g. '0 issue \"letsencrypt.org\"')".into(),
            ));
        }
    }
    let count = c
        .domains
        .get(domain)
        .map(|d| {
            d.records
                .iter()
                .filter(|r| {
                    r.name.trim().eq_ignore_ascii_case(name.trim())
                        && r.kind == kind_u
                        && Some(r.id.as_str()) != replacing_id
                })
                .count()
        })
        .unwrap_or(0);
    if count >= MAX_RECORDS_PER_NAME_KIND {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "too many {kind_u} records at '{}' (max {MAX_RECORDS_PER_NAME_KIND})",
                name.trim()
            ),
        ));
    }
    Ok(())
}

async fn domain_add_record(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
    Json(r): Json<AddRecordReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    validate_record_write(&c, &domain, &r.name, &r.kind, &r.value, r.priority, None)?;
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
    let added = c
        .domains
        .add_record(&domain, rec)
        .ok_or((StatusCode::NOT_FOUND, "no such domain".into()))?;
    c.audit.record(
        &t,
        "user",
        "create",
        "dns_record",
        &added.id,
        &format!("{} {} → {} ({domain})", added.kind, added.name, added.value),
    );
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
        c.audit
            .record(&t, "user", "delete", "dns_record", &id, &domain);
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
    validate_record_write(
        &c,
        &domain,
        &r.name,
        &r.kind,
        &r.value,
        r.priority,
        Some(&id),
    )?;
    let updated = c
        .domains
        .update_record(
            &domain,
            &id,
            r.name,
            r.kind.to_uppercase(),
            r.value,
            r.ttl,
            r.priority,
            r.comment,
        )
        .ok_or((StatusCode::NOT_FOUND, "no such record".into()))?;
    c.audit.record(
        &t,
        "user",
        "update",
        "dns_record",
        &updated.id,
        &format!(
            "{} {} → {} ({domain})",
            updated.kind, updated.name, updated.value
        ),
    );
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

/// Bulk-imports DNS records (structured and/or a pasted BIND-style zone file); idempotent, exact
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
        .filter(|r| !reserved_record_name(&r.name))
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
        recs.extend(
            parse_zone(&req.zone, &domain)
                .into_iter()
                .filter(|r| !reserved_record_name(&r.name)),
        );
    }
    let before = recs.len();
    recs.retain(|r| r.value.len() <= MAX_RECORD_VALUE_BYTES);
    let existing = c.domains.get(&domain);
    let mut counts: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    recs.retain(|r| {
        let key = (r.name.trim().to_ascii_lowercase(), r.kind.clone());
        let n = counts.entry(key).or_insert_with(|| {
            existing
                .as_ref()
                .map(|d| {
                    d.records
                        .iter()
                        .filter(|x| {
                            x.name.trim().eq_ignore_ascii_case(r.name.trim()) && x.kind == r.kind
                        })
                        .count()
                })
                .unwrap_or(0)
        });
        if *n >= MAX_RECORDS_PER_NAME_KIND {
            return false;
        }
        *n += 1;
        true
    });
    let skipped = before - recs.len();
    let added = c.domains.import_records(&domain, recs);
    if !added.is_empty() {
        c.audit.record(
            &t,
            "user",
            "import",
            "dns_record",
            &domain,
            &format!("imported {} record(s)", added.len()),
        );
        crate::persist::persist(&c);
    }
    Ok(Json(
        json!({ "imported": added.len(), "skipped": skipped, "records": added }),
    ))
}

/// Detects a domain's current public DNS records over DoH for the one-click migration; best-effort,
/// and nothing is added (the client imports the ones it wants).
async fn domain_scan_dns(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(domain): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_domain_owner(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let mut found: Vec<Value> = Vec::new();
    let queries: &[(&str, &str)] = &[
        ("", "A"),
        ("", "AAAA"),
        ("", "MX"),
        ("", "TXT"),
        ("", "NS"),
        ("", "CAA"),
        ("www", "CNAME"),
        ("www", "A"),
    ];
    for (sub, qtype) in queries {
        let qname = if sub.is_empty() {
            domain.clone()
        } else {
            format!("{sub}.{domain}")
        };
        let url = format!("https://cloudflare-dns.com/dns-query?name={qname}&type={qtype}");
        let resp = c
            .http
            .get(&url)
            .header("accept", "application/dns-json")
            .timeout(Duration::from_secs(4))
            .send()
            .await;
        let Ok(resp) = resp else { continue };
        let Ok(v) = resp.json::<Value>().await else {
            continue;
        };
        let Some(ans) = v.get("Answer").and_then(|a| a.as_array()) else {
            continue;
        };
        for a in ans {
            let rtype = a.get("type").and_then(|x| x.as_u64()).unwrap_or(0);
            let kind = dns_type_name(rtype);
            if kind != *qtype {
                continue;
            }
            let raw = a
                .get("data")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .trim_end_matches('.')
                .to_string();
            if raw.is_empty() {
                continue;
            }
            let ttl = a.get("TTL").and_then(|x| x.as_u64()).unwrap_or(3600) as u32;
            let (priority, value) =
                if (kind == "MX" || kind == "SRV") && raw.split_whitespace().count() >= 2 {
                    let mut it = raw.splitn(2, char::is_whitespace);
                    let p = it.next().unwrap_or("").parse::<u32>().ok();
                    (
                        p,
                        it.next()
                            .unwrap_or("")
                            .trim()
                            .trim_end_matches('.')
                            .to_string(),
                    )
                } else {
                    (None, raw.clone())
                };
            found.push(json!({
                "name": *sub,
                "type": kind,
                "value": value,
                "ttl": ttl,
                "priority": priority,
            }));
        }
    }
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

/// Tolerant BIND-style zone parser for the paste-your-zone import (`www 3600 IN A 76.76.21.21`,
/// `@ IN MX 10 mail.example.com`, minimal `www A 1.2.3.4`); unknown, blank and comment lines are
/// skipped.
fn parse_zone(text: &str, domain: &str) -> Vec<crate::dns::DnsRecord> {
    const TYPES: &[&str] = &[
        "A", "AAAA", "CNAME", "ALIAS", "MX", "TXT", "CAA", "NS", "SRV",
    ];
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        let Some(ti) = toks
            .iter()
            .position(|t| TYPES.contains(&t.to_uppercase().as_str()))
        else {
            continue;
        };
        let kind = toks[ti].to_uppercase();
        let mut name = String::new();
        if ti > 0 {
            let first = toks[0];
            let first_is_name = !first.eq_ignore_ascii_case("IN") && first.parse::<u32>().is_err();
            if first_is_name {
                name = if first == "@" {
                    String::new()
                } else {
                    first
                        .trim_end_matches(&format!(".{domain}"))
                        .trim_end_matches('.')
                        .to_string()
                };
            }
        }
        let ttl = toks[..ti]
            .iter()
            .find_map(|t| t.parse::<u32>().ok())
            .unwrap_or(3600);
        let rest: Vec<&str> = toks[ti + 1..].to_vec();
        if rest.is_empty() {
            continue;
        }
        let (priority, value) =
            if (kind == "MX" || kind == "SRV") && rest.len() >= 2 && rest[0].parse::<u32>().is_ok()
            {
                (rest[0].parse::<u32>().ok(), rest[1..].join(" "))
            } else {
                (None, rest.join(" "))
            };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_end_matches('.')
            .to_string();
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
    c.audit
        .record(&t, "user", "update", "nameservers", &domain, "");
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
    kick_custom_cert_pass(&c);
    let cert = c.domains.renew_ssl(&domain);
    c.audit.record(
        &t,
        "user",
        "update",
        "ssl_cert",
        &domain,
        "requested certificate reissue",
    );
    Ok(Json(json!(cert)))
}

/// Picks a globally-unique project name (aliases are global): a genuine redeploy (same name, repo
/// and tenant) keeps it, anything else gets a `-N` suffix.
fn unique_project_name(c: &Arc<CloudState>, desired: &str, repo_url: &str, tenant: &str) -> String {
    let base = if desired.trim().is_empty() {
        crate::git::project_name_from_url(repo_url)
    } else {
        desired.trim().to_string()
    };
    let taken_key_ci = |name: &str| {
        c.projects
            .find_key_ci(name)
            .or_else(|| c.projects.renamed_away(name).map(|_| name.to_string()))
    };
    let Some(hit) = taken_key_ci(&base) else {
        return base;
    };
    let same_tenant = norm(&c.projects.team_of(&hit)) == tenant;
    let same_repo =
        c.gw.git_for_project(&hit)
            .map(|g| crate::gitops::norm_repo(&g.repo_url) == crate::gitops::norm_repo(repo_url))
            .unwrap_or(false);
    if same_tenant && same_repo {
        return hit;
    }
    for i in 2..1000 {
        let cand = format!("{base}-{i}");
        if taken_key_ci(&cand).is_none() {
            return cand;
        }
    }
    format!("{base}-{}", now_ms())
}

fn clear_internal_orchestration_fields(req: &mut fluid_core::GitDeployRequest) {
    req.project_incarnation = None;
    req.no_fanout = false;
    req.fanout_secondary = false;
}

fn mesh_deploy_authority(claims: Option<&crate::auth::Claims>) -> bool {
    claims.is_some_and(crate::auth::is_mesh_internal)
}

pub(crate) async fn git_deploy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut req): Json<fluid_core::GitDeployRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let claims = claims.as_ref().map(|extension| &extension.0);
    if !mesh_deploy_authority(claims) {
        return Err((
            StatusCode::FORBIDDEN,
            "authenticated mesh-internal service authority required".into(),
        ));
    }
    let expected = req.project_incarnation.take().ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            "internal deploy is missing its server-issued project incarnation".into(),
        )
    })?;
    req.no_fanout = true;
    let t = tenant(&c, &headers, claims);
    start_named_deploy(&c, &t, req, Some(expected)).await
}

async fn git_deploy_public(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut req): Json<fluid_core::GitDeployRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let claims = claims.as_ref().map(|extension| &extension.0);
    if mesh_deploy_authority(claims) {
        let expected = req.project_incarnation.take().ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "internal deploy is missing its server-issued project incarnation".into(),
            )
        })?;
        req.no_fanout = true;
        let t = tenant(&c, &headers, claims);
        return start_named_deploy(&c, &t, req, Some(expected)).await;
    }

    clear_internal_orchestration_fields(&mut req);
    let t = tenant(&c, &headers, claims);
    start_named_deploy(&c, &t, req, None).await
}

/// Shared deploy entry for the git (`/v1/git/deploy`) and zip (`/v1/deploy/zip`) paths: assigns the
/// project to the tenant (unique name, conflict check, root-dir persist) then starts the async
/// build, so naming and placement do not depend on the source.
pub(crate) async fn start_named_deploy(
    c: &Arc<CloudState>,
    t: &str,
    mut req: fluid_core::GitDeployRequest,
    expected_incarnation: Option<fluid_core::ProjectIncarnation>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = t.to_string();
    let requested = req.project.clone().unwrap_or_default();
    let carries_canonical_name = req.no_fanout;
    let redeploy_existing = req.redeploy
        && !requested.trim().is_empty()
        && c.projects
            .find_key_ci(requested.trim())
            .map(|k| norm(&c.projects.team_of(&k)) == t)
            .unwrap_or(false);
    let reuse = if carries_canonical_name || requested.trim().is_empty() || redeploy_existing {
        None
    } else {
        c.projects.find_key_ci(requested.trim()).map(|key| {
            let verdict = crate::project_inventory::name_reuse(c, &key, &t, &req.repo_url);
            (key, verdict)
        })
    };
    if let Some((key, crate::project_inventory::NameReuse::InFlight { build_id })) = &reuse {
        return Err((
            StatusCode::CONFLICT,
            format!("A build ({build_id}) for project \"{key}\" is still running. Wait for it or cancel it first."),
        ));
    }
    let reuse_empty = matches!(
        reuse,
        Some((_, crate::project_inventory::NameReuse::ReuseEmpty))
    );
    let redeploy_existing = redeploy_existing || reuse_empty;
    if req.redeploy && expected_incarnation.is_none() && !redeploy_existing {
        return Err((
            StatusCode::NOT_FOUND,
            "the project no longer exists; start a new deployment instead of redeploying it".into(),
        ));
    }
    if !requested.trim().is_empty() {
        if let Some(tomb) = c.projects.renamed_away(requested.trim()) {
            return Err(renamed_away_refusal(requested.trim(), &tomb.new));
        }
    }
    let project = if (carries_canonical_name || redeploy_existing) && !requested.trim().is_empty() {
        c.projects
            .find_key_ci(requested.trim())
            .unwrap_or_else(|| requested.trim().to_string())
    } else {
        unique_project_name(c, &requested, &req.repo_url, &t)
    };
    let explicit_name_taken = !carries_canonical_name
        && !redeploy_existing
        && !requested.trim().is_empty()
        && project != requested.trim();
    if explicit_name_taken {
        let taken_by_caller = c
            .projects
            .find_key_ci(requested.trim())
            .map(|k| project_owned_by(c, &k, &t))
            .unwrap_or(false);
        let own_never_deployed = taken_by_caller
            && c.projects
                .get_if_set(requested.trim())
                .is_some_and(|row| row.first_deployed_ms == 0 && !row.last_build_id.is_empty());
        return Err((
            StatusCode::CONFLICT,
            if own_never_deployed {
                format!(
                    "You already have a project named \"{0}\" whose builds have not produced a \
                     deployment yet. Retry it from its project page (it is listed in Projects), \
                     delete it there, or choose a different name. Importing the same repository \
                     under this name retries it automatically.",
                    requested.trim()
                )
            } else if taken_by_caller {
                format!(
                    "You already have a project named \"{}\". Choose a different name.",
                    requested.trim()
                )
            } else {
                format!(
                    "The project name \"{}\" is already taken on this platform. Project names are \
                     global because they become storage identifiers (volumes, deploy directories, \
                     database files), so they cannot be reused even by a different team. Choose a \
                     different name.",
                    requested.trim()
                )
            },
        ));
    }
    let is_coordinator_path = !carries_canonical_name;
    if is_coordinator_path {
        let is_new_project = !c
            .projects
            .snapshot()
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(&project));
        if let Err(e) = c.billing.can_deploy(&t) {
            return Err((StatusCode::PAYMENT_REQUIRED, e));
        }
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
            let empty = crate::project_inventory::empty_project_count(c, &t);
            let max_empty = crate::project_inventory::max_empty_projects();
            if empty >= max_empty {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "You have {empty} projects whose first build never produced a deployment \
                         (limit {max_empty}). Retry or delete some of them before creating another."
                    ),
                ));
            }
        }
    }
    let existing_key = c.projects.find_key_ci(&project);
    if let Some(existing_key) = existing_key.as_deref() {
        if norm(&c.projects.team_of(existing_key)) != t {
            return Err((
                StatusCode::FORBIDDEN,
                "project belongs to a different team".into(),
            ));
        }
    }
    let admitted_incarnation = match expected_incarnation {
        Some(expected) => Some(expected),
        None => match existing_key {
            Some(existing_key) => {
                let _lifecycle = crate::project_settings::lifecycle_write(&existing_key).await;
                let incarnation = match c.projects.active_incarnation(&existing_key) {
                    Ok(incarnation) => incarnation,
                    Err(crate::project_settings::ProjectIncarnationError::Legacy) => c
                        .projects
                        .ensure_incarnation(&existing_key)
                        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?,
                    Err(error) => return Err((StatusCode::CONFLICT, error.to_string())),
                };
                Some(incarnation)
            }
            None => None,
        },
    };
    if reuse_empty {
        if let Some(incarnation) = admitted_incarnation {
            c.projects
                .reset_for_reuse_exact(
                    &project,
                    incarnation,
                    req.root_dir.as_deref().map(str::trim).unwrap_or(""),
                    req.branch.as_deref().map(str::trim).unwrap_or(""),
                )
                .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
        }
    }
    req.project = Some(project.clone());
    let build_id = crate::git::start_build(c.clone(), req, admitted_incarnation, Some(t.clone()))
        .await
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    Ok(Json(json!({ "build_id": build_id, "project": project })))
}

/// Zip-upload deploy, an alternative SOURCE to a git URL: the raw body is the `.zip` and metadata
/// rides as a base64 JSON `x-hive-deploy-meta` header. The archive is base64'd into the
/// `GitDeployRequest` so the same placement + fanout path ships it, capped under the 16 MB gossip
/// frame (SOURCE only, dependencies install during the build).
pub(crate) async fn deploy_zip(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    if body.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Empty upload — choose a .zip file.".into(),
        ));
    }
    if body.len() > DEPLOY_ZIP_MAX_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Zip too large (max 10 MB). Upload your SOURCE only — dependencies install during the build.".into(),
        ));
    }
    let looks_like_zip = body.starts_with(b"PK\x03\x04") || body.starts_with(b"PK\x05\x06");
    if !looks_like_zip {
        return Err((
            StatusCode::BAD_REQUEST,
            "That doesn't look like a .zip archive.".into(),
        ));
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
        /// Same as the git path's `redeploy`: the upload targets an EXISTING same-tenant project
        /// and keeps its name instead of 409ing.
        #[serde(default)]
        redeploy: Option<bool>,
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
    let names_own_project = || {
        meta.project
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .and_then(|n| c.projects.find_key_ci(n))
            .map(|k| norm(&c.projects.team_of(&k)) == t)
            .unwrap_or(false)
    };
    let redeploy = meta.redeploy.unwrap_or(false) && names_own_project();
    let req = fluid_core::GitDeployRequest {
        source_deployment_ids: Vec::new(),
        repo_url: format!("upload://{filename}"),
        branch: None,
        commit: None,
        head_repo_url: None,
        project: meta.project,
        project_incarnation: None,
        creator: Some("you".into()),
        production: meta.production.unwrap_or(true),
        target: None,
        use_cache: true,
        root_dir: None,
        env: meta.env,
        no_fanout: false,
        fanout_secondary: false,
        build_config: None,
        function_settings: None,
        redeploy,
        zip_b64: Some(zip_b64),
        image_ref: None,
        image_port: None,
        image_protocol: None,
        image_memory: None,
        image_cpus: None,
        image_pids: None,
        image_ports: None,
        git_token: None,
    };
    start_named_deploy(&c, &t, req, None).await
}

#[derive(serde::Deserialize)]
pub(crate) struct ImageDeployReq {
    /// OCI image reference, e.g. `fruitbox12/simplifi:latest`, `quay.io/org/img:tag`.
    image: String,
    #[serde(default)]
    project: Option<String>,
    /// Explicit container port; auto-detected from the image's `ExposedPorts` when
    /// omitted (see `image_container_manifest` in `git.rs`).
    #[serde(default)]
    port: Option<u16>,
    /// Explicit protocol override (see `fluid_core::ServiceProtocol`), independent of `port`;
    /// REQUIRED for a UDP-only image (e.g. Minecraft Bedrock), where auto-detection finds no TCP
    /// port.
    #[serde(default)]
    protocol: Option<fluid_core::ServiceProtocol>,
    /// Memory ceiling override (e.g. "4g", "2048m", "512"), same format as fluid.json
    /// `container.memory`; None = the node's default, always clamped to a fleet-wide ceiling
    /// (`ContainerLimits::for_container`).
    #[serde(default)]
    memory: Option<String>,
    /// CPU quota override (e.g. "2.0", "0.5"), same format as fluid.json `container.cpus`; None =
    /// the node's default, clamped fleet-wide.
    #[serde(default)]
    cpus: Option<String>,
    /// Max-PIDs override for the container's cgroup (fork-bomb guard), same as fluid.json
    /// `container.pids`; None = the node's default, clamped fleet-wide.
    #[serde(default)]
    pids: Option<u32>,
    /// Full multi-port override — see `fluid_core::GitDeployRequest::image_ports`'s
    /// doc for the replace-not-merge semantics and the mesh-forwarding caveat.
    #[serde(default)]
    ports: Option<Vec<fluid_core::PortSpec>>,
    #[serde(default)]
    env: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    production: Option<bool>,
    /// Fresh deployment of an EXISTING project (New Deployment modal) vs new project.
    #[serde(default)]
    redeploy: Option<bool>,
}

/// Deploys a PRE-BUILT container image from any registry: no clone or build, the target node pulls
/// it, auto-detects its port, attaches a persistent volume and runs it with the project's env
/// through the normal placement/fanout.
pub(crate) async fn deploy_image(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(body): Json<ImageDeployReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let image = body.image.trim().to_string();
    if image.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Provide an image reference, e.g. fruitbox12/simplifi:latest".into(),
        ));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let req = fluid_core::GitDeployRequest {
        source_deployment_ids: Vec::new(),
        repo_url: format!("image://{image}"),
        branch: None,
        commit: None,
        head_repo_url: None,
        project: body.project,
        project_incarnation: None,
        creator: Some("you".into()),
        production: body.production.unwrap_or(true),
        target: None,
        use_cache: true,
        root_dir: None,
        env: body.env,
        no_fanout: false,
        fanout_secondary: false,
        build_config: None,
        function_settings: None,
        redeploy: body.redeploy.unwrap_or(false),
        zip_b64: None,
        image_ref: Some(image),
        image_port: body.port,
        image_protocol: body.protocol,
        image_memory: body.memory,
        image_cpus: body.cpus,
        image_pids: body.pids,
        image_ports: body.ports,
        git_token: None,
    };
    start_named_deploy(&c, &t, req, None).await
}

pub(crate) async fn build_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let b = match c.builds.get(&id) {
        Some(b) => b,
        None => {
            if let Some(leader) = c.leader_forward_target() {
                if let Some(v) = fetch_from_host(&c, &leader, &format!("/v1/builds/{id}"), &t).await
                {
                    return Ok(Json(v));
                }
            }
            return Err(StatusCode::NOT_FOUND);
        }
    };
    if !build_owned_by(&c, &b, &t) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(b)))
}

/// Whether tenant `t` owns build `b`: the project row's team, else the tenant tag of the build's
/// deployment record (local `gw` or gossiped peer copy), since `Build` carries no tenant tag of its
/// own. Shared by `build_get` and `build_cancel`.
fn build_owned_by(c: &Arc<CloudState>, b: &crate::git::Build, t: &str) -> bool {
    norm(&c.projects.team_of(&b.project)) == norm(t)
        || b.deployment_id.as_deref().is_some_and(|did| {
            c.gw.deployment_records()
                .iter()
                .any(|r| r.id == did && record_tenant(&r.tenant) == norm(t))
                || c.peer_deployments
                    .read()
                    .values()
                    .flatten()
                    .any(|d| d.id.as_str() == did && record_tenant(&d.tenant) == norm(t))
        })
}

/// Cancels an in-flight build: kills the ACTUAL OS process (clone/install/build, or a remote fanout
/// target's own build) and marks the record `Cancelled`; a terminal build is a no-op (idempotent,
/// so a double-click or a race with completion is harmless). An id this node has never heard of is
/// proxied best-effort to the current control-plane leader, mirroring `build_get`.
pub(crate) async fn build_cancel(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(b) = c.builds.get(&id) else {
        if let Some(leader) = c.leader_forward_target() {
            if let Some(v) = post_to_host(&c, &leader, &format!("/v1/builds/{id}/cancel"), &t).await
            {
                return Ok(Json(v));
            }
        }
        return Err(StatusCode::NOT_FOUND);
    };
    if !build_owned_by(&c, &b, &t) {
        return Err(StatusCode::NOT_FOUND);
    }
    let in_flight = matches!(
        b.state,
        fluid_core::DeployState::Queued | fluid_core::DeployState::Building
    );
    if !in_flight {
        return Ok(Json(json!(b)));
    }
    c.builds.update(&id, |b| {
        b.state = fluid_core::DeployState::Cancelled;
        b.finished_ms = Some(now_ms());
        b.lines.push(crate::git::LogLine {
            ts_ms: now_ms(),
            line: "Build cancelled by user.".into(),
        });
    });
    crate::persist::persist(&c);
    c.build_cancels.cancel(&c, &id).await;
    let ev = c.event(
        &c.region,
        "CANCEL",
        &b.project,
        "/",
        200,
        "deploy",
        "build cancelled by user",
    );
    c.record(ev);
    crate::webhooks::dispatch(
        &c.webhooks,
        &t,
        &b.project,
        "deployment.canceled",
        json!({ "id": id, "project": b.project }),
    );
    Ok(Json(json!(c.builds.get(&id).unwrap_or(b))))
}

/// The build behind a deployment (status, timing, full log lines, a failed build's error); the
/// newest matching build wins, scoped to the requesting team.
pub(crate) async fn deployment_build(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let dep_record_tenant: Option<String> =
        c.gw.deployment_records()
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
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) =
            fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/build"), &t).await
        {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// A deployment's tamper-EVIDENCE chain (`hive_core::integrity`): each entry's fold is hashed into
/// `chain_head_sha256`, signed by this node's per-node key (`integrity_signer.rs`); a caller
/// re-derives the head with `hive_core::fold_integrity_chain` and checks `public_key_hex`.
/// Tamper-evidence, never tamper-prevention (see the module doc).
pub(crate) async fn deployment_integrity(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(acceptance) = c.deployment_ledger.acceptance(&id) {
        let team_ok = norm(&c.projects.team_of(&acceptance.input.project)) == norm(&t)
            || norm(&record_tenant(&acceptance.input.project)) == norm(&t);
        if !team_ok {
            return Err(StatusCode::NOT_FOUND);
        }
        let chain_head_sha256 = hive_core::fold_integrity_chain(&id, &acceptance.integrity_chain);
        let signature = c.integrity_signer.sign_chain_head(&chain_head_sha256);
        let sep_public_keys: Vec<&hive_core::IntegrityEntryKind> = acceptance
            .integrity_chain
            .iter()
            .map(|e| &e.kind)
            .filter(|k| matches!(k, hive_core::IntegrityEntryKind::SepKeyProvisioned { .. }))
            .collect();
        return Ok(Json(json!({
            "deployment_id": id,
            "chain": acceptance.integrity_chain,
            "chain_head_sha256": chain_head_sha256,
            "signatures": [signature],
            "sep_public_keys": sep_public_keys,
        })));
    }
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) =
            fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/integrity"), &t).await
        {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

#[derive(serde::Deserialize)]
pub(crate) struct ServeHostsQ {
    since: Option<u64>,
}

/// The host subdomains this node serves plus its gateway URL: the input to peers' cross-node routing tables.
pub(crate) async fn serve_hosts(
    State(c): State<Arc<CloudState>>,
    q: Option<Query<ServeHostsQ>>,
) -> Json<Value> {
    Json(serve_hosts_with(&c, q.and_then(|q| q.0.since)))
}

fn digest_of_sorted_hosts(hosts: &[String]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in hosts {
        h.write(s.as_bytes());
        h.write_u8(0xff);
    }
    h.finish()
}

/// This node's served hosts, or just its generation when the caller already holds `since`, so
/// steady-state gossip costs O(changes) instead of O(hosts ever deployed): any change to the host
/// set bumps `gen` and the next caller gets the full list once.
///
/// `containers` is always sent: it is tiny and drives lease election.
pub(crate) fn serve_hosts_with(c: &Arc<CloudState>, since: Option<u64>) -> Value {
    let mut hosts = c.gw.served_hosts();
    hosts.sort();
    let digest = digest_of_sorted_hosts(&hosts);
    let gen = {
        let mut last = c.route_pub_hash.write();
        if *last != digest {
            *last = digest;
            c.route_pub_gen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1
        } else {
            c.route_pub_gen.load(std::sync::atomic::Ordering::Relaxed)
        }
    };
    let mut out = json!({
        "node": c.node_name,
        "region": c.region,
        "gateway": c.public_base,
        "gen": gen,
        "containers": c.gw.container_projects(),
    });
    if since == Some(gen) {
        return out;
    }
    out["hosts"] = json!(hosts);
    out
}

/// Current container placement leases across the mesh (owner, fencing epoch, expiry): the single
/// owner of each stateful container, for visibility and the edge router.
pub(crate) async fn leases_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.leases.list()))
}

/// `{namespace: [{key, hash, timestamp}]}` for every GuardianDB namespace this node holds:
/// content-addressed HEAD metadata only, never values. The anti-entropy loop
/// (`spawn_anti_entropy_loop` in main.rs) diffs one random healthy peer's heads against
/// `guardian::namespace_heads()` to decide whether a real sync is needed; the public mount is
/// `guardian_heads_public`.
pub(crate) async fn guardian_heads(State(_c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(crate::guardian::namespace_heads().await))
}

/// Real cluster resource accounting (operator-only): this node's live CPU/mem/disk/network
/// (sysinfo) plus cluster TOTALS summed over every live node's gossiped `NodeInfo` capacity.
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

#[derive(Deserialize)]
struct SetProjectTeamReq {
    team: String,
}

/// Operator-only repair for a project stuck at `__untagged__` (created before tenancy tagging, or a
/// tag write that never landed): every ownership check refuses it, `norm("__untagged__")` is not
/// the empty-string "personal" case, and `spawn_tenancy_reconcile` only adds a tag it can infer
/// from a deployment or relational record. Mirrors `ProjectStore::set_team` (local write plus the
/// fleet relational mirror).
async fn admin_set_project_team(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(body): Json<SetProjectTeamReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let team = body.team.trim();
    if team.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "team must not be empty".into()));
    }
    c.projects.set_team(&project, team);
    crate::persist::persist(&c);
    Ok(Json(json!({ "project": project, "team": team })))
}

/// Serve a build-cache blob to mesh peers (the P2P side of the build cache).
/// Read-only; peers pull `node_modules` tarballs by content-addressed key.
async fn buildcache_get(
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(key): Path<String>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    if require_fleet_read(claims.as_ref().map(|e| &e.0)).is_err() {
        return Err(StatusCode::FORBIDDEN);
    }
    let is_not_hex_content_hash = key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit());
    if is_not_hex_content_hash {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = crate::git::cache_root().join(format!("{key}.tar"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut headers = vec![
                (
                    axum::http::header::CONTENT_TYPE.as_str().to_string(),
                    "application/x-tar".to_string(),
                ),
                (
                    crate::git::ARTIFACT_SHA_HEADER.to_string(),
                    crate::git::artifact_sha256(&bytes),
                ),
            ];
            if let Some(secret) = crate::git::artifact_secret() {
                headers.push((
                    crate::git::ARTIFACT_SIG_HEADER.to_string(),
                    crate::git::cache_artifact_sig(&secret, &key, &bytes),
                ));
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

/// The tenant (team slug) for a request, by strict trust order: verified JWT claims (the spoofable
/// `x-hive-team` header is then ignored entirely), a platform API key's bound team, the
/// `x-hive-team` header in dev mode only (`!auth::enforced()`), else "personal".
pub(crate) fn tenant(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
) -> String {
    let header_team = h
        .get("x-hive-team")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    resolve_tenant(
        claims,
        api_key_team(c, h),
        header_team,
        crate::auth::enforced(),
    )
}

/// Pure tenant-resolution priority (no request/state deps, so it's unit-testable).
/// JWT claim > API key > (dev-mode only) `x-hive-team` header > "personal".
fn resolve_tenant(
    claims: Option<&crate::auth::Claims>,
    api_team: Option<String>,
    header_team: Option<String>,
    enforced: bool,
) -> String {
    if let Some(cl) = claims {
        return norm(&cl.tenant).to_string();
    }
    if let Some(team) = api_team {
        return team;
    }
    if !enforced {
        if let Some(team) = header_team {
            return team;
        }
        return "personal".into();
    }
    ANON_TENANT.into()
}

/// Namespace for unauthenticated requests under JWT enforcement — owns no data.
/// Deliberately not a valid team slug so nothing is ever created under it.
pub(crate) const ANON_TENANT: &str = "__anon__";

/// Namespace for a STORED RECORD whose tenant tag was lost or never set (pre-tenancy snapshot,
/// stale-binary peer, a restored record never re-normalized, a project with no local row): `norm()`
/// would map the empty tag to the owner's live "personal" tenant and fail OPEN, so a lost tag
/// resolves here and stays invisible to every tenant. Use `record_tenant`, never `norm`, wherever a
/// STORED tag is read.
pub(crate) const UNTAGGED_TENANT: &str = "__untagged__";

/// A STORED record's tenant tag: empty (tag loss) resolves to the fail-closed `UNTAGGED_TENANT`,
/// never the owner's "personal"; `norm()` stays for resolving a REQUEST's tenant.
pub(crate) fn record_tenant(tag: &str) -> &str {
    if tag.trim().is_empty() {
        UNTAGGED_TENANT
    } else {
        tag
    }
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
    if team.is_empty() {
        "personal"
    } else {
        team
    }
}

/// Multi-tenant ownership guard: resolves the caller's tenant and verifies it owns `project` (403
/// otherwise). EVERY handler that takes a project name from the path or body must call it: the path
/// param is attacker-controlled, the tenant is not.
pub(crate) fn require_project(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    project: &str,
) -> Result<String, (StatusCode, String)> {
    let t = tenant(c, h, claims);
    let name_has_no_row = c.projects.get_if_set(project).is_none();
    if name_has_no_row {
        if let Some(tomb) = c.projects.renamed_away(project) {
            return Err(renamed_away_refusal(project, &tomb.new));
        }
    }
    if project_owned_by(c, project, &t) {
        Ok(t)
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "project belongs to a different team".into(),
        ))
    }
}

fn renamed_away_refusal(name: &str, renamed_to: &str) -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        format!(
            "The name \"{}\" is permanently reserved: a project was renamed from it to \"{}\". Choose a different name.",
            name, renamed_to
        ),
    )
}

/// `require_project`, then claims the row a settings write may have just materialized: left unclaimed it
/// stays `UNTAGGED_TENANT` for good when no deploy follows. A no-op when the row is already owned.
fn require_project_claiming_row(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    project: &str,
) -> Result<String, (StatusCode, String)> {
    let t = require_project(c, h, claims, project)?;
    c.projects.set_team(project, &t);
    Ok(t)
}

/// Whether `project` belongs to tenant `t`, judged fleet-aware. Order of evidence: the local
/// settings row; any deployment record (local gateway or gossiped peer copy) carrying the tenant
/// tag; else, with no row AND no deployment evidence anywhere in view, the caller's verified tenant
/// is trusted optimistically (never ANON_TENANT, which owns nothing). A visible deployment record
/// keeps it strict, so a wrong-team caller probing a real project still fails.
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
    if t == ANON_TENANT {
        return false;
    }
    record_tenant(&owner) == UNTAGGED_TENANT && !dep_seen
}

/// Ownership guard for TEAM-level resources: the caller's tenant must equal `slug` (a platform
/// operator may act on any team) and, when `min_role` is non-empty, the caller's OWN role in that
/// team (verified JWT claim) must be one of them. Every `/v1/teams/:slug*` handler must call it:
/// the path `slug` is attacker-controlled.
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
        return Err((
            StatusCode::FORBIDDEN,
            "team belongs to a different tenant".into(),
        ));
    }
    if !min_role.is_empty() {
        let role = claims.map(|cl| cl.role.as_str()).unwrap_or("");
        if !min_role.contains(&role) {
            return Err((
                StatusCode::FORBIDDEN,
                "insufficient team role for this action".into(),
            ));
        }
    }
    Ok(t)
}

/// Ownership guard for DOMAIN resources: get-or-create the record (the first registrant claims it,
/// as `DomainStore::ensure` does) and 403 when a DIFFERENT tenant already registered the domain
/// string. `DomainStore` is keyed globally with no ownership check of its own, so every
/// creating-or-reading `/v1/domains/:domain*` handler must call it.
fn require_domain_owner(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    domain: &str,
) -> Result<String, (StatusCode, String)> {
    let t = tenant(c, h, claims);
    let rec = c.domains.ensure(domain, &t);
    if norm(&rec.tenant) != norm(&t) {
        return Err((
            StatusCode::FORBIDDEN,
            "domain belongs to a different team".into(),
        ));
    }
    Ok(t)
}

/// Like [`require_domain_owner`] but never creates the record (for record-level edit/delete, where
/// `ensure()`'s create-as-a-side-effect would be wrong); a missing domain is left for the
/// underlying call to report NOT_FOUND.
fn require_domain_owner_if_exists(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    domain: &str,
) -> Result<String, (StatusCode, String)> {
    let t = tenant(c, h, claims);
    if let Some(rec) = c.domains.get(domain) {
        let locked = rec.verify.as_ref().is_some_and(|v| v.status == "verified");
        if locked && norm(&rec.tenant) != norm(&t) {
            return Err((
                StatusCode::FORBIDDEN,
                "domain belongs to a different team".into(),
            ));
        }
    }
    Ok(t)
}

/// Pure core of the operator check (no env/global deps, so it is unit-testable). Gates on
/// `platform_admin`, NOT `role`: `role: "owner"` only means owner of `tenant` and every user owns
/// their personal namespace, whereas `platform_admin` is derived at mint time from this backend's
/// own `owner_email`, never client-asserted.
fn operator_allowed(claims: Option<&crate::auth::Claims>, enforced: bool) -> bool {
    if !enforced {
        return true;
    }
    matches!(claims, Some(cl) if cl.platform_admin)
}

/// Platform-operator guard for GLOBAL (non-tenant) infrastructure mutations (WAF rules, bot policy,
/// CDN purge, routing, runtime-cache): enforced auth requires a genuine operator, dev mode is open.
pub(crate) fn require_operator(
    claims: Option<&crate::auth::Claims>,
) -> Result<(), (StatusCode, String)> {
    if operator_allowed(claims, crate::auth::enforced()) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "platform-level change requires a platform operator".into(),
        ))
    }
}

/// Who may read fleet-wide (non-tenant) state from the public listener: a platform operator (the
/// `require_operator` predicate) or a verified mesh-internal `service` delegation (tenant `mesh`,
/// minted by `gossip::fetch` and by `git.rs` build-cache pulls), which is how a peer reaches these
/// routes over the HTTP fallback when iroh is unavailable.
///
/// 403, never 401, so the dashboard's re-mint-and-retry path fires and a caller cannot tell
/// "forbidden" from "absent".
pub(crate) fn require_fleet_read(
    claims: Option<&crate::auth::Claims>,
) -> Result<(), (StatusCode, String)> {
    if !crate::auth::enforced() {
        return Ok(());
    }
    if matches!(claims, Some(cl) if cl.platform_admin || (crate::auth::is_mesh_internal(cl) && cl.tenant == "mesh"))
    {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        "fleet-wide read requires a platform operator or a mesh service token".into(),
    ))
}

/// Public mount for [`serve_hosts`] — see [`require_fleet_read`].
async fn serve_hosts_public(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    q: Option<Query<ServeHostsQ>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_fleet_read(claims.as_ref().map(|e| &e.0))?;
    Ok(serve_hosts(State(c), q).await)
}

/// Public mount for [`leases_get`] — see [`require_fleet_read`].
async fn leases_public(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_fleet_read(claims.as_ref().map(|e| &e.0))?;
    Ok(leases_get(State(c)).await)
}

/// Public mount for [`guardian_heads`] — see [`require_fleet_read`].
async fn guardian_heads_public(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_fleet_read(claims.as_ref().map(|e| &e.0))?;
    Ok(guardian_heads(State(c)).await)
}

/// Public mount for [`fleet_deployments`]; see [`require_fleet_read`].
async fn fleet_deployments_public(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_fleet_read(claims.as_ref().map(|e| &e.0))?;
    Ok(fleet_deployments(State(c)).await)
}

/// Public mount for [`anycast_table`]: the dashboard Network page reads it, so it takes the
/// authenticated-read guard [`require_auth_read`] (any verified session, 401 otherwise), like
/// `/v1/nodes`, and hands the claims on so the table is shaped per reader (operator or sanitized).
async fn anycast_public(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cl = claims.as_ref().map(|e| &e.0);
    require_auth_read(cl)?;
    Ok(anycast_table(State(c), cl).await)
}

/// Public mount for [`functions`]: the same CWE-306 class but tenant-shaped, so it takes
/// [`require_auth_read`] (401 without a session). It cannot stay open: the handler appends every
/// peer's `?local=true` slice, which a peer serves UNFILTERED on purpose (the billing meter's
/// aggregation path), and re-filters only this node's own stats. The mesh leg is unaffected: gossip
/// calls the handler directly and the peer hop carries a mesh token.
async fn functions_public(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    q: axum::extract::Query<LocalQ>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    Ok(functions(State(c), headers, claims, q).await)
}

/// Like [`require_operator`], but also honors the internal node-to-node trust (`x-hive-internal` ==
/// `HIVE_INTERNAL_TOKEN`, constant-time). The run-mutation endpoints that use it forward to the
/// project's HOST node (env decrypts locally) and `post_body_to_host` carries only `x-hive-team`
/// across that hop, so a bare `require_operator` would 403 every legitimately forwarded hop once
/// auth is enforced.
pub(crate) fn require_operator_or_internal(
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
) -> Result<(), (StatusCode, String)> {
    if operator_allowed(claims, crate::auth::enforced()) {
        return Ok(());
    }
    if let Ok(t) = std::env::var("HIVE_INTERNAL_TOKEN") {
        if !t.trim().is_empty()
            && headers
                .get("x-hive-internal")
                .and_then(|v| v.to_str().ok())
                .map(|v| ct_eq(v, &t))
                .unwrap_or(false)
        {
            return Ok(());
        }
    }
    Err((
        StatusCode::FORBIDDEN,
        "platform-level change requires a platform operator".into(),
    ))
}

/// Gate for a node-to-node RPC that the public admin router also mounts (database-replica control,
/// node announce, SMS egress relays). `auth::require_auth` only proves "some valid token", so an
/// ordinary tenant JWT or platform API key reaches the handler unless this runs first: it admits a
/// platform operator, the fleet's `x-hive-internal` secret, or a `mesh-internal` service delegation
/// (`auth::is_mesh_internal`). `team` names the tenant the RPC acts for: a delegation must then be
/// minted for exactly that tenant, so a hop can never act for a tenant its credential was not minted
/// for; `None` takes any delegation. The mesh dispatch arms call the inner handler directly, behind
/// the transport's own signer and trust gate, so a wrapper never changes the mesh path.
pub(crate) fn require_mesh_rpc(
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    team: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    if require_operator_or_internal(headers, claims).is_ok() {
        return Ok(());
    }
    let minted_for_team =
        |cl: &crate::auth::Claims| team.map_or(true, |t| norm(&cl.tenant) == norm(t));
    if claims.is_some_and(|cl| crate::auth::is_mesh_internal(cl) && minted_for_team(cl)) {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        "node-to-node RPC requires a platform operator or a fleet service credential".into(),
    ))
}

/// Authenticated-READ guard for the topology views every signed-in user sees: any verified claims
/// pass (the handler picks the operator or the sanitized tenant view). Missing claims while
/// enforced is 401, not 403, because the dashboard's transparent cookie re-mint keys on 401.
pub(crate) fn require_auth_read(
    claims: Option<&crate::auth::Claims>,
) -> Result<(), (StatusCode, String)> {
    if !crate::auth::enforced() || claims.is_some() {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "sign-in required".into()))
    }
}

/// Like [`require_auth_read`], but also honors the internal node-to-node trust (`x-hive-internal`
/// == `HIVE_INTERNAL_TOKEN`, constant-time), as `require_operator_or_internal` does: for handlers a
/// peer probes for its OWN infra facts on a caller's behalf (e.g. `billing_addons`' leader
/// preflight), a hop that carries no bearer.
pub(crate) fn require_auth_read_or_internal(
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
) -> Result<(), (StatusCode, String)> {
    if require_auth_read(claims).is_ok() {
        return Ok(());
    }
    if let Ok(t) = std::env::var("HIVE_INTERNAL_TOKEN") {
        if !t.trim().is_empty()
            && headers
                .get("x-hive-internal")
                .and_then(|v| v.to_str().ok())
                .map(|v| ct_eq(v, &t))
                .unwrap_or(false)
        {
            return Ok(());
        }
    }
    Err((StatusCode::UNAUTHORIZED, "sign-in required".into()))
}

/// Public ingress region code (the `<dep>.<code>.ngrok.pizza` label); an unknown region returns ""
/// so the dashboard falls back to the legacy region-agnostic zone URL.
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

async fn dep_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let node_region: std::collections::HashMap<String, String> = c
        .registry
        .nodes()
        .into_iter()
        .map(|n| (n.name, n.region))
        .collect();
    let mut host_regions: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();

    let mut list: Vec<_> =
        c.gw.list()
            .into_iter()
            .filter(|d| {
                let record_tag = record_tenant(&d.tenant);
                let effective_owner = if record_tag == UNTAGGED_TENANT {
                    record_tenant(&c.projects.team_of(&d.project)).to_string()
                } else {
                    record_tag.to_string()
                };
                effective_owner == t
            })
            .collect();
    for d in &list {
        host_regions
            .entry(d.id.to_string())
            .or_default()
            .insert(c.region.clone());
    }
    let mut seen: std::collections::HashSet<String> =
        list.iter().map(|d| d.id.to_string()).collect();
    for (node, deps) in c.peer_deployments.read().iter() {
        let nr = node_region.get(node);
        for d in deps {
            if record_tenant(&d.tenant) != t {
                continue;
            }
            if let Some(r) = nr {
                host_regions
                    .entry(d.id.to_string())
                    .or_default()
                    .insert(r.clone());
            }
            if seen.insert(d.id.to_string()) {
                list.push(d.clone());
            }
        }
    }
    list.sort_by_key(|d| std::cmp::Reverse(d.created_at_ms));
    let out: Vec<Value> = list
        .into_iter()
        .map(|d| {
            let hosts = host_regions.get(&d.id.to_string());
            let cfg = c
                .projects
                .get(&d.project)
                .functions
                .regions
                .first()
                .cloned();
            let region = match (hosts, &cfg) {
                (Some(set), Some(c)) if set.contains(c) => c.clone(),
                (Some(set), _) => set
                    .iter()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "san-jose".into()),
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
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let creator = req.creator.clone().unwrap_or_else(|| "you".into());
    let mut manifest = req.manifest;
    let project = manifest.project.clone();
    let _lifecycle = crate::project_settings::lifecycle_write(&project).await;
    let row = c.projects.get_if_set(&project);
    let seen_deployment =
        c.gw.list()
            .iter()
            .any(|deployment| deployment.project == project)
            || c.peer_deployments
                .read()
                .values()
                .flatten()
                .any(|deployment| deployment.project == project);
    if row.is_some() || seen_deployment {
        if !project_owned_by(&c, &project, &t) {
            return Err((
                StatusCode::FORBIDDEN,
                "project belongs to a different team".into(),
            ));
        }
        if row.is_none() {
            return Err((
                StatusCode::CONFLICT,
                "project incarnation authority is still converging; retry the deployment".into(),
            ));
        }
    }
    let legacy_ids = row
        .as_ref()
        .is_some_and(|settings| settings.incarnation.is_none())
        .then(|| legacy_deployment_ids_for_owner(&c, &project, &t))
        .unwrap_or_default();
    let incarnation = match c.projects.active_incarnation(&project) {
        Ok(incarnation) => incarnation,
        Err(crate::project_settings::ProjectIncarnationError::Legacy) => c
            .projects
            .ensure_incarnation(&project)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?,
        Err(crate::project_settings::ProjectIncarnationError::Missing) => c
            .projects
            .create(&project)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?,
        Err(error) => return Err((StatusCode::CONFLICT, error.to_string())),
    };
    c.projects
        .set_team_exact(&project, incarnation, &t)
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    if !legacy_ids.is_empty() {
        adopt_authorized_legacy_deployments(&c, &project, incarnation, legacy_ids)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    }
    for f in &mut manifest.functions {
        if f.browser.is_some() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "function {:?} opts into browser serving, but POST /deployments runs no bundle/persist step; browser opt-ins must deploy through the git build path",
                    f.name
                ),
            ));
        }
        f.browser_artifact = None;
    }
    let volume_names = if manifest
        .functions
        .iter()
        .any(|function| function.runtime == "container")
    {
        let names = crate::git::project_volume_names(&project, incarnation, &manifest.functions)
            .map_err(|reason| (StatusCode::BAD_REQUEST, reason))?;
        if names.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "container deployment has no exact current-incarnation volume claim".into(),
            ));
        }
        Some(names)
    } else {
        None
    };
    if let Err(e) =
        crate::raw_ports::allocate_raw_ports_coordinated(&c, &project, &mut manifest).await
    {
        tracing::warn!(project = %project, error = %e, "raw public-port allocation failed for direct deploy (raw ingress unavailable)");
    }
    if let Some(volume_names) = volume_names {
        c.projects
            .claim_volumes_exact(&project, incarnation, volume_names)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    }
    let info = c.gw.deploy_full_exact(
        req.root,
        manifest,
        creator,
        req.git,
        req.production,
        fluid_core::DeployState::Ready,
        t,
        incarnation,
    );
    causal_stamp_new_deployment(&c, &project, &info.id.0);
    crate::persist::persist(&c);
    Ok(Json(json!(info)))
}

/// Whether a just-registered deployment record is still authorized to exist. A captured incarnation
/// must still match the project's CURRENT active one (as `persist::restore`'s `Some` arm does); the
/// legacy `None` case has no captured fact to re-check, so it is authorized UNLESS the current row
/// proves a DIFFERENT, REAL incarnation is active (mere past deletion, or a row with no
/// incarnation, is never proof of staleness: preventing the interleaving race is
/// `lifecycle_write`'s job, not this heuristic's).
fn deployment_authorized(
    c: &Arc<CloudState>,
    project: &str,
    incarnation: Option<fluid_core::ProjectIncarnation>,
) -> bool {
    let snap = c.projects.snapshot_synced();
    match incarnation {
        Some(expected) => snap
            .rows
            .get(project)
            .is_some_and(|row| row.incarnation == Some(expected)),
        None => !snap
            .rows
            .get(project)
            .is_some_and(|row| row.incarnation.is_some()),
    }
}

/// Ids of the legacy (incarnation-less) deployment records of `project` that `team` owns or that
/// carry no tenant tag, from the local gateway and the gossiped peer copies; sorted and deduped.
pub(crate) fn legacy_deployment_ids_for_owner(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
) -> Vec<String> {
    let team = norm(team).to_string();
    let local = c.gw.deployment_records().into_iter().map(|record| {
        (
            record.id,
            record.project,
            record.project_incarnation,
            record.tenant,
        )
    });
    let peers: Vec<(
        String,
        String,
        Option<fluid_core::ProjectIncarnation>,
        String,
    )> = c
        .peer_deployments
        .read()
        .values()
        .flatten()
        .cloned()
        .map(|record| {
            (
                record.id.0,
                record.project,
                record.project_incarnation,
                record.tenant,
            )
        })
        .collect();
    let mut ids: Vec<String> = local
        .chain(peers)
        .filter(|(_, rec_project, incarnation, tenant)| {
            rec_project == project && incarnation.is_none() && {
                let owner = record_tenant(tenant);
                owner == team || owner == UNTAGGED_TENANT
            }
        })
        .map(|(id, _, _, _)| id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Persists and applies the exact-id migration authority for records observed before a live legacy
/// settings row acquired its first incarnation; the caller holds the project's lifecycle writer,
/// and a fresh or tombstone-ambiguous row yields an empty allow-set that adopts nothing.
pub(crate) fn adopt_authorized_legacy_deployments(
    c: &Arc<CloudState>,
    project: &str,
    incarnation: fluid_core::ProjectIncarnation,
    ids: Vec<String>,
) -> Result<Vec<String>, crate::project_settings::ProjectIncarnationError> {
    let allowed = c
        .projects
        .authorize_legacy_deployments(project, incarnation, ids)?;
    let adopted =
        c.gw.adopt_legacy_deployments(project, incarnation, &allowed);
    if !adopted.is_empty() {
        crate::persist::persist(c);
    }
    Ok(adopted)
}

fn authorize_local_deployment_incarnation(
    c: &Arc<CloudState>,
    project: &str,
    deployment_id: &str,
    recorded: Option<fluid_core::ProjectIncarnation>,
    team: &str,
) -> Result<fluid_core::ProjectIncarnation, StatusCode> {
    if let Some(expected) = recorded {
        c.projects
            .get_exact(project, expected)
            .map_err(|_| StatusCode::CONFLICT)?;
        return Ok(expected);
    }

    let incarnation = match c.projects.active_incarnation(project) {
        Ok(incarnation) => incarnation,
        Err(crate::project_settings::ProjectIncarnationError::Legacy) => {
            let ids = legacy_deployment_ids_for_owner(c, project, team);
            let incarnation = c
                .projects
                .ensure_incarnation(project)
                .map_err(|_| StatusCode::CONFLICT)?;
            adopt_authorized_legacy_deployments(c, project, incarnation, ids)
                .map_err(|_| StatusCode::CONFLICT)?;
            incarnation
        }
        Err(_) => return Err(StatusCode::CONFLICT),
    };
    let allowed = c
        .projects
        .legacy_deployment_ids_exact(project, incarnation)
        .map_err(|_| StatusCode::CONFLICT)?;
    if !allowed.iter().any(|id| id == deployment_id) {
        return Err(StatusCode::CONFLICT);
    }
    c.gw.adopt_legacy_deployments(project, incarnation, &allowed);
    c.gw.deployment_records()
        .iter()
        .any(|record| record.id == deployment_id && record.project_incarnation == Some(incarnation))
        .then_some(incarnation)
        .ok_or(StatusCode::CONFLICT)
}

/// Floors a fresh deployment's causal stamp at the project tombstone + 1
/// (`ProjectStore::tombstone_of`), but only once the record has proven it is still authorized to
/// exist. The ONE chokepoint every registration path (`POST /deployments`, `run_build`, the
/// first-deploy "Building…" placeholder) calls right after `gw.deploy_full[_with_runtime]`, a plain
/// sync `fn` so its callers need no edit. It closes the race where `run_build`'s "still current"
/// check passes BEFORE a concurrent `delete_project_local` tombstones the project and the build
/// then registers AFTER: a rejected registration is torn down completely, detached (no deployment,
/// alias or causal stamp survives a delete it lost the race against).
pub(crate) fn causal_stamp_new_deployment(c: &Arc<CloudState>, project: &str, id: &str) {
    let incarnation =
        c.gw.list()
            .into_iter()
            .find(|d| d.id.0 == id)
            .and_then(|d| d.project_incarnation);
    if !deployment_authorized(c, project, incarnation) {
        let (c2, id2, project2) = (c.clone(), id.to_string(), project.to_string());
        tokio::spawn(async move {
            let _lifecycle = crate::project_settings::lifecycle_write(&project2).await;
            let mut retained_authorized = false;
            let removed = match incarnation {
                Some(expected) => match c2.projects.get_exact(&project2, expected) {
                    Err(_) => c2.gw.remove_exact(&id2, expected).await,
                    Ok(_) => {
                        if let Some(tombstone) = c2.projects.tombstone_of(&project2) {
                            c2.gw.restamp_created(&id2, tombstone.saturating_add(1));
                        }
                        retained_authorized = true;
                        None
                    }
                },
                None => {
                    tracing::error!(
                        project = %project2,
                        deployment = %id2,
                        "rejected an identity-less deployment registration but retained it: no exact incarnation exists for safe removal"
                    );
                    None
                }
            };
            tracing::warn!(
                project = %project2,
                deployment = %id2,
                removed = removed.is_some(),
                retained_authorized,
                "serialized resolution of a deployment registration that initially lost project-incarnation authority"
            );
            crate::persist::persist(&c2);
        });
        return;
    }
    if let Some(t) = c.projects.tombstone_of(project) {
        c.gw.restamp_created(id, t.saturating_add(1));
    }
}

/// Roll back / promote: make an existing deployment the project's production.
pub(crate) async fn dep_promote(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t0 = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(d) = c.gw.list().into_iter().find(|d| d.id.0 == id) {
        if record_tenant(&d.tenant) != t0 && !project_owned_by(&c, &d.project, &t0) {
            return Err(StatusCode::NOT_FOUND);
        }
        let project = d.project.clone();
        let _lifecycle = crate::project_settings::lifecycle_write(&project).await;
        let selected =
            c.gw.list()
                .into_iter()
                .find(|candidate| candidate.id.0 == id)
                .ok_or(StatusCode::NOT_FOUND)?;
        let incarnation = authorize_local_deployment_incarnation(
            &c,
            &project,
            &id,
            selected.project_incarnation,
            &t0,
        )?;
        let info =
            c.gw.promote_exact(&id, incarnation)
                .ok_or(StatusCode::CONFLICT)?;
        crate::persist::persist(&c);
        let ev = c.event(
            &c.region,
            "PROMOTE",
            &info.alias,
            "/",
            200,
            "deploy",
            &format!("rolled back to {id}"),
        );
        c.record(ev);
        crate::webhooks::dispatch(
            &c.webhooks,
            &t0,
            &info.project,
            "deployment.promoted",
            json!({ "id": id, "project": info.project, "url": c.deploy_url(&info.alias) }),
        );
        return Ok(Json(json!(info)));
    }
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) =
            post_to_host(&c, &node, &format!("/v1/deployments/{id}/promote"), &t0).await
        {
            if let Some(project) = v.get("project").and_then(|p| p.as_str()) {
                c.gw.set_project_alias(project, &id);
                crate::persist::persist(&c);
            }
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// POST (no body) to a host node by NAME: HTTP admin when its URL is known, else the iroh mesh; the
/// mutation counterpart of `fetch_from_host`, used to proxy an action (e.g. instant rollback) to a
/// deployment's hosting node.
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
        if let Some(b) = crate::gossip::request_to(
            c,
            &id,
            &addr,
            hive_p2p::GOSSIP_POST,
            &p,
            &[],
            MESH_FORWARD_TIMEOUT_SECS,
        )
        .await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// DELETE (no body) to a host node by NAME, the same HTTP-then-mesh shape as `post_to_host`.
/// `hive_p2p` has only GOSSIP_GET/GOSSIP_POST, so the mesh fallback rides GOSSIP_POST like every
/// delete-shaped mutation (the receiving arm tells create from delete by PATH SHAPE, a trailing
/// snapshot-id segment, not by verb); used by `storage_api`'s snapshot delete.
pub(crate) async fn delete_to_host(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
) -> Option<Value> {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        if let Ok(r) = c
            .http
            .delete(format!("{admin}{path}"))
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
        if let Some(b) = crate::gossip::request_to(
            c,
            &id,
            &addr,
            hive_p2p::GOSSIP_POST,
            &p,
            &[],
            MESH_FORWARD_TIMEOUT_SECS,
        )
        .await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// PUT (JSON body) to a host node by NAME: HTTP admin PUT when its URL is known (the `/network`
/// route is registered only as `put(...)`, a `.post()` there would 405), else over the iroh mesh as
/// `GOSSIP_POST` (the mesh has no HTTP verb, only path and body; see the `/network` arm in
/// `gossip::dispatch`). The body-carrying counterpart of `post_to_host`, used to forward
/// `PUT /v1/projects/:project/network` when the record is not local.
pub(crate) async fn put_to_host(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
    body: &Value,
) -> Option<Value> {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        if let Ok(r) = c
            .http
            .put(format!("{admin}{path}"))
            .header("x-hive-team", team)
            .timeout(std::time::Duration::from_secs(15))
            .json(body)
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
        let body_bytes = serde_json::to_vec(body).unwrap_or_default();
        if let Some(b) =
            crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &body_bytes, 20)
                .await
        {
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

/// Deletes a single deployment under the same project lifecycle writer as build finalization,
/// promotion and full project deletion; the owned guard moves into a task before the first
/// cancellable await, so dropping the HTTP request drops only the waiter, never the serialized
/// deletion.
async fn dep_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(initial) =
        c.gw.list()
            .into_iter()
            .find(|deployment| deployment.id.0 == id)
    else {
        return Ok(Json(json!({ "removed": id, "project": null })));
    };
    if record_tenant(&initial.tenant) != team && !project_owned_by(&c, &initial.project, &team) {
        return Err(StatusCode::NOT_FOUND);
    }
    let project = initial.project.clone();
    let lifecycle = crate::project_settings::lifecycle_write(&project).await;
    let c2 = c.clone();
    let id2 = id.clone();
    let project2 = project.clone();
    let team2 = team.clone();
    let task = tokio::spawn(async move {
        let _lifecycle = lifecycle;
        let selected = c2
            .gw
            .list()
            .into_iter()
            .find(|deployment| deployment.id.0 == id2)
            .ok_or(StatusCode::NOT_FOUND)?;
        if selected.project != project2
            || (record_tenant(&selected.tenant) != team2
                && !project_owned_by(&c2, &project2, &team2))
        {
            return Err(StatusCode::NOT_FOUND);
        }
        let incarnation = authorize_local_deployment_incarnation(
            &c2,
            &project2,
            &id2,
            selected.project_incarnation,
            &team2,
        )?;
        let removed = c2
            .gw
            .remove_exact(&id2, incarnation)
            .await
            .ok_or(StatusCode::CONFLICT)?;
        let image = removed.manifest.image.clone();
        record_event(
            &c2,
            &project2,
            "delete",
            &format!("deleted deployment {id2}"),
        );
        if !c2
            .gw
            .list()
            .iter()
            .any(|deployment| deployment.project == project2)
        {
            let retired = crate::raw_ports::release_raw_ports_coordinated(&c2, &project2).await;
            if !retired.is_empty() {
                tracing::info!(project = %project2, ports = ?retired, "retired public raw port(s) — last deployment deleted; never re-granted");
            }
        }
        let image_unreferenced = image
            .as_deref()
            .is_some_and(|candidate| !c2.gw.image_is_referenced(candidate));
        crate::persist::persist(&c2);
        Ok::<_, StatusCode>((removed, image, image_unreferenced))
    });
    let (removed, image, image_unreferenced) = task.await.map_err(|error| {
        tracing::error!(deployment = %id, %error, "serialized deployment deletion task failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;
    Ok(Json(json!({
        "removed": removed.id,
        "project": removed.project,
        "image": image,
        "image_last_reference_removed": image_unreferenced,
    })))
}

#[derive(Clone)]
struct ProjectDeleteInventory {
    incarnation: fluid_core::ProjectIncarnation,
    deployments: Vec<fluid_core::DeployRecord>,
    ambiguous_deployments: Vec<fluid_core::DeployRecord>,
    databases: Vec<crate::databases::Database>,
    ambiguous_databases: Vec<crate::databases::Database>,
    build_ids: std::collections::BTreeSet<String>,
    current_volumes: std::collections::BTreeSet<String>,
    legacy_volumes: std::collections::BTreeSet<String>,
    volume_residuals: Vec<ProjectCleanupResidual>,
    images: std::collections::BTreeSet<String>,
}

fn path_build_id(
    project: &str,
    path: &std::path::Path,
    roots: &[std::path::PathBuf; 2],
) -> Option<String> {
    let parent = path.parent()?;
    if !roots.iter().any(|root| parent == root) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    crate::project_settings::checkout_prefixes_including_previous(project)
        .iter()
        .find_map(|prefix| deploy_entry_build_id(prefix, name))
        .map(str::to_string)
}

fn project_delete_inventory(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    settings: &crate::project_settings::ProjectSettings,
    active_checkouts: &[std::path::PathBuf],
) -> ProjectDeleteInventory {
    let incarnation = settings
        .incarnation
        .expect("delete inventory is built only after exact incarnation authority");
    let all_project_deployments: Vec<fluid_core::DeployRecord> =
        c.gw.deployment_records()
            .into_iter()
            .filter(|deployment| deployment.project == project)
            .collect();
    let (deployments, ambiguous_deployments): (Vec<_>, Vec<_>) = all_project_deployments
        .into_iter()
        .partition(|deployment| deployment.project_incarnation == Some(incarnation));
    let roots = [crate::git::deploy_root(), crate::git::legacy_deploy_root()];
    let mut build_ids: std::collections::BTreeSet<String> = c
        .builds
        .ids_for_incarnation(project, incarnation)
        .into_iter()
        .filter(|id| is_platform_build_id(id))
        .collect();
    for deployment in &deployments {
        if let Some(source) = deployment.git.as_ref() {
            if let Some(build_id) = crate::git::upload_source_build_id(source) {
                if is_platform_build_id(&build_id) {
                    build_ids.insert(build_id);
                }
            }
        }
        if let Some(build_id) =
            path_build_id(project, std::path::Path::new(&deployment.root), &roots)
        {
            build_ids.insert(build_id);
        }
    }
    for path in active_checkouts {
        if let Some(build_id) = path_build_id(project, path, &roots) {
            build_ids.insert(build_id);
        }
    }

    let services: std::collections::BTreeSet<String> = deployments
        .iter()
        .flat_map(|deployment| deployment.manifest.functions.iter())
        .filter(|function| function.runtime == "container")
        .map(|function| function.name.clone())
        .collect();
    let project_tag = crate::git::sanitize_tag(project);
    let legacy_base = format!("hive-vol-{project_tag}");

    let mut current_volumes: std::collections::BTreeSet<String> =
        settings.owned_volume_names.iter().cloned().collect();
    let mut volume_residuals = Vec::new();
    for deployment in &deployments {
        if !deployment
            .manifest
            .functions
            .iter()
            .any(|function| function.runtime == "container")
        {
            continue;
        }
        match crate::git::project_volume_names(project, incarnation, &deployment.manifest.functions)
        {
            Ok(names) => current_volumes.extend(names),
            Err(reason) => volume_residuals.push(ProjectCleanupResidual::new(
                "volume_ownership",
                &deployment.id,
                format!("retained: {reason}"),
            )),
        }
    }

    let mut legacy_volumes = std::collections::BTreeSet::from([legacy_base.clone()]);
    for service in &services {
        legacy_volumes.insert(format!(
            "{legacy_base}-{}",
            crate::git::sanitize_tag(service)
        ));
    }
    let images = deployments
        .iter()
        .filter_map(|deployment| deployment.manifest.image.clone())
        .collect();

    let owned_database_ids: std::collections::HashSet<&str> = settings
        .owned_database_ids
        .iter()
        .map(String::as_str)
        .collect();
    let (databases, ambiguous_databases): (Vec<_>, Vec<_>) = c
        .databases
        .list(Some(project))
        .into_iter()
        .partition(|database| {
            record_tenant(&database.team) == norm(team)
                && owned_database_ids.contains(database.id.as_str())
        });

    ProjectDeleteInventory {
        incarnation,
        deployments,
        ambiguous_deployments,
        databases,
        ambiguous_databases,
        build_ids,
        current_volumes,
        legacy_volumes,
        volume_residuals,
        images,
    }
}

/// Runs one destructive container/volume command under a hard per-attempt budget; `kill_on_drop`
/// makes a timeout real, so no child survives to mutate the host after the lifecycle writer has
/// returned an incomplete-cleanup result.
async fn run_cleanup_command(bin: &str, args: Vec<String>, path_env: &str) -> Result<(), String> {
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        let mut command = tokio::process::Command::new(bin);
        command
            .args(&args)
            .env("PATH", path_env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let result = tokio::time::timeout(Duration::from_secs(10), command.output()).await;
        let reason = match result {
            Ok(Ok(output)) if output.status.success() => return Ok(()),
            Ok(Ok(output)) => format!(
                "exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(Err(error)) => format!("could not start: {error}"),
            Err(_) => "timed out after 10s".to_string(),
        };
        if attempt == ATTEMPTS {
            return Err(format!("{reason} after {ATTEMPTS} attempts"));
        }
        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
    }
    unreachable!("bounded cleanup-command loop always returns")
}

/// Reclaims only the immutable inventory captured for one exact incarnation; the caller owns the
/// project lifecycle writer across the whole await chain, so database creation and deployment
/// registration cannot enter a new lifetime between proof and removal.
async fn purge_project_resources(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    delete_ms: u64,
    inventory: &ProjectDeleteInventory,
) -> Vec<ProjectCleanupResidual> {
    let mut residuals = Vec::new();
    c.audit.record(
        team,
        "user",
        "delete",
        "project",
        project,
        &format!(
            "incarnation={} delete_ms={} deployments={}",
            inventory.incarnation,
            delete_ms,
            inventory.deployments.len()
        ),
    );

    for deployment in &inventory.ambiguous_deployments {
        residuals.push(ProjectCleanupResidual::new(
            "deployment_ownership",
            &deployment.id,
            format!(
                "retained: deployment does not carry exact incarnation {}",
                inventory.incarnation
            ),
        ));
    }
    for database in &inventory.ambiguous_databases {
        residuals.push(ProjectCleanupResidual::new(
            "database_ownership",
            &database.id,
            format!(
                "retained: database team/creation stamp does not prove ownership by incarnation {}",
                inventory.incarnation
            ),
        ));
    }
    residuals.extend(inventory.volume_residuals.iter().cloned());

    let path_env = format!(
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    for database in &inventory.databases {
        crate::databases::note_teardown_request(&database.id);
        if !database.replicas.is_empty() {
            crate::db_replicate::remove_replicas(c.clone(), database.clone());
        }
        if let Some(containers) = database.container.as_deref() {
            for container in containers
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                if let Err(reason) = run_cleanup_command(
                    "podman",
                    hive_backend::container_cli::rm_args(false, container),
                    &path_env,
                )
                .await
                {
                    residuals.push(ProjectCleanupResidual::new(
                        "database_container",
                        container,
                        reason,
                    ));
                }
            }
        }
        if database.kind == crate::databases::DbKind::Supabase {
            let short = database
                .id
                .strip_prefix("db_")
                .and_then(|suffix| suffix.get(..8));
            match short {
                Some(short) => {
                    for volume in [
                        format!("hive-vol-supa-{short}"),
                        format!("hive-vol-supast-{short}"),
                    ] {
                        if let Err(reason) = run_cleanup_command(
                            "podman",
                            hive_backend::container_cli::volume_rm_args(false, &volume),
                            &path_env,
                        )
                        .await
                        {
                            residuals.push(ProjectCleanupResidual::new(
                                "database_volume",
                                volume,
                                reason,
                            ));
                        }
                    }
                }
                None => residuals.push(ProjectCleanupResidual::new(
                    "database_identity",
                    &database.id,
                    "retained backing volume: database id is not platform-issued db_<8+>",
                )),
            }
        }
        c.databases
            .remove_db_and_purge_data(&database.id, &database.team);
    }

    residuals.extend(
        purge_project_podman_volumes(&inventory.current_volumes, &inventory.legacy_volumes).await,
    );
    let source_residuals = purge_project_source_dirs(project, &inventory.build_ids).await;
    let source_complete = source_residuals.is_empty();
    residuals.extend(source_residuals);
    if source_complete {
        c.builds
            .remove_for_incarnation(project, inventory.incarnation);
    } else {
        residuals.push(ProjectCleanupResidual::new(
            "build_history",
            inventory.incarnation.to_string(),
            "retained because exact source cleanup is incomplete",
        ));
    }

    let retired = crate::raw_ports::release_raw_ports_coordinated(c, project).await;
    if !retired.is_empty() {
        tracing::info!(project, ports = ?retired, "retired public raw port(s) on exact project-incarnation delete — never re-granted");
    }

    for image in &inventory.images {
        if !c.gw.image_is_referenced(image) && c.gw.backend_name() == "litebox" {
            residuals.push(ProjectCleanupResidual::new(
                "litebox_image_reference",
                image,
                "authoritative last reference removed, but the active LiteboxBackend instance is not reachable through the authorized gateway surface",
            ));
        }
    }

    crate::persist::persist(c);
    residuals
}

/// Enumerates a backend first, then removes only incarnation-qualified names that are present;
/// legacy names are byte-ambiguous (service `v2` of `app` and project `app-v2` both map to
/// `hive-vol-app-v2`), so a present legacy candidate is retained and returned as an observable
/// residual.
async fn purge_project_podman_volumes(
    current: &std::collections::BTreeSet<String>,
    legacy: &std::collections::BTreeSet<String>,
) -> Vec<ProjectCleanupResidual> {
    const ATTEMPTS: u32 = 3;
    let mut residuals = Vec::new();
    let path_env = format!(
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    let backends: &[bool] = if hive_backend::container_cli::is_apple_default() {
        &[false, true]
    } else {
        &[false]
    };
    for &apple in backends {
        let bin = hive_backend::container_cli::bin(apple);
        let mut names = None;
        let mut last_reason = String::new();
        for attempt in 1..=ATTEMPTS {
            match tokio::time::timeout(
                Duration::from_secs(10),
                hive_backend::container_cli::try_list_volume_names(apple, &path_env),
            )
            .await
            {
                Ok(Ok(found)) => {
                    names = Some(found);
                    break;
                }
                Ok(Err(reason)) => last_reason = reason,
                Err(_) => last_reason = "volume enumeration timed out after 10s".to_string(),
            }
            if attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
            }
        }
        let Some(names) = names else {
            residuals.push(ProjectCleanupResidual::new(
                "volume_enumeration",
                bin,
                format!("{last_reason} after {ATTEMPTS} attempts; no volume was removed"),
            ));
            continue;
        };
        let names: std::collections::HashSet<String> = names.into_iter().collect();
        for name in legacy.iter().filter(|name| names.contains(*name)) {
            residuals.push(ProjectCleanupResidual::new(
                "legacy_volume_ownership",
                name,
                "retained: legacy volume name is byte-ambiguous with a prefix-sharing project/service",
            ));
        }
        for name in current.iter().filter(|name| names.contains(*name)) {
            if let Err(reason) = run_cleanup_command(
                bin,
                hive_backend::container_cli::volume_rm_args(apple, name),
                &path_env,
            )
            .await
            {
                residuals.push(ProjectCleanupResidual::new("volume_remove", name, reason));
            }
        }
    }
    residuals
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProjectCleanupResidual {
    pub resource: String,
    pub id: String,
    pub reason: String,
}

impl ProjectCleanupResidual {
    fn new(resource: &str, id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            resource: resource.to_string(),
            id: id.into(),
            reason: reason.into(),
        }
    }
}

/// A build id is always platform-issued: `dpl-` + 10 lowercase-hex chars.
fn is_platform_build_id(id: &str) -> bool {
    id.strip_prefix("dpl-").is_some_and(|suffix| {
        suffix.len() == 10
            && suffix
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Parses one current/legacy checkout, placeholder or retained-source name: syntax only identifies
/// the embedded build id, deletion additionally checks it against the immutable incarnation
/// lineage.
fn deploy_entry_build_id<'a>(prefix: &str, name: &'a str) -> Option<&'a str> {
    let rest = name.strip_prefix(prefix)?;
    if let Some(build_id) = rest.strip_suffix(".src.zip") {
        return is_platform_build_id(build_id).then_some(build_id);
    }
    let rest = rest.strip_prefix("building-").unwrap_or(rest);
    let (stamp, build_id) = rest.split_once('-')?;
    (!stamp.is_empty()
        && stamp.bytes().all(|byte| byte.is_ascii_digit())
        && is_platform_build_id(build_id))
    .then_some(build_id)
}

fn owned_deploy_entry(
    prefix: &str,
    name: &str,
    build_ids: &std::collections::BTreeSet<String>,
) -> bool {
    deploy_entry_build_id(prefix, name).is_some_and(|build_id| build_ids.contains(build_id))
}

/// Re-stats with `symlink_metadata` on every bounded attempt so a symlink is unlinked, never
/// traversed: files and symlinks use `remove_file`, real directories `remove_dir_all`, unsupported
/// special files are kept and reported.
async fn remove_owned_deploy_entry(path: &std::path::Path) -> Option<ProjectCleanupResidual> {
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) if attempt < ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                continue;
            }
            Err(error) => {
                return Some(ProjectCleanupResidual::new(
                    "source_metadata",
                    path.display().to_string(),
                    format!("symlink_metadata failed after {ATTEMPTS} attempts: {error}"),
                ));
            }
        };
        let file_type = metadata.file_type();
        let result = if file_type.is_file() || file_type.is_symlink() {
            tokio::fs::remove_file(path).await
        } else if file_type.is_dir() {
            tokio::fs::remove_dir_all(path).await
        } else {
            return Some(ProjectCleanupResidual::new(
                "source_special",
                path.display().to_string(),
                "unsupported filesystem entry retained",
            ));
        };
        match result {
            Ok(()) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) if attempt < ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
            }
            Err(error) => {
                return Some(ProjectCleanupResidual::new(
                    "source_remove",
                    path.display().to_string(),
                    format!("removal failed after {ATTEMPTS} attempts: {error}"),
                ));
            }
        }
    }
    unreachable!("bounded source-removal loop always returns")
}

async fn enumerate_owned_deploy_entries(
    base: &std::path::Path,
    prefixes: &[String],
    build_ids: &std::collections::BTreeSet<String>,
) -> Result<Vec<std::path::PathBuf>, String> {
    const ATTEMPTS: u32 = 3;
    let mut last_reason = String::new();
    for attempt in 1..=ATTEMPTS {
        let mut entries = match tokio::fs::read_dir(base).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                last_reason = format!("read_dir failed: {error}");
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                }
                continue;
            }
        };
        let mut targets = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if prefixes
                        .iter()
                        .any(|prefix| owned_deploy_entry(prefix, &name, build_ids))
                    {
                        targets.push(entry.path());
                    }
                }
                Ok(None) => return Ok(targets),
                Err(error) => {
                    last_reason = format!("next_entry failed: {error}");
                    break;
                }
            }
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
        }
    }
    Err(format!("{last_reason} after {ATTEMPTS} attempts"))
}

/// Removes only current/legacy source entries whose platform-issued build id is in the exact
/// deleted-incarnation lineage; enumeration, metadata, special-type and removal failures stay
/// observable in the returned residual set.
async fn purge_project_source_dirs(
    project: &str,
    build_ids: &std::collections::BTreeSet<String>,
) -> Vec<ProjectCleanupResidual> {
    let prefixes = crate::project_settings::checkout_prefixes_including_previous(project);
    let mut residuals = Vec::new();
    for base in [crate::git::deploy_root(), crate::git::legacy_deploy_root()] {
        let targets = match enumerate_owned_deploy_entries(&base, &prefixes, build_ids).await {
            Ok(targets) => targets,
            Err(reason) => {
                residuals.push(ProjectCleanupResidual::new(
                    "source_enumeration",
                    base.display().to_string(),
                    reason,
                ));
                continue;
            }
        };
        for path in targets {
            if let Some(residual) = remove_owned_deploy_entry(&path).await {
                residuals.push(residual);
            }
        }
    }
    residuals
}

/// Whether a `project_delete`/rename caller asserted SOME identity at all: verified claims (JWT or
/// platform API key) or an explicit `x-hive-team` header (a blank one counts as none, as in
/// `resolve_tenant`). `resolve_tenant`'s dev-mode "personal" default owns every project, so it must
/// never authorize a destructive fleet-cascading operation; this is the pure core of that gate,
/// unit-testable without a `HeaderMap`/`CloudState`.
fn has_explicit_caller_identity(claims_present: bool, team_header: Option<&str>) -> bool {
    claims_present || team_header.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

fn delete_forward_is_internal(headers: &HeaderMap, claims: Option<&crate::auth::Claims>) -> bool {
    let service_claim = claims.is_some_and(crate::auth::is_mesh_internal);
    let shared_secret = std::env::var("HIVE_INTERNAL_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .is_some_and(|t| {
            headers
                .get("x-hive-internal")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| ct_eq(v, &t))
        });
    service_claim || shared_secret
}

/// A delete generation may lead the receiver's clock only by the skew the origin intentionally adds
/// plus this receiver's opposite-direction skew; older generations stay valid forever for
/// idempotent retries.
pub(crate) const MAX_DELETE_GENERATION_FUTURE_MS: u64 =
    hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS * 2;

pub(crate) fn valid_delete_generation(delete_ms: u64) -> bool {
    delete_ms > 0 && delete_ms <= now_ms().saturating_add(MAX_DELETE_GENERATION_FUTURE_MS)
}

/// Newest causal version this coordinator can see for `project`: a fresh deletion generation is
/// issued strictly above it, so it dominates settings and deployment records already visible
/// anywhere in the fleet.
fn observed_project_version_ms(c: &Arc<CloudState>, project: &str) -> u64 {
    let settings_ms = c
        .projects
        .get_if_set(project)
        .map(|s| s.updated_ms)
        .unwrap_or(0);
    let local_ms =
        c.gw.list()
            .into_iter()
            .filter(|d| d.project == project)
            .map(|d| d.created_at_ms)
            .max()
            .unwrap_or(0);
    let peer_ms = c
        .peer_deployments
        .read()
        .values()
        .flatten()
        .filter(|d| d.project == project)
        .map(|d| d.created_at_ms)
        .max()
        .unwrap_or(0);
    let observed = settings_ms.max(local_ms).max(peer_ms);
    let ceiling = now_ms().saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS);
    if observed > ceiling {
        tracing::warn!(
            project,
            observed_ms = observed,
            ceiling_ms = ceiling,
            "project version is implausibly future-dated; bounding delete-generation input"
        );
        ceiling
    } else {
        observed
    }
}

async fn project_delete_capabilities() -> Json<Value> {
    Json(json!({ "delete_generation": "v1" }))
}

/// Deletes an entire project (deployments + settings), cascading across the mesh by default;
/// `?cascade=false` is the internal single-hop form used by a cascade receiver. Relocation uses the
/// separate, narrowly scoped `/reap-deployments` arm and never calls project deletion.
async fn project_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Query(q): Query<CascadeQ>,
) -> Result<Json<Value>, StatusCode> {
    if !has_explicit_caller_identity(
        claims.is_some(),
        headers.get("x-hive-team").and_then(|v| v.to_str().ok()),
    ) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let cascade = q.cascade.unwrap_or(true);
    let supplied_delete_ms = q.delete_ms;
    if supplied_delete_ms.is_some()
        && (cascade || !delete_forward_is_internal(&headers, claims.as_ref().map(|e| &e.0)))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if supplied_delete_ms.is_some_and(|ms| !valid_delete_generation(ms)) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let authorized = c
        .projects
        .get_if_set(&project)
        .map(|s| norm(&s.team) == t)
        .unwrap_or(false)
        || c.gw
            .list()
            .iter()
            .any(|d| d.project == project && record_tenant(&d.tenant) == t)
        || c.peer_deployments
            .read()
            .values()
            .flatten()
            .any(|d| d.project == project && record_tenant(&d.tenant) == t)
        || crate::relational::projects_for_team(&t)
            .await
            .iter()
            .any(|p| p == &project);
    if !authorized {
        if cascade {
            let delete_ms = supplied_delete_ms.unwrap_or_else(|| {
                now_ms().saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS)
            });
            let peers: Vec<String> = c
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self)
                .map(|n| n.name)
                .collect();
            let n_peers = peers.len();
            let results: Vec<bool> = futures::future::join_all(peers.iter().map(|node| {
                let c = c.clone();
                let project = project.clone();
                let t = t.clone();
                async move {
                    tokio::time::timeout(
                        PROJECT_DELETE_QUICK_PASS_PEER_BUDGET,
                        dispatch_project_delete_with(&c, node, &project, &t, delete_ms, 1),
                    )
                    .await
                    .unwrap_or(false)
                }
            }))
            .await;
            let accepted = results.iter().filter(|ok| **ok).count();
            let unreached: Vec<String> = peers
                .iter()
                .zip(results.iter())
                .filter(|(_, ok)| !**ok)
                .map(|(n, _)| n.clone())
                .collect();
            if accepted == 0 {
                tracing::warn!(
                    project, tenant = %t, peers = n_peers,
                    "project delete: not hosted locally and no peer accepted the cascade"
                );
                return Err(StatusCode::BAD_GATEWAY);
            }
            c.projects.apply_delete(&project, delete_ms);
            c.git_index.remove_project(&project);
            // Same node-local cron drop: this coordinator never hosted the project, but its own
            // cron set can still hold jobs for it, and those would keep firing here.
            c.cron.remove_project(&project);
            crate::persist::persist(&c);
            if !unreached.is_empty() {
                let (c2, project2, team2) = (c.clone(), project.clone(), t.clone());
                tokio::spawn(async move {
                    futures::future::join_all(unreached.iter().map(|node| {
                        dispatch_project_delete_with(&c2, node, &project2, &team2, delete_ms, 3)
                    }))
                    .await;
                });
            }
            return Ok(Json(json!({
                "project": &project,
                "delete_ms": delete_ms,
                "removed_deployments": [],
                "accepted_by": accepted,
                "logical_deleted": true,
                "cleanup_complete": false,
                "cleanup_residuals": [{
                    "phase": "peer_cleanup_observation",
                    "resource": &project,
                    "detail": "at least one peer accepted the logical delete, but this coordinator did not receive that peer's physical-cleanup outcome"
                }],
                "note": "not hosted here — logical deletion accepted by peers; physical cleanup remains unobserved on this coordinator",
            })));
        }
        return Err(StatusCode::NOT_FOUND);
    }
    let outcome = delete_project_local_requested(&c, &project, &t, supplied_delete_ms).await;
    if !outcome.logical_deleted && !outcome.recreated {
        tracing::warn!(project, residuals = ?outcome.cleanup_residuals, "project delete refused before logical tombstoning");
        return Err(StatusCode::CONFLICT);
    }
    let delete_ms = outcome.delete_ms;
    let ids = outcome.removed.clone();
    if outcome.logical_deleted && !outcome.recreated {
        crate::webhooks::dispatch(
            &c.webhooks,
            &t,
            &project,
            "project.removed",
            json!({
                "project": project,
                "deployments": ids.len(),
                "delete_ms": delete_ms,
                "cleanup_complete": outcome.cleanup_complete,
                "cleanup_residuals": &outcome.cleanup_residuals,
            }),
        );
    }

    if cascade {
        let c2 = c.clone();
        let project2 = project.clone();
        let team2 = t.clone();
        tokio::spawn(async move {
            let peers: Vec<String> = c2
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self)
                .map(|n| n.name)
                .collect();
            futures::future::join_all(
                peers
                    .iter()
                    .map(|node| dispatch_project_delete(&c2, node, &project2, &team2, delete_ms)),
            )
            .await;
        });
    }
    Ok(Json(json!({
        "project": project,
        "delete_ms": delete_ms,
        "incarnation": outcome.incarnation,
        "removed_deployments": ids,
        "recreated": outcome.recreated,
        "logical_deleted": outcome.logical_deleted,
        "cleanup_complete": outcome.cleanup_complete,
        "cleanup_residuals": outcome.cleanup_residuals,
    })))
}

/// LOCAL-ONLY relocation reap (receiving side of [`dispatch_deployments_reap`]): removes THIS
/// node's production-lane deployment records for the project and nothing else (previews,
/// ProjectSettings incl. team tag, production_branch and env, the relational row and resources all
/// stay). The full-teardown [`delete_project_local`] must never be reused for relocation: its
/// settings wipe made projects vanish from accounts and made push-classified builds treat previews
/// as production.
pub(crate) async fn reap_deployments_local(c: &Arc<CloudState>, project: &str) -> usize {
    let ids = c.gw.remove_project_superseded(project).await;
    if !ids.is_empty() {
        record_event(
            c,
            project,
            "reap",
            &format!(
                "relocation: removed {} superseded production record(s) of {project}",
                ids.len()
            ),
        );
        crate::persist::persist(c);
    }
    ids.len()
}

/// Relocation-reap dispatch to ONE peer over the iroh mesh. Deliberately a NEW arm, not a parameter
/// on the delete arm: a pre-upgrade peer answers NO_HANDLER (a safe no-op, its stale copies linger
/// until it upgrades) instead of an old binary ignoring the scope parameter and running the FULL
/// teardown.
pub(crate) async fn dispatch_deployments_reap(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    team: &str,
) -> bool {
    if node == c.node_name {
        return true;
    }
    for attempt in 0..2u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        }
        let target = c
            .registry
            .nodes()
            .into_iter()
            .find(|n| n.name == node)
            .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
        let Some((id, addr)) = target else {
            return false;
        };
        let path = format!(
            "/v1/projects/{project}/reap-deployments?{}",
            mesh_team_qs(team)
        );
        if let Some(body) = crate::gossip::request_to(
            c,
            &id,
            &addr,
            hive_p2p::GOSSIP_POST,
            &path,
            &[],
            MESH_FORWARD_TIMEOUT_SECS,
        )
        .await
        {
            let v: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
            if v.as_ref()
                .map(|v| v.get("reaped").is_some())
                .unwrap_or(false)
            {
                return true;
            }
            return false;
        }
    }
    false
}

/// Tears down a project on ONE peer over whatever control-plane transport reaches it: the HTTP
/// admin URL when known, else the iroh mesh (`node_admins` is EMPTY for Firecracker fleet nodes,
/// which is why HTTP-only cascades never reached them). Single hop, no loops on the receiving side
/// (`cascade=false`); returns `true` when a peer ACCEPTED the delete over some transport, so the
/// caller reports what it actually observed.
pub(crate) async fn dispatch_project_delete(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    team: &str,
    delete_ms: u64,
) -> bool {
    dispatch_project_delete_with(c, node, project, team, delete_ms, 3).await
}

/// [`dispatch_project_delete`] with an explicit retry budget: the AWAITED cascade (the
/// not-hosted-here path) uses `attempts = 1`, since the full 3-attempt budget lets one dead peer
/// hold the user's request past the leader-forward client's 30 s timeout; the caller spawns the
/// full-budget variant DETACHED for the peers the quick pass could not reach.
pub(crate) async fn dispatch_project_delete_with(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    team: &str,
    delete_ms: u64,
    attempts: u32,
) -> bool {
    if node == c.node_name {
        return true;
    }
    let mut http_generation_v1 = false;
    let mut mesh_generation_v1 = false;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(750 * attempt as u64)).await;
        }
        let admin = c.node_admins.read().get(node).cloned();
        if let Some(admin) = &admin {
            if !http_generation_v1 {
                http_generation_v1 = match c
                    .http
                    .get(format!("{admin}/v1/project-delete/capabilities"))
                    .timeout(std::time::Duration::from_secs(3))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        r.json::<Value>()
                            .await
                            .ok()
                            .and_then(|v| {
                                v.get("delete_generation")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .as_deref()
                            == Some("v1")
                    }
                    _ => false,
                };
            }
            let accepted = if http_generation_v1 {
                let mut request = c
                    .http
                    .delete(format!(
                        "{admin}/v1/projects/{project}?cascade=false&delete_ms={delete_ms}"
                    ))
                    .header("x-hive-team", team.to_string());
                if let Ok(token) = std::env::var("HIVE_INTERNAL_TOKEN") {
                    if !token.trim().is_empty() {
                        request = request.header("x-hive-internal", token);
                    }
                }
                if crate::auth::enforced() {
                    if let Ok(token) = crate::auth::issue(
                        "mesh-internal",
                        team,
                        "service",
                        false,
                        crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
                    ) {
                        request = request.bearer_auth(token);
                    }
                }
                match request
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        r.json::<Value>()
                            .await
                            .ok()
                            .and_then(|v| v.get("delete_ms").and_then(Value::as_u64))
                            == Some(delete_ms)
                    }
                    _ => false,
                }
            } else {
                false
            };
            if accepted {
                return true;
            }
        }
        let target = c
            .registry
            .nodes()
            .into_iter()
            .find(|n| n.name == node)
            .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
        if let Some((id, addr)) = target {
            if !mesh_generation_v1 {
                mesh_generation_v1 = crate::gossip::request_to(
                    c,
                    &id,
                    &addr,
                    hive_p2p::GOSSIP_GET,
                    "/v1/project-delete/capabilities",
                    &[],
                    6,
                )
                .await
                .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                .and_then(|v| {
                    v.get("delete_generation")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .as_deref()
                    == Some("v1");
            }
            if !mesh_generation_v1 {
                continue;
            }
            let path = format!(
                "/v1/projects/{project}/delete?{}&delete_ms={delete_ms}",
                mesh_team_qs(team)
            );
            if let Some(body) = crate::gossip::request_to(
                c,
                &id,
                &addr,
                hive_p2p::GOSSIP_POST,
                &path,
                &[],
                MESH_FORWARD_TIMEOUT_SECS,
            )
            .await
            {
                let v: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
                let accepted = v
                    .as_ref()
                    .map(|v| {
                        v.get("error").is_none()
                            && v.get("removed").is_some()
                            && v.get("delete_ms").and_then(Value::as_u64) == Some(delete_ms)
                    })
                    .unwrap_or(false);
                if accepted {
                    return true;
                }
                if v.map(|v| v.get("error").is_some()).unwrap_or(false) {
                    return false;
                }
            }
        } else if admin.is_none() {
            tracing::warn!(
                node,
                project,
                "project delete: no route to hosting node (no admin URL, no iroh identity)"
            );
            return false;
        }
    }
    tracing::warn!(
        node,
        project,
        attempts,
        "project delete dispatch failed on every transport after retries — peer may keep \
         serving the deleted project until it receives a delete for another reason"
    );
    false
}

/// Project names become real identifiers downstream, so they are bounded and their sanitized form
/// must equal the name itself (a name that sanitized differently would give a volume/checkout path
/// that does not match its own lookup prefix).
const MAX_PROJECT_NAME_LEN: usize = 64;

/// Query for `POST /v1/projects/:project/rename`: `rename_ms` is the immutable generation of the
/// originating rename, reused by every retry (absent on a user request); `name` rides the query on
/// the mesh path (a gossip POST has no body) and the JSON body on HTTP.
#[derive(Deserialize, Default)]
struct RenameQ {
    #[serde(default, deserialize_with = "de_lenient_bool")]
    cascade: Option<bool>,
    #[serde(default)]
    rename_ms: Option<u64>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct RenameBody {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProjectRenameOutcome {
    /// Deployments whose project name was rewritten on this node.
    pub renamed: usize,
    /// Trees rewritten, for the response's honest accounting.
    pub moved_domains: Vec<String>,
    pub moved_databases: usize,
    pub moved_service_graphs: usize,
    pub moved_metric_buckets: usize,
    pub moved_sandboxes: usize,
    /// True when the local rewrite committed and was persisted.
    pub local_renamed: bool,
    pub rename_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<fluid_core::ProjectIncarnation>,
    pub residuals: Vec<ProjectCleanupResidual>,
}

/// A rename generation is validated exactly like a delete generation (see
/// [`MAX_DELETE_GENERATION_FUTURE_MS`]): bounded future skew, older generations stay valid forever
/// so an idempotent retry is always accepted.
pub(crate) fn valid_rename_generation(rename_ms: u64) -> bool {
    valid_delete_generation(rename_ms)
}

/// Reject a new project name that cannot be used: empty, too long, or one whose
/// sanitized form differs from itself (see [`MAX_PROJECT_NAME_LEN`]).
pub(crate) fn invalid_project_name(new: &str) -> Option<&'static str> {
    if new.is_empty() {
        return Some("a new project name is required");
    }
    if new.len() > MAX_PROJECT_NAME_LEN {
        return Some("project name is too long (64 characters maximum)");
    }
    if crate::git::sanitize_tag(new) != new.to_ascii_lowercase() {
        return Some("project name may only contain letters, digits, '.', '_' and '-'");
    }
    None
}

async fn project_rename(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Query(q): Query<RenameQ>,
    body: Option<Json<RenameBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let claims_ref = claims.as_ref().map(|e| &e.0);
    if !has_explicit_caller_identity(
        claims.is_some(),
        headers.get("x-hive-team").and_then(|v| v.to_str().ok()),
    ) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "rename requires an authenticated caller (a JWT or an explicit x-hive-team header)"
                .into(),
        ));
    }
    let cascade = q.cascade.unwrap_or(true);
    let supplied_rename_ms = q.rename_ms;
    if supplied_rename_ms.is_some()
        && (cascade || !delete_forward_is_internal(&headers, claims_ref))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "a rename generation is only accepted on an internal single-hop forward".into(),
        ));
    }
    if supplied_rename_ms.is_some_and(|ms| !valid_rename_generation(ms)) {
        return Err((StatusCode::BAD_REQUEST, "invalid rename generation".into()));
    }
    let t = tenant(&c, &headers, claims_ref);
    let new = q
        .name
        .clone()
        .or_else(|| body.and_then(|Json(b)| b.name))
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if let Some(reason) = invalid_project_name(&new) {
        return Err((StatusCode::BAD_REQUEST, reason.into()));
    }
    if new.eq_ignore_ascii_case(&project) {
        return Err((
            StatusCode::BAD_REQUEST,
            "the project is already named that".into(),
        ));
    }
    if let Some(hit) = c.projects.find_key_ci(&new) {
        let taken_by_caller = project_owned_by(&c, &hit, &t);
        return Err((
            StatusCode::CONFLICT,
            if taken_by_caller {
                format!("You already have a project named \"{new}\". Choose a different name.")
            } else {
                format!("The name \"{new}\" is already taken by another project. Choose a different name.")
            },
        ));
    }
    if let Some(tomb) = c.projects.renamed_away(&new) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "The name \"{new}\" is permanently reserved: a project was renamed from it to \"{}\". Choose a different name.",
                tomb.new
            ),
        ));
    }
    let authorized = c
        .projects
        .get_if_set(&project)
        .map(|s| norm(&s.team) == t)
        .unwrap_or(false)
        || c.gw
            .list()
            .iter()
            .any(|d| d.project == project && record_tenant(&d.tenant) == t)
        || c.peer_deployments
            .read()
            .values()
            .flatten()
            .any(|d| d.project == project && record_tenant(&d.tenant) == t)
        || crate::relational::projects_for_team(&t)
            .await
            .iter()
            .any(|p| p == &project);
    if !authorized {
        if !cascade {
            return Err((
                StatusCode::NOT_FOUND,
                "no project by that name is owned by this tenant here".into(),
            ));
        }
        let rename_ms = supplied_rename_ms.unwrap_or_else(|| {
            now_ms().saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS)
        });
        let peers: Vec<String> = c
            .registry
            .nodes()
            .into_iter()
            .filter(|n| !n.is_self)
            .map(|n| n.name)
            .collect();
        let n_peers = peers.len();
        let results: Vec<bool> = futures::future::join_all(peers.iter().map(|node| {
            let c = c.clone();
            let project = project.clone();
            let new = new.clone();
            let t = t.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(28),
                    dispatch_project_rename_with(&c, node, &project, &new, &t, rename_ms, 1),
                )
                .await
                .unwrap_or(false)
            }
        }))
        .await;
        let accepted = results.iter().filter(|ok| **ok).count();
        if accepted == 0 {
            tracing::warn!(
                project, new, tenant = %t, peers = n_peers,
                "project rename: not owned locally and no peer accepted the rename"
            );
            return Err((
                StatusCode::BAD_GATEWAY,
                "no node that owns this project could be reached to perform the rename".into(),
            ));
        }
        c.projects
            .record_rename_tombstone(&project, &new, rename_ms);
        crate::persist::persist(&c);
        let unreached: Vec<String> = peers
            .iter()
            .zip(results.iter())
            .filter(|(_, ok)| !**ok)
            .map(|(n, _)| n.clone())
            .collect();
        if !unreached.is_empty() {
            let (c2, project2, new2, team2) = (c.clone(), project.clone(), new.clone(), t.clone());
            tokio::spawn(async move {
                futures::future::join_all(unreached.iter().map(|node| {
                    dispatch_project_rename_with(&c2, node, &project2, &new2, &team2, rename_ms, 3)
                }))
                .await;
            });
        }
        return Ok(Json(json!({
            "project": &new,
            "previous": &project,
            "rename_ms": rename_ms,
            "renamed": 0,
            "accepted_by": accepted,
            "local_renamed": false,
            "cleanup_residuals": [{
                "resource": "peer_rename_observation",
                "id": &project,
                "reason": "at least one peer accepted the rename, but this coordinator held no copy and did not observe the rewrite"
            }],
            "note": "not hosted here — rename accepted by peers; this coordinator performed no local rewrite",
        })));
    }

    let outcome = rename_project_local_requested(&c, &project, &new, &t, supplied_rename_ms).await;
    if !outcome.local_renamed {
        let reason = outcome
            .residuals
            .first()
            .map(|r| r.reason.clone())
            .unwrap_or_else(|| "the rename could not be applied locally".into());
        tracing::warn!(project, new, reason, "project rename refused");
        return Err((StatusCode::CONFLICT, reason));
    }
    let rename_ms = outcome.rename_ms;
    crate::webhooks::dispatch(
        &c.webhooks,
        &t,
        &new,
        "project.renamed",
        json!({
            "project": &new,
            "previous": &project,
            "deployments": outcome.renamed,
            "rename_ms": rename_ms,
        }),
    );
    if cascade {
        let c2 = c.clone();
        let project2 = project.clone();
        let new2 = new.clone();
        let team2 = t.clone();
        tokio::spawn(async move {
            let peers: Vec<String> = c2
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self)
                .map(|n| n.name)
                .collect();
            futures::future::join_all(peers.iter().map(|node| {
                dispatch_project_rename(&c2, node, &project2, &new2, &team2, rename_ms)
            }))
            .await;
        });
    }
    Ok(Json(json!({
        "project": &new,
        "previous": &project,
        "rename_ms": rename_ms,
        "incarnation": outcome.incarnation,
        "renamed": outcome.renamed,
        "moved_domains": outcome.moved_domains,
        "moved_databases": outcome.moved_databases,
        "moved_service_graphs": outcome.moved_service_graphs,
        "moved_metric_buckets": outcome.moved_metric_buckets,
        "moved_sandboxes": outcome.moved_sandboxes,
        "local_renamed": outcome.local_renamed,
        "cleanup_residuals": outcome.residuals,
    })))
}

pub(crate) async fn rename_project_local(
    c: &Arc<CloudState>,
    project: &str,
    new: &str,
    team: &str,
    rename_ms: u64,
) -> ProjectRenameOutcome {
    rename_project_local_requested(c, project, new, team, Some(rename_ms)).await
}

async fn rename_project_local_requested(
    c: &Arc<CloudState>,
    project: &str,
    new: &str,
    team: &str,
    requested_rename_ms: Option<u64>,
) -> ProjectRenameOutcome {
    let (lexicographically_first, lexicographically_second) =
        if project.to_ascii_lowercase() <= new.to_ascii_lowercase() {
            (project.to_string(), new.to_string())
        } else {
            (new.to_string(), project.to_string())
        };
    let _lifecycle_first = crate::project_settings::lifecycle_write(&lexicographically_first).await;
    let _lifecycle_second =
        crate::project_settings::lifecycle_write(&lexicographically_second).await;
    rename_project_owned(c, project, new, team, requested_rename_ms).await
}

/// The one place a rename is applied: holds BOTH lifecycle writers and rewrites every tree that
/// keys on the project name in one order, the settings row FIRST (the authority every other tree
/// keys off; its incarnation carries across, a rename is never a new identity), then routing, the
/// production-deployment row (dropping it blinds relocation), custom-domain attachments (routing
/// keys on the FULL hostname), databases, service graphs, metrics, sandboxes, webhooks, the git
/// index and poll watermark (the REPO is not renamed) and the relational mirror (an authorization
/// fallback), followed by ONE `persist()`. A rename is never a delete: no resource is purged and no
/// tenant data dropped.
async fn rename_project_owned(
    c: &Arc<CloudState>,
    project: &str,
    new: &str,
    team: &str,
    requested_rename_ms: Option<u64>,
) -> ProjectRenameOutcome {
    let refused = |reason: String| ProjectRenameOutcome {
        renamed: 0,
        moved_domains: Vec::new(),
        moved_databases: 0,
        moved_service_graphs: 0,
        moved_metric_buckets: 0,
        moved_sandboxes: 0,
        local_renamed: false,
        rename_ms: requested_rename_ms.unwrap_or(0),
        incarnation: None,
        residuals: vec![ProjectCleanupResidual::new("rename", project, reason)],
    };
    if let Some(tomb) = c.projects.rename_tombstone_of(project) {
        if tomb.new.eq_ignore_ascii_case(new) {
            return ProjectRenameOutcome {
                renamed: 0,
                moved_domains: Vec::new(),
                moved_databases: 0,
                moved_service_graphs: 0,
                moved_metric_buckets: 0,
                moved_sandboxes: 0,
                local_renamed: true,
                rename_ms: tomb.rename_ms,
                incarnation: tomb.incarnation,
                residuals: Vec::new(),
            };
        }
        return refused(format!(
            "\"{project}\" was already renamed to \"{}\"",
            tomb.new
        ));
    }
    let Some(row) = c.projects.get_if_set(project) else {
        return refused(
            "this node holds no settings row for the project; the rename must be applied by a node that owns it".into(),
        );
    };
    if record_tenant(&row.team) != UNTAGGED_TENANT && norm(&row.team) != norm(team) {
        return refused("active project settings belong to another tenant".into());
    }
    if c.projects.find_key_ci(new).is_some() {
        return refused(format!("a project named \"{new}\" already exists"));
    }
    if c.projects.renamed_away(new).is_some() {
        return refused(format!("the name \"{new}\" is permanently reserved"));
    }
    let incarnation = row.incarnation;
    let generation_floor = row.updated_ms.max(row.incarnation_created_ms);
    if let Some(rename_ms) = requested_rename_ms {
        if rename_ms < generation_floor {
            return refused("the rename generation is older than this project's last write".into());
        }
    }
    let mut residuals = Vec::new();
    if let Some(incarnation) = incarnation {
        if let Err(ids) = c
            .build_cancels
            .cancel_project_and_drain(c, project, incarnation, Duration::from_secs(10))
            .await
        {
            residuals.extend(ids.into_iter().map(|id| {
                ProjectCleanupResidual::new(
                    "build_drain",
                    id,
                    "build task did not release its Drop-owned completion before the 10s deadline",
                )
            }));
        }
        if let Err(paths) =
            crate::git::wait_for_checkout_drain(project, incarnation, Duration::from_secs(10)).await
        {
            residuals.extend(paths.into_iter().map(|path| {
                ProjectCleanupResidual::new(
                    "checkout_drain",
                    path.display().to_string(),
                    "checkout reservation did not release before the 10s deadline",
                )
            }));
        }
    }
    if !residuals.is_empty() {
        return ProjectRenameOutcome {
            renamed: 0,
            moved_domains: Vec::new(),
            moved_databases: 0,
            moved_service_graphs: 0,
            moved_metric_buckets: 0,
            moved_sandboxes: 0,
            local_renamed: false,
            rename_ms: requested_rename_ms.unwrap_or(0),
            incarnation,
            residuals,
        };
    }
    let rename_ms = requested_rename_ms
        .unwrap_or_else(|| now_ms().max(observed_project_version_ms(c, project).saturating_add(1)));
    let moved_row = c.projects.rename_row(project, new, rename_ms);
    if moved_row.is_none() {
        return refused("the project's settings row disappeared before it could be moved".into());
    }
    let renamed = c.gw.rename_project(project, new);
    c.production_deployments.rename(project, new);
    let moved_domains = c.domains.rename_project(project, new);
    let moved_databases = c.databases.rename_project(project, new);
    let moved_service_graphs = c.svcgraph.rename_project(project, new);
    let moved_metric_buckets = c.metrics.rename_project(project, new);
    let moved_sandboxes = c.sandboxes.rename_project(project, new);
    let moved_webhooks = c.webhooks.rename_project(norm(team), project, new);
    c.git_index.rename_project(project, new);
    {
        let mut seen = c.git_poll_seen.write();
        if let Some(sha) = seen.remove(project) {
            seen.entry(new.to_string()).or_insert(sha);
        }
    }
    crate::relational::rename_project(project, new, rename_ms).await;
    crate::persist::persist(c);
    c.audit.record(
        team,
        "system",
        "rename",
        "project",
        new,
        &format!(
            "from={project} incarnation={} rename_ms={rename_ms}",
            incarnation
                .map(|i| i.to_string())
                .unwrap_or_else(|| "none".into())
        ),
    );
    record_event(
        c,
        new,
        "rename",
        &format!("renamed from {project} at generation {rename_ms}; deployments={renamed}; webhooks={moved_webhooks}"),
    );
    ProjectRenameOutcome {
        renamed,
        moved_domains,
        moved_databases,
        moved_service_graphs,
        moved_metric_buckets,
        moved_sandboxes,
        local_renamed: true,
        rename_ms,
        incarnation,
        residuals: Vec::new(),
    }
}

/// Dispatches a rename to ONE peer over whichever control-plane transport reaches it (HTTP admin
/// when known, else the iroh mesh); true only when the peer ACCEPTED it and echoed the same
/// generation. Deliberately a NEW arm, not a parameter on the delete arm: a pre-upgrade peer
/// answers 404/NO_HANDLER, a safe no-op reported as a residual, instead of an old binary ignoring
/// the new name and running a teardown.
pub(crate) async fn dispatch_project_rename(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    new: &str,
    team: &str,
    rename_ms: u64,
) -> bool {
    dispatch_project_rename_with(c, node, project, new, team, rename_ms, 3).await
}

pub(crate) async fn dispatch_project_rename_with(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    new: &str,
    team: &str,
    rename_ms: u64,
    attempts: u32,
) -> bool {
    if node == c.node_name {
        return true;
    }
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(750 * attempt as u64)).await;
        }
        let admin = c.node_admins.read().get(node).cloned();
        if let Some(admin) = &admin {
            let mut request = c
                .http
                .post(format!(
                    "{admin}/v1/projects/{project}/rename?cascade=false&rename_ms={rename_ms}&name={new}"
                ))
                .header("x-hive-team", team.to_string());
            if let Ok(token) = std::env::var("HIVE_INTERNAL_TOKEN") {
                if !token.trim().is_empty() {
                    request = request.header("x-hive-internal", token);
                }
            }
            if crate::auth::enforced() {
                if let Ok(token) = crate::auth::issue(
                    "mesh-internal",
                    team,
                    "service",
                    false,
                    crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
                ) {
                    request = request.bearer_auth(token);
                }
            }
            match request
                .timeout(std::time::Duration::from_secs(8))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    let ok = r
                        .json::<Value>()
                        .await
                        .ok()
                        .map(|v| {
                            v.get("rename_ms").and_then(Value::as_u64) == Some(rename_ms)
                                && v.get("renamed").is_some()
                        })
                        .unwrap_or(false);
                    if ok {
                        return true;
                    }
                }
                _ => {}
            }
        }
        let target = c
            .registry
            .nodes()
            .into_iter()
            .find(|n| n.name == node)
            .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
        if let Some((id, addr)) = target {
            let path = format!(
                "/v1/projects/{project}/rename?{}&rename_ms={rename_ms}&name={new}",
                mesh_team_qs(team)
            );
            if let Some(body) = crate::gossip::request_to(
                c,
                &id,
                &addr,
                hive_p2p::GOSSIP_POST,
                &path,
                &[],
                MESH_FORWARD_TIMEOUT_SECS,
            )
            .await
            {
                let v: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
                let accepted = v
                    .as_ref()
                    .map(|v| {
                        v.get("error").is_none()
                            && v.get("renamed").is_some()
                            && v.get("rename_ms").and_then(Value::as_u64) == Some(rename_ms)
                    })
                    .unwrap_or(false);
                if accepted {
                    return true;
                }
                if v.map(|v| v.get("error").is_some()).unwrap_or(false) {
                    return false;
                }
            }
        } else if admin.is_none() {
            tracing::warn!(
                node,
                project,
                "project rename: no route to peer (no admin URL, no iroh identity)"
            );
            return false;
        }
    }
    false
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProjectDeleteOutcome {
    pub removed: Vec<String>,
    pub recreated: bool,
    pub logical_deleted: bool,
    pub cleanup_complete: bool,
    pub cleanup_residuals: Vec<ProjectCleanupResidual>,
    pub delete_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<fluid_core::ProjectIncarnation>,
}

fn delete_authority(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    requested_delete_ms: Option<u64>,
) -> Result<Option<(crate::project_settings::ProjectSettings, u64)>, ProjectCleanupResidual> {
    if let Some(row) = c.projects.get_if_set(project) {
        if record_tenant(&row.team) != UNTAGGED_TENANT && norm(&row.team) != norm(team) {
            return Err(ProjectCleanupResidual::new(
                "project_authority",
                project,
                "active project settings belong to another tenant",
            ));
        }
        if row.incarnation.is_some() {
            return Ok(Some((row.clone(), row.incarnation_created_ms)));
        }
        let generation_floor = row.updated_ms;
        let ids = legacy_deployment_ids_for_owner(c, project, team);
        let incarnation = c.projects.ensure_incarnation(project).map_err(|error| {
            ProjectCleanupResidual::new("project_authority", project, error.to_string())
        })?;
        adopt_authorized_legacy_deployments(c, project, incarnation, ids).map_err(|error| {
            ProjectCleanupResidual::new("project_authority", project, error.to_string())
        })?;
        let row = c
            .projects
            .get_exact(project, incarnation)
            .map_err(|error| {
                ProjectCleanupResidual::new("project_authority", project, error.to_string())
            })?;
        return Ok(Some((row, generation_floor)));
    }

    if let Some(delete_ms) = requested_delete_ms {
        if c.projects
            .tombstone_of(project)
            .is_some_and(|stored| stored >= delete_ms)
        {
            return Ok(None);
        }
    }

    let records: Vec<_> = c
        .gw
        .deployment_records()
        .into_iter()
        .filter(|record| record.project == project && record_tenant(&record.tenant) == norm(team))
        .collect();
    let incarnations: std::collections::BTreeSet<_> = records
        .iter()
        .filter_map(|record| record.project_incarnation)
        .collect();
    let generation_floor = records
        .iter()
        .map(|record| record.created_at_ms)
        .max()
        .unwrap_or(0);
    let incarnation = if incarnations.len() == 1 {
        *incarnations
            .iter()
            .next()
            .expect("length was checked above")
    } else {
        c.projects.ensure_incarnation(project).map_err(|error| {
            ProjectCleanupResidual::new("project_authority", project, error.to_string())
        })?
    };
    c.projects
        .ensure_incarnation_exact(project, incarnation)
        .map_err(|error| {
            ProjectCleanupResidual::new("project_authority", project, error.to_string())
        })?;
    let row = c
        .projects
        .get_exact(project, incarnation)
        .map_err(|error| {
            ProjectCleanupResidual::new("project_authority", project, error.to_string())
        })?;
    if record_tenant(&row.team) == UNTAGGED_TENANT {
        c.projects
            .set_team_exact(project, incarnation, team)
            .map_err(|error| {
                ProjectCleanupResidual::new("project_authority", project, error.to_string())
            })?;
    }
    let row = c
        .projects
        .get_exact(project, incarnation)
        .map_err(|error| {
            ProjectCleanupResidual::new("project_authority", project, error.to_string())
        })?;
    let generation_floor = if generation_floor == 0 {
        row.incarnation_created_ms
    } else {
        generation_floor
    };
    Ok(Some((row, generation_floor)))
}

async fn delete_project_owned(
    c: Arc<CloudState>,
    project: String,
    team: String,
    requested_delete_ms: Option<u64>,
    lifecycle: tokio::sync::OwnedMutexGuard<()>,
) -> ProjectDeleteOutcome {
    let _lifecycle = lifecycle;
    let authority = match delete_authority(&c, &project, &team, requested_delete_ms) {
        Ok(Some(authority)) => authority,
        Ok(None) => {
            let delete_ms = requested_delete_ms
                .or_else(|| c.projects.tombstone_of(&project))
                .unwrap_or(0);
            return ProjectDeleteOutcome {
                removed: Vec::new(),
                recreated: false,
                logical_deleted: true,
                cleanup_complete: false,
                cleanup_residuals: vec![ProjectCleanupResidual::new(
                    "cleanup_state",
                    &project,
                    "idempotent retry observed the logical tombstone, but no exact inventory survives to prove physical cleanup completed",
                )],
                delete_ms,
                incarnation: None,
            };
        }
        Err(residual) => {
            return ProjectDeleteOutcome {
                removed: Vec::new(),
                recreated: false,
                logical_deleted: false,
                cleanup_complete: false,
                cleanup_residuals: vec![residual],
                delete_ms: requested_delete_ms.unwrap_or(0),
                incarnation: None,
            };
        }
    };
    let (settings, generation_floor) = authority;
    let incarnation = settings
        .incarnation
        .expect("delete authority always has an incarnation");
    if let Some(delete_ms) = requested_delete_ms {
        if delete_ms < generation_floor {
            return ProjectDeleteOutcome {
                removed: Vec::new(),
                recreated: true,
                logical_deleted: false,
                cleanup_complete: true,
                cleanup_residuals: Vec::new(),
                delete_ms,
                incarnation: Some(incarnation),
            };
        }
    }

    let active_checkouts = crate::git::active_checkout_paths(&project, incarnation);
    let mut drain_residuals = Vec::new();
    if let Err(ids) = c
        .build_cancels
        .cancel_project_and_drain(&c, &project, incarnation, Duration::from_secs(10))
        .await
    {
        drain_residuals.extend(ids.into_iter().map(|id| {
            ProjectCleanupResidual::new(
                "build_drain",
                id,
                "build task did not release its Drop-owned completion before the 10s deadline",
            )
        }));
    }
    if let Err(paths) =
        crate::git::wait_for_checkout_drain(&project, incarnation, Duration::from_secs(10)).await
    {
        drain_residuals.extend(paths.into_iter().map(|path| {
            ProjectCleanupResidual::new(
                "checkout_drain",
                path.display().to_string(),
                "checkout reservation did not release before the 10s deadline",
            )
        }));
    }
    let inventory = project_delete_inventory(&c, &project, &team, &settings, &active_checkouts);

    let delete_ms = match requested_delete_ms {
        Some(delete_ms) => {
            c.projects
                .apply_delete_exact(&project, incarnation, delete_ms);
            delete_ms
        }
        None => match c.projects.begin_delete_exact(
            &project,
            incarnation,
            observed_project_version_ms(&c, &project),
        ) {
            Ok((_, delete_ms)) => delete_ms,
            Err(error) => {
                return ProjectDeleteOutcome {
                    removed: Vec::new(),
                    recreated: true,
                    logical_deleted: false,
                    cleanup_complete: false,
                    cleanup_residuals: vec![ProjectCleanupResidual::new(
                        "project_tombstone",
                        &project,
                        error.to_string(),
                    )],
                    delete_ms: 0,
                    incarnation: Some(incarnation),
                };
            }
        },
    };

    c.git_index.remove_project(&project);
    c.production_deployments.remove(&project);
    // Cron is NODE-LOCAL: each node drops its own copy as part of this delete, never through
    // store_sync. Without this the jobs survive in memory AND in the persisted snapshot (which
    // `persist.rs` restores with `replace_all` on every boot), so a deleted project's cron keeps
    // firing a 503 `no-deployment` forever — the `shoomoo` orphan.
    let purged_crons = c.cron.remove_project(&project);
    if purged_crons > 0 {
        tracing::info!(project, purged_crons, "dropped cron jobs on project delete");
    }
    let preserved_domains: std::collections::HashSet<String> = c
        .projects
        .all_domains()
        .into_iter()
        .map(|(_, domain)| domain)
        .collect();
    c.domains
        .clear_verify_for_project(&project, &preserved_domains);
    crate::persist::persist(&c);

    let removed_records = c.gw.remove_project_incarnation(&project, incarnation).await;
    let removed: Vec<String> = removed_records
        .iter()
        .map(|record| record.id.clone())
        .collect();
    let mut cleanup_residuals = drain_residuals;
    if cleanup_residuals.is_empty() {
        cleanup_residuals
            .extend(purge_project_resources(&c, &project, &team, delete_ms, &inventory).await);
    } else {
        for database in &inventory.databases {
            cleanup_residuals.push(ProjectCleanupResidual::new(
                "database_cleanup_deferred",
                &database.id,
                "retained because exact build/checkout drain did not complete",
            ));
        }
        for volume in &inventory.current_volumes {
            cleanup_residuals.push(ProjectCleanupResidual::new(
                "volume_cleanup_deferred",
                volume,
                "retained because exact build/checkout drain did not complete",
            ));
        }
        for build_id in &inventory.build_ids {
            cleanup_residuals.push(ProjectCleanupResidual::new(
                "source_cleanup_deferred",
                build_id,
                "retained because exact build/checkout drain did not complete",
            ));
        }
        cleanup_residuals.extend(inventory.ambiguous_deployments.iter().map(|deployment| {
            ProjectCleanupResidual::new(
                "deployment_ownership",
                &deployment.id,
                format!(
                    "retained: deployment does not carry exact incarnation {}",
                    inventory.incarnation
                ),
            )
        }));
    }
    let cleanup_complete = cleanup_residuals.is_empty();
    c.audit.record(
        &team,
        "system",
        if cleanup_complete {
            "cleanup-complete"
        } else {
            "cleanup-incomplete"
        },
        "project",
        &project,
        &format!(
            "incarnation={} delete_ms={} residuals={}",
            incarnation,
            delete_ms,
            cleanup_residuals.len()
        ),
    );
    record_event(
        &c,
        &project,
        "delete",
        &format!(
            "deleted incarnation {incarnation} at generation {delete_ms}; deployments={}; cleanup_complete={cleanup_complete}; residuals={}",
            removed.len(),
            cleanup_residuals.len()
        ),
    );
    crate::persist::persist(&c);
    ProjectDeleteOutcome {
        removed,
        recreated: false,
        logical_deleted: true,
        cleanup_complete,
        cleanup_residuals,
        delete_ms,
        incarnation: Some(incarnation),
    }
}

/// LOCAL-ONLY project teardown for HTTP and mesh receivers: the owned lifecycle guard moves into a
/// spawned task before any await a request cancellation can drop, and that task owns cancellation,
/// tombstoning, exact route removal and bounded physical cleanup to completion.
async fn delete_project_local_requested(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    requested_delete_ms: Option<u64>,
) -> ProjectDeleteOutcome {
    let lifecycle = crate::project_settings::lifecycle_write(project).await;
    let task = tokio::spawn(delete_project_owned(
        c.clone(),
        project.to_string(),
        team.to_string(),
        requested_delete_ms,
        lifecycle,
    ));
    match task.await {
        Ok(outcome) => outcome,
        Err(error) => ProjectDeleteOutcome {
            removed: Vec::new(),
            recreated: false,
            logical_deleted: false,
            cleanup_complete: false,
            cleanup_residuals: vec![ProjectCleanupResidual::new(
                "delete_task",
                project,
                format!("serialized deletion task failed: {error}"),
            )],
            delete_ms: requested_delete_ms.unwrap_or(0),
            incarnation: None,
        },
    }
}

/// Mixed-version mesh/main call seam: propagated deletes always carry a generation; user-originated
/// deletes call `delete_project_local_requested` with `None` so the exact owner issues the
/// generation under its writer.
pub(crate) async fn delete_project_local(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    delete_ms: u64,
) -> ProjectDeleteOutcome {
    delete_project_local_requested(c, project, team, Some(delete_ms)).await
}

/// Query for project_delete: `cascade=false` deletes only on this node (no mesh fan-out);
/// `delete_ms` is the immutable generation of the originating delete, reused by every retry (absent
/// on a user request and on pre-upgrade peers); cascade defaults to true.
#[derive(Deserialize, Default)]
struct CascadeQ {
    #[serde(default, deserialize_with = "de_lenient_bool")]
    cascade: Option<bool>,
    #[serde(default)]
    delete_ms: Option<u64>,
}

/// Body of the Redeploy modal (every field optional, a bare `{}` works): `target` picks the
/// environment (production/preview), `use_cache` toggles the build cache.
#[derive(Deserialize, Default)]
struct RedeployBody {
    #[serde(default)]
    target: Option<String>,
    #[serde(default = "default_true_b")]
    use_cache: bool,
    /// GitHub token for a private-repo redeploy, attached server-side by the
    /// dashboard's /api/projects/:project/redeploy route (never sent by the browser).
    #[serde(default)]
    git_token: Option<String>,
}

/// The newest git source of a project from this node's gateway or, when the placement scheduler put
/// the project on a peer, from the gossiped fleet deployments, so redeploy works for remotely
/// hosted projects.
pub(crate) fn git_for_project_fleet(
    c: &Arc<CloudState>,
    project: &str,
) -> Option<fluid_core::GitSource> {
    if let Some(g) = c.gw.git_for_project(project) {
        return Some(g);
    }
    c.peer_deployments
        .read()
        .values()
        .flatten()
        .filter(|d| d.project == project)
        .filter(|d| d.git.as_ref().is_some_and(|g| g.is_real_git()))
        .max_by_key(|d| d.created_at_ms)
        .and_then(|d| d.git.clone())
}

/// The newest deployment's SOURCE for `project`, fleet-aware and INCLUDING the non-real-git
/// pseudo-sources (`upload://` zip, `image://` prebuilt image), unlike `git_for_project_fleet`
/// (which filters `is_real_git` for GitHub-webhook lookups): redeploy must rebuild zip- and
/// image-based projects too. Returns the source, whether it came from the PRODUCTION lane and the
/// selected upload source's platform-issued BUILD ids (`GitSource.commit`; checkout dirs and
/// retained archives are keyed by build id, not deployment id). A redeploy rebuilds from the newest
/// PRODUCTION deployment when one exists (the newest record across all environments let a later
/// preview hijack "Redeploy"), and the LIVE promoted deployment (`production == true`) outranks the
/// lane heuristic entirely: `target` is immutable while promote/rollback flips only `production`,
/// so sourcing the newest `target = "production"` record would re-promote the very deployment the
/// operator just rolled back from.
fn source_for_project_fleet(
    c: &Arc<CloudState>,
    project: &str,
) -> Option<(fluid_core::GitSource, bool, Vec<String>)> {
    let mut best_live: Option<(u64, fluid_core::GitSource)> = None;
    let mut best_prod: Option<(u64, fluid_core::GitSource)> = None;
    let mut best_any: Option<(u64, fluid_core::GitSource)> = None;
    let mut consider = |ts: u64, g: fluid_core::GitSource, target: &str, production: bool| {
        if production {
            if best_live.as_ref().map_or(true, |(t, _)| ts >= *t) {
                best_live = Some((ts, g.clone()));
            }
        }
        let prod_lane = match target.trim() {
            "production" => true,
            "preview" => false,
            _ => production,
        };
        if prod_lane {
            if best_prod.as_ref().map_or(true, |(t, _)| ts >= *t) {
                best_prod = Some((ts, g.clone()));
            }
        }
        if best_any.as_ref().map_or(true, |(t, _)| ts >= *t) {
            best_any = Some((ts, g));
        }
    };
    for r in c.gw.deployment_records() {
        if r.project == project {
            if let Some(g) = r.git.clone() {
                consider(r.created_at_ms, g, &r.target, r.production);
            }
        }
    }
    for d in c.peer_deployments.read().values().flatten() {
        if d.project == project {
            if let Some(g) = d.git.clone() {
                consider(d.created_at_ms, g, &d.target, d.production);
            }
        }
    }
    drop(consider);
    let lineage_ids = |source: &fluid_core::GitSource| {
        crate::git::upload_source_build_id(source)
            .into_iter()
            .collect()
    };
    if let Some((_, g)) = best_live {
        let ids = lineage_ids(&g);
        return Some((g, true, ids));
    }
    if let Some((_, g)) = best_prod {
        let ids = lineage_ids(&g);
        return Some((g, true, ids));
    }
    best_any.map(|(_, g)| {
        let ids = lineage_ids(&g);
        (g, false, ids)
    })
}

/// The port + protocol the project's newest deployment actually ran with (its `web` function's
/// declared port, an explicit override or the auto-detected value), from LOCAL records only:
/// `peer_deployments` carries no port info, so a project placed entirely on a peer has none here.
/// Feeds `redeploy_request` so an image redeploy restores the ORIGINAL port/protocol instead of
/// re-running auto-detection (which drops a manual override and cannot get a multi-port image
/// right); `None` when no prior deployment declared a port.
fn image_port_spec_for_project_fleet(
    c: &Arc<CloudState>,
    project: &str,
) -> Option<fluid_core::PortSpec> {
    fn spec_from(f: &fluid_core::FunctionConfig) -> Option<fluid_core::PortSpec> {
        if let Some(p) = f.ports.first() {
            return Some(fluid_core::PortSpec::single(p.container_port, p.protocol));
        }
        if f.runtime == "container"
            && f.start_cmd.first().map(String::as_str) == Some("__container__")
        {
            let port: u16 = f.start_cmd.get(2)?.parse().ok()?;
            return Some(fluid_core::PortSpec::single(port, f.protocol));
        }
        None
    }
    let mut best_live: Option<(u64, fluid_core::PortSpec)> = None;
    let mut best_prod: Option<(u64, fluid_core::PortSpec)> = None;
    let mut best_any: Option<(u64, fluid_core::PortSpec)> = None;
    for r in c.gw.deployment_records() {
        if r.project != project {
            continue;
        }
        let Some(f) = r.manifest.functions.first() else {
            continue;
        };
        let Some(spec) = spec_from(f) else { continue };
        if r.production
            && best_live
                .as_ref()
                .map_or(true, |(ts, _)| r.created_at_ms >= *ts)
        {
            best_live = Some((r.created_at_ms, spec.clone()));
        }
        let prod_lane = match r.target.trim() {
            "production" => true,
            "preview" => false,
            _ => r.production,
        };
        if prod_lane
            && best_prod
                .as_ref()
                .map_or(true, |(ts, _)| r.created_at_ms >= *ts)
        {
            best_prod = Some((r.created_at_ms, spec.clone()));
        }
        if best_any
            .as_ref()
            .map_or(true, |(ts, _)| r.created_at_ms >= *ts)
        {
            best_any = Some((r.created_at_ms, spec));
        }
    }
    best_live.or(best_prod).or(best_any).map(|(_, spec)| spec)
}

/// Builds a placement `Target` addressing a node by NAME: self (both None), its HTTP admin URL when
/// known, else its iroh mesh route; `None` when the node is unknown or unreachable. Used to pin a
/// zip redeploy to the node holding the source.
fn target_for_node(c: &Arc<CloudState>, node: &str) -> Option<crate::schedule::Target> {
    if node == c.node_name {
        return Some(crate::schedule::Target {
            node: node.to_string(),
            admin: None,
            iroh: None,
        });
    }
    if let Some(a) = c.node_admins.read().get(node).cloned() {
        return Some(crate::schedule::Target {
            node: node.to_string(),
            admin: Some(a),
            iroh: None,
        });
    }
    let n = c.registry.nodes().into_iter().find(|n| n.name == node)?;
    match (n.peer_id.clone(), n.iroh_addr.clone()) {
        (Some(id), Some(addr)) => Some(crate::schedule::Target {
            node: node.to_string(),
            admin: None,
            iroh: Some((id, addr)),
        }),
        _ => None,
    }
}

/// One deterministic node hosting `project`, preferring the live production record, then the
/// newest; project-level owner proxies use it when public round-robin ingress lands on a node with
/// no local deployment state.
fn host_node_for_project(c: &Arc<CloudState>, project: &str) -> Option<String> {
    let mut candidates = Vec::new();
    for deployment in c.gw.list() {
        if deployment.project == project {
            candidates.push((
                deployment.production,
                deployment.created_at_ms,
                c.node_name.clone(),
            ));
        }
    }
    for (node, deployments) in c.peer_deployments.read().iter() {
        for deployment in deployments {
            if deployment.project == project {
                candidates.push((
                    deployment.production,
                    deployment.created_at_ms,
                    node.clone(),
                ));
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    candidates.into_iter().next().map(|(_, _, node)| node)
}

/// Peer node NAMES hosting `project`, addressed by node (not HTTP admin URL) so callers reach them
/// over the iroh mesh via `fetch_from_host` (FC nodes have no HTTP admin URL in `node_admins`);
/// pairs with `peer_nodes_for_tenant`.
fn host_nodes_for_project(c: &Arc<CloudState>, project: &str) -> Vec<String> {
    c.peer_deployments
        .read()
        .iter()
        .filter(|(_, deps)| deps.iter().any(|d| d.project == project))
        .map(|(node, _)| node.clone())
        .collect()
}

/// GET `path` from a peer admin, forwarding the team header, as raw bytes: the byte-oriented
/// sibling of [`fetch_from_host`] for payloads that are not JSON (blob objects). Deliberately
/// HTTP-admin-only: its callers all fall back to their existing not-found behaviour on `None`, so
/// it can never make a read worse.
pub(crate) async fn fetch_bytes_from_host(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
) -> Option<Vec<u8>> {
    let admin = c.node_admins.read().get(node).cloned()?;
    let mut rb = c
        .http
        .get(format!("{admin}{path}"))
        .header("x-hive-team", team)
        .timeout(std::time::Duration::from_secs(15));
    if crate::auth::enforced() {
        if let Ok(tok) = crate::auth::issue(
            "mesh-internal",
            team,
            "service",
            false,
            crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
        ) {
            rb = rb.bearer_auth(tok);
        }
    }
    let resp = rb.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

async fn proxy_get_json(c: &Arc<CloudState>, admin: &str, path: &str, team: &str) -> Option<Value> {
    let mut rb = c
        .http
        .get(format!("{admin}{path}"))
        .header("x-hive-team", team)
        .timeout(std::time::Duration::from_secs(10));
    if crate::auth::enforced() {
        if let Ok(tok) = crate::auth::issue(
            "mesh-internal",
            team,
            "service",
            false,
            crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
        ) {
            rb = rb.bearer_auth(tok);
        }
    }
    let resp = rb.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Host node NAME for deployment `id` (no HTTP-admin requirement).
pub(crate) fn host_node_for_deployment(c: &Arc<CloudState>, id: &str) -> Option<String> {
    c.peer_deployments
        .read()
        .iter()
        .find(|(_, deps)| deps.iter().any(|d| d.id.to_string() == id))
        .map(|(node, _)| node.clone())
}

/// Host node NAME for the selected upload source lineage; matching the platform-issued build id
/// stamped in `GitSource.commit` is load-bearing, since a host carrying only a newer preview
/// checkout cannot rebuild production.
fn host_node_for_source_ids(c: &Arc<CloudState>, ids: &[String]) -> Option<String> {
    c.peer_deployments
        .read()
        .iter()
        .find(|(_, deps)| {
            deps.iter().any(|d| {
                d.git.as_ref().is_some_and(|g| {
                    crate::git::upload_source_build_id(g)
                        .is_some_and(|id| ids.iter().any(|allowed| allowed == &id))
                })
            })
        })
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

/// Every deployment across the WHOLE fleet, ALL tenants (operator scope): the locally hosted set
/// plus every gossiped `peer_deployments` entry, deduped by id. `dep_list`'s aggregation without
/// the tenant filter (operator-only callers), since a bare `c.gw.list()` returns only what this
/// node hosts (often nothing on a coordinator).
fn fleet_deployments_all(c: &Arc<CloudState>) -> Vec<Value> {
    let mut list = c.gw.list();
    let mut seen: std::collections::HashSet<String> =
        list.iter().map(|d| d.id.to_string()).collect();
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

/// Aggregates per-function usage stats across the WHOLE fleet: this node's live stats plus each
/// peer's `/v1/functions` (HTTP admin or iroh gossip), so the billing meter still bills compute
/// that ran on Firecracker nodes behind a NAT'd coordinator; each `FunctionStats` carries its
/// `tenant`.
pub async fn fleet_function_stats(c: &Arc<CloudState>) -> Vec<fluid_compute::FunctionStats> {
    let mut out: Vec<fluid_compute::FunctionStats> = c.fluid.stats();
    for v in fan_out_peers(c, &all_healthy_peers(c), "", "/v1/functions?local=true").await {
        if let Ok(mut stats) = serde_json::from_value::<Vec<fluid_compute::FunctionStats>>(v) {
            out.append(&mut stats);
        }
    }
    out
}

/// The mesh tenant query segment: `team=<t>` plus a signed, short-lived `&tok=<jwt>` delegation
/// when JWT signing is configured; the host node verifies the token as an authoritative expiring
/// tenant assertion instead of trusting the raw `team=` param (`gossip::team_claims`). An empty
/// `team` yields an empty segment.
pub(crate) fn mesh_team_qs(team: &str) -> String {
    if team.is_empty() {
        return String::new();
    }
    if crate::auth::enforced() {
        if let Ok(tok) = crate::auth::issue(
            "mesh-internal",
            team,
            "service",
            false,
            crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
        ) {
            return format!("team={team}&tok={tok}");
        }
    }
    format!("team={team}")
}

/// GETs a read-view from a host node by NAME: the HTTP admin when known, else over the iroh mesh
/// (node id + addr from the gossip registry), where the team rides as a query param because that
/// transport has no HTTP headers.
pub(crate) async fn fetch_from_host(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
) -> Option<Value> {
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
        if let Some(b) = crate::gossip::request_to(
            c,
            &id,
            &addr,
            hive_p2p::GOSSIP_GET,
            &p,
            &[],
            MESH_FORWARD_TIMEOUT_SECS,
        )
        .await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// Per-peer budget for CONTROL fan-outs (the billing meter's `fleet_function_stats`):
/// `fetch_from_host`'s own 10 s HTTP / 20 s iroh budgets are dial-resilience budgets, wrong on a
/// request path (one cold-trunk peer made `/v1/functions` and `/v1/metrics` take a flat ~20 s per
/// poll). Fetches run concurrently, so the whole fan-out costs at most one budget and a peer that
/// misses it contributes to the next poll; polled dashboard reads use
/// [`POLLED_READ_PER_PEER_BUDGET`].
const PER_PEER_BUDGET: Duration = Duration::from_secs(8);

/// Per-peer budget for a POLLED dashboard read (every `fan_out_peers_polled` caller): a peer's
/// `?local=true` handler answers in ~0.5 ms and the mesh round trip is 100-200 ms, so 3 s is >10x
/// headroom over a healthy peer, while 8 s turned a congested trunk into a flat 8 s page load (the
/// admin fan-out shares the pooled trunks with gossip and the multi-MB store_sync pulls). A peer
/// that misses it is merged on the next poll.
const POLLED_READ_PER_PEER_BUDGET: Duration = Duration::from_secs(3);

/// TTL of the dashboard-polled response caches (`/v1/functions`, `/v1/metrics`): it MUST outlast
/// one poll interval plus one build, or a poller never sees a warm entry (a 3 s TTL expired before
/// the dashboard's 3-5 s cadence asked again, so every poll rebuilt, while back-to-back `curl`
/// calls looked cached). 15 s is >4x the measured worst-case build; metrics are minute-granular and
/// the functions list only changes on a deploy.
const POLLED_RESPONSE_CACHE_TTL: Duration = Duration::from_secs(15);

/// Fans `path` (identical for every peer) out to every node in `peers` CONCURRENTLY and returns
/// each reachable peer's parsed JSON; unreachable or malformed peers are silently absent, never
/// failing the whole read. `join_all` bounds the added latency to the SLOWEST single hop instead of
/// the sum of sequential hops; callers keep their own merge logic, and `peers` is caller-supplied
/// (`peer_nodes_for_tenant` narrows to the nodes hosting the tenant's projects).
async fn fan_out_peers(
    c: &Arc<CloudState>,
    peers: &[String],
    team: &str,
    path: &str,
) -> Vec<Value> {
    fan_out_peers_bounded(c, peers, team, path, PER_PEER_BUDGET).await
}

/// [`fan_out_peers`] for the endpoints the dashboard re-asks every few seconds (functions, metrics,
/// cron, workflows, webhooks, notifications, ops overview, service graph), with the tighter
/// [`POLLED_READ_PER_PEER_BUDGET`]. Deliberately NOT used by the billing meter's
/// `fleet_function_stats`: under-counting metered compute is a revenue-correctness question and
/// that loop is not latency-critical.
async fn fan_out_peers_polled(
    c: &Arc<CloudState>,
    peers: &[String],
    team: &str,
    path: &str,
) -> Vec<Value> {
    fan_out_peers_bounded(c, peers, team, path, POLLED_READ_PER_PEER_BUDGET).await
}

async fn fan_out_peers_bounded(
    c: &Arc<CloudState>,
    peers: &[String],
    team: &str,
    path: &str,
    budget: Duration,
) -> Vec<Value> {
    futures::future::join_all(peers.iter().map(|name| async move {
        tokio::time::timeout(budget, fetch_from_host(c, name, path, team))
            .await
            .ok()
            .flatten()
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Every OTHER healthy node in the registry — the peer set for fan-outs that
/// aren't tenant-scoped (metrics/cron/functions/overview see the whole fleet).
fn all_healthy_peers(c: &Arc<CloudState>) -> Vec<String> {
    let self_name = c.node_name.clone();
    c.registry
        .nodes()
        .into_iter()
        .filter(|n| n.name != self_name && n.healthy)
        .map(|n| n.name)
        .collect()
}

/// Assembles the deploy request for a redeploy from the resolved source. `no_fanout` pins a zip
/// redeploy to the node holding the retained source, git/image redeploys leave it false so the
/// placement scheduler runs. `image_port_spec` is the ORIGINAL port/protocol of the project's
/// newest deployment, restored onto `image_port`/`image_protocol` so an image redeploy keeps an
/// explicit override (or a UDP-only image's only port); harmless for git/zip, where `image_ref`
/// stays `None`.
fn redeploy_request(
    project: &str,
    src: &fluid_core::GitSource,
    target: Option<String>,
    use_cache: bool,
    root_dir: Option<String>,
    no_fanout: bool,
    git_token: Option<String>,
    image_port_spec: Option<fluid_core::PortSpec>,
    source_deployment_ids: Vec<String>,
) -> fluid_core::GitDeployRequest {
    fluid_core::GitDeployRequest {
        source_deployment_ids,
        repo_url: src.repo_url.clone(),
        branch: Some(src.branch.clone()).filter(|b| !b.is_empty()),
        commit: None,
        head_repo_url: None,
        project: Some(project.to_string()),
        project_incarnation: None,
        creator: Some("you".into()),
        production: true,
        target,
        use_cache,
        root_dir,
        env: None,
        no_fanout,
        fanout_secondary: false,
        build_config: None,
        function_settings: None,
        redeploy: false,
        zip_b64: None,
        image_ref: None,
        image_port: image_port_spec.as_ref().map(|p| p.container_port),
        image_protocol: image_port_spec.as_ref().map(|p| p.protocol),
        image_memory: None,
        image_cpus: None,
        image_pids: None,
        image_ports: None,
        git_token,
    }
}

/// Redeploys a project from its newest deployment's source (see [`source_for_project_fleet`]): a
/// zip project rebuilds from the RETAINED source (locally when this node holds it, else dispatched
/// to the holder), an image project is redeployed as an image, a real git source re-clones through
/// the normal placement path. Without an explicit environment the rebuild lands in the sourced
/// deployment's OWN environment, never branch-vs-production_branch guessing. A project that never
/// deployed falls back to its `last_build` source (a failed first build is retried on the
/// production lane); the webhook and the git poll keep ignoring never-deployed projects.
async fn project_redeploy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(body): Json<RedeployBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let incarnation = {
        let _lifecycle = crate::project_settings::lifecycle_write(&project).await;
        require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
        match c.projects.active_incarnation(&project) {
            Ok(incarnation) => incarnation,
            Err(crate::project_settings::ProjectIncarnationError::Legacy) => c
                .projects
                .ensure_incarnation(&project)
                .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?,
            Err(error) => return Err((StatusCode::NOT_FOUND, error.to_string())),
        }
    };

    let target = body
        .target
        .as_ref()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| t == "production" || t == "preview");
    let root_dir = Some(
        c.projects
            .get_exact(&project, incarnation)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?
            .build
            .root_dir,
    )
    .filter(|s| !s.is_empty());

    let last_build_source = || {
        let row = c.projects.get_if_set(&project)?;
        let build = row.last_build?;
        if build.repo_url.is_empty()
            || build.repo_url.starts_with("upload://")
            || build.repo_url.starts_with("image://")
        {
            return None;
        }
        Some((
            fluid_core::GitSource {
                repo_url: build.repo_url,
                branch: build.branch,
                commit: String::new(),
                commit_message: String::new(),
            },
            true,
            Vec::new(),
        ))
    };
    let Some((src, prod_lane, source_ids)) =
        source_for_project_fleet(&c, &project).or_else(last_build_source)
    else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Project '{project}' has no deployment or build to redeploy from yet."),
        ));
    };
    let target = target.or_else(|| Some(if prod_lane { "production" } else { "preview" }.into()));
    let image_spec = image_port_spec_for_project_fleet(&c, &project);

    if !src.is_real_git() && src.repo_url.starts_with("upload://") {
        if crate::git::has_local_source_for_ids(&project, &source_ids) {
            let req = redeploy_request(
                &project,
                &src,
                target.clone(),
                body.use_cache,
                root_dir.clone(),
                true,
                body.git_token.clone(),
                image_spec.clone(),
                source_ids.clone(),
            );
            let build_id = crate::git::start_build(c.clone(), req, Some(incarnation), None)
                .await
                .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
            return Ok(Json(json!({ "build_id": build_id })));
        }
        if let Some(host) = host_node_for_source_ids(&c, &source_ids) {
            if host != c.node_name {
                if let Some(t) = target_for_node(&c, &host) {
                    let req = redeploy_request(
                        &project,
                        &src,
                        target.clone(),
                        body.use_cache,
                        root_dir.clone(),
                        true,
                        body.git_token.clone(),
                        image_spec.clone(),
                        source_ids.clone(),
                    );
                    let build_id = crate::git::redeploy_on_host(
                        c.clone(),
                        project.clone(),
                        req,
                        t,
                        Some(incarnation),
                    )
                    .await
                    .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
                    return Ok(Json(json!({ "build_id": build_id })));
                }
            }
        }
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("The uploaded source for '{project}' is not available on a reachable node — re-upload the archive to redeploy."),
        ));
    }

    if src.repo_url.starts_with("image://") {
        let image_ref = src
            .repo_url
            .strip_prefix("image://")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(image_ref) = image_ref else {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("The image reference for '{project}' is unavailable — redeploy from a new image."),
            ));
        };
        let mut req = redeploy_request(
            &project,
            &src,
            target.clone(),
            body.use_cache,
            root_dir.clone(),
            false,
            body.git_token.clone(),
            image_spec.clone(),
            source_ids.clone(),
        );
        req.repo_url = src.repo_url.clone();
        req.branch = None;
        req.image_ref = Some(image_ref);
        let build_id = crate::git::start_build(c.clone(), req, Some(incarnation), None)
            .await
            .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
        return Ok(Json(json!({ "build_id": build_id })));
    }

    let req = redeploy_request(
        &project,
        &src,
        target,
        body.use_cache,
        root_dir,
        false,
        body.git_token.clone(),
        image_spec,
        source_ids,
    );
    let build_id = crate::git::start_build(c.clone(), req, Some(incarnation), None)
        .await
        .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
    Ok(Json(json!({ "build_id": build_id })))
}

#[derive(Default, serde::Deserialize)]
pub(crate) struct GitopsProjectsQuery {
    /// `summary`: the dashboard's inventory view (`project_inventory::summary`), served by the leader so
    /// a just-finished build is visible from any node; absent: the GitOps artifact rows below.
    #[serde(default)]
    pub view: Option<String>,
    /// Set by a proxying node: answer from this node's own view, never re-proxy.
    #[serde(default)]
    pub local: Option<bool>,
}

/// All projects the tenant owns with their settings and git source, the data the dashboard
/// serializes into the committed `openedge.yaml`. Fleet-aware: the union of local settings rows,
/// every project a fleet-aggregated deployment names for this tenant and the fleet-replicated
/// relational mirror, so a project created (even with zero deployments) on ANOTHER node still
/// appears. `view=summary` serves the inventory view (`project_inventory::summary`).
pub(crate) async fn gitops_projects(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<GitopsProjectsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if q.view.as_deref() == Some("summary") {
        require_auth_read(claims.as_ref().map(|e| &e.0))?;
        if q.local != Some(true) {
            if let Some(leader) = c.leader_forward_target() {
                let read = fetch_from_host(
                    &c,
                    &leader,
                    "/v1/gitops/projects?view=summary&local=true",
                    &t,
                );
                if let Ok(Some(v)) = tokio::time::timeout(POLLED_READ_PER_PEER_BUDGET, read).await {
                    return Ok(Json(v));
                }
            }
        }
        return Ok(Json(json!(crate::project_inventory::summary(&c, &t))));
    }
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (project, settings) in c.projects.snapshot() {
        if norm(&settings.team) != t {
            continue;
        }
        seen.insert(project.clone());
        let git = c.gw.git_for_project(&project);
        let prod =
            c.gw.list()
                .into_iter()
                .find(|d| d.project == project && d.production);
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
    out.sort_by(|a, b| {
        a["project"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project"].as_str().unwrap_or(""))
    });
    Ok(Json(json!(out)))
}

async fn gitops_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
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

async fn gitops_unlink(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
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

/// Inbound GitHub webhook: a push (or an opened/updated PR) to a repo that backs existing projects
/// triggers a build of the notified commit; a PR always builds a PREVIEW, a push is classified per
/// project from its branch. The route is in the auth `open` allowlist (GitHub cannot present a
/// platform JWT), so the HMAC-SHA256 signature is the gate: verified whenever
/// `GITHUB_WEBHOOK_SECRET` is set, and with no secret an unsigned delivery is rejected (401) unless
/// `GITHUB_WEBHOOK_ALLOW_UNSIGNED=1` (local/dev only).
async fn git_webhook(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    let secret = std::env::var("GITHUB_WEBHOOK_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let allow_unsigned = secret.is_none()
        && std::env::var("GITHUB_WEBHOOK_ALLOW_UNSIGNED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if !sig.is_empty() {
        match &secret {
            Some(s) => {
                if !verify_github_sig(s.as_bytes(), &body, sig) {
                    let repo = serde_json::from_slice::<Value>(&body)
                        .ok()
                        .and_then(|v| {
                            v.get("repository")
                                .and_then(|r| r.get("full_name"))
                                .and_then(|n| n.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    let event = headers
                        .get("x-github-event")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    tracing::warn!(
                        repo = %repo,
                        event = %event,
                        "webhook REJECTED: HMAC signature did not verify. Either a forged \
                         delivery, or GITHUB_WEBHOOK_SECRET differs between the UI that \
                         installed this hook and this backend — in which case every genuine \
                         delivery is being dropped and git auto-deploy is inert. Compare the \
                         value both halves read (the RUNNING process env, not the unit file)."
                    );
                    return Err((StatusCode::UNAUTHORIZED, "bad signature".into()));
                }
            }
            None => {
                if !allow_unsigned {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "signed delivery but GITHUB_WEBHOOK_SECRET is not configured".into(),
                    ));
                }
            }
        }
    } else if !allow_unsigned {
        return Err((
            StatusCode::UNAUTHORIZED,
            "unsigned webhook delivery rejected. The GitHub hook has no signing \
             secret; reconnect GitHub to re-sign it, or set \
             GITHUB_WEBHOOK_ALLOW_UNSIGNED=1 to accept unsigned deliveries."
                .into(),
        ));
    } else {
        tracing::warn!("accepting UNSIGNED webhook delivery (GITHUB_WEBHOOK_ALLOW_UNSIGNED=1) — re-sign this hook to restore signature verification");
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("push")
        .to_string();
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("bad json: {e}")))?;

    let repo_full = payload["repository"]["full_name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let mut target: Option<String> = None;
    let mut pr_number: Option<u64> = None;
    let (branch, commit, head_repo_url) = match event.as_str() {
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or("");
            if action == "closed" {
                return Ok(Json(json!({ "ignored": "pr closed" })));
            }
            target = Some("preview".into());
            pr_number = payload["number"]
                .as_u64()
                .or_else(|| payload["pull_request"]["number"].as_u64());
            let head_ref = payload["pull_request"]["head"]["ref"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let sha = payload["pull_request"]["head"]["sha"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let head_repo_full = payload["pull_request"]["head"]["repo"]["full_name"]
                .as_str()
                .map(str::to_string);
            let is_fork = match head_repo_full.as_deref() {
                Some(h) => crate::gitops::norm_repo(h) != crate::gitops::norm_repo(&repo_full),
                None => false,
            };
            let head_repo_url = if is_fork {
                payload["pull_request"]["head"]["repo"]["clone_url"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| {
                        head_repo_full
                            .as_deref()
                            .map(|f| format!("https://github.com/{f}.git"))
                    })
            } else {
                None
            };
            (head_ref, sha, head_repo_url)
        }
        "ping" => return Ok(Json(json!({ "pong": true }))),
        _ => {
            if payload["deleted"].as_bool().unwrap_or(false) {
                return Ok(Json(json!({ "ignored": "branch deleted" })));
            }
            let r = payload["ref"].as_str().unwrap_or("");
            let branch = r.rsplit('/').next().unwrap_or("").to_string();
            let sha = payload["after"].as_str().unwrap_or("").to_string();
            (branch, sha, None)
        }
    };

    if repo_full.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "missing repository".into()));
    }
    let want = crate::gitops::norm_repo(&repo_full);
    tracing::info!(event = %event, repo = %repo_full, branch = %branch, commit = %commit, pr = ?pr_number, fork = ?head_repo_url, "git_webhook: received");

    let webhook_git_token: Option<String> = if crate::github_app_auth::configured() {
        match repo_full.split_once('/') {
            Some((owner, repo)) => {
                match crate::github_app_auth::installation_token_for_repo(owner, repo).await {
                    Ok(Some(tok)) => Some(tok),
                    Ok(None) => {
                        tracing::info!(repo = %want, "git_webhook: GitHub App not installed on this repo — falling back to node GITHUB_TOKEN");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(repo = %want, error = %e, "git_webhook: GitHub App installation-token mint failed — falling back to node GITHUB_TOKEN");
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

    let candidates: Vec<String> = if !c.git_index.is_empty() {
        c.git_index.projects_for(&want)
    } else {
        tracing::warn!("git_webhook: reverse index is empty/uninitialized — falling back to a full project scan");
        c.projects.snapshot().into_keys().collect()
    };

    let mut triggered = Vec::new();
    for project in candidates {
        let Some(git) = git_for_project_fleet(&c, &project) else {
            continue;
        };
        if crate::gitops::norm_repo(&git.repo_url) != want {
            continue;
        }
        let deploy_branch = if branch.is_empty() {
            git.branch.clone()
        } else {
            branch.clone()
        };

        if !commit.is_empty() {
            let already_building = c.builds.list().iter().any(|b| {
                b.project == project
                    && matches!(
                        b.state,
                        fluid_core::DeployState::Queued | fluid_core::DeployState::Building
                    )
                    && crate::git::commit_eq(&b.commit, &commit)
            });
            if already_building {
                tracing::info!(
                    project = %project,
                    commit = %commit,
                    "git_webhook: a build for this exact commit is already in flight — skipping duplicate"
                );
                continue;
            }
        }

        let root_dir = Some(c.projects.root_dir_of(&project)).filter(|s| !s.is_empty());
        let req = fluid_core::GitDeployRequest {
            source_deployment_ids: Vec::new(),
            repo_url: git.repo_url.clone(),
            branch: Some(deploy_branch).filter(|b| !b.is_empty()),
            commit: Some(commit.clone()).filter(|s| !s.is_empty()),
            head_repo_url: head_repo_url.clone(),
            project: Some(project.clone()),
            project_incarnation: None,
            creator: Some("github".into()),
            production: true,
            target: target.clone(),
            use_cache: true,
            root_dir,
            env: None,
            no_fanout: false,
            fanout_secondary: false,
            build_config: None,
            function_settings: None,
            redeploy: false,
            zip_b64: None,
            image_ref: None,
            image_port: None,
            image_protocol: None,
            image_memory: None,
            image_cpus: None,
            image_pids: None,
            image_ports: None,
            git_token: webhook_git_token.clone(),
        };
        let webhook_team = norm(&c.projects.team_of(&project)).to_string();
        let build_id = crate::git::start_build(c.clone(), req, None, Some(webhook_team))
            .await
            .map_err(|error| {
                tracing::warn!(project = %project, %error, "git_webhook: start_build rejected the push");
                error
            });
        let Ok(build_id) = build_id else {
            continue;
        };
        let ev = c.event(
            &c.region,
            "DEPLOY",
            &format!("{project}.localhost"),
            "/",
            200,
            "gitops",
            &format!(
                "github {} {} @ {}",
                event,
                want,
                &commit.chars().take(7).collect::<String>()
            ),
        );
        c.record(ev);
        triggered.push(json!({
            "project": project,
            "build_id": build_id,
            "target": target.clone().unwrap_or_else(|| "auto".into()),
            "branch": branch,
        }));
    }

    if triggered.is_empty() {
        tracing::warn!(repo = %want, event = %event, "git_webhook: no project matched this repo — build not started");
    } else {
        tracing::info!(repo = %want, event = %event, triggered = triggered.len(), "git_webhook: build(s) started");
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
    let Some(hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let expected = hmac_sha256(secret, body);
    let expected_hex = hex_lower(&expected);
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

/// HMAC-SHA256, hex-encoded: the one implementation of the primitive shared by every webhook
/// verifier in this crate (GitHub's `X-Hub-Signature-256`, Stripe's `Stripe-Signature`).
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

/// HTTP mount of [`node_announce`]: a self-report rewrites the registry entry for any node name
/// (public address, health, peer id: what Seer, placement and the leader election read) and
/// raises the fencing epoch, so only a node may send it ([`require_mesh_rpc`]: the HTTP fallback of
/// `gossip::fetch` carries a `mesh-internal` delegation). The gossip arm calls the inner handler
/// directly, behind the transport's signer and trust gate.
async fn node_announce_public(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(node): Json<hive_edge::NodeInfo>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // A fleet-wide RPC with no per-tenant meaning: a delegation minted for ANY team still satisfies
    // `is_mesh_internal` alone, so `Some("mesh")` (never `None`) closes that a tenant's own
    // delegation, however it was obtained, can never reach this route.
    require_mesh_rpc(&headers, claims.as_ref().map(|e| &e.0), Some("mesh"))?;
    Ok(node_announce(State(c), Json(node)).await)
}

pub(crate) async fn node_announce(
    State(c): State<Arc<CloudState>>,
    Json(node): Json<hive_edge::NodeInfo>,
) -> Json<Value> {
    c.cluster.adopt_epoch(node.cp_epoch);
    c.registry.upsert_peer_self_report(node);
    Json(json!(c.registry.nodes()))
}

/// Observability for the hot-join mesh: the persisted key-addressed roster (this node's dial map,
/// `endpoint_id -> (endpoint_id, iroh_addr)`, no IPs) plus the live trust-set size, so an operator
/// sees who this node knows and trusts without SSH. Read-only, operator token when JWT is enforced.
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
    Ok(Json(
        json!({ "roster": roster, "trusted_peer_count": trusted }),
    ))
}

#[derive(Deserialize)]
struct MeshAdmitReq {
    endpoint_id: String,
}

/// Operator-initiated admission: pre-registers an endpoint id into this node's trust set before it
/// ever connects (a node whose key arrived out-of-band). Not required by the zero-touch join-proof
/// path (`STREAM_JOIN`: a node holding `HIVE_JWT_SECRET` self-admits on first contact); it exists
/// for `HIVE_PEER_TRUST` fleets that want an explicit, audited allowlist edit. It does not
/// propagate to peers directly: the id becomes visible to them when this node next gossips a
/// NodeInfo carrying that `iroh_addr`.
async fn mesh_admit(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<MeshAdmitReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let id = req.endpoint_id.trim();
    if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "endpoint_id must be a 64-char hex iroh NodeId".into(),
        ));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Ok(mut set) = c.trusted_peer_ids.write() {
        set.insert(id.to_string());
    }
    c.audit.record(
        &t,
        "user",
        "admit",
        "mesh_peer",
        id,
        "manually admitted to the P2P trust set",
    );
    Ok(Json(
        json!({ "admitted": id, "trusted_peer_count": c.trusted_peer_ids.read().map(|s| s.len()).unwrap_or(0) }),
    ))
}

/// Unauthenticated, liveness-adjacent mesh-membership probe (`/v1/mesh`): no JWT, like `/healthz`,
/// since the watchdog polling it has none; deliberately separate from `/healthz` because liveness
/// says nothing about mesh membership (see `MeshHealth`'s doc).
async fn mesh_health(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.mesh_health()))
}

/// The transport's connection-ESTABLISHMENT counters (`hive_p2p::EstablishStats`), the numbers
/// meshwatch's `establishment_wedge` trigger reads. Operator-only, unlike `/v1/mesh`: to an
/// outsider they are an exact oracle for the watchdog's arming state and how close each inbound
/// budget is to refusing. NODE-LOCAL like `/v1/mesh/discovery`: query a node's own admin port (the
/// dashboard's `/ops/*` proxy reads the leader's).
async fn mesh_establish(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(hive_p2p::establish_stats(
        crate::meshwatch::establish_wedge_secs()
    ))))
}

/// This node's tokio-runtime stall counters (`runtime_watch`). Node-local by
/// nature -- every node reports its own runtime.
#[derive(serde::Deserialize)]
struct PlacementQ {
    /// Comma-separated region codes; empty = every region.
    region: Option<String>,
    container: Option<String>,
    gpu: Option<String>,
    runtime_artifact: Option<String>,
    toolchain: Option<String>,
}

fn flag(v: Option<&String>) -> bool {
    v.map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn admin_placement(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<PlacementQ>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let regions: Vec<String> = q
        .region
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let diag = crate::schedule::build_placement_diagnosis(
        &c,
        &regions,
        flag(q.container.as_ref()),
        flag(q.gpu.as_ref()),
        false,
        flag(q.runtime_artifact.as_ref()),
        q.toolchain.as_deref(),
    );
    Ok(Json(json!(diag)))
}

async fn admin_runtime(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(crate::runtime_watch::stats()))
}

/// Which algorithm content verifiers on THIS node actually accepted: operator-only and node-local
/// (like `/v1/admin/runtime`). `accepted_sha256` still climbing means stored digests this node
/// reads are legacy-minted, so a writer flip would make them read as a miss or an orphan; a writer
/// may move only once every node reports it flat across a full deploy cycle, never on a flag day.
async fn admin_content_hash(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(
        json!({ "content_hash": fluid_core::content_verify_stats() }),
    ))
}

/// THIS node's supervised background loops (restart counts, heartbeat age). Operator-only and
/// deliberately NODE-LOCAL, with no leader proxy: a dead reconciler on node X is only visible by
/// asking X, so the round-robin read split is the semantics here, not a bug. Answers "is the world
/// reconciler / lock sweep / anti-entropy loop alive on this node?" without inferring it from log
/// lines that stopped appearing.
async fn tasks_health(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(
        json!({ "tasks": crate::supervise::snapshot(), "memory": crate::supervise::memory_pressure() }),
    ))
}

/// Platform overview: operators get the platform-wide counters and control-plane state; every other
/// signed-in user gets the topology shape only (a tenant's own usage is the tenant-scoped
/// `/v1/metrics`). `control_plane.degraded` means peers are configured but nothing synced within
/// the TTL (the data plane keeps serving local known-good state; this only makes the staleness
/// observable), and `mesh` is the membership health distinct from it: a node launched with no
/// `--peer` args has zero configured peers, so `degraded` is always false for it even when it is
/// fully isolated (see `MeshHealth`). `peer_trust` reports whether P2P admission is enforced
/// (`HIVE_PEER_TRUST`) and how many peers are trusted.
async fn overview(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cl = claims.as_ref().map(|e| &e.0);
    require_auth_read(cl)?;
    if !operator_allowed(cl, crate::auth::enforced()) {
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
            "last_gossip_ms": c.last_gossip_ms(),
            "degraded": c.control_plane_degraded(crate::state::CP_DEGRADED_TTL_MS),
            "mesh": c.mesh_health(),
        },
        "peer_trust": {
            "enforced": std::env::var("HIVE_PEER_TRUST").map(|v| v == "1" || v == "true").unwrap_or(false),
            "trusted_peers": c.trusted_peer_ids.read().map(|s| s.len()).unwrap_or(0),
        },
    })))
}

/// Typed connectivity of `n` as THIS observer sees it, derived from signals the node already
/// tracks: the boolean `healthy` collapses three situations an operator must tell apart. `"self"`;
/// `"suspect"` (gossip-alive but this observer's probes fail repeatedly, withdrawn from
/// placement/DNS by `health::demote`'s guarded path); `"degraded"` (gossip-alive and probes pass,
/// but THIS observer's trunk to it recently failed, `health::is_cold`: an observer-local transport
/// fault, not a sick node); `"healthy"`. A node with no gossip for 30 s is absent from the registry
/// list, so "offline" never appears here. Per-observer by design: through the ops proxy it answers
/// with the LEADER's view.
fn node_connectivity(c: &Arc<CloudState>, n: &hive_edge::region::NodeInfo) -> &'static str {
    if n.is_self {
        "self"
    } else if !n.healthy {
        "suspect"
    } else if crate::health::is_cold(&c.registry, &n.name) {
        "degraded"
    } else {
        "healthy"
    }
}

/// The node record served to a signed-in reader who is not a platform operator: topology, health
/// and capacity descriptors for the network page (mesh map, health, capacity totals), and NONE of
/// the mesh-internal addressing (`iroh_addr`, `peer_id`, `public_ip`, `public_url`, `relay_url`,
/// `guardian_iroh_addr`, `private_addr`: all dialable endpoints). The GPU fields are hardware
/// descriptors like `cpu_cores`, not addresses; omitting them blanked the regions map's GPU
/// colouring and badge for every non-operator viewer. Every node view that reaches a non-operator
/// goes through this one projection.
fn non_operator_node_view(n: &hive_edge::region::NodeInfo, connectivity: &'static str) -> Value {
    json!({
        "id": n.name,
        "name": n.name,
        "region": n.region,
        "city": n.city,
        "country": n.country,
        "lat": n.lat,
        "lon": n.lon,
        "healthy": n.healthy,
        "connectivity": connectivity,
        "last_seen_ms": n.last_seen_ms,
        "is_self": n.is_self,
        "backend": n.backend,
        "cpu_cores": n.cpu_cores,
        "mem_total_mb": n.mem_total_mb,
        "disk_total_gb": n.disk_total_gb,
        "gpu_count": n.gpu_count,
        "gpu_model": n.gpu_model,
        "gpu_vram_mb": n.gpu_vram_mb,
    })
}

/// `/v1/nodes`: operators (and the mesh-internal gossip path, whose peer sync needs
/// `iroh_addr`/`peer_id`) get the full node records plus `connectivity`; every other signed-in user
/// gets [`non_operator_node_view`].
pub(crate) async fn nodes(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cl = claims.as_ref().map(|e| &e.0);
    require_auth_read(cl)?;
    let list = c.registry.nodes();
    if operator_allowed(cl, crate::auth::enforced()) {
        let full: Vec<Value> = list
            .iter()
            .map(|n| {
                let mut v = json!(n);
                v["connectivity"] = json!(node_connectivity(&c, n));
                v
            })
            .collect();
        return Ok(Json(json!(full)));
    }
    let sanitized: Vec<Value> = list
        .iter()
        .map(|n| non_operator_node_view(n, node_connectivity(&c, n)))
        .collect();
    Ok(Json(json!(sanitized)))
}

async fn cluster_status(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    let members: Vec<String> = c.registry.nodes().into_iter().map(|n| n.id).collect();
    Ok(Json(json!(c.cluster.status(members))))
}

/// Anycast routing table: every node with latency and health, the node this edge would route a
/// request to (lowest-latency healthy, region-preferred), and `serving`: per node, how many
/// deployment hosts it actually serves (this node's local ones plus peers' via `peer_routes`;
/// absent = standby), independent of the single anycast pick. Operators (and dev mode) get the full
/// node records; every other reader gets [`non_operator_node_view`] plus `latency_ms` (latency is
/// not addressing), so this route never hands out what `/v1/nodes` withholds.
async fn anycast_table(
    State(c): State<Arc<CloudState>>,
    claims: Option<&crate::auth::Claims>,
) -> Json<Value> {
    let selected = c.registry.anycast(Some(&c.region));
    use std::collections::{HashMap, HashSet};
    let mut serving: HashMap<String, HashSet<String>> = HashMap::new();
    let self_subs: HashSet<String> =
        c.gw.served_hosts()
            .into_iter()
            .filter_map(|h| h.split('.').next().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    if !self_subs.is_empty() {
        serving.insert(c.node_name.clone(), self_subs);
    }
    for (sub, routes) in c.peer_routes.read().iter() {
        for r in routes {
            serving
                .entry(r.node_id.clone())
                .or_default()
                .insert(sub.clone());
        }
    }
    let serving_counts: HashMap<String, usize> =
        serving.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    let table = if operator_allowed(claims, crate::auth::enforced()) {
        json!(c.registry.routing_table())
    } else {
        let rows: Vec<Value> = c
            .registry
            .routing_table()
            .iter()
            .map(|n| {
                let mut row = non_operator_node_view(n, node_connectivity(&c, n));
                row["latency_ms"] = json!(n.latency_ms);
                row
            })
            .collect();
        json!(rows)
    };
    Json(json!({
        "region": c.region,
        "selected": selected.as_ref().map(|n| n.name.clone()),
        "table": table,
        "serving": serving_counts,
    }))
}

async fn ratelimit_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.ratelimit.stats())))
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

/// TENANT-SCOPED function stats (Usage page cost breakdown, Functions list, service-graph and
/// deployment-canvas overlays), scoped like `databases_list`. A verified `service` delegation
/// asking for its own `local=true` slice is the billing meter's unfiltered aggregation path
/// (`fleet_function_stats`); every other caller sees only its tenant's rows. A top-level read also
/// fans out to every healthy peer's `?local=true` (functions run wherever placement put them; the
/// guard stops the recursion) and then RE-SCOPES the merged rows to the caller, because a peer
/// answers `?local=true` UNFILTERED on purpose: without it one tenant's page would list every other
/// tenant's functions, and an anonymous read would match the peer slice. The result is cached per
/// tenant for `POLLED_RESPONSE_CACHE_TTL` (never the inner hop), since the dashboard polls every
/// 3-5 s from several components and tabs.
pub async fn functions(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Query(q): axum::extract::Query<LocalQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let internal = claims
        .as_ref()
        .is_some_and(|e| crate::auth::is_mesh_internal(&e.0));
    let mut list: Vec<Value> = c
        .fluid
        .stats()
        .into_iter()
        .filter(|f| (internal && q.local == Some(true)) || norm(&f.tenant) == t)
        .map(|f| json!(f))
        .collect();
    let is_top_level = q.local != Some(true);
    let cache_key = format!("functions:{t}:{}", internal);
    if is_top_level {
        if let Some(v) = c.resp_cache.get(&cache_key, POLLED_RESPONSE_CACHE_TTL) {
            return Json(v);
        }
    }
    if is_top_level {
        let peers = all_healthy_peers(&c);
        for v in fan_out_peers_polled(&c, &peers, &t, "/v1/functions?local=true").await {
            if let Some(arr) = v.as_array() {
                list.extend(arr.iter().cloned());
            }
        }
        let mut seen = std::collections::HashSet::new();
        list.retain(|v| {
            seen.insert(format!(
                "{}|{}",
                v.get("id").and_then(|x| x.as_str()).unwrap_or(""),
                v.get("name").and_then(|x| x.as_str()).unwrap_or("")
            ))
        });
        if !internal {
            list.retain(|v| norm(v.get("tenant").and_then(|x| x.as_str()).unwrap_or("")) == t);
        }
    }
    let out = json!(list);
    if is_top_level {
        c.resp_cache.set(cache_key, out.clone());
    }
    Json(out)
}

/// Tunnel reuse + #14 byte/backpressure metering for this node's gateway.
async fn tunnels(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.gw.tunnel_stats().await)))
}

/// Relay cost accounting: relay-vs-direct connection and byte breakdown for this node's mesh trunks
/// (`relayed_*` bytes transit a relay server: a real cost and a holepunch-failure signal), plus
/// per-trunk QUIC path state (rtt, cwnd, loss, mtu, selected relay-vs-direct: read it DURING a bulk
/// pull, it is the only surface that separates a relayed path, a loss-collapsed window and an
/// app-limited sender), iroh timeout counters, DNS-reconciler and TLS state, and gossip-verify and
/// post-quantum key-exchange counters. `pq_kex` counts REAL negotiated handshakes
/// (`hive_p2p::record_kex_telemetry`), not a config flag; `classical_fallback` includes every peer
/// that cannot speak X25519MLKEM768, notably the wasm32 browser client, a permanent boundary rather
/// than a bug. Empty when P2P (iroh) is not enabled.
async fn relay_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let pool = c.mesh.read().clone();
    Ok(match pool {
        Some(pool) => {
            let s = pool.relay_stats().await;
            let total =
                s.relayed_bytes_tx + s.relayed_bytes_rx + s.direct_bytes_tx + s.direct_bytes_rx;
            let relayed = s.relayed_bytes_tx + s.relayed_bytes_rx;
            let relayed_pct = if total > 0 {
                relayed as f64 / total as f64
            } else {
                0.0
            };
            Json(json!({
                "enabled": true,
                "relayed_conns": s.relayed_conns,
                "direct_conns": s.direct_conns,
                "relayed_bytes_tx": s.relayed_bytes_tx,
                "relayed_bytes_rx": s.relayed_bytes_rx,
                "direct_bytes_tx": s.direct_bytes_tx,
                "direct_bytes_rx": s.direct_bytes_rx,
                "relayed_bytes_pct": relayed_pct,
                "trunks": s.trunks.iter().map(|t| json!({
                    "endpoint_id": t.endpoint_id,
                    "incarnation": t.incarnation,
                    "has_direct": t.has_direct,
                    "selected_is_relay": t.selected_is_relay,
                    "udp_tx_bytes": t.udp_tx_bytes,
                    "udp_rx_bytes": t.udp_rx_bytes,
                    "lost_packets": t.lost_packets,
                    "lost_bytes": t.lost_bytes,
                    "paths": t.paths.iter().map(|p| json!({
                        "remote": p.remote,
                        "selected": p.selected,
                        "is_ip": p.is_ip,
                        "is_relay": p.is_relay,
                        "rtt_ms": p.rtt_ms,
                        "cwnd": p.cwnd,
                        "congestion_events": p.congestion_events,
                        "spurious_congestion_events": p.spurious_congestion_events,
                        "lost_packets": p.lost_packets,
                        "lost_bytes": p.lost_bytes,
                        "udp_tx_bytes": p.udp_tx_bytes,
                        "udp_rx_bytes": p.udp_rx_bytes,
                        "udp_tx_datagrams": p.udp_tx_datagrams,
                        "udp_rx_datagrams": p.udp_rx_datagrams,
                        "current_mtu": p.current_mtu,
                        "black_holes_detected": p.black_holes_detected,
                        "sent_plpmtud_probes": p.sent_plpmtud_probes,
                        "lost_plpmtud_probes": p.lost_plpmtud_probes,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "timeouts": s.timeouts.iter().map(|t| json!({
                    "node_id": t.node_id,
                    "phase": t.phase,
                    "count": t.count,
                })).collect::<Vec<_>>(),
                "dns_reconciler": {
                    "passes": crate::vercel_dns::STATS.passes.load(std::sync::atomic::Ordering::Relaxed),
                    "creates": crate::vercel_dns::STATS.creates.load(std::sync::atomic::Ordering::Relaxed),
                    "deletes": crate::vercel_dns::STATS.deletes.load(std::sync::atomic::Ordering::Relaxed),
                    "api_errors": crate::vercel_dns::STATS.api_errors.load(std::sync::atomic::Ordering::Relaxed),
                    "empty_set_blocks": crate::vercel_dns::STATS.empty_set_blocks.load(std::sync::atomic::Ordering::Relaxed),
                    "per_name_holds": crate::vercel_dns::STATS.per_name_holds.load(std::sync::atomic::Ordering::Relaxed),
                    "affinity_records": crate::vercel_dns::STATS.affinity_records.load(std::sync::atomic::Ordering::Relaxed),
                    "last_pass_ms": crate::vercel_dns::STATS.last_pass_ms.load(std::sync::atomic::Ordering::Relaxed),
                },
                "tls_zones": crate::acme::installed_zones(),
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
                "pq_kex": {
                    "hybrid_pq": hive_p2p::pq_kex_stats().0,
                    "classical_fallback": hive_p2p::pq_kex_stats().1,
                    "unknown": hive_p2p::pq_kex_stats().2,
                },
            }))
        }
        None => Json(json!({ "enabled": false })),
    })
}

/// Serverless GPU pool snapshot (operator-only): every healthy `gpu_count > 0` node grouped into a
/// named pool by region, with live aggregate and per-node free VRAM; see `gpu_pool`'s module doc
/// for the allocate/release discipline and the fan-out rationale.
async fn gpu_pools(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let regions = crate::gpu_pool::snapshot(&c).await;
    Ok(Json(json!(regions)))
}

/// Managed-inference endpoint listing (operator): every project with an inference spec, its
/// deterministic coordinator/port/URL, and THIS node's live server statuses, authoritative only for
/// endpoints coordinated here (the listing names the coordinator).
async fn inference_endpoints(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let desired: Vec<Value> = c
        .projects
        .snapshot()
        .into_iter()
        .filter_map(|(p, s)| s.inference.map(|i| (p, i)))
        .map(|(p, i)| {
            let coord = crate::inference::coordinator_for(&c, &p);
            let port = crate::inference::port_for(&p);
            let url = coord
                .as_ref()
                .and_then(|n| n.public_ip.clone())
                .map(|ip| format!("http://{ip}:{port}/v1"));
            json!({
                "project": p,
                "model": i.model,
                "pool": i.pool,
                "coordinator": coord.map(|n| n.name),
                "port": port,
                "url": url,
            })
        })
        .collect();
    Ok(Json(json!({
        "endpoints": desired,
        "local_servers": c.inference.statuses(),
        "node": c.node_name,
    })))
}

/// Live memory accounting (operator): the OS view (RSS, thread count), jemalloc's own
/// `stats.allocated/active/resident/retained` (which tell a leak from dirty-page fragmentation;
/// `stats_print` only writes at exit and a SIGKILLed process never reaches it) and every
/// tenant-driven growth bound this node enforces as a live gauge. Unlike `/v1/debug/heap` it is
/// CHEAP (no sampling, no dump), so it is safe to poll and to scrape. NODE-LOCAL, like
/// `/v1/dns/stats`: through the dashboard's `/ops/*` proxy you read the LEADER's memory, not the
/// page-serving node's.
async fn memory_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let mut v = crate::memwatch::snapshot(Some(&c.hive));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("node".into(), json!(c.node_name));
        obj.insert("region".into(), json!(c.region));
    }
    Ok(Json(v))
}

/// Restart audit (operator): did this node die, when and why. Returns this boot's verdict with the
/// EVIDENCE it was reached from (kernel line, cgroup counter, boot id, last-heartbeat RSS against
/// the memory ceiling), the bounded restart history with each previous process's uptime, and the
/// live 24h counters. A cgroup OOM kill is SIGKILL, so the dying process cannot log it: this audit,
/// and the gossiped `NodeInfo::started_ms` / `oom_restarts_24h` / `last_oom_ms` on `/v1/nodes` (the
/// fleet view), are the only record. NODE-LOCAL: the marker file, the history file and `/proc`
/// belong to the answering node, so through the dashboard's `/ops/*` proxy you read the LEADER's
/// restarts.
async fn node_restarts(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(crate::restart_audit::snapshot(&c.node_name)))
}

/// Health-demotion guard (operator): how often THIS observer withdrew a peer from DNS/placement,
/// how often it declined because the peer was still gossiping, and which peers are in a local
/// routing penalty. `refused_gossip_alive` is the number to read during an incident: a large value
/// means this node's transports fail against peers that are demonstrably alive, i.e. the fault is
/// local. NODE-LOCAL by design, since a health verdict is per-observer.
async fn mesh_health_guard(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let mut v = crate::health::stats();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("node".into(), json!(c.node_name));
        obj.insert("region".into(), json!(c.region));
    }
    Ok(Json(v))
}

/// Live heap profile (operator): `GET /v1/debug/heap` returns a jemalloc heap profile in jeprof's
/// native format (`jeprof --show_bytes --pdf <binary> heap.prof`, or
/// `jeprof --base=first.prof <binary> second.prof` for a diff). Sampling is OFF at boot
/// (`prof_active:false` in main.rs's `malloc_conf`); the first call turns it on for the process and
/// leaves it on, because a leak hunt needs the allocations AFTER activation, so the intended use is
/// a diff of two dumps taken while RSS climbs; `?deactivate=1` turns it back off. Linux-only:
/// elsewhere jemalloc profiling is unavailable and this returns 501 instead of failing the build.
#[cfg(target_os = "linux")]
async fn heap_profile(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;
    require_operator(claims.as_ref().map(|e| &e.0))?;

    let want_off = q.get("deactivate").is_some_and(|v| v == "1" || v == "true");
    let set_active = |on: bool| -> Result<(), String> {
        unsafe { tikv_jemalloc_ctl::raw::write(b"prof.active\0", on) }.map_err(|e| e.to_string())
    };

    if want_off {
        set_active(false).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("prof.active=false: {e}"),
            )
        })?;
        tracing::warn!("heap profiling DEACTIVATED via /v1/debug/heap");
        return Ok(Json(json!({ "profiling_active": false })).into_response());
    }

    let already =
        unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0") }.unwrap_or(false);
    if !already {
        set_active(true).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("prof.active=true: {e}"),
            )
        })?;
        tracing::warn!(
            "heap profiling ACTIVATED via /v1/debug/heap — sampling every ~512KiB until deactivated. \
             Take a SECOND dump after RSS has grown; the diff is what identifies a leak."
        );
    }

    let path = format!("/tmp/hive-heap-{}-{}.prof", std::process::id(), now_ms());
    let mut c_path = path.clone().into_bytes();
    c_path.push(0);
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"prof.dump\0", c_path.as_ptr() as *const std::ffi::c_char)
    }
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("prof.dump: {e} (was profiling just enabled? a dump needs samples)"),
        )
    })?;
    let body = std::fs::read(&path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read {path}: {e}"),
        )
    })?;
    let _ = std::fs::remove_file(&path);

    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"heap.prof\"",
            ),
        ],
        body,
    )
        .into_response())
}

#[cfg(not(target_os = "linux"))]
async fn heap_profile(
    axum::extract::Query(_q): axum::extract::Query<std::collections::HashMap<String, String>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Err((
        StatusCode::NOT_IMPLEMENTED,
        "heap profiling is Linux-only".to_string(),
    ))
}

/// Host listener audit (operator), NODE-LOCAL: it describes the answering node, so through the
/// dashboard's `/ops/*` proxy you read the LEADER's host (the fleet view is
/// `scripts/audit-public-listeners.sh`). `last` is `null` until the first pass (~30 s after boot);
/// `supported: false` means the host has no `/proc/net/tcp` (macOS), which is "not audited", never
/// "clean". See `listener_audit`.
async fn host_listeners(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let range = crate::listener_audit::audited_range();
    Ok(Json(json!({
        "node": c.node_name,
        "range": [range.0, range.1],
        "last": crate::listener_audit::last(),
    })))
}

/// Geo-DNS observability (operator): live Seer query counters, the tailored-vs-generic split that
/// tells whether proximity routing works, the local geo table's identity and hit rate
/// (`table_source: "none"` means it failed validation and every answer is generic, the one state
/// worth alerting on) plus the optional remote fallback's memo state and persistence counters, the
/// delegation record count the reconciler published, the per-node proof verdicts and the histogram
/// of which address each answer handed out first. `validation` is derived from verdicts computed
/// FRESH in this response, never from the reconciler-written counters, which lag on non-leaders and
/// right after a restart and contradicted the per-node verdicts; those stay exported as
/// `reconciler_last_pass_*`.
async fn dns_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    use std::sync::atomic::Ordering;
    let s = &crate::dnsserver::DNS_STATS;
    let geo = c.dns_geo.stats();
    let (table_source, table_v4, table_v6) = crate::geoip::table_info();
    let (loaded_at_boot, cache_writes) = c.dns_geo.persist_stats();
    let verdicts = crate::dns_probe::validate_nameservers(&c.registry.nodes());
    let answers: std::collections::BTreeMap<String, u64> = crate::dnsserver::ANSWER_FIRST
        .lock()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    Ok(Json(json!({
        "node": c.node_name,
        "deploy_zone": crate::dnsserver::deploy_zone(),
        "listening": std::env::var("HIVE_DNS_ADDR").ok(),
        "queries": {
            "total": s.queries.load(Ordering::Relaxed),
            "a": s.queries_a.load(Ordering::Relaxed),
            "aaaa": s.queries_aaaa.load(Ordering::Relaxed),
            "other": s.queries_other.load(Ordering::Relaxed),
            "nxdomain": s.nxdomain.load(Ordering::Relaxed),
            "over_tcp": s.over_tcp.load(Ordering::Relaxed),
        },
        "geo": {
            "tailored": s.tailored.load(Ordering::Relaxed),
            "generic": s.generic.load(Ordering::Relaxed),
            "with_ecs": s.with_ecs.load(Ordering::Relaxed),
            "table_source": table_source,
            "table_rows_v4": table_v4,
            "table_rows_v6": table_v6,
            "local_hits": geo.local_hits,
            "local_misses": geo.local_misses,
            "remote_enabled": geo.remote_enabled,
            "remote_known": geo.remote_known,
            "remote_pending": geo.remote_pending,
            "remote_unlocatable": geo.remote_unlocatable,
            "cache_loaded_at_boot": loaded_at_boot,
            "cache_writes": cache_writes,
            "cache_file": crate::dns_geo::cache_path().display().to_string(),
        },
        "delegation_records": crate::vercel_dns::STATS.geo_delegation_records.load(Ordering::Relaxed),
        "nameservers": verdicts
            .iter()
            .map(|v| json!({
                "node": v.node.clone(),
                "region": v.region.clone(),
                "ip4": v.ip4.clone(),
                "ip6": v.ip6.clone(),
                "declared": v.declared,
                "validated": v.validated,
                "reason": v.reason.clone(),
                "attesters": v.attesters.clone(),
                "attester_regions": v.attester_regions.clone(),
                "required_regions": v.required_regions,
            }))
            .collect::<Vec<_>>(),
        "probes": c.dns_probes.snapshot().into_iter().map(|(node, p)| json!({
            "node": node,
            "ip": p.ip,
            "ok": p.ok,
            "attested": p.attested,
            "reason": p.reason,
            "rtt_ms": p.rtt_ms,
            "answers": p.answers,
            "queries": p.queries,
            "client_subnets": p.subnets,
            "fail_streak": p.fail_streak,
            "checked_ms": p.checked_ms,
        })).collect::<Vec<_>>(),
        "validation": {
            "proven": verdicts.iter().filter(|v| v.validated).count(),
            "unproven": verdicts.iter().filter(|v| v.declared && !v.validated).count(),
            "reconciler_last_pass_proven": crate::vercel_dns::STATS.geo_ns_validated.load(Ordering::Relaxed),
            "reconciler_last_pass_unproven": crate::vercel_dns::STATS.geo_ns_unproven.load(Ordering::Relaxed),
            "delegation_holds": crate::vercel_dns::STATS.geo_delegation_holds.load(Ordering::Relaxed),
            "api_delegation_holds": crate::vercel_dns::STATS.api_delegation_holds.load(Ordering::Relaxed),
            "create_circuit_skips": crate::vercel_dns::STATS.create_circuit_skips.load(Ordering::Relaxed),
            "delegation_cutovers": crate::vercel_dns::STATS.delegation_cutovers.load(Ordering::Relaxed),
            "delegation_cutover_rollbacks": crate::vercel_dns::STATS.delegation_cutover_rollbacks.load(Ordering::Relaxed),
            "acme_orphans_swept": crate::vercel_dns::STATS.acme_orphans_swept.load(Ordering::Relaxed),
            "min_attester_regions": crate::dns_probe::MIN_ATTESTER_REGIONS,
            "failed_rounds_before_withdraw": crate::dns_probe::FAILED_ROUNDS_BEFORE_WITHDRAW,
        },
        "answer_first_histogram": answers,
    })))
}

/// Mesh address-lookup observability (operator): which address SOURCES this node registered and
/// what the public mainline-DHT provider has done since boot, so "the DHT is enabled" is evidence
/// (resolves, hits, publishes requested) rather than a config claim. A node whose `seed_providers`,
/// `pkarr_providers` and `n0_enabled` are all zero/false has NO address source, so
/// `PeerPool::dial_fresh` burns its budget for nothing. `publishes_requested` is requested, not
/// succeeded (`AddressLookup::publish` is fire-and-forget), and a `discovery_budget_ms` at or below
/// ~4 s means the DHT provider is registered but cancelled before it can answer (measured resolve
/// 2.0-4.6 s on a cold routing table). NODE-LOCAL by construction, like `/v1/dns/stats`: through
/// the dashboard's `/ops/*` proxy you read the LEADER's numbers, so query a specific node's admin
/// port to ask about that node.
async fn mesh_discovery(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let s = hive_p2p::dht::stats();
    let sources = usize::from(s.n0_enabled) + s.seed_providers + s.pkarr_providers;
    Ok(Json(json!({
        "node": c.node_name,
        "endpoint_id": c.iroh.read().as_ref().map(|e| e.id().to_string()),
        "sources": {
            "bootstrap_seeds": s.seed_providers,
            "pkarr_relays": s.pkarr_providers,
            "n0_public": s.n0_enabled,
            "mainline_dht": s.dht_registered,
            "non_dht_total": sources,
        },
        "dht": {
            "registered": s.dht_registered,
            "skip_reason": s.dht_skip_reason,
            "bootstrap_nodes": s.dht_bootstrap_nodes,
            "port": s.dht_port,
            "publish": s.dht_publish,
            "publish_direct": s.dht_publish_direct,
            "publishes_requested": s.publishes_requested,
            "last_publish_request_ms": s.last_publish_request_ms,
            "resolves": s.resolves,
            "resolve_hits": s.resolve_hits,
            "resolve_misses": s.resolve_misses,
            "resolve_errors": s.resolve_errors,
        },
        "discovery_budget_ms": hive_p2p::dial_discovery_budget().as_millis() as u64,
        "dial_fallback_ceiling_ms": hive_p2p::dial_fallback_ceiling().as_millis() as u64,
    })))
}

/// `/v1/logs` query. EVERY field is bounded at parse time (see [`logs`]), because an unbounded
/// window, list or limit on a fan-out endpoint is a denial-of-service vector, and a malformed term
/// is a 400, never a silently ignored filter. Every parameter must be reconstructed in BOTH
/// [`crate::gossip::logs_query`] (the mesh dispatch arm) and the fan-out query string built by
/// [`logs_fanout_qs`]: one added in only one place works on the owner node and silently returns
/// unfiltered or empty data on every peer.
#[derive(Deserialize)]
pub(crate) struct LogsQ {
    pub(crate) limit: Option<usize>,
    /// Filter events to a project (matches the deployment host subdomain).
    pub(crate) project: Option<String>,
    /// Filter events to ONE deployment: events tagged with this deployment id at record time plus
    /// untagged events on its own alias hosts; composes with `project`.
    #[serde(default)]
    pub(crate) deployment: Option<String>,
    /// Free-text search across path/host/detail.
    pub(crate) q: Option<String>,
    /// Internal: when set by a fleet-aggregation proxy, return ONLY this node's
    /// local events (no fan-out) — prevents proxy recursion.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
    /// Named window: `30m` | `1h` | `24h` | `7d` | `30d`, resolved to absolute bounds server-side
    /// and never forwarded as a string (a peer resolving it against its own clock would widen or
    /// shrink the window).
    #[serde(default)]
    pub(crate) range: Option<String>,
    /// Absolute inclusive lower bound (ms epoch).
    #[serde(default)]
    pub(crate) since_ms: Option<u64>,
    /// Absolute inclusive upper bound (ms epoch).
    #[serde(default)]
    pub(crate) until_ms: Option<u64>,
    /// Status filter: comma-separated codes and/or classes (`404,5xx`).
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) method: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) route: Option<String>,
    /// `info` | `warning` | `error` | `fatal` (derived — see `event_log`).
    #[serde(default)]
    pub(crate) level: Option<String>,
    /// `hit` | `miss` | `stale` | `store` | `revalidate`.
    #[serde(default)]
    pub(crate) cache: Option<String>,
    /// `production` | `preview` (derived from the deployment's target).
    #[serde(default)]
    pub(crate) env: Option<String>,
    /// `static` | `function` | `fullstack` (derived from the deployment's kind).
    #[serde(default)]
    pub(crate) resource: Option<String>,
    /// Rows to skip AFTER the fleet-wide merge (coordinator-only: a peer
    /// skipping its own first N would page the wrong rows).
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    /// Return only events strictly older than this `ts_ms` (per-node bound).
    #[serde(default)]
    pub(crate) cursor: Option<u64>,
}

/// Default `limit` — unchanged, and deliberately small: the dashboard polls
/// this endpoint every 1.5 s.
pub(crate) const LOGS_LIMIT_DEFAULT: usize = 100;
/// Hard ceiling on `limit`. Was unclamped (`limit=100000` was accepted), and
/// the coordinator sorts + merges `nodes x limit` every poll.
pub(crate) const LOGS_LIMIT_MAX: usize = 1000;
/// Hard ceiling on `offset` (pagination depth).
pub(crate) const LOGS_OFFSET_MAX: usize = 100_000;
/// Retention ceiling: no plan reaches further back than this.
pub(crate) const LOGS_MAX_AGE_MS: u64 = 30 * crate::event_log::DAY_MS;
/// Slack on the age comparison: the coordinator resolves `range=7d` into an absolute `since_ms` on
/// ITS clock and every peer re-checks it against its own, so without slack a peer whose clock is
/// milliseconds ahead refuses a window the coordinator accepted and the whole fleet-wide read fails
/// on clock skew. 5 minutes is slack against a 7/30-day window, not a loophole.
pub(crate) const LOGS_AGE_TOLERANCE_MS: u64 = 5 * 60_000;

/// One extra row per node beyond `limit + offset`: fetching exactly `limit + offset` makes `total`
/// land on `limit` whenever more rows exist, so `has_more` would be false on page 1 of every result
/// set and the UI's "Next" could never advance. Peer plus local fetch stays under
/// `LOGS_LIMIT_MAX + 1`, so it does not widen the fan-out bound.
const LOGS_PROBE_ROWS: usize = 1;

/// Named window -> span in ms. `None` = unknown range (caller turns that into a
/// 400 rather than silently ignoring it).
pub(crate) fn logs_range_ms(r: &str) -> Option<u64> {
    match r.trim().to_ascii_lowercase().as_str() {
        "30m" | "30min" => Some(30 * 60_000),
        "1h" | "60m" => Some(60 * 60_000),
        "24h" | "1d" => Some(crate::event_log::DAY_MS),
        "7d" | "1w" => Some(7 * crate::event_log::DAY_MS),
        "30d" => Some(30 * crate::event_log::DAY_MS),
        _ => None,
    }
}

fn logs_err(code: &str, message: String) -> String {
    serde_json::to_string(&json!({ "error": { "code": code, "message": message } }))
        .unwrap_or_else(|_| code.to_string())
}

/// Validate every term of a comma-separated enum filter against `allowed`.
fn logs_validate_terms(
    raw: Option<&str>,
    field: &str,
    allowed: &[&str],
) -> Result<(), (StatusCode, String)> {
    let Some(raw) = raw.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    for term in raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .take(16)
    {
        if term.is_empty() {
            continue;
        }
        if !allowed.contains(&term.as_str()) {
            return Err((
                StatusCode::BAD_REQUEST,
                logs_err(
                    "LOGS_BAD_FILTER",
                    format!(
                        "logs: {field}='{term}' is not one of {}",
                        allowed.join(", ")
                    ),
                ),
            ));
        }
    }
    Ok(())
}

fn logs_hdr(h: &mut HeaderMap, k: &'static str, v: impl std::fmt::Display) {
    if let Ok(val) = HeaderValue::from_str(&v.to_string()) {
        h.insert(k, val);
    }
}

/// Event log read (`/v1/logs`): every input is bounded first and a malformed filter is a 400, never
/// ignored. The window resolves to absolute bounds (`range` or `since_ms`/`until_ms`) and is gated
/// on the OWNING TENANT's plan SERVER-SIDE (history older than the hobby window of 7 days is
/// Pro/Enterprise, up to the 30-day retention ceiling): a request past it is REFUSED with a typed
/// error, never silently truncated, with no operator carve-out because platform admins read as
/// `enterprise` through the billing floor (the team row wins over billing, as in `team_plan`).
/// Scoping runs INSIDE the scan: only events of projects this team owns, UNATTRIBUTED events (empty
/// project: apex bot probes, unresolved hosts, control-plane noise) are operator-only in the
/// unfiltered view because they leak cross-tenant request metadata, and a project view matches the
/// EXACT project tag or untagged events on one of that project's real deployment hostnames, never a
/// substring of host/detail (pre-tag deployment events match on the host's first label only, since
/// project and branch aliases move on promote). Requests are recorded on the node that SERVES them,
/// so a top-level read merges the relevant peers' events (`local=true` against recursion; a project
/// filter targets its host node(s), otherwise every peer hosting one of this tenant's projects,
/// addressed by NODE through `fetch_from_host` because FC nodes have no HTTP admin URL), surfaces a
/// peer's error envelope (plan gate, bad filter) instead of merging it as an empty array, and sorts
/// newest-first with a deterministic tie-break so paging does not reshuffle.
pub(crate) async fn logs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<LogsQ>,
) -> Result<(HeaderMap, Json<Value>), (StatusCode, String)> {
    use crate::event_log::{parse_list, DeployFacts, EventFilter, StatusPred};

    let now = hive_core::now_ms();
    let limit = q
        .limit
        .unwrap_or(LOGS_LIMIT_DEFAULT)
        .clamp(1, LOGS_LIMIT_MAX);
    let offset = q
        .offset
        .unwrap_or(0)
        .min(LOGS_OFFSET_MAX)
        .min(LOGS_LIMIT_MAX.saturating_sub(limit));
    let cl = claims.as_ref().map(|e| &e.0);
    let is_operator = operator_allowed(cl, crate::auth::enforced());
    let t = tenant(&c, &headers, cl);

    let mut since_ms = q.since_ms.unwrap_or(0);
    let mut until_ms = q.until_ms.unwrap_or(u64::MAX);
    let range_name = q
        .range
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    if let Some(r) = range_name.as_deref() {
        if q.since_ms.is_some() || q.until_ms.is_some() {
            return Err((
                StatusCode::BAD_REQUEST,
                logs_err(
                    "LOGS_BAD_RANGE",
                    "logs: pass either range= or since_ms/until_ms=, not both".into(),
                ),
            ));
        }
        let span = logs_range_ms(r).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                logs_err(
                    "LOGS_BAD_RANGE",
                    format!("logs: unknown range '{r}' — use 30m, 1h, 24h, 7d or 30d"),
                ),
            )
        })?;
        since_ms = now.saturating_sub(span);
        until_ms = u64::MAX;
    }
    if let Some(cur) = q.cursor {
        until_ms = until_ms.min(cur.saturating_sub(1));
    }
    if since_ms > 0 && until_ms < since_ms {
        return Err((
            StatusCode::BAD_REQUEST,
            logs_err(
                "LOGS_BAD_RANGE",
                "logs: until_ms/sort cursor is before since_ms".into(),
            ),
        ));
    }

    if since_ms > 0 {
        let age_ms = now.saturating_sub(since_ms);
        let span_ms = until_ms.min(now).saturating_sub(since_ms);
        if age_ms > LOGS_MAX_AGE_MS + LOGS_AGE_TOLERANCE_MS
            || span_ms > LOGS_MAX_AGE_MS + LOGS_AGE_TOLERANCE_MS
        {
            return Err((
                StatusCode::BAD_REQUEST,
                logs_err(
                    "LOGS_RANGE_TOO_LARGE",
                    format!(
                        "logs: requested window is wider than the {}-day history the platform retains",
                        LOGS_MAX_AGE_MS / crate::event_log::DAY_MS
                    ),
                ),
            ));
        }
        let plan = match q.project.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => team_plan(&c, p),
            None => c
                .teams
                .get(&t)
                .map(|x| x.plan)
                .unwrap_or_else(|| c.billing.account(&t).plan),
        };
        let max_age = crate::billing::plan_max_log_age_ms(&plan);
        if age_ms > max_age + LOGS_AGE_TOLERANCE_MS || span_ms > max_age + LOGS_AGE_TOLERANCE_MS {
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                serde_json::to_string(&json!({
                    "error": {
                        "code": "PLAN_UPGRADE_REQUIRED",
                        "feature": "extended_log_history",
                        "message": format!(
                            "Log history older than {} days requires the Pro or Enterprise plan (current plan: {plan}).",
                            crate::billing::plan_max_log_age_ms("hobby") / crate::event_log::DAY_MS
                        ),
                        "plan": plan,
                        "requested_age_ms": age_ms,
                        "max_age_ms": max_age,
                    }
                }))
                .unwrap_or_else(|_| "PLAN_UPGRADE_REQUIRED".to_string()),
            ));
        }
    }

    logs_validate_terms(
        q.level.as_deref(),
        "level",
        &["info", "warning", "error", "fatal"],
    )?;
    logs_validate_terms(
        q.cache.as_deref(),
        "cache",
        &["hit", "miss", "stale", "store", "revalidate"],
    )?;
    logs_validate_terms(q.env.as_deref(), "env", &["production", "preview"])?;
    logs_validate_terms(
        q.resource.as_deref(),
        "resource",
        &["static", "function", "fullstack"],
    )?;
    let mut statuses: Vec<StatusPred> = Vec::new();
    if let Some(raw) = q
        .status
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        for term in raw.split(',').take(16) {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            match StatusPred::parse(term) {
                Some(p) => statuses.push(p),
                None => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        logs_err(
                            "LOGS_BAD_FILTER",
                            format!("logs: status='{term}' is not a code (404) or a class (5xx)"),
                        ),
                    ))
                }
            }
        }
    }
    let text =
        q.q.as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.len() > 256 {
                    s[..256].to_string()
                } else {
                    s
                }
            });

    let scope_project = q
        .project
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| p.to_lowercase());
    let scope_deployment = q
        .deployment
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|d| d.to_lowercase());
    let project_hosts: Vec<(String, String)> = match scope_project.as_deref() {
        Some(pl) => {
            c.gw.list()
                .into_iter()
                .filter(|d| d.project.eq_ignore_ascii_case(pl) && record_tenant(&d.tenant) == t)
                .flat_map(|d| {
                    [d.alias, d.commit_alias, d.branch_alias, d.id_alias]
                        .into_iter()
                        .filter(|h| !h.is_empty())
                        .map(|h| h.to_lowercase())
                })
                .map(|h| {
                    let dot = format!("{h}.");
                    (h, dot)
                })
                .collect()
        }
        None => Vec::new(),
    };
    let owned_cache: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
        Default::default();
    let scope: std::sync::Arc<dyn Fn(&crate::state::Event) -> bool + Send + Sync> = {
        let cc = c.clone();
        let tt = t.clone();
        let hosts = project_hosts.clone();
        let oc = owned_cache.clone();
        Arc::new(move |e: &crate::state::Event| {
            if e.project.is_empty() {
                if !is_operator {
                    return false;
                }
            } else {
                let key = e.project.to_lowercase();
                let hit = oc.lock().ok().and_then(|m| m.get(&key).copied());
                let ok = match hit {
                    Some(v) => v,
                    None => {
                        let v = project_owned_by(&cc, &e.project, &tt);
                        if let Ok(mut m) = oc.lock() {
                            if m.len() < 4096 {
                                m.insert(key, v);
                            }
                        }
                        v
                    }
                };
                if !ok {
                    return false;
                }
            }
            if let Some(pl) = scope_project.as_ref() {
                if e.project.to_lowercase() != *pl {
                    if !e.project.is_empty() {
                        return false;
                    }
                    let h = e.host.to_lowercase();
                    if !hosts.iter().any(|(ph, phd)| {
                        h == *ph || h.starts_with(phd) || ph.starts_with(h.as_str())
                    }) {
                        return false;
                    }
                }
            }
            if let Some(dl) = scope_deployment.as_ref() {
                if e.deployment.to_lowercase() != *dl {
                    if !e.deployment.is_empty() {
                        return false;
                    }
                    let h = e.host.to_lowercase();
                    let label = h
                        .split(':')
                        .next()
                        .unwrap_or(&h)
                        .split('.')
                        .next()
                        .unwrap_or(&h)
                        .to_string();
                    if label != *dl {
                        return false;
                    }
                }
            }
            true
        })
    };

    let fetch = limit + offset + LOGS_PROBE_ROWS;
    let filter = EventFilter {
        since_ms,
        until_ms,
        limit: fetch,
        statuses,
        methods: parse_list(q.method.as_deref(), 8, 16).unwrap_or_default(),
        hosts: parse_list(q.host.as_deref(), 8, 128).unwrap_or_default(),
        routes: parse_list(q.route.as_deref(), 8, 256).unwrap_or_default(),
        levels: parse_list(q.level.as_deref(), 4, 16).unwrap_or_default(),
        caches: parse_list(q.cache.as_deref(), 6, 16).unwrap_or_default(),
        envs: parse_list(q.env.as_deref(), 4, 32).unwrap_or_default(),
        resources: parse_list(q.resource.as_deref(), 4, 32).unwrap_or_default(),
        text,
        scope: Some(scope),
    };
    let facts = DeployFacts::build(&c.gw.list());

    let evs = match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| c.query_events(&filter, &facts))
        }
        _ => c.query_events(&filter, &facts),
    };
    let mut out: Vec<Value> = evs
        .into_iter()
        .map(|e| {
            let d = facts.derive(&e);
            let mut v = json!(e);
            if let Some(o) = v.as_object_mut() {
                o.insert("env".into(), json!(d.env));
                o.insert("resource".into(), json!(d.resource));
                o.insert("cache".into(), json!(d.cache));
                o.insert("level".into(), json!(d.level));
            }
            v
        })
        .collect();

    if !q.local.unwrap_or(false) {
        let qs = logs_fanout_qs(&q, fetch, since_ms, until_ms);
        let nodes: Vec<String> = match q.project.as_deref() {
            Some(p) if !p.is_empty() => host_nodes_for_project(&c, p),
            _ => peer_nodes_for_tenant(&c, &t),
        };
        for node in nodes {
            if let Some(v) = fetch_from_host(&c, &node, &format!("/v1/logs?{qs}"), &t).await {
                if v.get("error").is_some() {
                    let st = v
                        .get("error")
                        .and_then(|e| e.get("status"))
                        .and_then(|s| s.as_u64())
                        .and_then(|s| StatusCode::from_u16(s as u16).ok())
                        .unwrap_or(StatusCode::BAD_GATEWAY);
                    return Err((st, v.to_string()));
                }
                if let Some(arr) = v.as_array() {
                    out.extend(arr.iter().cloned());
                }
            }
        }
        out.sort_by(|a, b| {
            let ts = |v: &Value| v.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0);
            ts(b)
                .cmp(&ts(a))
                .then_with(|| {
                    a.get("host")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .cmp(b.get("host").and_then(|x| x.as_str()).unwrap_or(""))
                })
                .then_with(|| {
                    a.get("path")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .cmp(b.get("path").and_then(|x| x.as_str()).unwrap_or(""))
                })
        });
    } else {
        out.sort_by(|a, b| {
            b.get("ts_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                .cmp(&a.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0))
        });
    }
    let total = out.len();
    let has_more = total > offset + limit;
    let out: Vec<Value> = out.into_iter().skip(offset).take(limit).collect();

    let mut hdrs = HeaderMap::new();
    logs_hdr(&mut hdrs, "x-hive-logs-limit", limit);
    logs_hdr(&mut hdrs, "x-hive-logs-offset", offset);
    logs_hdr(&mut hdrs, "x-hive-logs-returned", out.len());
    logs_hdr(&mut hdrs, "x-hive-logs-total", total);
    logs_hdr(
        &mut hdrs,
        "x-hive-logs-has-more",
        if has_more { "1" } else { "0" },
    );
    logs_hdr(&mut hdrs, "x-hive-logs-oldest-ms", c.events_oldest_ms());
    if since_ms > 0 {
        logs_hdr(&mut hdrs, "x-hive-logs-since-ms", since_ms);
    }
    if until_ms != u64::MAX {
        logs_hdr(&mut hdrs, "x-hive-logs-until-ms", until_ms);
    }
    if let Some(r) = range_name.as_deref() {
        logs_hdr(&mut hdrs, "x-hive-logs-range", r);
    }
    Ok((hdrs, Json(json!(out))))
}

/// The query string the coordinator sends to each peer. EVERY filter of [`LogsQ`] must appear here
/// or it works on the owner node and silently returns unfiltered data everywhere else. `range=` is
/// deliberately NOT forwarded (it is resolved to absolute bounds first, so a peer cannot widen the
/// window by re-resolving it against its own clock) and neither is `offset=` (paging happens once,
/// after the merge).
fn logs_fanout_qs(q: &LogsQ, fetch: usize, since_ms: u64, until_ms: u64) -> String {
    let mut s = format!("limit={fetch}&local=true");
    if let Some(p) = q.project.as_ref().filter(|p| !p.is_empty()) {
        s.push_str(&format!("&project={}", urlencode(p)));
    }
    if let Some(d) = q.deployment.as_ref().filter(|d| !d.is_empty()) {
        s.push_str(&format!("&deployment={}", urlencode(d)));
    }
    if let Some(qq) = q.q.as_ref().filter(|s| !s.is_empty()) {
        s.push_str(&format!("&q={}", urlencode(qq)));
    }
    if since_ms > 0 {
        s.push_str(&format!("&since_ms={since_ms}"));
    }
    if until_ms != u64::MAX {
        s.push_str(&format!("&until_ms={until_ms}"));
    }
    for (k, v) in [
        ("status", q.status.as_deref()),
        ("method", q.method.as_deref()),
        ("host", q.host.as_deref()),
        ("route", q.route.as_deref()),
        ("level", q.level.as_deref()),
        ("cache", q.cache.as_deref()),
        ("env", q.env.as_deref()),
        ("resource", q.resource.as_deref()),
    ] {
        if let Some(v) = v.map(|x| x.trim()).filter(|x| !x.is_empty()) {
            s.push_str(&format!("&{k}={}", urlencode(v)));
        }
    }
    s
}

async fn waf_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(
        json!({ "managed": c.waf.managed_enabled(), "rules": c.waf.rules() }),
    ))
}

async fn waf_add_rule(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(rule): Json<WafRule>,
) -> Result<Json<Value>, (StatusCode, String)> {
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

async fn cdn_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let (hits, misses, stale, entries, ratio) = c.cdn.stats();
    Ok(Json(
        json!({ "hits": hits, "misses": misses, "stale": stale, "entries": entries, "hit_ratio": ratio }),
    ))
}

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

async fn rc_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let (entries, reads, writes, hits, revalidations) = c.runtime_cache.stats();
    Ok(Json(json!({
        "entries": entries, "reads": reads, "writes": writes,
        "hits": hits, "revalidations": revalidations,
    })))
}

/// The scope is `{project}:{env}` and the caller must own the project. A request with no credentials
/// is served only without auth enforcement (dev); the public API ingress runs the same router as the
/// loopback listener and the auth middleware never rejects a read, so under enforcement it is 401.
fn rc_authorize(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    scope: &str,
) -> Result<(), (StatusCode, String)> {
    let has_credentials = claims.is_some()
        || headers.contains_key("x-hive-team")
        || headers.contains_key(axum::http::header::AUTHORIZATION);
    if !has_credentials && !crate::auth::enforced() {
        return Ok(());
    }
    require_auth_read(claims)?;
    let project = scope.split(':').next().unwrap_or("");
    let t = tenant(c, headers, claims);
    if project_owned_by(c, project, &t) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "runtime-cache scope belongs to a different team".into(),
        ))
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
        Some(v) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            v,
        )
            .into_response(),
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
    let tags: Vec<String> = q
        .tags
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    match c
        .runtime_cache
        .set(&q.scope, &q.key, body.to_vec(), q.ttl, tags)
    {
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

async fn concurrency_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.limiter.stats())))
}

async fn routing_get(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(
        json!({ "redirects": c.router.redirects(), "rewrites": c.router.rewrites() }),
    ))
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
    let kept: Vec<Redirect> = c
        .router
        .redirects()
        .into_iter()
        .filter(|r| r.source != b.source)
        .collect();
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
    let kept: Vec<Rewrite> = c
        .router
        .rewrites()
        .into_iter()
        .filter(|r| r.source != b.source)
        .collect();
    c.router.set_rewrites(kept);
    crate::persist::persist(&c);
    Ok(Json(json!({ "rewrites": c.router.rewrites() })))
}

/// Tenant-scoped cron list: this team's jobs (legacy jobs with no recorded tenant are attributed to
/// their target project's owning team), deduped by id, plus every healthy peer's local list
/// (`?local=true` against recursion). Cron is NODE-LOCAL (a `vercel.json` cron is registered on the
/// node that BUILT the deployment, a manual job on the control-plane leader, and neither
/// replicates), so one node alone shows only its slice; this is a READ merge, never store
/// replication plus leader-only execution, so each node keeps firing exactly its own jobs and
/// nothing double-fires or drops a follower-hosted deployment's cron.
pub(crate) async fn cron_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<LocalQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut jobs: Vec<CronJob> = c
        .cron
        .list()
        .into_iter()
        .filter(|j| {
            let owner = if j.tenant.is_empty() {
                norm(&c.projects.team_of(&j.deployment)).to_string()
            } else {
                j.tenant.clone()
            };
            owner == t
        })
        .filter(|j| seen.insert(j.id.clone()))
        .collect();
    if !q.local.unwrap_or(false) {
        for v in fan_out_peers_polled(&c, &all_healthy_peers(&c), &t, "/v1/cron?local=true").await {
            if let Ok(peer_jobs) = serde_json::from_value::<Vec<CronJob>>(v) {
                for j in peer_jobs {
                    if seen.insert(j.id.clone()) {
                        jobs.push(j);
                    }
                }
            }
        }
    }
    Json(json!(jobs))
}

async fn cron_add(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut job): Json<CronJob>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &job.deployment)?;
    job.tenant = t;
    match c.cron.add(job) {
        Ok(j) => {
            crate::persist::persist(&c);
            Ok(Json(json!(j)))
        }
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
            let owner = if j.tenant.is_empty() {
                norm(&c.projects.team_of(&j.deployment)).to_string()
            } else {
                j.tenant.clone()
            };
            owner == t
        }
    });
    if !owned {
        return Err((
            StatusCode::FORBIDDEN,
            "cron job belongs to a different team".into(),
        ));
    }
    c.cron.remove(&id);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
}

/// A cron job's owning team — legacy jobs (no tenant recorded) are attributed
/// by their target project's owning team, matching `cron_list`/`cron_del`.
fn cron_job_owner(c: &Arc<CloudState>, job: &CronJob) -> String {
    if job.tenant.is_empty() {
        norm(&c.projects.team_of(&job.deployment)).to_string()
    } else {
        job.tenant.clone()
    }
}

/// Manually triggers a cron job now (the dashboard's "Run" button) through the SAME invocation path
/// as the scheduler's tick loop (`spawn_cron_loop`/`invoke`), recorded as a manual run and leaving
/// the job's own `next_run_ms` untouched.
async fn cron_run(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let job = c
        .cron
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "no such cron job".into()))?;
    if cron_job_owner(&c, &job) != t {
        return Err((
            StatusCode::FORBIDDEN,
            "cron job belongs to a different team".into(),
        ));
    }
    let (status, detail) = match crate::invoke(&c, &job.deployment, &job.path).await {
        Ok((s, _)) => (s, format!("cron {} -> {s} (manual run)", job.name)),
        Err(e) => (0, format!("cron {} error: {e} (manual run)", job.name)),
    };
    let ev = c.event(
        &c.region,
        "CRON",
        &job.deployment,
        &job.path,
        status,
        "cron",
        &detail,
    );
    c.record(ev);
    let updated = c.cron.record_manual_run(&id, now_ms());
    crate::persist::persist(&c);
    Ok(Json(json!({ "status": status, "job": updated })))
}

async fn project_cron_enabled_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<CronToggle>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project_claiming_row(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_cron_enabled(&project, b.enabled);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

#[derive(Deserialize)]
struct CronToggle {
    enabled: bool,
}

/// Lenient `Option<bool>` query-param deserializer: axum's default accepts only `"true"`/`"false"`
/// and 400s on anything else, but callers (the dashboard, `?summary=1`) send `"1"`/`"0"`, the
/// convention `gossip.rs`'s `wf_query`/`logs_query` already special-case on the mesh-RPC path.
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

#[derive(Clone, Deserialize)]
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
    /// Scope a run-scoped read (e.g. `/v1/workflows/hooks`) to a single run.
    #[serde(default, rename = "runId")]
    pub(crate) run_id: Option<String>,
    /// Internal: skip the per-project WORLD reads (`worlds=0`), set by the fleet-aggregation
    /// fan-out only. A world is read over the project's own REST URL, which answers from ANY node,
    /// so the aggregating node reads every tenant-owned project's world itself and each peer hop
    /// would otherwise re-read the same worlds just to have its rows discarded as duplicates.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) worlds: Option<bool>,
}

/// Does this workflow's project belong to the requesting team? Fleet-aware —
/// the node-local project row is UNTAGGED on a node that never ran the deploy.
fn wf_in_team(c: &Arc<CloudState>, project: &str, team: &str) -> bool {
    project_owned_by(c, project, norm(team))
}

/// Body for a run operation (the console's 3-dots menu). All optional.
#[derive(serde::Deserialize, Default)]
pub(crate) struct RunOpBody {
    /// Which project's world holds the run (lets the op skip the auto-scan).
    #[serde(default)]
    pub(crate) project: Option<String>,
    /// Cancel reason (cancel op only).
    #[serde(default, rename = "cancelReason")]
    pub(crate) cancel_reason: Option<String>,
    /// Internal: set on the host-forwarded hop to prevent a re-forward loop.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
}

async fn wf_run_cancel(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    body: Option<Json<RunOpBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    wf_run_op_dispatch(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &id,
        "cancel",
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}
async fn wf_run_replay(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    body: Option<Json<RunOpBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    wf_run_op_dispatch(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &id,
        "replay",
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}
async fn wf_run_reenqueue(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    body: Option<Json<RunOpBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    wf_run_op_dispatch(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &id,
        "reenqueue",
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}
async fn wf_run_wakeup(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    body: Option<Json<RunOpBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    wf_run_op_dispatch(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &id,
        "wakeup",
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}

/// Shared dispatch for the four run operations: resolves the project (given or auto-scanned),
/// checks tenant ownership, then runs the op on the project's HOST node (env decrypts locally): in
/// place when this node hosts it, else forwarded over the iroh mesh with `local=1` so the host runs
/// it without re-forwarding.
pub(crate) async fn wf_run_op_dispatch(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    id: &str,
    op: &str,
    body: RunOpBody,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = tenant(c, headers, claims);
    let is_forwarded = body.local.unwrap_or(false);
    let cancel_reason = body.cancel_reason.clone();

    let project = if let Some(p) = body.project.clone() {
        p
    } else {
        let locals: Vec<String> = {
            let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
            for d in c.gw.list() {
                if record_tenant(&d.tenant) == team {
                    s.insert(d.project);
                }
            }
            s.into_iter()
                .filter(|p| crate::world::has_world(c, p))
                .collect()
        };
        let mut found = String::new();
        for p in &locals {
            if crate::world::run_detail(c, p, id)
                .await
                .map(|d| d.get("run").map(|r| !r.is_null()).unwrap_or(false))
                .unwrap_or(false)
            {
                found = p.clone();
                break;
            }
        }
        if found.is_empty() && !is_forwarded {
            let peers = peer_nodes_for_tenant(c, &team);
            let body_json = json!({ "cancelReason": cancel_reason, "local": true });
            for node in peers {
                if let Some(v) = post_body_to_host(
                    c,
                    &node,
                    &format!("/v1/workflows/runs/{id}/{op}"),
                    &team,
                    &body_json,
                )
                .await
                {
                    if v.get("error").is_none() {
                        return Ok(Json(v));
                    }
                }
            }
            return Err((
                StatusCode::NOT_FOUND,
                "run not found on any reachable host".into(),
            ));
        }
        found
    };

    if project.is_empty() {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    if !wf_in_team(c, &project, &team) {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    if c.gw.git_for_project(&project).is_none() && !is_forwarded {
        if let Some(node) = host_node_for_project(c, &project) {
            let body_json =
                json!({ "project": project, "cancelReason": cancel_reason, "local": true });
            if let Some(v) = post_body_to_host(
                c,
                &node,
                &format!("/v1/workflows/runs/{id}/{op}"),
                &team,
                &body_json,
            )
            .await
            {
                return if v.get("error").is_some() {
                    Err((
                        StatusCode::BAD_GATEWAY,
                        v.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("host op failed")
                            .to_string(),
                    ))
                } else {
                    Ok(Json(v))
                };
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("host node '{node}' for project '{project}' unreachable"),
            ));
        }
    }
    match crate::world::run_op(c, &project, id, op, cancel_reason.as_deref()).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// POSTs a JSON body to a host node by NAME: the HTTP admin when its URL is known, else the iroh
/// mesh (`GOSSIP_POST`); the body-carrying counterpart of `post_to_host`/`put_to_host`, used to
/// forward run operations to a project's host. A configured `HIVE_INTERNAL_TOKEN` rides along as
/// `x-hive-internal`, so handlers gated by `require_operator_or_internal` (run events and
/// attributes) still pass on this forwarded hop, which never carries the caller's Authorization
/// header (an inert extra header for every other route).
pub(crate) async fn post_body_to_host(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
    body: &Value,
) -> Option<Value> {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        let mut req = c
            .http
            .post(format!("{admin}{path}"))
            .header("x-hive-team", team);
        if let Ok(t) = std::env::var("HIVE_INTERNAL_TOKEN") {
            if !t.trim().is_empty() {
                req = req.header("x-hive-internal", t);
            }
        }
        if let Ok(r) = req
            .timeout(std::time::Duration::from_secs(20))
            .json(body)
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
    if let Some((peer_id, addr)) = target {
        let sep = if path.contains('?') { '&' } else { '?' };
        let p = format!("{path}{sep}{}", mesh_team_qs(team));
        let body_bytes = serde_json::to_vec(body).unwrap_or_default();
        if let Some(b) = crate::gossip::request_to(
            c,
            &peer_id,
            &addr,
            hive_p2p::GOSSIP_POST,
            &p,
            &body_bytes,
            25,
        )
        .await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// Body for the operator-only generic run-event append, matching the `@workflow/world` spec's
/// `events.create(runId, data, params)`: `eventType` (required, which makes the endpoint GENERIC),
/// `eventData` (the spec's `data`) and `correlationId` (`params.correlationId`).
#[derive(serde::Deserialize, Default)]
pub(crate) struct RunEventBody {
    /// Which project's world holds the run (lets the op skip the auto-scan).
    #[serde(default)]
    pub(crate) project: Option<String>,
    #[serde(default, rename = "eventType")]
    pub(crate) event_type: Option<String>,
    /// Arbitrary event payload; defaults to `{}`.
    #[serde(default, rename = "eventData")]
    pub(crate) event_data: Option<Value>,
    #[serde(default, rename = "correlationId")]
    pub(crate) correlation_id: Option<String>,
    /// Internal: set on the host-forwarded hop to prevent a re-forward loop.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
}

/// Body for the operator-only run-attributes merge, matching the spec's
/// `experimentalSetAttributes(runId, changes, options)`: `attributes` is the spec's `changes`,
/// merged (top-level keys, overwriting) into the run's stored `attributes`.
#[derive(serde::Deserialize, Default)]
pub(crate) struct RunAttributesBody {
    #[serde(default)]
    pub(crate) project: Option<String>,
    /// Required — must be a JSON object.
    #[serde(default)]
    pub(crate) attributes: Option<Value>,
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub(crate) local: Option<bool>,
}

async fn wf_run_add_event(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    body: Option<Json<RunEventBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    wf_run_event_dispatch(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &id,
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}

async fn wf_run_set_attributes(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    body: Option<Json<RunAttributesBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    wf_run_attributes_dispatch(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &id,
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}

/// Shared dispatch for the operator-only "append arbitrary event to a run" op: the same project
/// resolution and host forwarding as [`wf_run_op_dispatch`] (the data is the node-local world that
/// cancel/replay/wakeup mutate), but gated by [`require_operator_or_internal`] rather than tenant
/// ownership, because it is a manual incident-recovery primitive for platform operators, not tenant
/// self-service.
pub(crate) async fn wf_run_event_dispatch(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    id: &str,
    body: RunEventBody,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator_or_internal(headers, claims)?;
    let event_type = body
        .event_type
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "eventType is required".to_string()))?;
    let event_data = body.event_data.clone().unwrap_or_else(|| json!({}));
    let correlation_id = body.correlation_id.clone();
    let team = tenant(c, headers, claims);
    let is_forwarded = body.local.unwrap_or(false);

    let project = if let Some(p) = body.project.clone() {
        p
    } else {
        let locals: Vec<String> = {
            let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
            for d in c.gw.list() {
                if record_tenant(&d.tenant) == team {
                    s.insert(d.project);
                }
            }
            s.into_iter()
                .filter(|p| crate::world::has_world(c, p))
                .collect()
        };
        let mut found = String::new();
        for p in &locals {
            if crate::world::run_detail(c, p, id)
                .await
                .map(|d| d.get("run").map(|r| !r.is_null()).unwrap_or(false))
                .unwrap_or(false)
            {
                found = p.clone();
                break;
            }
        }
        if found.is_empty() && !is_forwarded {
            let peers = peer_nodes_for_tenant(c, &team);
            let body_json = json!({ "eventType": event_type, "eventData": event_data, "correlationId": correlation_id, "local": true });
            for node in peers {
                if let Some(v) = post_body_to_host(
                    c,
                    &node,
                    &format!("/v1/workflows/runs/{id}/events"),
                    &team,
                    &body_json,
                )
                .await
                {
                    if v.get("error").is_none() {
                        return Ok(Json(v));
                    }
                }
            }
            return Err((
                StatusCode::NOT_FOUND,
                "run not found on any reachable host".into(),
            ));
        }
        found
    };

    if project.is_empty() {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    if !wf_in_team(c, &project, &team) {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    if c.gw.git_for_project(&project).is_none() && !is_forwarded {
        if let Some(node) = host_node_for_project(c, &project) {
            let body_json = json!({ "project": project, "eventType": event_type, "eventData": event_data, "correlationId": correlation_id, "local": true });
            if let Some(v) = post_body_to_host(
                c,
                &node,
                &format!("/v1/workflows/runs/{id}/events"),
                &team,
                &body_json,
            )
            .await
            {
                return if v.get("error").is_some() {
                    Err((
                        StatusCode::BAD_GATEWAY,
                        v.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("host op failed")
                            .to_string(),
                    ))
                } else {
                    Ok(Json(v))
                };
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("host node '{node}' for project '{project}' unreachable"),
            ));
        }
    }
    match crate::world::append_run_event(
        c,
        &project,
        id,
        &event_type,
        event_data,
        correlation_id.as_deref(),
    )
    .await
    {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// Shared dispatch for the operator-only "merge run attributes" op, shaped like
/// [`wf_run_event_dispatch`] and routed to [`crate::world::merge_run_attributes`].
pub(crate) async fn wf_run_attributes_dispatch(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    id: &str,
    body: RunAttributesBody,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator_or_internal(headers, claims)?;
    let attributes = body.attributes.clone().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "attributes is required".to_string(),
        )
    })?;
    if !attributes.is_object() {
        return Err((
            StatusCode::BAD_REQUEST,
            "attributes must be a JSON object".to_string(),
        ));
    }
    let team = tenant(c, headers, claims);
    let is_forwarded = body.local.unwrap_or(false);

    let project = if let Some(p) = body.project.clone() {
        p
    } else {
        let locals: Vec<String> = {
            let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
            for d in c.gw.list() {
                if record_tenant(&d.tenant) == team {
                    s.insert(d.project);
                }
            }
            s.into_iter()
                .filter(|p| crate::world::has_world(c, p))
                .collect()
        };
        let mut found = String::new();
        for p in &locals {
            if crate::world::run_detail(c, p, id)
                .await
                .map(|d| d.get("run").map(|r| !r.is_null()).unwrap_or(false))
                .unwrap_or(false)
            {
                found = p.clone();
                break;
            }
        }
        if found.is_empty() && !is_forwarded {
            let peers = peer_nodes_for_tenant(c, &team);
            let body_json = json!({ "attributes": attributes, "local": true });
            for node in peers {
                if let Some(v) = post_body_to_host(
                    c,
                    &node,
                    &format!("/v1/workflows/runs/{id}/attributes"),
                    &team,
                    &body_json,
                )
                .await
                {
                    if v.get("error").is_none() {
                        return Ok(Json(v));
                    }
                }
            }
            return Err((
                StatusCode::NOT_FOUND,
                "run not found on any reachable host".into(),
            ));
        }
        found
    };

    if project.is_empty() {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    if !wf_in_team(c, &project, &team) {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    if c.gw.git_for_project(&project).is_none() && !is_forwarded {
        if let Some(node) = host_node_for_project(c, &project) {
            let body_json = json!({ "project": project, "attributes": attributes, "local": true });
            if let Some(v) = post_body_to_host(
                c,
                &node,
                &format!("/v1/workflows/runs/{id}/attributes"),
                &team,
                &body_json,
            )
            .await
            {
                return if v.get("error").is_some() {
                    Err((
                        StatusCode::BAD_GATEWAY,
                        v.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("host op failed")
                            .to_string(),
                    ))
                } else {
                    Ok(Json(v))
                };
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("host node '{node}' for project '{project}' unreachable"),
            ));
        }
    }
    match crate::world::merge_run_attributes(c, &project, id, &attributes).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// Workflow definitions for the caller's team. Workflows are ingested on the node that HOSTS a
/// deployment: a project placed on a peer is proxied to its host node, and the global view (no
/// project filter) merges in the workflows registered on every peer hosting one of this tenant's
/// projects.
pub(crate) async fn wf_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() {
            if let Some(node) = host_node_for_project(&c, project) {
                if let Some(v) = fetch_from_host(
                    &c,
                    &node,
                    &format!("/v1/workflows?project={project}&local=true"),
                    &team,
                )
                .await
                {
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
    if q.project.is_none() && !q.local.unwrap_or(false) {
        let mut seen: std::collections::HashSet<String> = defs
            .iter()
            .map(|d| {
                format!(
                    "{}\u{0}{}",
                    d.get("project").and_then(|x| x.as_str()).unwrap_or(""),
                    d.get("id").and_then(|x| x.as_str()).unwrap_or("")
                )
            })
            .collect();
        for v in fan_out_peers_polled(
            &c,
            &peer_nodes_for_tenant(&c, &team),
            &team,
            "/v1/workflows?local=true",
        )
        .await
        {
            if let Some(arr) = v.as_array() {
                for d in arr {
                    let key = format!(
                        "{}\u{0}{}",
                        d.get("project").and_then(|x| x.as_str()).unwrap_or(""),
                        d.get("id").and_then(|x| x.as_str()).unwrap_or("")
                    );
                    if seen.insert(key) {
                        defs.push(d.clone());
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
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &def.project)?;
    c.workflows.define(def);
    let defs: Vec<WorkflowDef> = c
        .workflows
        .defs()
        .into_iter()
        .filter(|d| wf_in_team(&c, &d.project, &t))
        .collect();
    Ok(Json(json!(defs)))
}

const WF_LAST_GOOD_TTL_MS: u64 = 60_000;
type WfLastGoodMap = std::collections::HashMap<String, (u64, Value)>;
fn wf_last_good() -> &'static parking_lot::Mutex<WfLastGoodMap> {
    static LG: std::sync::OnceLock<parking_lot::Mutex<WfLastGoodMap>> = std::sync::OnceLock::new();
    LG.get_or_init(|| parking_lot::Mutex::new(WfLastGoodMap::new()))
}

/// Workflow runs for the caller's team, the most expensive read on the dashboard (fleet fan-out
/// plus per-project world reads), cached for 2 s per team/project/summary. The inner `local=true`
/// hop runs inline, uncached and uncoalesced (its caller carries the budget); a top-level call
/// aggregates on a DETACHED task coalesced per cache key: the dashboard's world proxy aborts a
/// request at 20 s and axum then drops the handler future, so caches written only at handler
/// completion starved `resp_cache` AND the last-known-good belt forever (every poll re-ran the slow
/// path and aborted identically). Detaching means the computation always finishes and fills the
/// caches, and coalescing means five aborted-and-retried polls spawn ONE aggregation. The in-flight
/// map entry MUST clear even if the collector panics, or a dead sender sends every later poll down
/// the uncached inline path forever.
pub(crate) async fn wf_runs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let is_top_level = !q.local.unwrap_or(false);
    let cache_key = format!(
        "wf_runs:{team}:{}:{}",
        q.project.as_deref().unwrap_or(""),
        q.summary.unwrap_or(false)
    );
    if is_top_level {
        if let Some(v) = c.resp_cache.get(&cache_key, Duration::from_secs(2)) {
            return Json(v);
        }
    }
    if !is_top_level {
        return Json(wf_runs_collect(c, team, q).await);
    }
    type WfShared = tokio::sync::watch::Receiver<Option<Value>>;
    fn wf_inflight() -> &'static parking_lot::Mutex<std::collections::HashMap<String, WfShared>> {
        static M: std::sync::OnceLock<
            parking_lot::Mutex<std::collections::HashMap<String, WfShared>>,
        > = std::sync::OnceLock::new();
        M.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
    }
    let mut rx = {
        let mut m = wf_inflight().lock();
        match m.get(&cache_key) {
            Some(rx) => rx.clone(),
            None => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                m.insert(cache_key.clone(), rx.clone());
                let (c2, team2, q2, key2) = (c.clone(), team.clone(), q.clone(), cache_key.clone());
                tokio::spawn(async move {
                    struct ClearOnDrop(String);
                    impl Drop for ClearOnDrop {
                        fn drop(&mut self) {
                            wf_inflight().lock().remove(&self.0);
                        }
                    }
                    let _clear = ClearOnDrop(key2.clone());
                    let v = wf_runs_collect(c2.clone(), team2, q2).await;
                    c2.resp_cache.set(key2, v.clone());
                    let _ = tx.send(Some(v));
                });
                rx
            }
        }
    };
    // Stale-while-revalidate: with a computation in flight, a recent last-known-good answer beats
    // blocking this poll; only the FIRST poll (nothing known yet) waits it out.
    {
        let g = wf_last_good().lock();
        if let Some((at, v)) = g.get(&cache_key) {
            if hive_core::now_ms().saturating_sub(*at) < WF_LAST_GOOD_TTL_MS {
                return Json(v.clone());
            }
        }
    }
    loop {
        if let Some(v) = rx.borrow().clone() {
            return Json(v);
        }
        if rx.changed().await.is_err() {
            return Json(wf_runs_collect(c, team, q).await);
        }
    }
}

/// Per-project world-read circuit breaker: a project whose world store does not answer (a dead or
/// expired Upstash endpoint, tenant configuration no node can repair) used to cost every poll its
/// full read budget forever (six of nine projects on the busiest tenant were in that state, about
/// 2.5 s per poll for rows that never arrive). After a timeout the project's world is skipped for
/// this long and retried when it lapses, so recovery is automatic and the steady state stays fast.
const WORLD_COOLDOWN: Duration = Duration::from_secs(60);

fn world_cooldowns(
) -> &'static parking_lot::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    M.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// The actual aggregation `wf_runs` serves: the local workflow store, per-project Upstash world
/// reads and the peer fan-out, each slow phase under its OWN budget so the worst case stays inside
/// the dashboard's 20 s client abort (the dial-resilience budgets, 10 s HTTP and 20 s iroh per peer
/// and 20 s per world read, are right for one-shot control operations and wrong for a polled read:
/// a phase that misses one poll's budget simply contributes to the next). World reads run
/// concurrently and are budgeted PER PROJECT, peer fetches PER PEER, never as one timeout over the
/// whole join: one dead world or peer used to burn the phase budget and discard every healthy
/// project's or peer's rows, so the runs table came back empty or incomplete (and the incomplete
/// result overwrote last-known-good). A tenant-owned project with a world config is read directly
/// on EVERY node (the env is gossiped and the world answers over its own REST URL), never only
/// through the mesh forward, which returned a false `[]` on any fan-out hiccup (the "rows load then
/// flicker and disappear" report); a best-effort deduplicated host merge still adds the host's
/// engine-local rows. `?summary=1` strips each step's `output` (the table renders
/// name/status/duration; the full detail is `/v1/workflows/runs/:id`). The LAST-KNOWN-GOOD belt is
/// written here and consulted for empties, so completion, not the HTTP caller's patience, populates
/// it: a poll that comes up empty while a recent one had rows serves those rows (a transient outage
/// degrades to seconds-stale, never false-empty, and genuine emptiness wins once
/// `WF_LAST_GOOD_TTL_MS` expires).
async fn wf_runs_collect(c: Arc<CloudState>, team: String, q: WfQuery) -> Value {
    let is_top_level = !q.local.unwrap_or(false);
    let cache_key = format!(
        "wf_runs:{team}:{}:{}",
        q.project.as_deref().unwrap_or(""),
        q.summary.unwrap_or(false)
    );
    /// One phase's budget: generous for a healthy fleet, and small enough that
    /// world-read + fan-out in sequence still complete under the client abort.
    const PHASE_BUDGET: Duration = Duration::from_secs(8);
    /// One project's world read budget: the world store is an EXTERNAL REST endpoint, so a hung one
    /// must not eat the caller's whole deadline or take the healthy projects' rows down with it.
    const WORLD_READ_BUDGET: Duration = Duration::from_millis(2_500);
    let scoped_world_local = q
        .project
        .as_deref()
        .map(|p| crate::world::has_world(&c, p) && project_owned_by(&c, p, &team))
        .unwrap_or(false);
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() && !scoped_world_local {
            if let Some(node) = host_node_for_project(&c, project) {
                if let Ok(Some(v)) = tokio::time::timeout(
                    PHASE_BUDGET,
                    fetch_from_host(
                        &c,
                        &node,
                        &format!("/v1/workflows/runs?project={project}&local=true"),
                        &team,
                    ),
                )
                .await
                {
                    return v;
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
    {
        let mut locals: Vec<String> = match q.project.as_deref() {
            Some(p) if c.gw.git_for_project(p).is_some() || scoped_world_local => {
                vec![p.to_string()]
            }
            Some(_) => vec![],
            None => {
                let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
                for d in c.gw.list() {
                    if record_tenant(&d.tenant) == team {
                        s.insert(d.project);
                    }
                }
                for (name, _) in c.projects.snapshot() {
                    if !s.contains(&name)
                        && crate::world::has_world(&c, &name)
                        && project_owned_by(&c, &name, &team)
                    {
                        s.insert(name);
                    }
                }
                s.into_iter().collect()
            }
        };
        if q.worlds == Some(false) {
            locals.clear();
        }
        locals.retain(|p| crate::world::has_world(&c, p));
        {
            let mut cd = world_cooldowns().lock();
            let now = std::time::Instant::now();
            let mut cooled: usize = 0;
            locals.retain(|p| match cd.get(p) {
                Some(until) if *until > now => {
                    cooled += 1;
                    false
                }
                Some(_) => {
                    cd.remove(p);
                    true
                }
                None => true,
            });
            if cooled > 0 {
                tracing::debug!(
                    team,
                    cooled,
                    "wf_runs: skipping world reads for projects still in cooldown"
                );
            }
        }
        let world_start = std::time::Instant::now();
        let world_reads: Vec<Value> = futures::future::join_all(locals.iter().map(|p| {
            let c = c.clone();
            async move {
                let started = std::time::Instant::now();
                match tokio::time::timeout(WORLD_READ_BUDGET, crate::world::list_runs(&c, p, 100))
                    .await
                {
                    Ok(rows) => rows.unwrap_or_default(),
                    Err(_) => {
                        tracing::warn!(
                            project = %p,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "wf_runs: world read exceeded its budget — serving this poll without that project's runs"
                        );
                        world_cooldowns()
                            .lock()
                            .insert(p.to_string(), std::time::Instant::now() + WORLD_COOLDOWN);
                        Vec::new()
                    }
                }
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect();
        runs.extend(world_reads);
        if world_start.elapsed() > Duration::from_secs(2) {
            tracing::warn!(
                team,
                projects = locals.len(),
                elapsed_ms = world_start.elapsed().as_millis() as u64,
                "wf_runs: world reads were slow this poll"
            );
        }
    }
    let run_key = |r: &Value| -> Option<String> {
        r.get("runId")
            .or_else(|| r.get("id"))
            .and_then(|x| x.as_str())
            .map(String::from)
    };
    if q.project.is_none() && !q.local.unwrap_or(false) {
        let mut seen: std::collections::HashSet<String> = runs.iter().filter_map(run_key).collect();
        let peers = peer_nodes_for_tenant(&c, &team);
        let peer_results: Vec<Value> = futures::future::join_all(peers.iter().map(|node| {
            let c = c.clone();
            let team = team.clone();
            async move {
                tokio::time::timeout(
                    PHASE_BUDGET,
                    fetch_from_host(&c, node, "/v1/workflows/runs?local=true&worlds=0", &team),
                )
                .await
                .ok()
                .flatten()
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect();
        for v in peer_results {
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
    } else if let Some(project) = q.project.as_deref() {
        if scoped_world_local
            && !q.local.unwrap_or(false)
            && c.gw.git_for_project(project).is_none()
        {
            if let Some(node) = host_node_for_project(&c, project) {
                let mut seen: std::collections::HashSet<String> =
                    runs.iter().filter_map(run_key).collect();
                if let Ok(Some(v)) = tokio::time::timeout(
                    PHASE_BUDGET,
                    fetch_from_host(
                        &c,
                        &node,
                        &format!("/v1/workflows/runs?project={project}&local=true"),
                        &team,
                    ),
                )
                .await
                {
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
    }
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
    if is_top_level {
        if runs.is_empty() {
            let g = wf_last_good().lock();
            if let Some((at, v)) = g.get(&cache_key) {
                if hive_core::now_ms().saturating_sub(*at) < WF_LAST_GOOD_TTL_MS {
                    return v.clone();
                }
            }
        } else {
            wf_last_good()
                .lock()
                .insert(cache_key.clone(), (hive_core::now_ms(), json!(runs)));
        }
    }
    json!(runs)
}

/// One run with full step detail (for the trace timeline / Gantt): resolved from our engine first,
/// else from the project's WDK "world" store, proxied to the hosting node when the project lives on
/// a peer (the world env is decrypted there); `?project=` is required to locate a world run.
pub(crate) async fn wf_run_detail(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    Query(q): Query<WfQuery>,
) -> Result<Json<Value>, StatusCode> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(r) = c.workflows.run(&id) {
        if wf_in_team(&c, &r.project, &team) {
            return Ok(Json(json!(r)));
        }
        return Err(StatusCode::NOT_FOUND);
    }
    let found = |v: &Value| v.get("run").map(|r| !r.is_null()).unwrap_or(false);
    if let Some(project) = q.project.as_deref() {
        if !q.local.unwrap_or(false) {
            let hosts = host_nodes_for_project(&c, project);
            let path = format!("/v1/workflows/runs/{id}?project={project}&local=true");
            if let Some(v) = fan_out_peers_polled(&c, &hosts, &team, &path)
                .await
                .into_iter()
                .find(|v| found(v))
            {
                return Ok(Json(v));
            }
        }
        if let Some(detail) = crate::world::run_detail(&c, project, &id).await {
            if found(&detail) {
                return Ok(Json(detail));
            }
        }
    }
    let locals: Vec<String> = {
        let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
        for d in c.gw.list() {
            if record_tenant(&d.tenant) == team {
                s.insert(d.project);
            }
        }
        s.into_iter().collect()
    };
    let world_locals: Vec<String> = locals
        .into_iter()
        .filter(|p| crate::world::has_world(&c, p))
        .collect();
    let details = futures::future::join_all(
        world_locals
            .iter()
            .map(|p| crate::world::run_detail(&c, p, &id)),
    )
    .await;
    if let Some(detail) = details.into_iter().flatten().find(|d| found(d)) {
        return Ok(Json(detail));
    }
    if !q.local.unwrap_or(false) {
        let peers = peer_nodes_for_tenant(&c, &team);
        let path = format!("/v1/workflows/runs/{id}?local=true");
        if let Some(v) = fan_out_peers_polled(&c, &peers, &team, &path)
            .await
            .into_iter()
            .find(|v| found(v))
        {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// Per-project rollup for the global "All Projects" workflows view.
pub(crate) async fn wf_summary(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    use std::collections::BTreeMap;
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // project -> (created, completed, failed, active)
    let mut agg: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for r in c.workflows.runs() {
        if !wf_in_team(&c, &r.project, &team) {
            continue;
        }
        let proj = if r.project.is_empty() {
            "default".to_string()
        } else {
            r.project.clone()
        };
        let e = agg.entry(proj).or_insert((0, 0, 0, 0));
        e.0 += 1;
        match r.status {
            hive_edge::workflows::RunStatus::Succeeded => e.1 += 1,
            hive_edge::workflows::RunStatus::Failed => e.2 += 1,
            hive_edge::workflows::RunStatus::Running | hive_edge::workflows::RunStatus::Pending => {
                e.3 += 1
            }
            _ => {}
        }
    }
    if !q.local.unwrap_or(false) {
        let peers = peer_nodes_for_tenant(&c, &team);
        for v in fan_out_peers_polled(&c, &peers, &team, "/v1/workflows/summary?local=true").await {
            if let Some(arr) = v.as_array() {
                for r in arr {
                    let proj = r
                        .get("project")
                        .and_then(|x| x.as_str())
                        .unwrap_or("default")
                        .to_string();
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
    let rows: Vec<Value> = agg
        .into_iter()
        .map(|(project, (created, completed, failed, active))| {
            json!({ "project": project, "created": created, "completed": completed, "failed": failed, "active": active })
        })
        .collect();
    Json(json!(rows))
}

/// Lists workflow HOOKS for the observability console's Hooks tab. Hooks live ONLY in the deployed
/// app's WDK "world" store, so this reads them from every project hosted on this node and fans out
/// to the peers hosting the tenant's other projects, with the same tenant scoping, project
/// filtering, short-TTL collapse and `?local=` fleet merge as `wf_runs`; `?runId=` scopes the read
/// to one run.
pub(crate) async fn wf_hooks(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let is_top_level = !q.local.unwrap_or(false);
    let run_id = q.run_id.as_deref();
    let cache_key = format!(
        "wf_hooks:{team}:{}:{}",
        q.project.as_deref().unwrap_or(""),
        run_id.unwrap_or("")
    );
    if is_top_level {
        if let Some(v) = c.resp_cache.get(&cache_key, Duration::from_secs(2)) {
            return Json(v);
        }
    }
    let rid_q = run_id.map(|r| format!("&runId={r}")).unwrap_or_default();
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() {
            if let Some(node) = host_node_for_project(&c, project) {
                let path = format!("/v1/workflows/hooks?project={project}&local=true{rid_q}");
                if let Some(v) = fetch_from_host(&c, &node, &path, &team).await {
                    if is_top_level {
                        c.resp_cache.set(cache_key.clone(), v.clone());
                    }
                    return Json(v);
                }
            }
        }
    }
    let mut hooks: Vec<Value> = Vec::new();
    {
        let locals: Vec<String> = match q.project.as_deref() {
            Some(p) if c.gw.git_for_project(p).is_some() => vec![p.to_string()],
            Some(_) => vec![],
            None => {
                let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
                for d in c.gw.list() {
                    if record_tenant(&d.tenant) == team {
                        s.insert(d.project);
                    }
                }
                s.into_iter().collect()
            }
        };
        let world_locals: Vec<String> = locals
            .into_iter()
            .filter(|p| crate::world::has_world(&c, p))
            .collect();
        for whooks in futures::future::join_all(
            world_locals
                .iter()
                .map(|p| crate::world::list_hooks(&c, p, run_id, 50)),
        )
        .await
        .into_iter()
        .flatten()
        {
            hooks.extend(whooks);
        }
    }
    let hook_key = |h: &Value| -> Option<String> {
        let hid = h.get("hookId").and_then(|x| x.as_str()).unwrap_or("");
        let rid = h.get("runId").and_then(|x| x.as_str()).unwrap_or("");
        if hid.is_empty() && rid.is_empty() {
            None
        } else {
            Some(format!("{rid}\u{0}{hid}"))
        }
    };
    if q.project.is_none() && !q.local.unwrap_or(false) {
        let mut seen: std::collections::HashSet<String> =
            hooks.iter().filter_map(hook_key).collect();
        let peers = peer_nodes_for_tenant(&c, &team);
        let path = format!("/v1/workflows/hooks?local=true{rid_q}");
        for v in fan_out_peers_polled(&c, &peers, &team, &path).await {
            if let Some(arr) = v.as_array() {
                for h in arr {
                    if let Some(k) = hook_key(h) {
                        if seen.insert(k) {
                            hooks.push(h.clone());
                        }
                    }
                }
            }
        }
    }
    hooks.sort_by(|a, b| {
        let am = a.get("created_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        let bm = b.get("created_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        bm.cmp(&am)
    });
    let result = json!(hooks);
    if is_top_level {
        c.resp_cache.set(cache_key, result.clone());
    }
    Json(result)
}

async fn wf_run(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(def) = c.workflows.defs().into_iter().find(|d| d.id == id) {
        if !wf_in_team(&c, &def.project, &t) {
            return Err((
                StatusCode::FORBIDDEN,
                "workflow belongs to a different team".into(),
            ));
        }
    }
    let invoker = crate::wf_invoker(c.clone());
    match c.workflows.start(&id, invoker) {
        Ok(run_id) => Ok(Json(json!({ "run_id": run_id }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

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

/// Longest a single-use `/v1/sandbox` cell may run; the caller's `timeout_secs` is clamped to it.
const SANDBOX_TIMEOUT_MAX_SECS: u64 = 120;
/// Single-use cells one tenant may have running on this node at once.
const SANDBOX_TENANT_MAX_JOBS: usize = 2;

static SANDBOX_INFLIGHT: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(Default::default);

/// One running `/v1/sandbox` cell charged to a tenant; the slot is released on drop, so a client
/// that leaves mid-run (axum drops the future) never leaks it.
struct SandboxSlot(String);

impl SandboxSlot {
    fn acquire(tenant: &str) -> Option<SandboxSlot> {
        let mut inflight = SANDBOX_INFLIGHT.lock();
        let running = inflight.entry(tenant.to_string()).or_insert(0);
        if *running >= SANDBOX_TENANT_MAX_JOBS {
            return None;
        }
        *running += 1;
        Some(SandboxSlot(tenant.to_string()))
    }
}

impl Drop for SandboxSlot {
    fn drop(&mut self) {
        let mut inflight = SANDBOX_INFLIGHT.lock();
        if let Some(running) = inflight.get_mut(&self.0) {
            *running = running.saturating_sub(1);
            if *running == 0 {
                inflight.remove(&self.0);
            }
        }
    }
}

/// The dashboard's single-use terminal cell (`POST /v1/sandbox`). Any signed-in tenant may run
/// commands in a cell of the platform's default image: the image name selects a rootfs and the
/// data drive named like it on the node, so choosing one is a platform-operator action, the
/// run time is capped at [`SANDBOX_TIMEOUT_MAX_SECS`] and a tenant holds at most
/// [`SANDBOX_TENANT_MAX_JOBS`] cells at once.
async fn sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<SandboxReq>,
) -> Result<Json<SandboxResp>, (StatusCode, String)> {
    let claims = claims.as_ref().map(|e| &e.0);
    let t = tenant(&c, &headers, claims);
    if t == ANON_TENANT {
        return Err((StatusCode::UNAUTHORIZED, "sign-in required".into()));
    }
    // A Mock backend runs every command on the HOST's own /bin/sh with no isolation at all
    // (hive_backend::mock); `/v1/sandbox` exists to run TENANT-supplied shell commands, so it must
    // never be reachable where the node cannot actually isolate them.
    if c.gw.backend_name() == "mock" {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "this node's isolation backend cannot run sandbox commands".into(),
        ));
    }
    if req.image != default_image() {
        require_operator(claims).map_err(|_| {
            (
                StatusCode::FORBIDDEN,
                "choosing the sandbox image requires a platform operator".into(),
            )
        })?;
    }
    let slot = SandboxSlot::acquire(&t).ok_or((
        StatusCode::TOO_MANY_REQUESTS,
        format!("at most {SANDBOX_TENANT_MAX_JOBS} sandboxes may run at once"),
    ))?;
    let timeout_secs = req.timeout_secs.clamp(5, SANDBOX_TIMEOUT_MAX_SECS);
    let started = now_ms();
    let job = BuildJob::builder(req.image)
        .commands(req.commands)
        .resources(ResourceSpec {
            vcpus: 1,
            mem_mib: 512,
            disk_mib: 1024,
            timeout_secs,
        })
        .build();
    let id = c.hive.submit(job);
    c.audit
        .record(&t, "user", "run", "sandbox", &id.to_string(), "");

    // The slot is charged to the JOB, not to this HTTP request: a client that disconnects mid-run
    // drops this future (and would drop a request-held guard with it) while the submitted cell keeps
    // running to its own timeout regardless, so a tenant that fires six requests and disconnects
    // each after one second used to free six slots for six still-live cells. Ownership of `slot`
    // moves into a detached watcher that releases it only once the job itself reaches a terminal
    // state (or vanishes), bounded by the job's own timeout plus margin so a stuck state can never
    // hold a slot forever either.
    let watch_cloud = c.clone();
    let watch_id = id.clone();
    tokio::spawn(async move {
        let _slot = slot;
        for _ in 0..(timeout_secs * 20 + 100) {
            match watch_cloud.hive.job_view(&watch_id) {
                Some(v) if v.state.is_terminal() => break,
                Some(_) => {}
                None => break,
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let mut state = JobState::Queued;
    for _ in 0..(timeout_secs * 20) {
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
    Ok(Json(SandboxResp {
        job_id: id.to_string(),
        state: format!("{state:?}"),
        exit_code: view.and_then(|v| v.exit_code),
        logs,
        duration_ms: now_ms().saturating_sub(started),
    }))
}

#[derive(Deserialize)]
struct CreateTeam {
    name: String,
    #[serde(default = "default_plan")]
    plan: String,
    /// Optional EXACT tenant slug (e.g. a Clerk-org-derived id already used by existing
    /// tenant-scoped data that predates this team row). Absent or empty falls back to slugify(name)
    /// plus a uniqueness suffix; present, the team is created with exactly this slug and never
    /// silently re-derived. Operator-only (see `team_create`).
    #[serde(default)]
    slug: String,
}
fn default_plan() -> String {
    "hobby".into()
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
    c.teams
        .get(&slug)
        .map(|t| Json(json!(t)))
        .ok_or((StatusCode::NOT_FOUND, "no such team".into()))
}

async fn team_create(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<CreateTeam>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !b.slug.trim().is_empty() {
        require_operator(claims.as_ref().map(|e| &e.0))?;
    }
    let plan = b.plan.trim().to_ascii_lowercase();
    if !matches!(plan.as_str(), "hobby" | "pro" | "enterprise") {
        return Err((StatusCode::BAD_REQUEST, "unknown plan".into()));
    }
    if plan != "hobby"
        && !claims
            .as_ref()
            .map(|claims| claims.0.platform_admin)
            .unwrap_or(false)
    {
        return Err((
            StatusCode::PAYMENT_REQUIRED,
            "create the team on Hobby, then complete checkout to upgrade".into(),
        ));
    }
    let creator_email = match claims.as_ref() {
        Some(claims) => c
            .identity
            .users()
            .into_iter()
            .find(|user| user.id == claims.0.sub && !user.email.trim().is_empty())
            .map(|user| user.email)
            .or_else(|| claims.0.platform_admin.then(|| c.owner_email.clone()))
            .ok_or((
                StatusCode::PRECONDITION_FAILED,
                "team creation requires a verified user email".into(),
            ))?,
        None if !crate::auth::enforced() => c.owner_email.clone(),
        None => {
            return Err((StatusCode::UNAUTHORIZED, "authentication required".into()));
        }
    };
    let slug = b.slug.trim();
    let t = if slug.is_empty() {
        c.teams.create(&b.name, &plan, &creator_email)
    } else {
        c.teams
            .create_with_slug(slug, &b.name, &plan, &creator_email)
            .ok_or((
                StatusCode::CONFLICT,
                format!("team slug '{slug}' already exists"),
            ))?
    };
    apply_plan_everywhere(&c, &t.slug, &plan);
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
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &slug,
        &["owner", "admin"],
    )?;
    if matches!(b.role, crate::teams::Role::Owner) {
        let is_owner_caller = claims
            .as_ref()
            .map(|e| e.0.platform_admin || e.0.role == "owner")
            .unwrap_or(false);
        if !is_owner_caller {
            return Err((
                StatusCode::FORBIDDEN,
                "only an Owner may grant the Owner role".into(),
            ));
        }
    }
    let team = c
        .teams
        .get(&slug)
        .ok_or((StatusCode::NOT_FOUND, "team not found".into()))?;
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

/// Removes a team member. The ex-member's push/SMS registrations for THIS team are purged (email
/// resolved to a Clerk user id through the identity mirror), because the rows capture the tenant at
/// registration and the dispatcher would otherwise keep fanning the team's notifications to them.
/// The membership change and purge are persisted BEFORE the first await (a client cancelled during
/// the relay revocation must not restore the removed member on the next restart); then the team's
/// browser grants are revoked fail-closed (membership is keyed by email and grants by subject id,
/// so remaining members renew from a fresh platform session while the removed member loses service
/// now), and the revocation's own persisted state is saved again.
async fn team_remove_member(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((slug, email)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &slug,
        &["owner", "admin"],
    )?;
    let t = c
        .teams
        .remove_member(&slug, &email)
        .ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    if let Some(uid) = c
        .identity
        .users()
        .into_iter()
        .find(|u| u.email.eq_ignore_ascii_case(email.trim()))
        .map(|u| u.id)
    {
        let purged = c.push.purge_user_tenant(&uid, &norm(&slug));
        if purged > 0 {
            tracing::info!(team = %slug, user = %uid, purged, "revoked ex-member push/SMS registrations");
        }
    }
    crate::persist::persist(&c);
    crate::browser_admission::revoke_team(&c, &slug).await;
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

/// Applies a tier change to a tenant EVERYWHERE it is stored: `c.teams` (feature gating via
/// `team_plan()`) and `c.billing` (project/seat quotas, the `can_deploy` credit lock, everything
/// the billing UI shows). Four call sites used to write only the billing half (the free-plan
/// checkout shortcut, Stripe `customer.subscription.deleted`, the operator grant, checkout
/// confirmation), so a tenant could read Enterprise for features while quota-limited as Hobby;
/// every tier change goes through here. `teams.set_plan` returning `None` is normal: personal
/// namespaces (`personal`, `u_<uid>`) have a billing account but no team row. A downgrade drops
/// Enterprise-only SSO, matching `team_set_plan`.
pub(crate) fn apply_plan_everywhere(c: &Arc<CloudState>, tenant: &str, plan: &str) {
    c.teams.set_plan(tenant, plan);
    c.billing.set_plan(tenant, plan);
    if !crate::billing::plan_allows_sso(plan) {
        c.teams.set_sso(tenant, false);
    }
}

/// Locks a platform-admin's tenant at enterprise permanently: `apply_plan_everywhere` plus an
/// unbypassable tier FLOOR (`set_tier_lock`, which `billing.rs`'s `set_plan` honors, so the next
/// downgrade cannot undo it) that no automatic downgrade path (free-plan checkout, Stripe
/// `customer.subscription.deleted`) can drop. Called from `mint_token` on the billing writer only,
/// and idempotent.
pub(crate) fn apply_admin_enterprise(c: &Arc<CloudState>, tenant: &str) {
    apply_plan_everywhere(c, tenant, "enterprise");
    c.billing.set_tier_lock(tenant, Some("enterprise"));
    crate::persist::persist(c);
    tracing::info!(tenant, "platform admin tenant locked to enterprise");
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
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &slug,
        &["owner", "admin"],
    )?;
    let plan = b.plan.to_lowercase();
    if !matches!(plan.as_str(), "hobby" | "pro" | "enterprise") {
        return Err((StatusCode::BAD_REQUEST, "unknown plan".into()));
    }
    c.teams
        .get(&slug)
        .ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    apply_plan_everywhere(&c, &slug, &plan);
    crate::persist::persist(&c);
    Ok(Json(json!(c.teams.get(&slug))))
}

#[derive(Deserialize)]
struct SetSso {
    enabled: bool,
}

/// Deletes a team. Owner-only, and refuses where deleting would strand resources or break the
/// caller's identity: a PERSONAL namespace (`personal`, `u_<uid>`) is a user's own space (removing
/// it would leave that user with no tenant), and a team that still owns projects would orphan
/// records that carry its slug (delete or move the projects first). The billing account is cleared
/// with the team record so no stray tier or quota row remains, the same both-halves-or-neither rule
/// as `apply_plan_everywhere`. Like `team_remove_member`, it persists BEFORE awaiting the
/// browser-grant revocation (a cancelled request must not resurrect the team) and again after.
async fn team_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &slug,
        &["owner"],
    )?;
    let s = norm(&slug).to_string();
    if s == "personal" || s.starts_with("u_") {
        return Err((
            StatusCode::BAD_REQUEST,
            "a personal namespace cannot be deleted".into(),
        ));
    }
    let projects = c.projects.count_for_team(&s);
    if projects > 0 {
        return Err((
            StatusCode::CONFLICT,
            format!("team still owns {projects} project(s) — delete or move them first"),
        ));
    }
    c.teams
        .remove(&s)
        .ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    c.billing.remove_account(&s);
    crate::persist::persist(&c);
    crate::browser_admission::revoke_team(&c, &s).await;
    c.audit
        .record(&s, "user", "team_delete", "team", &s, "team deleted");
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true, "deleted": s })))
}

/// Toggle team/org SSO — Enterprise only.
async fn team_set_sso(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(slug): Path<String>,
    Json(b): Json<SetSso>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &slug,
        &["owner", "admin"],
    )?;
    let team = c
        .teams
        .get(&slug)
        .ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
    if !crate::billing::plan_allows_sso(&team.plan) {
        return Err((
            StatusCode::FORBIDDEN,
            "SSO requires the Enterprise plan".into(),
        ));
    }
    let t = c
        .teams
        .set_sso(&slug, b.enabled)
        .ok_or((StatusCode::NOT_FOUND, "no such team".into()))?;
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

/// Whether `project` belongs to NOBODY: its own tag is `__untagged__` AND no deployment record
/// anywhere names a real tenant. That state is a trap: `require_project` matches the owner tag
/// against the caller's tenant and `__untagged__` matches none, while `project_owned_by`'s
/// optimistic fallback applies only when the project has NO deployments, so an untagged project
/// WITH untagged deployments is claimable by no account and invisible in every list while still
/// holding its name platform-wide (creating a project then fails on a duplicate name its owner
/// cannot find). Deliberately strict: if ANY deployment names a real tenant, that tenant owns the
/// project and this returns false, so adoption can never take a project from a live account.
fn project_unowned(c: &Arc<CloudState>, project: &str) -> bool {
    if record_tenant(&c.projects.team_of(project)) != UNTAGGED_TENANT {
        return false;
    }
    let local_clean = c.gw.deployment_records().iter().all(|r| {
        !r.project.eq_ignore_ascii_case(project) || record_tenant(&r.tenant) == UNTAGGED_TENANT
    });
    let peer_clean = c.peer_deployments.read().values().flatten().all(|d| {
        !d.project.eq_ignore_ascii_case(project) || record_tenant(&d.tenant) == UNTAGGED_TENANT
    });
    local_clean && peer_clean
}

/// Moves a project to another team (or changes its protection): only the CURRENT owning team may,
/// EXCEPT that a platform operator may adopt an UNOWNED project (`project_unowned`, which refuses
/// the moment any deployment names a real tenant); without that exception an orphaned project is
/// frozen forever, since no tenant matches `__untagged__`.
async fn project_team_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<ProjectTeam>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let adopting_orphan =
        project_unowned(&c, &project) && require_operator(claims.as_ref().map(|e| &e.0)).is_ok();
    if !adopting_orphan {
        require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    } else {
        tracing::warn!(
            %project,
            new_team = %b.team,
            "adopting an UNOWNED project into a tenant (operator action)"
        );
    }
    c.projects.set_team(&project, &b.team);
    c.projects
        .set_preview_protection(&project, b.preview_protection);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

#[derive(Deserialize)]
struct CreateApiKey {
    name: String,
    #[serde(default)]
    role: String,
}

/// Lists the caller's API keys. `apikeys` is NODE-LOCAL state while `admin_ingress` forwards
/// mutations to the leader and serves GETs locally, so a key created from the dashboard was stored
/// on the leader and read back from whichever node round-robin DNS picked ("generating an API key
/// doesn't work"): a non-leader reads the leader's authoritative copy, the same shape as
/// `project_settings_get`, falling back to local only when the leader is unreachable (a read local
/// state can answer must never fail).
async fn apikeys_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !c.is_control_plane_leader() {
        if let Some(leader) = c.leader_forward_target() {
            if let Some(v) = fetch_from_host(&c, &leader, "/v1/apikeys", &t).await {
                return Json(v);
            }
        }
    }
    Json(json!(c
        .apikeys
        .list(&t)
        .iter()
        .map(|k| k.public())
        .collect::<Vec<_>>()))
}

/// Mints a team-scoped API key. Its role must be one a tenant may carry
/// (`apikeys::MINTABLE_ROLES`, never the mesh's `service`) and no higher than the caller's own
/// (`apikeys::check_new_role`): a role string is stored data, and fleet-internal gates read it.
async fn apikey_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<CreateApiKey>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let creator = claims
        .as_ref()
        .map(|e| (e.0.role.as_str(), e.0.platform_admin));
    let role =
        crate::apikeys::check_new_role(&b.role, creator).map_err(|refusal| match refusal {
            crate::apikeys::RoleRefusal::NotMintable => (
                StatusCode::BAD_REQUEST,
                format!(
                    "role must be one of {}",
                    crate::apikeys::MINTABLE_ROLES.join(", ")
                ),
            ),
            crate::apikeys::RoleRefusal::AboveCreator => (
                StatusCode::FORBIDDEN,
                "a key cannot carry a higher role than its creator".into(),
            ),
        })?;
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let (key, token) = c.apikeys.create(&b.name, &t, role);
    crate::persist::persist(&c);
    // The plaintext token is returned exactly once.
    let mut v = key.public();
    v["token"] = json!(token);
    Ok(Json(v))
}

async fn apikey_revoke(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Revoking is credential management (it can destroy the team's OWN owner-role key), so it needs
    // a role at least as elevated as minting one conceptually requires — unlike `put_env`'s
    // `member` floor, `viewer` AND `member` are both refused here (confirmed live: a viewer key
    // revoked the owner key with no gate at all before this).
    require_team(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &t,
        &["owner", "admin"],
    )?;
    let ok = c.apikeys.revoke(&id, &t);
    crate::persist::persist(&c);
    Ok(Json(json!({ "revoked": ok, "id": id })))
}

async fn integrations_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!(c
        .integrations
        .list(&t)
        .iter()
        .map(|i| i.public())
        .collect::<Vec<_>>()))
}

async fn integration_upsert(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<crate::integrations::UpsertReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if b.provider.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "provider is required".into()));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let rec = c.integrations.upsert(&t, b);
    crate::persist::persist(&c);
    let ev = c.event(
        &c.region,
        "INTEGRATION",
        &rec.provider,
        "/",
        200,
        "link",
        &format!(
            "integration {} linked for {} ({} env var(s))",
            rec.provider,
            t,
            rec.env.len()
        ),
    );
    c.record(ev);
    Ok(Json(rec.public()))
}

async fn integration_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.integrations
        .get(&t, &id)
        .map(|i| Json(i.public()))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn integration_credentials(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.integrations
        .get(&t, &id)
        .map(|i| Json(i.full()))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn integration_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ok = c.integrations.delete(&t, &id);
    crate::persist::persist(&c);
    Json(json!({ "deleted": ok, "id": id }))
}

/// Scan a hosted deployment's on-disk source tree and store its service graph.
/// Works for existing/failed deployments (the checkout is kept for live ones).
async fn scan_root_graph(
    c: &Arc<CloudState>,
    project: &str,
    dep_id: &str,
    root_str: &str,
) -> Option<crate::svcgraph::ServiceGraph> {
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
    let env_keys: Vec<String> = c
        .projects
        .get_masked(project)
        .env
        .iter()
        .map(|e| e.key.clone())
        .collect();
    let g = crate::svcgraph::build_graph(project, dep_id, &scan, &env_keys);
    c.svcgraph.put(g.clone());
    crate::persist::persist(c);
    Some(g)
}

pub(crate) async fn scan_record_graph(
    c: &Arc<CloudState>,
    rec: &fluid_core::DeployRecord,
) -> Option<crate::svcgraph::ServiceGraph> {
    scan_root_graph(c, &rec.project, &rec.id, &rec.root).await
}

/// The LOCAL service graph for a project: the stored one, else scanned on-demand
/// from the newest locally-hosted deployment record. No fan-out (caller does that).
pub(crate) async fn local_project_graph(
    c: &Arc<CloudState>,
    project: &str,
) -> Option<crate::svcgraph::ServiceGraph> {
    if let Some(g) = c.svcgraph.latest_for_project(project) {
        return Some(g);
    }
    let mut recs: Vec<_> =
        c.gw.deployment_records()
            .into_iter()
            .filter(|r| r.project == project)
            .collect();
    recs.sort_by_key(|r| r.created_at_ms);
    while let Some(rec) = recs.pop() {
        if let Some(g) = scan_record_graph(c, &rec).await {
            return Some(g);
        }
    }
    if let Some(dir) = crate::git::newest_deploy_dir(project) {
        return scan_root_graph(c, project, project, &dir.to_string_lossy()).await;
    }
    None
}

/// The LOCAL service graph for a deployment id: stored, else scanned on-demand from
/// its locally-hosted record. No proxy (caller does that).
pub(crate) async fn local_deployment_graph(
    c: &Arc<CloudState>,
    id: &str,
) -> Option<crate::svcgraph::ServiceGraph> {
    if let Some(g) = c.svcgraph.get(id) {
        return Some(g);
    }
    let rec = c.gw.deployment_records().into_iter().find(|r| r.id == id)?;
    scan_record_graph(c, &rec).await
}

pub(crate) async fn deployment_service_graph(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.gw.deployment_records().iter().any(|r| r.id == id) {
        if let Some(g) = local_deployment_graph(&c, &id).await {
            if project_owned_by(&c, &g.project, norm(&t)) {
                return Ok(Json(json!(g)));
            }
        }
    }
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) = fetch_from_host(
            &c,
            &node,
            &format!("/v1/deployments/{id}/service-graph"),
            &t,
        )
        .await
        {
            return Ok(Json(v));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn project_service_graph(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(g) = local_project_graph(&c, &project).await {
        return Ok(Json(json!(g)));
    }
    let self_name = c.node_name.clone();
    let peers: Vec<String> = c
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.name != self_name && n.iroh_addr.is_some())
        .map(|n| n.name)
        .collect();
    let path = format!("/v1/projects/{project}/service-graph");
    if let Some(v) = fan_out_peers_polled(&c, &peers, &t, &path)
        .await
        .into_iter()
        .next()
    {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

async fn webhook_events() -> Json<Value> {
    Json(json!(crate::webhooks::ALL_EVENTS))
}

async fn webhooks_all(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    // Tenant-scoped: the payload includes signing secrets.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let list: Vec<_> = c
        .webhooks
        .list(crate::webhooks::Scope::Team(&t), None)
        .into_iter()
        .map(|w| w.decrypted())
        .collect();
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

async fn webhook_create_team(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<CreateTeamWebhook>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if t == ANON_TENANT {
        return Err((StatusCode::UNAUTHORIZED, "sign-in required".into()));
    }
    let targets: Vec<String> = if b.projects.is_empty() || b.projects.iter().any(|p| p == "*") {
        vec!["*".into()]
    } else {
        b.projects.clone()
    };
    if let Some(foreign) = targets
        .iter()
        .find(|p| p.as_str() != "*" && !project_owned_by(&c, p, &t))
    {
        return Err((
            StatusCode::FORBIDDEN,
            format!("project {foreign:?} belongs to a different team"),
        ));
    }
    let mut created = Vec::new();
    for p in targets {
        created.push(
            c.webhooks
                .add(crate::webhooks::Webhook {
                    id: String::new(),
                    team: t.clone(),
                    project: p,
                    url: b.url.clone(),
                    events: b.events.clone(),
                    secret: String::new(),
                    enabled: true,
                    created_ms: 0,
                })
                .map_err(|refused| (StatusCode::BAD_REQUEST, refused.to_string()))?,
        );
    }
    crate::persist::persist(&c);
    let created: Vec<_> = created.into_iter().map(|w| w.decrypted()).collect();
    Ok(Json(json!(created)))
}

async fn webhooks_for_project(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!([]));
    }
    let list: Vec<_> = c
        .webhooks
        .list(crate::webhooks::Scope::Team(&t), Some(&project))
        .into_iter()
        .map(|w| w.decrypted())
        .collect();
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
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Err((StatusCode::NOT_FOUND, "no such project".into()));
    }
    let wh = c
        .webhooks
        .add(crate::webhooks::Webhook {
            id: String::new(),
            team: t,
            project,
            url: b.url,
            events: b.events,
            secret: String::new(),
            enabled: true,
            created_ms: 0,
        })
        .map_err(|refused| (StatusCode::BAD_REQUEST, refused.to_string()))?;
    crate::persist::persist(&c);
    Ok(Json(json!(wh.decrypted())))
}

async fn webhook_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.webhooks.remove(crate::webhooks::Scope::Team(&t), &id)
        == crate::webhooks::Removal::NotYours
    {
        return Err(StatusCode::NOT_FOUND);
    }
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
}

/// Webhook delivery log. The delivery ring is in-process and NOT replicated, and dispatches fire
/// both from the leader (promote/delete/database/incident mutations) and from whichever node ran a
/// build, so one node's view is a slice (an operator saw an empty panel and concluded webhooks were
/// broken when they had delivered); the top-level read merges every healthy peer's ring
/// (`local=true` stops the recursion).
async fn webhook_deliveries(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Query(q): axum::extract::Query<LocalQ>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let mut all: Vec<Value> = c
        .webhooks
        .deliveries(100)
        .into_iter()
        .map(|d| json!(d))
        .collect();
    if q.local != Some(true) {
        let peers = all_healthy_peers(&c);
        for v in fan_out_peers_polled(&c, &peers, "", "/v1/webhooks/deliveries?local=true").await {
            if let Some(arr) = v.as_array() {
                all.extend(arr.iter().cloned());
            }
        }
        let ts = |v: &Value| v.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        all.sort_by(|a, b| ts(b).cmp(&ts(a)));
        let mut seen = std::collections::HashSet::new();
        all.retain(|v| {
            seen.insert(
                v.get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        });
        all.truncate(100);
    }
    Ok(Json(json!(all)))
}

/// The leader's answer for a tenant-scoped databases path, or `None` when this node IS the leader
/// (nothing above it to ask, no recursion). Database records are replicated, but not
/// instantaneously or always completely (measured: a SQLite database created through the leader was
/// present on 6 of 11 nodes and absent on the rest long after, the nodes that had been restarting
/// or OOM-thrashing, with nothing to backfill them), and `admin_ingress` serves every GET locally
/// while the public host is round-robin DNS, so a real, ready database 404'd on about half of all
/// requests, `/credentials` (the ONLY way to obtain the connection string) included. This is the
/// documented owner/leader fallback for that round-robin-reads-vs-leader-writes split; the leader
/// re-runs its own tenant check before answering, so proxying can never widen access.
async fn databases_from_leader(c: &Arc<CloudState>, path: &str, team: &str) -> Option<Value> {
    if c.is_control_plane_leader() {
        return None;
    }
    let leader = c.leader_forward_target()?;
    fetch_from_host(c, &leader, path, team).await
}

/// Lists the caller's databases: the local view MERGED with the leader's (never replaced, and never
/// short-circuited on "local is non-empty", which would serve a short list that reads as "my
/// database was deleted" instead of the replication gap it is), in a deterministic order so a
/// re-poll never reshuffles the rendered list.
async fn databases_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let mut list: Vec<crate::databases::Database> = c
        .databases
        .list(None)
        .into_iter()
        .filter(|d| record_tenant(&d.team) == t)
        .collect();

    if let Some(v) = databases_from_leader(&c, "/v1/databases", &t).await {
        if let Ok(remote) = serde_json::from_value::<Vec<crate::databases::Database>>(v) {
            let have: std::collections::HashSet<String> =
                list.iter().map(|d| d.id.clone()).collect();
            for d in remote {
                if record_tenant(&d.team) == t && !have.contains(&d.id) {
                    list.push(d);
                }
            }
        }
    }
    list.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
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

async fn databases_for_project(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!([]));
    }
    let list: Vec<_> = c
        .databases
        .list(Some(&project))
        .into_iter()
        .filter(|d| record_tenant(&d.team) == t)
        .collect();
    Json(json!(list))
}

async fn database_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(d) = c.databases.get(&id) {
        if record_tenant(&d.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
        return Ok(Json(json!(d)));
    }
    if let Some(v) = databases_from_leader(&c, &format!("/v1/databases/{id}"), &t).await {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

async fn database_credentials(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    // Unmasked connection secrets: strictly tenant-scoped.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(d) = c.databases.credentials(&id) {
        if record_tenant(&d.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
        return Ok(Json(json!(d)));
    }
    if let Some(v) = databases_from_leader(&c, &format!("/v1/databases/{id}/credentials"), &t).await
    {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

/// One-click "Open Studio" for a managed Supabase database: mints a short-lived, nonce-bearing
/// login ticket, returned only in a no-store JSON body; the dashboard POSTs it to the reserved
/// database-host exchange path, so no bearer enters a URL, history, referrer or upstream Studio
/// request.
async fn database_studio_open(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<(HeaderMap, Json<Value>), StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let (studio_password, studio_url, db_id, is_supabase) =
        if let Some(d) = c.databases.get_raw(&id) {
            if record_tenant(&d.team) != t {
                return Err(StatusCode::NOT_FOUND);
            }
            (
                d.connection
                    .get("STUDIO_PASSWORD")
                    .cloned()
                    .unwrap_or_default(),
                d.connection.get("STUDIO_URL").cloned().unwrap_or_default(),
                d.id.clone(),
                d.kind == crate::databases::DbKind::Supabase,
            )
        } else if let Some(v) =
            databases_from_leader(&c, &format!("/v1/databases/{id}/credentials"), &t).await
        {
            let conn = v.get("connection").cloned().unwrap_or_default();
            let g = |k: &str| {
                conn.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            (
                g("STUDIO_PASSWORD"),
                g("STUDIO_URL"),
                v.get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&id)
                    .to_string(),
                v.get("kind").and_then(|x| x.as_str()) == Some("supabase"),
            )
        } else {
            return Err(StatusCode::NOT_FOUND);
        };
    if !is_supabase || studio_password.is_empty() || !studio_url.starts_with("https://") {
        return Err(StatusCode::CONFLICT);
    }
    let ticket = crate::db_rest::mint_studio_ticket(
        &studio_password,
        &db_id,
        &c.node_name,
        crate::db_rest::STUDIO_TICKET_TTL_MS,
    );
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok((
        response_headers,
        Json(json!({
            "action": format!(
                "{}/_hive/studio-login",
                studio_url.trim_end_matches('/')
            ),
            "ticket": ticket,
            "expires_in_secs": crate::db_rest::STUDIO_TICKET_TTL_MS / 1000,
        })),
    ))
}

/// The env-var (KEY, VALUE, sensitive) triples a database injects into its owning project so
/// deployments reach it automatically: canonical names (DATABASE_URL, REDIS_URL, ...) for the
/// common single-DB case plus name-prefixed copies that stay collision-free when a project has
/// several DBs of one kind. Pure, so create (apply) and delete (remove by key) agree on the key
/// set.
fn db_egress_pairs(
    c: &Arc<CloudState>,
    d: &crate::databases::Database,
) -> Vec<(String, String, bool)> {
    let api_base = c.api_base();
    let prefix = crate::databases::env_prefix(&d.name);
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for (k, v) in crate::databases::env_exports(d, &api_base) {
        let kl = k.to_lowercase();
        let sensitive = kl.contains("url")
            || kl.contains("token")
            || kl.contains("password")
            || kl.contains("key")
            || kl.contains("secret");
        out.push((k.clone(), v.clone(), sensitive));
        if !prefix.is_empty() {
            out.push((format!("{prefix}_{k}"), v, sensitive));
        }
    }
    out
}

pub(crate) fn apply_db_egress(c: &Arc<CloudState>, d: &crate::databases::Database) {
    for (k, v, sensitive) in db_egress_pairs(c, d) {
        c.projects.put_env(
            &d.project,
            crate::project_settings::EnvVar {
                key: k,
                value: v,
                target: "all".into(),
                scope: "runtime".into(),
                sensitive,
                updated_ms: now_ms(),
            },
        );
    }
}

/// NON-SECRET directory of gateway-addressable DBs hosted on this node (routing metadata only:
/// `{id, db_host, host_node, kind}`), the same payload as the `/v1/db-directory` gossip arm; the
/// DNS leader fans it out to publish per-DB A records for DBs provisioned on other nodes. Only this
/// HTTP path is operator-gated (it stops a direct internet client using it as cross-tenant DB-id
/// recon, a stepping stone to the `database_replica` finding); the gossip fan-out has its own
/// inline arm in `gossip.rs` that never calls this handler.
async fn db_directory(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.databases.directory())))
}

const DATABASE_INCARNATION_PROVIDER_PREFIX: &str = "__hive_internal_project_incarnation_v1__:";

/// Carries lifecycle authority through the existing `ProvisionReq` mesh ABI: the iroh dispatch arm
/// deserializes the body into `ProvisionReq` before calling this module, so an additive top-level
/// field would be dropped there. `provider` is cosmetic and optional, so a capability-gated
/// internal sender temporarily wraps its original value in this marker and the receiver restores it
/// before the record is created. Public callers can never make the marker authoritative: only the
/// internal provision handler decodes it, after its peer/operator gate has passed.
fn database_internal_provision(
    mut req: crate::databases::ProvisionReq,
    incarnation: fluid_core::ProjectIncarnation,
) -> crate::databases::ProvisionReq {
    let provider = serde_json::to_vec(&req.provider).unwrap_or_else(|_| b"null".to_vec());
    let provider = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(provider);
    req.provider = Some(format!(
        "{DATABASE_INCARNATION_PROVIDER_PREFIX}{incarnation}:{provider}"
    ));
    req
}

fn database_internal_authority(
    mut req: crate::databases::ProvisionReq,
) -> Result<
    (
        crate::databases::ProvisionReq,
        fluid_core::ProjectIncarnation,
    ),
    (StatusCode, String),
> {
    let encoded = req
        .provider
        .as_deref()
        .and_then(|provider| provider.strip_prefix(DATABASE_INCARNATION_PROVIDER_PREFIX))
        .ok_or((
            StatusCode::CONFLICT,
            "internal database provision is missing its server-issued project incarnation".into(),
        ))?;
    let (incarnation, provider) = encoded.split_once(':').ok_or((
        StatusCode::BAD_REQUEST,
        "internal database provision carries a malformed project incarnation".into(),
    ))?;
    let incarnation = incarnation.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "internal database provision carries an invalid project incarnation".into(),
        )
    })?;
    let provider = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(provider)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "internal database provision carries malformed provider state".into(),
            )
        })?;
    req.provider = serde_json::from_slice(&provider).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "internal database provision carries invalid provider state".into(),
        )
    })?;
    Ok((req, incarnation))
}

fn database_project_authority(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    expected: Option<fluid_core::ProjectIncarnation>,
) -> Result<Option<(fluid_core::ProjectIncarnation, u64)>, (StatusCode, String)> {
    if project.is_empty() {
        return Ok(None);
    }
    if let Some(row) = c.projects.get_if_set(project) {
        let owner = record_tenant(&row.team);
        if owner != UNTAGGED_TENANT && owner != norm(team) {
            return Err((
                StatusCode::FORBIDDEN,
                "project belongs to a different team".into(),
            ));
        }
    }
    let legacy_ids = legacy_deployment_ids_for_owner(c, project, team);
    let incarnation = match expected {
        Some(expected) => c
            .projects
            .ensure_incarnation_exact(project, expected)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?,
        None => c
            .projects
            .ensure_incarnation(project)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?,
    };
    c.projects
        .set_team_exact(project, incarnation, team)
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    if !legacy_ids.is_empty() {
        adopt_authorized_legacy_deployments(c, project, incarnation, legacy_ids)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    }
    let settings = c
        .projects
        .get_exact(project, incarnation)
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    Ok(Some((incarnation, settings.incarnation_created_ms)))
}

async fn database_incarnation_capable(c: &Arc<CloudState>, node: &str) -> bool {
    let capable =
        |value: &Value| value.get("delete_generation").and_then(Value::as_str) == Some("v1");
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        if let Ok(response) = c
            .http
            .get(format!("{admin}/v1/project-delete/capabilities"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            if response.status().is_success() {
                if let Ok(value) = response.json::<Value>().await {
                    if capable(&value) {
                        return true;
                    }
                }
            }
        }
    }
    let target = c
        .registry
        .nodes()
        .into_iter()
        .find(|candidate| candidate.name == node)
        .and_then(|candidate| Some((candidate.peer_id?, candidate.iroh_addr?)));
    let Some((id, address)) = target else {
        return false;
    };
    crate::gossip::request_to(
        c,
        &id,
        &address,
        hive_p2p::GOSSIP_GET,
        "/v1/project-delete/capabilities",
        &[],
        10,
    )
    .await
    .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
    .as_ref()
    .is_some_and(capable)
}

fn database_ready_callback(
    cloud: Arc<CloudState>,
    project: String,
    incarnation: Option<fluid_core::ProjectIncarnation>,
) -> impl Fn(crate::databases::Database) + Send + 'static {
    move |database| {
        let cloud = cloud.clone();
        let project = project.clone();
        tokio::spawn(async move {
            let _lifecycle = match incarnation {
                Some(_) => Some(crate::project_settings::lifecycle_write(&project).await),
                None => None,
            };
            if let Some(expected) = incarnation {
                let project_is_current =
                    cloud
                        .projects
                        .get_exact(&project, expected)
                        .is_ok_and(|settings| {
                            record_tenant(&settings.team) == norm(&database.team)
                                && settings.owned_database_ids.contains(&database.id)
                        });
                let record_is_current =
                    cloud
                        .databases
                        .get_raw(&database.id)
                        .is_some_and(|current| {
                            current.id == database.id
                                && current.project == project
                                && current.team == database.team
                                && current.created_ms == database.created_ms
                        });
                if !project_is_current || !record_is_current {
                    tracing::warn!(
                        project,
                        incarnation = %expected,
                        database = %database.id,
                        "discarding database completion after its project incarnation changed"
                    );
                    teardown_db_backing(&database).await;
                    cloud
                        .databases
                        .remove_db_and_purge_data(&database.id, &database.team);
                    crate::persist::persist(&cloud);
                    return;
                }
            }
            if !database.project.is_empty()
                && matches!(database.status, crate::databases::DbStatus::Ready)
            {
                apply_db_egress(&cloud, &database);
            }
            if !database.replicas.is_empty() {
                crate::db_replicate::ensure_replicas(cloud.clone(), database.clone());
            }
            crate::persist::persist(&cloud);
            crate::webhooks::dispatch(
                &cloud.webhooks,
                &database.team,
                &project,
                "database.ready",
                json!({
                    "id": database.id,
                    "name": database.name,
                    "kind": database.kind,
                    "status": database.status,
                }),
            );
        });
    }
}

/// Creates a managed database. The tenant is ALWAYS the server-resolved one (a client-supplied
/// `team` used to be trusted whenever non-empty, letting any caller register a database and
/// auto-inject its connection env under a DIFFERENT tenant). Admission is a project-lifecycle
/// mutation: the record is inserted while holding the writer project deletion owns (an empty
/// project is unbound storage with no lifetime to serialize against), ownership is enforced only
/// for an ALREADY registered project (provisioning for a brand-new name before its first deploy is
/// a normal flow), and the identity is persisted before dispatch so the internal target can
/// validate it even if the request is cancelled. The create is idempotent: a same-tenant,
/// same-project, same-name, same-kind, non-error database already existing means this is a retry (a
/// double-click once produced three identical blob databases), so it is returned, unmasked and with
/// usable credentials, instead of minting another. Placement is region-aware (`schedule::place`,
/// only when a region was requested and a healthy, reachable, public candidate exists; any miss
/// falls through to this node, never failing the create) and must be ADMISSIBLE
/// (`databases::db_host_admissible`): a database born on a node that cannot publish a public
/// address (NAT'd, a macOS launchd node, or one that left the registry) is unreachable from
/// creation while still advertising `ready`, so such a node is replaced by one that can, and with
/// none in the registry the database is hosted here and the gap is logged loudly. A PEER host gets
/// the provisioning work dispatched to it (`database_provision_local`; podman containers only run
/// where the process that spawns them lives, so relabeling `host_node` alone would create a record
/// claiming containers that never existed); its record is adopted into this node's store
/// immediately (`upsert_replica`), because a delete arriving back here before the replication tick
/// found nothing and answered `removed: true` while the remote container leaked. When dispatch
/// fails, provisioning locally beats no database, and the requested region is dropped so the record
/// does not claim a region its bytes do not live in.
async fn database_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut req): Json<crate::databases::ProvisionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cloud = c.clone();
    req.team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let project = req.project.clone();
    let _lifecycle = if project.is_empty() {
        None
    } else {
        Some(crate::project_settings::lifecycle_write(&project).await)
    };
    if !project.is_empty() && c.projects.snapshot().contains_key(&project) {
        require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    }
    let authority = database_project_authority(&c, &project, &req.team, None)?;
    let incarnation = authority.map(|(incarnation, _)| incarnation);
    let owned_database_ids: std::collections::HashSet<String> = incarnation
        .and_then(|expected| c.projects.get_exact(&project, expected).ok())
        .map(|settings| settings.owned_database_ids.into_iter().collect())
        .unwrap_or_default();
    if incarnation.is_some() {
        crate::persist::persist(&c);
    }
    if let Some(existing_id) = c
        .databases
        .list(Some(&project))
        .into_iter()
        .find(|d| {
            d.team == req.team
                && d.name == req.name
                && d.kind == req.kind
                && (incarnation.is_none() || owned_database_ids.contains(&d.id))
                && !matches!(d.status, crate::databases::DbStatus::Error)
        })
        .map(|d| d.id)
    {
        if let Some(existing) = c.databases.get_raw(&existing_id) {
            return Ok(Json(json!(existing)));
        }
    }
    let requested_region = req
        .region
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    let admissible = |node: &str| crate::databases::db_host_admissible(&c, node);
    let mut host_node: Option<String> = requested_region
        .as_deref()
        .and_then(|region| {
            let regions = [region.to_string()];
            crate::schedule::place(&c, &regions, true, true, false, false, false, false, None)
                .into_iter()
                .next()
                .map(|t| t.node)
        })
        .filter(|node| admissible(node));
    if host_node.is_none() {
        if let Some(region) = requested_region.as_deref() {
            tracing::warn!(
                region = %region,
                "database create: no registry node with a public address serves this region, so a \
                 database placed there could never publish a resolvable <slug> A record; picking a \
                 host that can"
            );
        }
        if admissible(&c.node_name) {
            host_node = Some(c.node_name.clone());
        } else {
            host_node =
                crate::schedule::place(&c, &[], true, true, false, false, false, false, None)
                    .into_iter()
                    .map(|t| t.node)
                    .find(|node| admissible(node));
            if let Some(node) = &host_node {
                tracing::warn!(
                    node = %node,
                    "database create: this node advertises no public address, so a database hosted \
                     here could never publish an A record; placing it on a node that can"
                );
            }
        }
    }
    let host_node = host_node.unwrap_or_else(|| {
        tracing::warn!(
            node = %c.node_name,
            "database create: NO node in the registry advertises a public address; hosting here \
             anyway, but this database's <slug> A record cannot be published until one does"
        );
        c.node_name.clone()
    });
    let mut local_req = req.clone();

    if host_node != c.node_name {
        let remote_req = match incarnation {
            Some(expected) if database_incarnation_capable(&c, &host_node).await => {
                Some(database_internal_provision(req.clone(), expected))
            }
            Some(expected) => {
                tracing::warn!(
                    node = %host_node,
                    project,
                    incarnation = %expected,
                    "database create: target does not advertise project-lifecycle fencing; provisioning locally"
                );
                None
            }
            None => Some(req.clone()),
        };
        if let Some(remote_req) = remote_req {
            if let Some(v) = post_body_to_host(
                &c,
                &host_node,
                "/v1/internal/databases/provision-local",
                &req.team,
                &json!(remote_req),
            )
            .await
            {
                if let Some(error) = v.get("error").and_then(Value::as_str) {
                    return Err((StatusCode::CONFLICT, error.to_string()));
                }
                let db = serde_json::from_value::<crate::databases::Database>(v.clone()).map_err(
                    |error| {
                        (
                            StatusCode::BAD_GATEWAY,
                            format!("database owner returned an invalid record: {error}"),
                        )
                    },
                )?;
                if let Some(expected) = incarnation {
                    c.projects
                        .claim_database_exact(&project, expected, db.id.clone())
                        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
                }
                c.databases.upsert_replica(db);
                crate::persist::persist(&c);
                return Ok(Json(v));
            }
        }
        tracing::warn!(
            node = %host_node,
            requested_region = %requested_region.as_deref().unwrap_or_default(),
            actual_host = %c.node_name,
            "database create: chosen region node unreachable, provisioning locally instead — the \
             record will name THIS node, not the requested region"
        );
        local_req.region = None;
    }

    let db = crate::databases::provision(
        c.databases.clone(),
        c.region.clone(),
        local_req,
        c.db_domain.clone(),
        c.node_name.clone(),
        c.api_base(),
        database_ready_callback(cloud, project.clone(), incarnation),
    );
    if let Some(expected) = incarnation {
        if let Err(error) = c
            .projects
            .claim_database_exact(&project, expected, db.id.clone())
        {
            teardown_db_backing(&db).await;
            c.databases.remove_db_and_purge_data(&db.id, &db.team);
            return Err((StatusCode::CONFLICT, error.to_string()));
        }
    }
    crate::persist::persist(&c);
    crate::webhooks::dispatch(
        &c.webhooks,
        &db.team,
        &db.project,
        "database.created",
        json!({ "id": db.id, "name": db.name, "kind": db.kind }),
    );
    Ok(Json(json!(db)))
}

/// Internal-only mirror of [`database_create`]'s local-provisioning tail, reached via
/// [`post_body_to_host`] when that handler's region-aware placement chose a DIFFERENT node than the
/// one that received the client's request: the same `provision()` +
/// egress/replication/persist/webhooks sequence, always local (never re-dispatching: placement is
/// already resolved, and re-resolving on a `req.region` this node might interpret differently risks
/// a dispatch loop). Gated by [`require_operator_or_internal`]: the forwarded hop carries
/// `x-hive-internal`, never the original caller's Authorization header. A cancelled coordinator
/// request can be retried after this node already admitted the first copy, so it deduplicates under
/// the same lifecycle writer.
pub(crate) async fn database_provision_local(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<crate::databases::ProvisionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator_or_internal(&headers, claims.as_ref().map(|e| &e.0))?;
    let (req, incarnation) = if req.project.is_empty() {
        (req, None)
    } else {
        let (req, incarnation) = database_internal_authority(req)?;
        (req, Some(incarnation))
    };
    let project = req.project.clone();
    let _lifecycle = if project.is_empty() {
        None
    } else {
        Some(crate::project_settings::lifecycle_write(&project).await)
    };
    let authority = database_project_authority(&c, &project, &req.team, incarnation)?;
    let incarnation = authority.map(|(incarnation, _)| incarnation);
    let owned_database_ids: std::collections::HashSet<String> = incarnation
        .and_then(|expected| c.projects.get_exact(&project, expected).ok())
        .map(|settings| settings.owned_database_ids.into_iter().collect())
        .unwrap_or_default();
    if incarnation.is_some() {
        crate::persist::persist(&c);
    }

    if let Some(existing) = c
        .databases
        .list(Some(&project))
        .into_iter()
        .find(|database| {
            database.team == req.team
                && database.name == req.name
                && database.kind == req.kind
                && (incarnation.is_none() || owned_database_ids.contains(&database.id))
                && !matches!(database.status, crate::databases::DbStatus::Error)
        })
        .and_then(|database| c.databases.get_raw(&database.id))
    {
        return Ok(Json(json!(existing)));
    }

    let cloud = c.clone();
    let db = crate::databases::provision(
        c.databases.clone(),
        c.region.clone(),
        req,
        c.db_domain.clone(),
        c.node_name.clone(),
        c.api_base(),
        database_ready_callback(cloud, project.clone(), incarnation),
    );
    if let Some(expected) = incarnation {
        if let Err(error) = c
            .projects
            .claim_database_exact(&project, expected, db.id.clone())
        {
            teardown_db_backing(&db).await;
            c.databases.remove_db_and_purge_data(&db.id, &db.team);
            return Err((StatusCode::CONFLICT, error.to_string()));
        }
    }
    crate::persist::persist(&c);
    crate::webhooks::dispatch(
        &c.webhooks,
        &db.team,
        &db.project,
        "database.created",
        json!({ "id": db.id, "name": db.name, "kind": db.kind }),
    );
    Ok(Json(json!(db)))
}

/// Best-effort teardown of a database's backing containers, volume and SQLite file: the part of
/// delete that only means something ON THE NODE THAT HOSTS THEM, split out so it runs locally (this
/// node IS `host_node`) or is dispatched to the real owner over the mesh
/// (`database_teardown_local`). Region-aware placement made that necessary: a database living on a
/// peer would otherwise leak its containers and (for Supabase) its data volume forever, since a
/// podman `rm` of a name that does not exist locally silently no-ops.
async fn teardown_db_backing(d: &crate::databases::Database) {
    let id = &d.id;
    crate::databases::note_teardown_request(id);
    // Order matters: close the streams (one mid-BEGIN holds the write lock), then the pool, then
    // unlink the file, or the unlink races a live writer.
    if d.kind == crate::databases::DbKind::Sqlite {
        crate::hrana::close_streams(id);
        crate::sqlite_pool::close(id);
        // Both engines' files, because a database may have been migrated and rolled back: whichever
        // exist are removed (a `.musq` left behind would be served as a live database by the
        // sidecar after its record is gone); musql keeps `.musq`, a 0-byte `.musq.lock` and
        // transiently a `.musq.delta`, SQLite `-wal`/`-shm`; missing files are expected.
        for suffix in ["", "-wal", "-shm"] {
            if let Some(p) = crate::databases::sqlite_file_path(id) {
                let p = if suffix.is_empty() {
                    p
                } else {
                    std::path::PathBuf::from(format!("{}{suffix}", p.display()))
                };
                if let Err(e) = std::fs::remove_file(&p) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(db = %id, path = %p.display(), error = %e, "could not remove SQLite database file");
                    }
                }
            }
        }
        for suffix in ["", ".lock", ".delta"] {
            if let Some(p) = crate::databases::sqlite_musql_path(id) {
                let p = if suffix.is_empty() {
                    p
                } else {
                    std::path::PathBuf::from(format!("{}{suffix}", p.display()))
                };
                if let Err(e) = std::fs::remove_file(&p) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(db = %id, path = %p.display(), error = %e, "could not remove musql database file");
                    }
                }
            }
        }
    }
    // The record's comma-joined `container` field (a Supabase record carries its whole stack), else
    // the platform-templated names by kind: a delete racing an in-flight provision reads
    // `container` while it is still None and would leak the container the provision starts seconds
    // later. `-v` is the podman lock-pool rule.
    let short = &id[3..11.min(id.len())];
    let names: Vec<String> = match d.container.as_deref() {
        Some(container) => container
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        None => {
            if d.kind == crate::databases::DbKind::Supabase {
                vec![
                    format!("hive-supa-{short}-db"),
                    format!("hive-supa-{short}-meta"),
                    format!("hive-supa-{short}-studio"),
                ]
            } else {
                vec![format!("hive-db-{short}")]
            }
        }
    };
    for one in names {
        let _ = tokio::process::Command::new("podman")
            .args(["rm", "-f", "-v", &one])
            .env(
                "PATH",
                format!(
                    "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    if d.kind == crate::databases::DbKind::Supabase {
        // The stack's named volumes are platform-templated (`hive-vol-supa[st]-<id8>`), never
        // tenant-named, so this cannot touch another volume.
        for vol in [
            format!("hive-vol-supa-{short}"),
            format!("hive-vol-supast-{short}"),
        ] {
            let _ = tokio::process::Command::new("podman")
                .args(["volume", "rm", "-f", &vol])
                .env(
                    "PATH",
                    format!(
                        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }
    }
}

/// Record-free teardown by platform template: the kind is unknowable without the catalog record, so
/// both naming families are covered (single-container `hive-db-<id8>` and the Supabase trio plus
/// its named volume). Every removal is idempotent best-effort; a name that never existed is a
/// no-op.
async fn teardown_db_backing_blind(id: &str) {
    if id.len() <= 3 {
        return;
    }
    crate::databases::note_teardown_request(id);
    let short = &id[3..11.min(id.len())];
    let path_env = format!(
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    for one in [
        format!("hive-db-{short}"),
        format!("hive-supa-{short}-db"),
        format!("hive-supa-{short}-meta"),
        format!("hive-supa-{short}-auth"),
        format!("hive-supa-{short}-rest"),
        format!("hive-supa-{short}-storage"),
        format!("hive-supa-{short}-studio"),
    ] {
        let _ = tokio::process::Command::new("podman")
            .args(["rm", "-f", "-v", &one])
            .env("PATH", &path_env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    for vol in [
        format!("hive-vol-supa-{short}"),
        format!("hive-vol-supast-{short}"),
    ] {
        let _ = tokio::process::Command::new("podman")
            .args(["volume", "rm", "-f", &vol])
            .env("PATH", &path_env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
}

/// Internal-only: runs [`teardown_db_backing`] for `id` on THIS node, reached via
/// [`post_body_to_host`] from `database_delete` when the record's `host_node` is a peer; gated by
/// [`require_operator_or_internal`] like [`database_provision_local`]. With no local record (the
/// catalog delete's store_sync can beat this dispatch) it tears down by platform template instead
/// of removing nothing and still answering `torn_down`.
pub(crate) async fn database_teardown_local(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(id): Json<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator_or_internal(&headers, claims.as_ref().map(|e| &e.0))?;
    if let Some(d) = c.databases.get_raw(&id) {
        teardown_db_backing(&d).await;
    } else {
        tracing::warn!(
            db = %id,
            "database teardown-local: no local record (catalog removal replicated first) — tearing down by platform template"
        );
        teardown_db_backing_blind(&id).await;
    }
    Ok(Json(json!({ "torn_down": id })))
}

/// Deletes a database: removes the auto-injected egress env vars from the owning project, tears
/// down cross-region replicas, and tears down the backing locally when this node is `host_node`,
/// else dispatches to the owner (`database_teardown_local`), best-effort: an unreachable peer must
/// not block the catalog delete (a stranded container is caught by the orphan-reap sweep, the
/// customer asked for the database gone). Replication lag is retried (3 x 400 ms), both for the
/// record itself (a database created on a peer via `database_provision_local` may not have
/// replicated here yet, and a no-op on an unknown id used to answer `removed: true` while the
/// container ran on forever) and for an unset `host_node`, which is NEVER read as "must be local":
/// an unknown host is logged loudly (the backing was NOT torn down, the catalog entry is removed
/// regardless) so an operator can find it through the orphan sweep. The catalog removal also purges
/// the queue/vector/blob PAYLOAD data (keyed by a team-prefixed connection name, not the db id):
/// removing only the catalog entry left customer data, possibly PII in vector metadata or queue
/// message bodies, orphaned in memory and on disk and re-persisted in every snapshot.
async fn database_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let mut record = c.databases.get_raw(&id);
    if record.is_none() {
        for _ in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            record = c.databases.get_raw(&id);
            if record.is_some() {
                break;
            }
        }
    }
    if let Some(d) = record {
        if record_tenant(&d.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
        if !d.project.is_empty() {
            for (k, _, _) in db_egress_pairs(&c, &d) {
                c.projects.delete_env(&d.project, &k);
            }
        }
        if !d.replicas.is_empty() {
            crate::db_replicate::remove_replicas(c.clone(), d.clone());
        }
        let mut d = d;
        if d.host_node.is_empty() {
            for _ in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                if let Some(fresh) = c.databases.get_raw(&id) {
                    if !fresh.host_node.is_empty() {
                        d = fresh;
                        break;
                    }
                }
            }
        }
        if d.host_node == c.node_name {
            teardown_db_backing(&d).await;
        } else if d.host_node.is_empty() {
            tracing::warn!(
                db = %id,
                "database delete: host_node unknown on this node (stale replication?) — \
                 backing container/volume was NOT torn down; catalog entry removed regardless"
            );
        } else {
            if post_body_to_host(
                &c,
                &d.host_node,
                "/v1/internal/databases/teardown-local",
                &t,
                &json!(id),
            )
            .await
            .is_none()
            {
                tracing::warn!(
                    db = %id,
                    node = %d.host_node,
                    "database delete: host node unreachable, backing container/volume may be leaked (catalog entry removed regardless)"
                );
            }
        }
    }
    c.databases.remove_db_and_purge_data(&id, &t);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
}

/// Regenerates a Blob database's access credentials without touching the underlying data or bucket
/// identity: the fix for a leaked credential (log, transcript, accidental print) that does not
/// warrant losing the data. The egress env is refreshed with the new credentials, so deployments
/// pick them up on their next restart instead of the stale ones.
async fn database_rotate(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(d) = c.databases.get_raw(&id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if record_tenant(&d.team) != t {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(rotated) = crate::databases::rotate_blob_credentials(&c.databases, &id) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if !rotated.project.is_empty() {
        apply_db_egress(&c, &rotated);
    }
    crate::persist::persist(&c);
    Ok(Json(json!(rotated)))
}

/// HTTP mount of [`database_replica`]: a replica register/remove rewrites the owning project's env
/// and can stop a container, so only a platform operator, the fleet's `x-hive-internal` secret or a
/// `mesh-internal` delegation minted for the replicated database's own team may call it
/// ([`require_mesh_rpc`]). The gossip arm calls the inner handler directly, behind the transport's
/// signer and trust gate.
async fn database_replica_public(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = body
        .pointer("/db/team")
        .and_then(Value::as_str)
        .unwrap_or("");
    require_mesh_rpc(&headers, claims.as_ref().map(|e| &e.0), Some(team))?;
    database_replica(State(c), Json(body)).await
}

/// Mesh-internal: register or remove a cross-region REPLICA of a database on this node. Called by
/// the primary node's replication fanout (the iroh gossip arm, or [`database_replica_public`] over
/// HTTP). On register we record the database locally as a `replica` in THIS node's region,
/// auto-inject its egress env into the owning project (so in-region deployments reach it), and, for
/// Postgres/Redis, provision a real in-region backing container.
///
/// The body is peer-supplied, so the record it names is trusted only as far as THIS node's own
/// state confirms it: an existing record must belong to the claimed team, and the project whose env
/// it feeds or strips must belong to that team too.
pub(crate) async fn database_replica(
    State(c): State<Arc<CloudState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let op = body.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let mut db: crate::databases::Database =
        serde_json::from_value(body.get("db").cloned().unwrap_or(Value::Null))
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("bad db payload: {e}")))?;
    if !db.id.trim().is_empty() {
        if let Some(existing) = c.databases.get_raw(&db.id) {
            if record_tenant(&existing.team) != norm(&db.team) {
                return Err((
                    StatusCode::FORBIDDEN,
                    "db belongs to a different team".into(),
                ));
            }
        }
    }
    if !db.project.is_empty() && !project_owned_by(&c, &db.project, norm(&db.team)) {
        return Err((
            StatusCode::FORBIDDEN,
            "db project belongs to a different team".into(),
        ));
    }
    match op {
        "register" => {
            if db.role.is_empty() {
                db.role = "replica".into();
            }
            if db.region.is_empty() {
                db.region = c.region.clone();
            }
            let backed =
                crate::databases::provision_replica_backing(c.databases.clone(), &db).await;
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
                        .env(
                            "PATH",
                            format!(
                                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
                                std::env::var("PATH").unwrap_or_default()
                            ),
                        )
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

/// Applies a MIRRORED storage write received over the iroh mesh (the HTTP admin path goes through
/// the normal handlers). The path is `/v1/storage/<kind>/<name>[/<key>]` with `?team=`, and the
/// write lands in the SAME tenant namespace (`<team>::<name>`) so the replica holds identical data;
/// it is never re-mirrored (already a replicated write). Realtime rooms ride the same broker as
/// pub/sub topics, so a fanned-out room frame is published to it like a topic message (room frames
/// are raw text on the wire, kept raw so a mirrored frame is byte-identical to a locally published
/// one).
pub(crate) async fn apply_mirrored_write(c: &Arc<CloudState>, path: &str, body: &[u8]) {
    let team = qs(path, "team").unwrap_or_else(|| "personal".into());
    let no_q = path.split('?').next().unwrap_or(path);
    let segs: Vec<&str> = no_q.trim_start_matches('/').split('/').collect();
    let kind = segs.get(2).copied().unwrap_or("");
    match kind {
        "blob" => {
            if let (Some(bucket), Some(key)) = (segs.get(3), segs.get(4)) {
                c.databases
                    .blob_put(&format!("{team}::{bucket}"), key, body.to_vec());
            }
        }
        "queue" => {
            if let Some(queue) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<Value>(body) {
                    if let Some(msg) = v.get("message") {
                        c.databases
                            .queue_push(&format!("{team}::{queue}"), msg.to_string());
                    }
                }
            }
        }
        "vector" => {
            if let Some(index) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<VectorUpsert>(body) {
                    c.databases.vector_upsert(
                        &format!("{team}::{index}"),
                        &v.id,
                        v.vector,
                        v.metadata,
                    );
                }
            }
        }
        "pubsub" => {
            if let Some(topic) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<Value>(body) {
                    if let Some(msg) = v.get("message") {
                        c.databases
                            .publish(&format!("{team}::{topic}"), msg.to_string());
                    }
                }
            }
        }
        "realtime" => {
            if let Some(room) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<Value>(body) {
                    if let Some(msg) = v.get("message") {
                        let text = msg
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| msg.to_string());
                        c.databases.publish(&format!("{team}::{room}"), text);
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

/// Tenant-prefixed namespace (`<tenant>::<name>`) for a bucket, queue, index, topic or room named
/// in a request path, so two teams using the same name get DISJOINT storage. Responses echo the
/// caller's un-prefixed name.
fn ns(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    name: &str,
) -> String {
    format!("{}::{}", tenant(c, h, claims), name)
}

/// Resolves the (team, is_mirror) scope for a storage WRITE. A mesh-internal mirrored write is a
/// trusted replication call from a primary node, so its team is authoritative and replicated data
/// lands in the SAME tenant namespace on the replica. That MUST be bound to a verified identity,
/// never to header presence: this admin router is bound to the public API host, so an arbitrary
/// internet caller can set `x-hive-mirror`/`x-hive-team` themselves. `x-hive-mirror-tok` carries a
/// short-lived signed token (minted like `mesh_team_qs`:
/// `crate::auth::issue("mesh-internal", team, "service", false, crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS)`),
/// which only a caller holding `HIVE_JWT_SECRET` can produce, closing the cross-tenant write
/// bypass. When JWT enforcement is off (dev/single-node) the raw header is trusted (nothing to
/// forge against). Normal writes resolve the tenant as usual.
fn write_scope(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
) -> (String, bool) {
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
        let team = h
            .get("x-hive-team")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "personal".into());
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
    crate::db_replicate::on_write(
        &c,
        is_mirror,
        &team,
        &bucket,
        "PUT",
        format!("/v1/storage/blob/{bucket}/{key}"),
        "application/octet-stream",
        body.to_vec(),
    );
    Json(
        json!({ "bucket": bucket, "key": key, "size": size, "url": format!("/v1/storage/blob/{bucket}/{key}") }),
    )
}

/// Reads a blob from the caller's tenant namespace. Blob bytes live on node-local disk, but
/// `admin_ingress` forwards mutations to the control-plane leader while serving reads LOCALLY, and
/// `BLOB_ENDPOINT` handed to every app points at the round-robin api host, so a PUT lands on the
/// leader and the matching GET lands anywhere. A local miss therefore falls back to the leader (the
/// same fallback `build_get` uses); without it the GET would 404 forever, with no sync path that
/// ever heals it.
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
        None => {
            if let Some(leader) = c.leader_forward_target() {
                let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
                let path = format!("/v1/storage/blob/{bucket}/{key}");
                if let Some(b) = fetch_bytes_from_host(&c, &leader, &path, &t).await {
                    return Ok(b.into_response());
                }
            }
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// Lists a bucket's keys in the caller's tenant namespace. An empty listing on a non-leader almost
/// always means the objects were PUT on the leader rather than that the bucket is empty, so it
/// falls back to the leader's listing (see [`blob_get`]).
async fn blob_list_keys(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(bucket): Path<String>,
) -> Json<Value> {
    let nsb = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &bucket);
    let keys = c.databases.blob_list(&nsb);
    if let (true, Some(leader)) = (keys.is_empty(), c.leader_forward_target()) {
        let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
        if let Some(v) =
            fetch_from_host(&c, &leader, &format!("/v1/storage/blob/{bucket}"), &t).await
        {
            return Json(v);
        }
    }
    Json(json!({ "bucket": bucket, "keys": keys }))
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
    crate::db_replicate::on_write(
        &c,
        is_mirror,
        &team,
        &queue,
        "POST",
        format!("/v1/storage/queue/{queue}"),
        "application/json",
        mirror_body,
    );
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
    let parsed = msg
        .as_ref()
        .and_then(|m| serde_json::from_str::<Value>(m).ok());
    Json(
        json!({ "queue": queue, "message": parsed.or(msg.map(Value::String)), "depth": c.databases.queue_depth(&nsq) }),
    )
}

/// Depth of a tenant's queue. `queue_push` and `queue_pop` are mutations and so run on the leader,
/// while this read serves locally, so every non-leader node would report 0 however deep the real
/// queue is: a depth of 0 on a non-leader falls back to the leader's answer. Depth is served by GET
/// on the queue route itself (`/v1/storage/queue/:queue`); there is no `/depth` sub-route.
async fn queue_depth(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(queue): Path<String>,
) -> Json<Value> {
    let nsq = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &queue);
    let depth = c.databases.queue_depth(&nsq);
    if let (0, Some(leader)) = (depth, c.leader_forward_target()) {
        let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
        if let Some(v) =
            fetch_from_host(&c, &leader, &format!("/v1/storage/queue/{queue}"), &t).await
        {
            return Json(v);
        }
    }
    Json(json!({ "queue": queue, "depth": depth }))
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
    let mirror_body =
        serde_json::to_vec(&json!({ "id": b.id, "vector": b.vector, "metadata": b.metadata }))
            .unwrap_or_default();
    c.databases.vector_upsert(&nsi, &b.id, b.vector, b.metadata);
    crate::db_replicate::on_write(
        &c,
        is_mirror,
        &team,
        &index,
        "POST",
        format!("/v1/storage/vector/{index}"),
        "application/json",
        mirror_body,
    );
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

/// Enqueues a delayed HTTP callback (`target_url`, `headers`, `payload`, `delay_seconds`,
/// `max_attempts`) in the managed world queue, tenant-scoped like every other `/v1/storage/*`-style
/// mutation. The target URL is caller-supplied, so the caller must resolve to a real authenticated
/// tenant: an anonymous (`ANON_TENANT`) caller never schedules arbitrary HTTP callbacks.
async fn wqueue_enqueue(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<WqueueEnqueueReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if t == ANON_TENANT {
        return Err((StatusCode::UNAUTHORIZED, "authentication required".into()));
    }
    let id = c
        .world_queue
        .enqueue(
            t,
            b.target_url,
            b.headers,
            b.payload,
            b.delay_seconds,
            b.max_attempts,
        )
        .map_err(|refused| (StatusCode::BAD_REQUEST, refused.to_string()))?;
    Ok(Json(json!({ "message_id": id })))
}

async fn wqueue_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(c.world_queue.stats()))
}

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

/// Publishes to the tenant-namespaced topic on the local broker and fans the message out to EVERY
/// node, not just replica regions: the broker is per-process and subscribers sit on whichever node
/// their WebSocket landed on, so anything less silently delivers to a subset (usually just the
/// leader's own).
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
    crate::db_replicate::fanout_all(
        &c,
        is_mirror,
        &team,
        format!("/v1/storage/pubsub/{topic}/publish"),
        "application/json",
        mirror_body,
    );
    Json(json!({ "topic": topic, "delivered": delivered, "fanout": !is_mirror }))
}

/// WebSocket subscription to a tenant-namespaced pub/sub topic. A subscriber that lags skips the
/// frames it missed.
async fn ws_pubsub(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(topic): Path<String>,
) -> axum::response::Response {
    let mut rx = c
        .databases
        .subscribe(&ns(&c, &headers, claims.as_ref().map(|e| &e.0), &topic));
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let _ = socket
            .send(Message::Text(
                json!({ "type": "subscribed", "topic": topic }).to_string(),
            ))
            .await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(m) => { if socket.send(Message::Text(m)).await.is_err() { break; } }
                    Err(_) => continue,
                },
                client = socket.recv() => match client {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    })
}

/// WebSocket realtime room (tenant-namespaced, bidirectional): a client message is broadcast to the
/// whole room. Subscribers sit on whichever node their socket landed on, so every frame is also
/// fanned out to every node; locally only, two browsers in the same room would never see each
/// other.
async fn ws_realtime(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(room): Path<String>,
) -> axum::response::Response {
    let raw_room = room.clone();
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let room = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &room);
    let mut rx = c.databases.subscribe(&room);
    let db = c.databases.clone();
    let cloud = c.clone();
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let _ = socket
            .send(Message::Text(
                json!({ "type": "joined", "room": room }).to_string(),
            ))
            .await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(m) => { if socket.send(Message::Text(m)).await.is_err() { break; } }
                    Err(_) => continue,
                },
                client = socket.recv() => match client {
                    Some(Ok(Message::Text(t))) => {
                        db.publish(&room, t.clone());
                        crate::db_replicate::fanout_all(
                            &cloud,
                            false,
                            &team,
                            format!("/v1/storage/realtime/{raw_room}"),
                            "application/json",
                            serde_json::to_vec(&json!({ "message": t })).unwrap_or_default(),
                        );
                    }
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
                Message::Text(t) => {
                    if socket
                        .send(Message::Text(format!("echo: {t}")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

async fn securelinks_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    Json(json!(c.securelinks.list(&tenant(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0)
    ))))
}

/// Provisions a secure-compute (WireGuard) link. The tenant is always the server-resolved one (a
/// client-supplied `team` is never trusted, as in `database_create`), and project ownership is
/// enforced only when wiring into an ALREADY-registered project (an unset or new project name is
/// the normal "create the tunnel first" flow). The connector's local address is then injected as a
/// project env var, so deployed functions reach the private backend through the encrypted tunnel
/// transparently.
async fn securelink_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(mut req): Json<crate::securelink::ProvisionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    req.team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(p) = req.project.as_deref().filter(|p| !p.is_empty()) {
        if c.projects.snapshot().contains_key(p) {
            require_project(&c, &headers, claims.as_ref().map(|e| &e.0), p)?;
        }
    }
    let region = c.region.clone();
    let rec = c
        .securelinks
        .provision(req, &region)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if !rec.project.is_empty() && !rec.env_var.is_empty() {
        c.projects.put_env(
            &rec.project,
            crate::project_settings::EnvVar {
                key: rec.env_var.clone(),
                value: rec.local_addr.clone(),
                target: "all".into(),
                scope: "runtime".into(),
                sensitive: false,
                updated_ms: now_ms(),
            },
        );
        crate::persist::persist(&c);
        let ev = c.event(
            &c.region,
            "SECURE",
            &rec.target,
            "/",
            200,
            "deploy",
            &format!(
                "secure tunnel {} → {} wired to {}.{}",
                rec.local_addr, rec.target, rec.project, rec.env_var
            ),
        );
        c.record(ev);
    }
    Ok(Json(json!(rec)))
}

async fn securelink_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(l) = c.securelinks.all().into_iter().find(|l| l.id == id) {
        if norm(&l.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    c.securelinks.remove(&id);
    Ok(Json(json!({ "removed": id })))
}

#[derive(Deserialize)]
struct MetricsQ {
    minutes: Option<usize>,
    project: Option<String>,
    /// "minute" (default) | "hour" | "day": the Usage page's Daily/Weekly/Monthly toggle.
    /// Unrecognized or absent falls back to "minute" via `Granularity::parse`, never a 400: it is a
    /// display parameter.
    gran: Option<String>,
    /// Set by the fleet fan-out's own inner call to request ONLY this node's local view, with no
    /// further fan-out (one hop, no recursion). Absent/false is the top-level call, which fans out
    /// to every other healthy node (see `metrics_get`).
    #[serde(default, deserialize_with = "de_lenient_bool")]
    local: Option<bool>,
}

/// Merges one peer's `/v1/metrics?local=true` JSON response into the running accumulators (series
/// by `t_ms`, status/top-paths/projects by key). It silently no-ops on a malformed or unreachable
/// peer response: a single unreachable node must not fail the whole tenant's usage view (matches
/// `fleet_function_stats`'s best-effort fan-out).
fn merge_peer_metrics(
    v: &Value,
    series: &mut Vec<crate::metrics::Bucket>,
    status_distribution: &mut std::collections::HashMap<String, u64>,
    top_paths: &mut std::collections::HashMap<String, u64>,
    projects: &mut std::collections::HashMap<String, u64>,
) {
    if let Some(peer_series) = v
        .get("series")
        .and_then(|s| serde_json::from_value::<Vec<crate::metrics::Bucket>>(s.clone()).ok())
    {
        let mut by_t: std::collections::HashMap<u64, usize> = series
            .iter()
            .enumerate()
            .map(|(i, b)| (b.t_ms, i))
            .collect();
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
    if let Some(peer_status) = v.get("status_distribution").and_then(|s| {
        serde_json::from_value::<std::collections::HashMap<String, u64>>(s.clone()).ok()
    }) {
        for (k, n) in peer_status {
            *status_distribution.entry(k).or_insert(0) += n;
        }
    }
    for (dst, key) in [(&mut *top_paths, "top_paths"), (&mut *projects, "projects")] {
        let (name_key, count_key) = if key == "top_paths" {
            ("path", "count")
        } else {
            ("project", "requests")
        };
        if let Some(rows) = v.get(key).and_then(|s| s.as_array()) {
            for row in rows {
                if let (Some(name), Some(n)) = (
                    row.get(name_key).and_then(|x| x.as_str()),
                    row.get(count_key).and_then(|x| x.as_u64()),
                ) {
                    *dst.entry(name.to_string()).or_insert(0) += n;
                }
            }
        }
    }
}

/// TENANT-SCOPED usage series and breakdowns for the Usage page: every series/status/path read is
/// confined to the caller's tenant (the owner ops console reads global metrics through the separate
/// operator-gated `admin_overview`). FLEET-AGGREGATE: `MetricsStore` is node-local with zero
/// cross-node merge (never gossiped, and deliberately NOT adopted from a peer on restore, see
/// `guardian::strip_node_local`: it would double-count that node's traffic, unlike billing, which
/// has a single elected metering owner), and a tenant's traffic can land on ANY healthy node the
/// routing mesh sends it to, so without fanning out `/v1/metrics` answers depend on which node
/// serves the dashboard's request (the same tenant once showed 0 requests on 7 of 8 nodes and real
/// traffic only on the 8th). `q.local=true` (set only on the inner fan-out call) stops a peer from
/// fanning out again: single hop, no N-way amplification. The requested span is clamped to what the
/// resolution's ring buffer retains (`Granularity::max_span_minutes`: minute 24 h, hour 30 d, day
/// about 13 months, see `metrics.rs`), and the fleet-fan-out result (never the inner `local=true`
/// hop) is cached for a few seconds, collapsing redundant fan-outs from several tabs or team
/// members into one cross-node round.
async fn metrics_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<MetricsQ>,
) -> Json<Value> {
    let gran = crate::metrics::Granularity::parse(q.gran.as_deref());
    let minutes = q.minutes.unwrap_or(60).min(gran.max_span_minutes());
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let scope = Some(t.as_str());
    let project = q.project.as_deref().filter(|p| !p.is_empty());
    let is_top_level = !q.local.unwrap_or(false);
    let cache_key = format!(
        "metrics:{t}:{minutes}:{}:{}",
        q.gran.as_deref().unwrap_or("minute"),
        project.unwrap_or("")
    );
    if is_top_level {
        if let Some(v) = c.resp_cache.get(&cache_key, POLLED_RESPONSE_CACHE_TTL) {
            return Json(v);
        }
    }
    let mut series = c.metrics.series(gran, minutes, now_ms(), scope, project);
    let mut status_distribution = c.metrics.status_distribution(scope);
    let mut top_paths: std::collections::HashMap<String, u64> =
        c.metrics.top_paths(scope, 200).into_iter().collect();
    let mut projects: std::collections::HashMap<String, u64> = c
        .metrics
        .project_totals(gran, minutes, now_ms(), scope)
        .into_iter()
        .collect();

    if !q.local.unwrap_or(false) {
        let mut path = format!(
            "/v1/metrics?local=true&minutes={minutes}&gran={}",
            q.gran.as_deref().unwrap_or("minute")
        );
        if let Some(p) = project {
            path.push_str(&format!("&project={}", urlencode(p)));
        }
        for v in fan_out_peers_polled(&c, &all_healthy_peers(&c), &t, &path).await {
            merge_peer_metrics(
                &v,
                &mut series,
                &mut status_distribution,
                &mut top_paths,
                &mut projects,
            );
        }
    }

    let total_req: u64 = series.iter().map(|b| b.requests).sum();
    let total_err: u64 = series.iter().map(|b| b.errors + b.client_err).sum();
    let total_blocked: u64 = series.iter().map(|b| b.blocked).sum();
    let hits: u64 = series.iter().map(|b| b.cache_hits).sum();
    let miss: u64 = series.iter().map(|b| b.cache_miss).sum();
    let cache_ratio = if hits + miss == 0 {
        0.0
    } else {
        hits as f64 / (hits + miss) as f64
    };
    let err_rate = if total_req == 0 {
        0.0
    } else {
        total_err as f64 / total_req as f64
    };
    let mut top_paths_v: Vec<(String, u64)> = top_paths.into_iter().collect();
    top_paths_v.sort_by(|a, b| b.1.cmp(&a.1));
    top_paths_v.truncate(10);
    let mut projects_v: Vec<(String, u64)> = projects.into_iter().collect();
    projects_v.sort_by(|a, b| b.1.cmp(&a.1));
    let result = json!({
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
    });
    if is_top_level {
        c.resp_cache.set(cache_key, result.clone());
    }
    Json(result)
}

#[derive(Deserialize)]
struct SpeedInsightsQ {
    minutes: Option<usize>,
    /// "desktop" | "mobile" — absent means both.
    device: Option<String>,
    /// Set by the fleet fan-out's own inner call: the same single-hop rationale as
    /// `MetricsQ::local` (RUM samples are node-local, a tenant's visitors can land on any node the
    /// routing mesh sends them to).
    #[serde(default, deserialize_with = "de_lenient_bool")]
    local: Option<bool>,
}

fn parse_device(s: Option<&str>) -> Option<RumDevice> {
    match s {
        Some("mobile") => Some(RumDevice::Mobile),
        Some("desktop") => Some(RumDevice::Desktop),
        _ => None,
    }
}

/// Default Speed Insights window: the last 7 days.
const SPEED_INSIGHTS_DEFAULT_MINUTES: usize = 10_080;

/// Real User Monitoring summary for the Speed Insights page: p75/p90/p95/p99 per Core Web Vital, a
/// computed Real Experience Score, real top routes by beacon count and the true sample count. See
/// `fluid_gateway::RumStore` for where the underlying beacon data lives.
async fn speed_insights_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<SpeedInsightsQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let minutes = q.minutes.unwrap_or(SPEED_INSIGHTS_DEFAULT_MINUTES);
    let device = parse_device(q.device.as_deref());
    let mut raw = c.gw.rum_raw(&t, minutes, device);
    if q.local.unwrap_or(false) {
        return Json(json!(raw));
    }
    let cache_key = format!(
        "speed-insights:{t}:{minutes}:{}",
        q.device.as_deref().unwrap_or("")
    );
    if let Some(v) = c.resp_cache.get(&cache_key, Duration::from_secs(5)) {
        return Json(v);
    }
    let dev_qs = q
        .device
        .as_deref()
        .map(|d| format!("&device={d}"))
        .unwrap_or_default();
    let path = format!("/v1/speed-insights?local=true&minutes={minutes}{dev_qs}");
    for v in fan_out_peers_polled(&c, &all_healthy_peers(&c), &t, &path).await {
        if let Ok(peer_raw) = serde_json::from_value::<RumRaw>(v) {
            raw.merge(&peer_raw);
        }
    }
    let result = json!(raw.summarize());
    c.resp_cache.set(cache_key, result.clone());
    Json(result)
}

#[derive(Deserialize, Default)]
struct OverviewQ {
    /// Set by the fleet fan-out's own inner call: the same single-hop rationale as
    /// `MetricsQ::local` (`c.counters()` and `c.metrics` are both node-local with no live
    /// cross-node merge).
    #[serde(default, deserialize_with = "de_lenient_bool")]
    local: Option<bool>,
}

/// The operator ops-console overview: GLOBAL across all tenants (owner-only) and FLEET-AGGREGATE.
/// `c.counters()` and `c.metrics` are node-local (the same defect as `metrics_get`), so without the
/// fan-out the headline request, blocked and error-rate tiles would reflect only whichever node
/// serves the request and understate fleet traffic by up to 8x; `local=true` on the fan-out call
/// stops peer recursion (one hop). The raw 30-minute request/error counts are returned, not just
/// the ratio, so a fan-out call can weight-average across nodes instead of averaging
/// already-averaged per-node ratios. The result is cached like `metrics_get` (the fan-out costs
/// ~100 ms per call on the leader and two admin pages poll this every few seconds). Deployments are
/// local plus gossiped `peer_deployments` (deduped by id), because the placement scheduler hosts
/// deployments on peers and a bare `c.gw.list()` would under-count to zero here; projects are the
/// tenant-tagged projects store (authoritative, gossiped) UNION any project that only shows up as a
/// hosted deployment, so a project is counted even before its record replicates. Cluster capacity
/// is the sum of every live node's host resources.
async fn admin_overview(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    q: Option<Query<OverviewQ>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let local = q
        .as_ref()
        .map(|q| q.0.local.unwrap_or(false))
        .unwrap_or(false);
    if !local {
        if let Some(v) = c
            .resp_cache
            .get("admin_overview", POLLED_RESPONSE_CACHE_TTL)
        {
            return Ok(Json(v));
        }
    }
    let (mut reqs, mut blocked) = c.counters();
    let fstats = c.fluid.stats();
    let instances: usize = fstats.iter().map(|f| f.instances).sum();
    let nodes = c.registry.nodes();
    let dbs = c.databases.list(None);
    let live_dbs = dbs.iter().filter(|d| d.mode == "live").count();
    let series = c.metrics.series(
        crate::metrics::Granularity::Minute,
        30,
        now_ms(),
        None,
        None,
    );
    let mut req30: u64 = series.iter().map(|b| b.requests).sum();
    let mut err30: u64 = series.iter().map(|b| b.errors).sum();
    if !local {
        for v in fan_out_peers_polled(
            &c,
            &all_healthy_peers(&c),
            "",
            "/v1/admin/overview?local=true",
        )
        .await
        {
            reqs += v.get("requests").and_then(|x| x.as_u64()).unwrap_or(0);
            blocked += v.get("blocked").and_then(|x| x.as_u64()).unwrap_or(0);
            req30 += v.get("req30").and_then(|x| x.as_u64()).unwrap_or(0);
            err30 += v.get("err30").and_then(|x| x.as_u64()).unwrap_or(0);
        }
    }
    let err_rate = if req30 == 0 {
        0.0
    } else {
        err30 as f64 / req30 as f64
    };
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
    let overview = json!({
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
        "req30": req30,
        "err30": err30,
        "incidents_open": c.incidents.open_count(),
        "cluster": c.cluster.status(nodes.iter().map(|n| n.id.clone()).collect()),
        "webhooks": c.webhooks.list(crate::webhooks::Scope::Operator, None).len(),
        "resources": {
            "cpu_cores": nodes.iter().map(|n| n.cpu_cores as u64).sum::<u64>(),
            "mem_total_mb": nodes.iter().map(|n| n.mem_total_mb).sum::<u64>(),
            "disk_total_gb": nodes.iter().map(|n| n.disk_total_gb).sum::<u64>(),
        },
    });
    if !local {
        c.resp_cache.set("admin_overview".into(), overview.clone());
    }
    Ok(Json(overview))
}

/// The durable, append-only audit log of every state mutation, newest first (operator only).
async fn admin_audit(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(json!(c.audit.recent(300, None))))
}

#[derive(Deserialize)]
struct IncidentsQ {
    /// `open` | `resolved` | `all` (default: all, the pre-paging shape).
    status: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Newest first. `x-hive-incidents-total` counts every row the filter admits
/// and `x-hive-incidents-open` every unresolved row, so a client pages
/// without a second call.
async fn incidents_list(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<IncidentsQ>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let filter = crate::incidents::Filter::parse(q.status.as_deref());
    let (rows, total, open) =
        c.incidents
            .page(filter, q.limit.unwrap_or(usize::MAX), q.offset.unwrap_or(0));
    let mut headers = HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(&total.to_string()) {
        headers.insert("x-hive-incidents-total", v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&open.to_string()) {
        headers.insert("x-hive-incidents-open", v);
    }
    Ok((headers, Json(json!(rows))).into_response())
}

/// The public `/status` board's feed: operator-declared incidents only — open
/// ones plus the 20 most recently resolved. Unauthenticated by design (the
/// page is public); automated incidents never appear here because their
/// messages carry node names, pids and command lines.
async fn status_incidents(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.incidents.public_board()))
}

/// Egress relay for Textbelt sends (see `push::send_sms`): runs the DIRECT send from THIS node so a
/// regional key is presented from an NA-classified IP. Reached via `put_to_host` (HTTP admin in
/// dev, gossip arm on the mesh), so it spends the platform's Textbelt key for whoever calls it:
/// [`require_mesh_rpc`] keeps every tenant token out.
async fn push_sms_relay(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(v): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // See `node_announce_public`: a fleet-wide RPC, `Some("mesh")` so a delegation minted for any
    // one tenant's own team can never reach it.
    require_mesh_rpc(&headers, claims.as_ref().map(|e| &e.0), Some("mesh"))?;
    Ok(Json(crate::push::sms_relay_exec(&c, v).await))
}

/// Direct-MX carrier-gateway SMS relay: run the direct-MX blast on THIS node
/// (chosen by the caller for its open outbound :25). Same node-to-node relay
/// shape and gate as `/v1/push/sms-relay`; the leader forwards here to an NA-egress peer.
async fn push_sms_direct_mx(
    State(_c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(v): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // See `node_announce_public`.
    require_mesh_rpc(&headers, claims.as_ref().map(|e| &e.0), Some("mesh"))?;
    Ok(Json(crate::push::sms_direct_mx_exec(v).await))
}

/// Leader-forward for mutations of leader-to-follower REGISTRY-synced stores (incidents, push): a
/// follower-side write returns 200, persists locally, then gets silently CLOBBERED by the next sync
/// cycle's wholesale snapshot adoption (an incident created on a follower vanished from it minutes
/// later). Mutations therefore run ONLY on the leader: a non-leader forwards the request there,
/// carrying the caller's own credentials so the leader re-verifies the same JWT against the
/// fleet-shared secret, and returns the leader's response, or fails LOUDLY with 502. It must never
/// fall back to the local write: accepting a write the sync loop will erase is strictly worse than
/// refusing it. With no other owner to forward to (a configured chain dark in this node's view, or
/// this node owns but may not serve: isolated) it answers a retryable 503, never a local apply.
///
/// Deliberately NO iroh-mesh fallback (unlike `put_to_host`): mesh delegation tokens are stripped
/// of `platform_admin` on verification (`gossip::team_claims`, so a compromised trusted peer cannot
/// forge operator authority fleet-wide; closing that needs per-node signing keys), so an
/// operator-gated mutation cannot ride the mesh under the current threat model. In practice the
/// dashboard's `/ops` proxy already talks to the leader directly, so the 502 only surfaces on
/// direct API calls against a follower whose HTTP admin map lacks the leader, which previously LOST
/// the write silently and now names the fix. Every credential and tenant-context header the leader
/// re-derives auth and tenant from is forwarded: Authorization (bearer), Cookie (the dashboard's
/// `hive_jwt` is a cookie, not a bearer, and dropping it 401'd cookie-auth mutations) and
/// `x-hive-team` (the tenant selector `tenant()` reads in unenforced mode; dropping it silently
/// mis-stored the row under "personal").
async fn forward_mutation_to_leader(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    method: reqwest::Method,
    path: &str,
    body: &Value,
) -> Result<Json<Value>, (StatusCode, String)> {
    let Some(leader) = c.leader_forward_target() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no serving control-plane leader is resolvable from this node right now — retry shortly"
                .into(),
        ));
    };
    let admin = c.node_admins.read().get(&leader).cloned();
    let Some(admin) = admin else {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("this mutation must run on the control-plane leader '{leader}', whose admin URL this node does not know — retry via the leader"),
        ));
    };
    let mut req = c
        .http
        .request(method, format!("{admin}{path}"))
        .timeout(std::time::Duration::from_secs(15));
    if !body.is_null() {
        req = req.json(body);
    }
    for h in [
        axum::http::header::AUTHORIZATION,
        axum::http::header::COOKIE,
    ] {
        if let Some(v) = headers.get(&h) {
            req = req.header(h, v.clone());
        }
    }
    if let Some(v) = headers.get("x-hive-team") {
        req = req.header("x-hive-team", v.clone());
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {
            let v = r.json::<Value>().await.map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("leader '{leader}' returned an unreadable incident response: {e}"),
                )
            })?;
            Ok(Json(v))
        }
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let detail = r.text().await.unwrap_or_default();
            Err((
                status,
                if detail.is_empty() {
                    format!("leader '{leader}' rejected the incident mutation")
                } else {
                    detail
                },
            ))
        }
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            format!("incident mutation forward to leader '{leader}' failed: {e}"),
        )),
    }
}

async fn incident_open(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<crate::incidents::OpenReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::POST,
            "/v1/incidents",
            &json!(req),
        )
        .await;
    }
    let inc = c.incidents.open_new(req);
    crate::persist::persist(&c);
    crate::webhooks::dispatch_platform(
        &c.webhooks,
        "incident.opened",
        json!({ "id": inc.id, "title": inc.title, "severity": inc.severity }),
    );
    Ok(Json(json!(inc)))
}

async fn incident_update(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
    Json(req): Json<crate::incidents::UpdateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::POST,
            &format!("/v1/incidents/{id}/updates"),
            &json!(req),
        )
        .await;
    }
    let resolved = matches!(req.status, crate::incidents::IncidentStatus::Resolved);
    let inc = c
        .incidents
        .update(&id, req)
        .ok_or((StatusCode::NOT_FOUND, "no such incident".into()))?;
    crate::persist::persist(&c);
    if resolved {
        crate::webhooks::dispatch_platform(
            &c.webhooks,
            "incident.resolved",
            json!({ "id": inc.id, "title": inc.title }),
        );
    }
    Ok(Json(json!(inc)))
}

/// Permanently remove an incident (vs. resolving it, which keeps it in the
/// history). The leader-authored delete propagates to every node via the
/// store-sync follower loop's wholesale snapshot adoption.
async fn incident_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::DELETE,
            &format!("/v1/incidents/{id}"),
            &Value::Null,
        )
        .await;
    }
    let inc = c
        .incidents
        .remove(&id)
        .ok_or((StatusCode::NOT_FOUND, "no such incident".into()))?;
    crate::persist::persist(&c);
    crate::webhooks::dispatch_platform(
        &c.webhooks,
        "incident.deleted",
        json!({ "id": inc.id, "title": inc.title }),
    );
    Ok(Json(json!({ "ok": true, "deleted": inc.id })))
}

/// Caller's user id from verified claims (`sub` = Clerk user id via the mint);
/// "local" in unenforced dev mode. Push/SMS rows are keyed by this so
/// `push_settings` can never return a teammate's devices or phone number.
fn push_user_id(claims: Option<&crate::auth::Claims>) -> String {
    claims
        .map(|cl| cl.sub.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// Public VAPID key for `pushManager.subscribe`; it requires a signed-in session (the key itself is
/// public by design, the gate just keeps the surface consistent with every other dashboard read).
/// The key is generated ONLY on the leader (a follower minting its own key forks the fleet keypair
/// and strands every subscription made against the other key), and a follower before the leader's
/// key has synced in answers a transient, retryable 503.
async fn push_vapid_key(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    crate::push::ensure_vapid_on_leader(&c);
    let keys = c.push.vapid();
    if keys.public_b64.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "push keys initializing; retry shortly".into(),
        ));
    }
    Ok(Json(json!({ "key": keys.public_b64 })))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PushSubscribeBody {
    endpoint: String,
    p256dh: String,
    auth: String,
    #[serde(default)]
    label: String,
}

/// Registers (or re-registers) this browser's push subscription under the caller's VERIFIED tenant:
/// the tenant comes from `tenant()` (JWT claim first, never a bare spoofable header under
/// enforcement), which is the entire cross-tenant isolation story for delivery, since the
/// dispatcher only fans a tenant's notifications to rows stored under that exact tenant. The
/// endpoint host must be a recognized push service (so our VAPID-signed server-side POST cannot be
/// aimed at an arbitrary attacker URL) and the subscriber keys well-formed (garbage keys create
/// rows that fail encryption on every tick forever); both are validated BEFORE forwarding to the
/// leader.
async fn push_subscribe(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<PushSubscribeBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Err(e) = crate::push::valid_subscription_input(&b.endpoint, &b.p256dh, &b.auth) {
        return Err((StatusCode::BAD_REQUEST, e.into()));
    }
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::POST,
            "/v1/push/subscribe",
            &json!(b),
        )
        .await;
    }
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let user = push_user_id(claims.as_ref().map(|e| &e.0));
    match c.push.upsert_subscription(crate::push::PushSubscription {
        endpoint: b.endpoint,
        p256dh: b.p256dh,
        auth: b.auth,
        tenant: team,
        user_id: user,
        label: b.label.chars().take(64).collect(),
        created_ms: hive_core::now_ms(),
    }) {
        crate::push::SubscribeResult::Ok => {
            crate::persist::persist(&c);
            Ok(Json(json!({ "ok": true })))
        }
        crate::push::SubscribeResult::EndpointOwnedByOther => Err((
            StatusCode::CONFLICT,
            "this push endpoint is registered to a different account".into(),
        )),
        crate::push::SubscribeResult::CapReached => Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many registered devices for this account; remove one first".into(),
        )),
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PushUnsubscribeBody {
    endpoint: String,
}

async fn push_unsubscribe(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<PushUnsubscribeBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::DELETE,
            "/v1/push/subscribe",
            &json!(b),
        )
        .await;
    }
    let user = push_user_id(claims.as_ref().map(|e| &e.0));
    let removed = c.push.remove_subscription(&b.endpoint, Some(&user));
    if removed {
        crate::persist::persist(&c);
    }
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

/// The CALLER's own delivery config for the current tenant, never anyone else's (rows are filtered
/// by the verified user id AND tenant): registered devices, SMS state (`verified` gates delivery;
/// `sms_quota` is served from a short-TTL cache so this read never blocks on a live Textbelt round
/// trip) and the key state for the operator UI (which source is live and a masked tail, never the
/// key itself).
async fn push_settings(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let user = push_user_id(claims.as_ref().map(|e| &e.0));
    let devices: Vec<Value> = c
        .push
        .devices_for(&user, &team)
        .into_iter()
        .map(|d| json!({ "endpoint": d.endpoint, "label": d.label, "created_ms": d.created_ms }))
        .collect();
    let sms = c
        .push
        .sms_for(&user, &team)
        .map(|s| json!({ "phone": s.phone, "enabled": s.enabled, "verified": s.verified }));
    let quota = crate::push::sms_quota_cached(&c).await;
    let (key_source, key_masked) = match c.push.sms_key_override() {
        Some(k) => (
            "override",
            json!(format!("…{}", &k[k.len().saturating_sub(6)..])),
        ),
        None => match std::env::var("HIVE_TEXTBELT_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        {
            Some(k) => (
                "env",
                json!(format!("…{}", &k[k.len().saturating_sub(6)..])),
            ),
            None => ("none", Value::Null),
        },
    };
    Ok(Json(
        json!({ "devices": devices, "sms": sms, "sms_quota": quota,
        "sms_key_source": key_source, "sms_key": key_masked }),
    ))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PushSmsBody {
    phone: String,
    enabled: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PushSmsKeyBody {
    /// The Textbelt API key to use platform-wide; empty string CLEARS the
    /// override (falls back to the HIVE_TEXTBELT_KEY env).
    key: String,
}

/// Operator-set Textbelt key (see `PushState::sms_key_override`): refilling Textbelt is a purchase
/// that funds a specific key, and per-node env surgery to activate it left SMS dead even after
/// payment. Platform-admin gated (it changes billing-bearing behavior for the whole platform) and
/// leader-forwarded so the store-sync registry replicates it fleet-wide (including the NA-egress
/// relay peers) instead of a follower write being clobbered. It responds with the masked state plus
/// the NEW key's live quota, so the dashboard confirms the paste worked in one round trip.
async fn push_sms_key_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<PushSmsKeyBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if crate::auth::enforced() && !claims.as_ref().map(|e| e.0.platform_admin).unwrap_or(false) {
        return Err((StatusCode::FORBIDDEN, "platform operator only".into()));
    }
    let key = b.key.trim().to_string();
    if key.len() > 128 || key.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err((StatusCode::BAD_REQUEST, "malformed key".into()));
    }
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::POST,
            "/v1/push/sms-key",
            &json!(PushSmsKeyBody { key }),
        )
        .await;
    }
    c.push.set_sms_key_override(&key);
    crate::push::reset_sms_quota_cache();
    crate::persist::persist(&c);
    let quota = crate::push::sms_quota_cached(&c).await;
    let masked = if key.is_empty() {
        Value::Null
    } else {
        json!(format!("…{}", &key[key.len().saturating_sub(6)..]))
    };
    Ok(Json(
        json!({ "ok": true, "sms_key": masked, "sms_key_source": if key.is_empty() { "env" } else { "override" }, "sms_quota": quota }),
    ))
}

/// Deterministic 6-digit code from cryptographic bytes (no PRNG dep needed).
fn sms_verification_code() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 4];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    format!("{:06}", u32::from_be_bytes(b) % 1_000_000)
}

/// Sets (or changes) the SMS number. This does NOT enable delivery: it texts a verification code to
/// the number and stores it PENDING. Delivery starts only after `push_sms_verify` confirms the
/// code, so a caller can never point notifications at a phone number they do not control
/// (anti-SMS-bombing). Toggling an already-verified, unchanged number on/off skips re-verification
/// and sends no text, and a send within the resend cooldown spends no further SMS.
async fn push_sms_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<PushSmsBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let phone = b.phone.trim().to_string();
    let digits_ok = phone
        .strip_prefix('+')
        .is_some_and(|d| (8..=15).contains(&d.len()) && d.chars().all(|c| c.is_ascii_digit()));
    if !digits_ok {
        return Err((
            StatusCode::BAD_REQUEST,
            "phone must be E.164 (e.g. +15551234567)".into(),
        ));
    }
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::PUT,
            "/v1/push/sms",
            &json!({ "phone": phone, "enabled": b.enabled }),
        )
        .await;
    }
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let user = push_user_id(claims.as_ref().map(|e| &e.0));

    if let Some(existing) = c.push.sms_for(&user, &team) {
        if existing.verified && existing.phone == phone {
            c.push.set_sms_enabled(&user, &team, b.enabled);
            crate::persist::persist(&c);
            return Ok(Json(
                json!({ "ok": true, "verified": true, "code_sent": false }),
            ));
        }
    }

    let code = sms_verification_code();
    let sent = c
        .push
        .set_sms_pending(&user, &team, &phone, &code, hive_core::now_ms());
    if !sent {
        crate::persist::persist(&c);
        return Ok(Json(
            json!({ "ok": true, "verified": false, "code_sent": false, "note": "a code was already sent recently; check your messages or wait a minute" }),
        ));
    }
    let msg =
        format!("[shadw] Your verification code is {code}. Enter it to enable SMS notifications.");
    match crate::push::send_sms(&c, &phone, &msg, false).await {
        Ok(()) => {
            crate::persist::persist(&c);
            Ok(Json(
                json!({ "ok": true, "verified": false, "code_sent": true }),
            ))
        }
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            format!("could not send verification SMS: {e}"),
        )),
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PushSmsVerifyBody {
    code: String,
}

/// Confirm ownership of the pending SMS number by entering the texted code.
async fn push_sms_verify(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<PushSmsVerifyBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !c.is_control_plane_leader() {
        return forward_mutation_to_leader(
            &c,
            &headers,
            reqwest::Method::POST,
            "/v1/push/sms/verify",
            &json!({ "code": b.code }),
        )
        .await;
    }
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let user = push_user_id(claims.as_ref().map(|e| &e.0));
    if c.push
        .verify_sms(&user, &team, b.code.trim(), hive_core::now_ms())
    {
        crate::persist::persist(&c);
        Ok(Json(json!({ "ok": true, "verified": true })))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "invalid or expired verification code".into(),
        ))
    }
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct PushTestBody {
    /// Route the SMS leg through Textbelt's `_test` key variant (validates the
    /// whole request path without sending or consuming quota) — used by
    /// automated verification; the settings-page button sends for real.
    #[serde(default)]
    sms_test: bool,
}

/// Per-user rate limit for the test endpoint: it triggers real FCM sends and
/// (for a verified number) a real SMS, so a signed-in user must not be able to
/// spam it. One test per user per 30s.
static PUSH_TEST_LAST: std::sync::LazyLock<
    parking_lot::RwLock<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
const PUSH_TEST_COOLDOWN_MS: u64 = 30_000;

/// Sends a real test notification through the REAL delivery pipeline to the caller's own registered
/// devices/SMS for the current tenant. Deliberately NOT leader-gated: it mutates nothing in the
/// store apart from pruning, and running it on any node is exactly what makes dev/non-leader
/// verification possible. It is rate-limited per user, and the SMS leg only ever reaches the
/// caller's OWN verified and enabled number. A 404/410 from a device means the subscription is
/// permanently gone (browser profile closed or unsubscribed): it is pruned rather than reported as
/// a scary failure, so the next test is clean, exactly like the background dispatcher does, durably
/// (locally on the leader, else through the leader-forwarded DELETE, since the PushStore is
/// REGISTRY-synced and a follower-local remove would be clobbered by the next sync).
async fn push_test(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    body: Option<Json<PushTestBody>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let user = push_user_id(claims.as_ref().map(|e| &e.0));
    let opts = body.map(|Json(b)| b).unwrap_or_default();

    let rl_key = format!("{user}|{team}");
    {
        let now = hive_core::now_ms();
        let mut g = PUSH_TEST_LAST.write();
        if let Some(last) = g.get(&rl_key) {
            if now.saturating_sub(*last) < PUSH_TEST_COOLDOWN_MS {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "please wait a moment before sending another test".into(),
                ));
            }
        }
        g.insert(rl_key, now);
    }

    let test_notif = crate::notifications::Notification {
        id: format!("test-{}", hive_core::now_ms()),
        severity: "info".into(),
        category: "test".into(),
        project: team.clone(),
        environment: String::new(),
        message: "Test notification — push delivery is working.".into(),
        ts_ms: hive_core::now_ms(),
        read: false,
        archived: false,
    };
    let payload = crate::push::push_payload(&test_notif);

    let devices = c.push.devices_for(&user, &team);
    let mut sent = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut dead: Vec<String> = Vec::new();
    for d in &devices {
        match crate::push::send_web_push(&c, d, &payload).await {
            Ok(true) => sent += 1,
            Ok(false) => dead.push(d.endpoint.clone()),
            Err(e) => errors.push(format!("{}: {e}", d.label)),
        }
    }
    for endpoint in &dead {
        if c.is_control_plane_leader() {
            c.push.remove_subscription(endpoint, None);
        } else {
            let _ = forward_mutation_to_leader(
                &c,
                &headers,
                reqwest::Method::DELETE,
                "/v1/push/subscribe",
                &json!({ "endpoint": endpoint }),
            )
            .await;
        }
    }
    if !dead.is_empty() && c.is_control_plane_leader() {
        crate::persist::persist(&c);
    }
    let pruned = dead.len();

    let sms_result = match c.push.sms_for(&user, &team) {
        Some(t) if t.enabled && t.verified => match crate::push::send_sms(
            &c,
            &t.phone,
            &crate::push::sms_body(&test_notif),
            opts.sms_test,
        )
        .await
        {
            Ok(()) => json!({ "attempted": true, "ok": true, "test_mode": opts.sms_test }),
            Err(e) => json!({ "attempted": true, "ok": false, "error": e }),
        },
        _ => json!({ "attempted": false, "ok": false }),
    };

    Ok(Json(json!({
        "web_push": { "sent": sent, "failed": errors.len(), "errors": errors, "devices": devices.len(), "pruned": pruned },
        "sms": sms_result,
    })))
}

/// Computes the live notification list for a team from real platform signals (failed deploys and
/// builds, 5xx error anomalies, blocked-traffic usage anomalies), applying the user's read/archived
/// state; archived items keep their `archived` flag so the client can render an Archive tab. A
/// failed deploy or build is live only while it is still the most recent thing that happened to its
/// project: the newest event timestamp per project is taken across BOTH deployments and builds, and
/// a later deployment or build (for example a successful redeploy) makes the failure moot, where
/// without that check a failure from weeks ago kept notifying forever. Deployment ownership uses
/// the authoritative record tag (present on the local record, unlike the node-local project row,
/// which is UNTAGGED on a non-hosting node), while builds and edge events (which carry no tenant
/// tag) are judged through the fleet-aware `project_owned_by` predicate, so a member's alerts are
/// not dropped on a node whose project row is absent.
pub(crate) fn build_notifications(
    c: &Arc<CloudState>,
    team: &str,
) -> Vec<crate::notifications::Notification> {
    use crate::notifications::Notification;
    use std::collections::HashMap;
    let team = norm(team).to_string();
    let mut out: Vec<Notification> = Vec::new();

    let mut newest_per_project: HashMap<String, u64> = HashMap::new();
    for d in c.gw.list() {
        if record_tenant(&d.tenant) != team {
            continue;
        }
        let e = newest_per_project.entry(d.project.clone()).or_insert(0);
        *e = (*e).max(d.created_at_ms);
    }
    for b in c.builds.list() {
        if !project_owned_by(&c, &b.project, &team) {
            continue;
        }
        let e = newest_per_project.entry(b.project.clone()).or_insert(0);
        *e = (*e).max(b.started_ms);
    }

    for d in c.gw.list() {
        if record_tenant(&d.tenant) != team {
            continue;
        }
        if newest_per_project
            .get(&d.project)
            .is_some_and(|&t| t > d.created_at_ms)
        {
            continue;
        }
        if d.state == fluid_core::DeployState::Error {
            let env = if d.production {
                "Production"
            } else {
                "Preview"
            };
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

    for b in c.builds.list() {
        if !project_owned_by(&c, &b.project, &team) {
            continue;
        }
        if newest_per_project
            .get(&b.project)
            .is_some_and(|&t| t > b.started_ms)
        {
            continue;
        }
        if b.state == fluid_core::DeployState::Error {
            out.push(Notification {
                id: format!("build-{}", b.id),
                severity: "warning".into(),
                category: "deploy".into(),
                project: b.project.clone(),
                environment: "Production".into(),
                message: format!(
                    "{} failed to deploy in the Production environment",
                    b.project
                ),
                ts_ms: b.started_ms,
                read: false,
                archived: false,
            });
        }
    }

    let mut err5xx: HashMap<String, u64> = HashMap::new();
    let mut blocked: HashMap<String, u64> = HashMap::new();
    for ev in c.recent_events(300) {
        if ev.project.is_empty() {
            continue;
        }
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

/// The tenant's notification list (the inbox bell). Every ITEM is derived from node-local sources
/// (`gw.list`, `builds.list`, `recent_events`) even though the read/archived flags are replicated,
/// so a deploy that fails on the node hosting it produced no notification on the others and the
/// bell's poll made the badge appear and vanish depending on which node answered (a failed
/// production deploy could go unseen). It therefore merges the peers' lists by the
/// already-deterministic notification id; `local=true` (the inner fan-out call) answers from this
/// node only.
async fn notifications_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Query(q): axum::extract::Query<LocalQ>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let items = build_notifications(&c, &team);
    if q.local == Some(true) {
        return Json(json!({ "unread": 0, "inbox": 0, "items": items }));
    }
    let mut merged: Vec<Value> = items.iter().map(|n| json!(n)).collect();
    let peers = peer_nodes_for_tenant(&c, &team);
    for v in fan_out_peers_polled(&c, &peers, &team, "/v1/notifications?local=true").await {
        if let Some(arr) = v.get("items").and_then(|x| x.as_array()) {
            merged.extend(arr.iter().cloned());
        }
    }
    let mut seen = std::collections::HashSet::new();
    merged.retain(|v| {
        seen.insert(
            v.get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        )
    });
    let ts = |v: &Value| v.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0);
    merged.sort_by(|a, b| ts(b).cmp(&ts(a)));
    let flag = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let inbox = merged.iter().filter(|n| !flag(n, "archived")).count();
    let unread = merged
        .iter()
        .filter(|n| !flag(n, "archived") && !flag(n, "read"))
        .count();
    Json(json!({ "unread": unread, "inbox": inbox, "items": merged }))
}

/// Archives one notification, only if the id belongs to the caller's own team's feed: the ids are
/// predictable (`deploy-<id>`, `anom-*`), so an unchecked archive would let one team hide another
/// team's alert by guessing the format.
async fn notification_archive(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !build_notifications(&c, &team).iter().any(|n| n.id == id) {
        return Err((StatusCode::NOT_FOUND, "no such notification".into()));
    }
    c.notifications.archive(&id);
    Ok(Json(json!({ "archived": id })))
}

async fn notifications_archive_all(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ids: Vec<String> = build_notifications(&c, &team)
        .into_iter()
        .filter(|n| !n.archived)
        .map(|n| n.id)
        .collect();
    c.notifications.archive_all(&ids);
    Json(json!({ "archived": ids.len() }))
}

async fn notifications_read(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let ids: Vec<String> = build_notifications(&c, &team)
        .into_iter()
        .map(|n| n.id)
        .collect();
    c.notifications.mark_read(&ids);
    Json(json!({ "read": ids.len() }))
}

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

/// Pure decision core of `identity_sync` (no request/state deps, unit-testable): given the caller's
/// OWN verified claims (if any), the enforcement mode, the org they are claiming (if any) and their
/// raw user id, resolves `(tenant, org_slug_to_index, is_owner)`. `body_email_is_owner` is ONLY
/// consulted in the unenforced-dev fallback (`auth_is_owner: None, enforced: false`) and must never
/// be trusted when `enforced` is true. An org claimed in the request is honoured only if it matches
/// the caller's own verified tenant (already Clerk-membership-checked at JWT-mint time in
/// `/api/token`): a caller authenticated as a DIFFERENT tenant never indexes, or reports, an org
/// they are not verified to belong to, and falls back to their own real tenant. Personal scope: the
/// OWNER keeps the legacy "personal" namespace and every other verified caller is isolated under
/// their own `u_<uid>`, never the shared literal "personal".
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
                (
                    auth_tenant
                        .map(str::to_string)
                        .unwrap_or_else(|| ANON_TENANT.to_string()),
                    None,
                )
            } else {
                (slug.to_string(), Some(slug.to_string()))
            }
        }
        None => match auth_is_owner {
            Some(true) => ("personal".to_string(), None),
            Some(false) => (format!("u_{user_id}"), None),
            None if !enforced => ("personal".to_string(), None),
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

/// The dashboard syncs the signed-in user and active org from the identity provider (Clerk); we
/// index them into the store, scoped to the active tenant namespace. `is_owner` and the indexed
/// tenant come from the caller's OWN verified JWT claims (`platform_admin`/`tenant`) when auth is
/// enforced, NEVER from the request body's `user.email`/`org` (see [`resolve_identity_sync`]): this
/// route requires only SOME authenticated caller, so trusting the body let any authenticated user,
/// even from an unprivileged tenant, POST the owner's email and get back `is_owner: true`, which
/// the dashboard client then persisted (`hive_is_owner`) and used to route every later request to
/// the owner's "personal" tenant. Unenforced (dev/local, no `HIVE_JWT_SECRET`) mode keeps the
/// body-trusting behaviour: there is no tenant boundary to protect there.
///
/// Verified org membership is mirrored into the teams roster (`team_create` seeds only the platform
/// owner, so Clerk org members were otherwise invisible to `GET /v1/teams`). `org_slug` is only
/// Some when the caller's verified JWT tenant equals the claimed org, and the mirror is gated on
/// `x-hive-internal` so the email is exactly the Clerk-verified one the server-side mint route
/// posts: a browser-originated sync cannot write an arbitrary body email into its org's roster.
/// Members are added if absent only (`add_member` overwrites an existing member's role, and a
/// manually granted Owner/Admin must never be demoted by a routine sync).
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
    let claimed_org_slug = req.org.as_ref().map(|o| {
        if o.slug.is_empty() {
            o.id.clone()
        } else {
            o.slug.clone()
        }
    });
    let body_email_is_owner = !c.owner_email.trim().is_empty()
        && req.user.email.eq_ignore_ascii_case(c.owner_email.trim());

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
            let already = c
                .teams
                .get(slug)
                .map(|t| t.member(email).is_some())
                .unwrap_or(false);
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

/// The single node currently metering usage into `BillingStore`: the SAME designation
/// `spawn_billing_meter_loop`'s gate uses (`leadership::job_owner(BillingMeter)`: the strict chain
/// owner, or on a chain-less mesh the `HIVE_BILLING_COORDINATOR_NODE` pin or the election). Every
/// OTHER node's local `BillingStore` is stale or empty for live reads (only bootstrapped from a
/// peer snapshot at boot, never kept live-current). `None` while no owner is resolvable (the caller
/// serves its local copy).
fn billing_authority_node(c: &Arc<CloudState>) -> Option<String> {
    crate::leadership::job_owner(c, crate::leadership::Job::BillingMeter)
}

/// Default `billing_proxy_budget()`: 3 s.
const BILLING_PROXY_BUDGET_DEFAULT: Duration = Duration::from_millis(3000);

/// Budget for the proxy-to-authority hop on a USER-FACING billing read. `fetch_from_host`'s own
/// budgets (10 s HTTP admin, then 20 s iroh) are DIAL-RESILIENCE budgets, right for a one-shot
/// control operation and wrong on a request path (a follower's `/v1/billing/ledger` once spent ~20
/// s inside them). A healthy hop is one mesh round trip (100-200 ms measured), so 3 s is more than
/// 10x headroom; a hop that misses it falls through to this node's own in-process billing state,
/// the cheaper correct answer at that point, not an error. `HIVE_BILLING_PROXY_READ_MS` overrides.
fn billing_proxy_budget() -> Duration {
    static T: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("HIVE_BILLING_PROXY_READ_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(BILLING_PROXY_BUDGET_DEFAULT)
    })
}

/// Proxies a billing GET to the authority node when this node is not it, so `/v1/billing` answers
/// no longer diverge by node (5 distinct billing states, including a `plan` disagreement, were once
/// witnessed across 8 nodes for the SAME tenant). It falls back to this node's own (possibly stale)
/// local value if the proxy is unreachable rather than erroring the page, and `None` means this
/// node IS the authority (the caller serves its own local read). Every caller is bounded by
/// `billing_proxy_budget()`: these are user-facing reads, and the authority's own answer (the
/// terminal hop of this proxy, see `gossip::dispatch`'s `/v1/billing/*` arms) is itself bounded, so
/// a hop that misses the budget means the mesh is sick and this node's local state is the right
/// thing to serve rather than the reason to wait longer.
async fn proxy_billing_read(c: &Arc<CloudState>, path: &str, team: &str) -> Option<Value> {
    let authority = billing_authority_node(c)?;
    if authority == c.node_name {
        return None;
    }
    match tokio::time::timeout(
        billing_proxy_budget(),
        fetch_from_host(c, &authority, path, team),
    )
    .await
    {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(
                path,
                budget_ms = billing_proxy_budget().as_millis() as u64,
                authority = %authority,
                "billing: proxy-to-authority missed its budget; serving this node's local billing state"
            );
            None
        }
    }
}

/// The tenant's billing view: plan, live quota usage (the business-locking gauges), the metered
/// rate card and this period's draft invoice, so the UI shows limits, usage and invoicing in one
/// place. It is proxied to the metering authority when this node is not it (see
/// [`proxy_billing_read`]).
pub(crate) async fn billing_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(v) = proxy_billing_read(&c, "/v1/billing", &t).await {
        return Json(v);
    }
    let acc = c.billing.account(&t);
    let plan = acc.plan.clone();
    let projects_used = c.projects.count_for_team(&t) as u32;
    let members_used = c
        .teams
        .get(&t)
        .map(|team| team.members.len() as u32)
        .unwrap_or(0);
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

/// Invoices for the tenant (finalized periods plus the current draft, newest first). The read order
/// is deliberate: (1) the LOCAL fleet-replicated relational mirror, bounded by
/// `relational::billing_read_budget()`: no network hop, and the only source of finalized invoice
/// history that exists on EVERY node; (2) the HTTP/mesh proxy to the billing authority, bounded by
/// `billing_proxy_budget()`; (3) this node's own in-process `BillingStore` (instant, never fails).
///
/// Putting (2) before (3) is load-bearing, not a latency preference: `BillingStore`'s invoices are
/// NOT part of the `store_sync` `billing` snapshot (accounts and ledger only, see
/// `store_sync::REGISTRY`), so a non-authority node's local `invoices` map holds finalized invoices
/// for nobody, and serving it before the authority hop would replace a tenant's real invoice
/// history with a lone draft for the whole duration of any mirror outage. The authority's own store
/// DOES hold it (and its mirror read is bounded by the same budget, so the hop terminates there
/// instead of hanging). The worst case is therefore bounded: budget + budget + instant, instead of
/// the live `SQL_OP_TIMEOUT` (10 s) plus an unbounded proxy hop.
pub(crate) async fn billing_invoices(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(v) = billing_invoices_from_mirror(&c, &t).await {
        return Json(v);
    }
    if let Some(v) = proxy_billing_read(&c, "/v1/billing/invoices", &t).await {
        return Json(v);
    }
    Json(json!(c.billing.invoices(&t)))
}

/// The mirror half of `billing_invoices`, or `None` when the mirror cannot answer for this tenant
/// (not mirrored here, or the read missed its budget): the caller falls through to the authority,
/// never to a fabricated empty list. It is equivalent to `BillingStore::invoices` (the last-resort
/// answer in `billing_invoices`), which returns the stored FINALIZED invoices plus
/// `current_invoice` (the in-progress period's draft) sorted newest-first by `period_start_ms`: the
/// mirror only ever holds finalized invoices (drafts are never persisted, see
/// `relational::upsert_billing`), so the draft is appended here and the array re-sorted on the same
/// key. No double-count is possible: `billing::build_invoice` mints a fresh random id for every
/// draft, so the appended one can never be an invoice the mirror already holds.
async fn billing_invoices_from_mirror(c: &Arc<CloudState>, t: &str) -> Option<Value> {
    let invoices_json = match crate::relational::billing_snapshot(t).await {
        crate::relational::BillingSnapshot::Mirrored {
            invoices: Some(j), ..
        } => j,
        _ => return None,
    };
    let Ok(Value::Array(mut arr)) = serde_json::from_str::<Value>(&invoices_json) else {
        return None;
    };
    arr.push(json!(c.billing.current_invoice(t)));
    arr.sort_by(|a, b| {
        let pa = a
            .get("period_start_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let pb = b
            .get("period_start_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        pb.cmp(&pa)
    });
    Some(Value::Array(arr))
}

/// Ledger entries for the tenant (newest first) — same bounded read order as
/// `billing_invoices` (mirror, then authority, then this node's store); see
/// that function's doc for why the authority hop precedes the local store.
pub(crate) async fn billing_ledger(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let crate::relational::BillingSnapshot::Mirrored {
        ledger: Some(ledger_json),
        ..
    } = crate::relational::billing_snapshot(&t).await
    {
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
    /// "plan" | "credits" | "addon"
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    plan: Option<String>,
    /// For credit top-ups, the amount in cents.
    #[serde(default)]
    amount_cents: Option<u64>,
    /// For `kind == "addon"`: which addon (only `tencent_eip::SKU` is real today).
    #[serde(default)]
    sku: Option<String>,
    /// For `kind == "addon"`: the target resource id (a project name, for
    /// "dedicated_ipv4") — must be a project the caller's tenant owns.
    #[serde(default)]
    target: Option<String>,
}
fn default_kind() -> String {
    "plan".into()
}

/// Which purchasable addons this fleet can actually deliver and, when it cannot, WHY: the dashboard
/// renders a disabled control with the operator's remedy instead of a Buy button that answers with
/// an HTTP error only after the user clicks it. Tenant-authed (same read bar as billing).
///
/// GETs are served node-local (see the `admin_ingress` dispatch doc in `main.rs`), which is correct
/// for gossip-replicated tenant state and wrong here: Tencent credentials are NODE-LOCAL
/// infrastructure config, so different nodes give genuinely different (not eventually-consistent)
/// answers, and a tenant landing on a non-Tencent or not-yet-configured node saw "not available"
/// even on a fleet where the purchase path (leader-forwarded, always a credentialed node per
/// `HIVE_CP_OWNER_CHAIN`) would have succeeded. On a local preflight failure it therefore asks the
/// leader's own view before answering "unavailable" (the leader is always Tencent-credentialed by
/// the same invariant `provision_from_checkout` relies on), failing OPEN toward the local answer if
/// the leader cannot be reached: it never hangs the endpoint.
async fn billing_addons(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read_or_internal(&headers, claims.as_ref().map(|e| &e.0))
        .map_err(|e| (e.0, e.1))?;
    let mut result = crate::tencent_eip::preflight();
    if result.is_err() && !c.is_control_plane_leader() {
        if let Some(leader_result) = crate::tencent_eip::leader_preflight(&c).await {
            result = leader_result;
        }
    }
    let (available, reason) = match result {
        Ok(()) => (true, String::new()),
        Err(why) => (
            false,
            format!(
                "{why} — an operator must configure the Tencent API credentials, EIP region and \
                 this node's CVM instance id before dedicated addresses can be sold."
            ),
        ),
    };
    Ok(Json(json!({
        "addons": [{
            "sku": crate::tencent_eip::SKU,
            "name": "Dedicated IPv4",
            "price_cents": crate::tencent_eip::PRICE_CENTS,
            "available": available,
            "unavailable_reason": reason,
        }]
    })))
}

/// Opens a checkout for a plan, credits or an addon (`req.kind`). Errors carry their REASON: every
/// arm used to return a bare StatusCode, so the dashboard's Buy button could only render "POST
/// /v1/billing/checkout -> 502", indistinguishable from a platform bug when the cause was usually
/// "this fleet has no Tencent credentials configured"; `apiSend` surfaces the response body
/// verbatim, so a sentence here is a sentence in the UI. An addon is PREFLIGHTED before any
/// checkout record exists (discovering missing config after confirming a checkout left a confirmed
/// purchase with no address and told the buyer nothing), and because it is scoped to a project
/// (e.g. dedicated_ipv4) Stripe lands the buyer back on that project's network settings rather than
/// the unrelated billing overview.
///
/// A free plan (Hobby, or any future $0 tier) has nothing to charge (Stripe Checkout rejects a $0
/// payment or subscription outright), so it is applied immediately, as the mock path always did.
/// The same $0 shortcut for an addon (currently `tencent_eip::PRICE_CENTS == 0`, an operator-set
/// testing price, never assumed permanent) goes through the SAME `open_checkout_full` ->
/// `confirm_checkout` -> `provision_from_checkout` sequence the paid path uses, just without the
/// Stripe session a $0 amount could never produce; `provision_from_checkout` is idempotent on
/// `ProjectSettings::dedicated_ipv4`, so hitting it more than once for the same project is safe.
/// Otherwise it opens a real Stripe Checkout when configured and the local mock checkout when not.
async fn billing_checkout(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<CheckoutReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let mut target = String::new();
    let (plan, amount, label, price_id): (String, u64, String, Option<String>) =
        match req.kind.as_str() {
            "credits" => {
                let amt = req.amount_cents.unwrap_or(1000);
                (
                    "".to_string(),
                    amt,
                    format!("OpenEdge credits (${:.2})", amt as f64 / 100.0),
                    None,
                )
            }
            "addon" => {
                if req.sku.as_deref() != Some(crate::tencent_eip::SKU) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!(
                            "unknown addon sku {:?} — the only addon today is {:?}",
                            req.sku.as_deref().unwrap_or(""),
                            crate::tencent_eip::SKU
                        ),
                    ));
                }
                target = req.target.clone().unwrap_or_default();
                if target.is_empty() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "this addon is purchased for a specific project, but no project was named"
                            .into(),
                    ));
                }
                if !project_owned_by(&c, &target, norm(&t)) {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!("project {target:?} is not owned by your team"),
                    ));
                }
                if let Err(why) = crate::tencent_eip::preflight() {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!(
                            "Dedicated IPv4 is not available on this fleet yet: {why}. This is an \
                             operator setup step (Tencent API credentials, EIP region and the \
                             node's CVM instance id) — nothing was purchased or charged."
                        ),
                    ));
                }
                (
                    "".to_string(),
                    crate::tencent_eip::PRICE_CENTS,
                    format!("Dedicated IPv4 — {target}"),
                    crate::tencent_eip::price_id(),
                )
            }
            _ => {
                let plan = req.plan.unwrap_or_else(|| "pro".into());
                let spec = crate::billing::plan_spec(&plan);
                (
                    plan,
                    spec.price_cents,
                    format!("OpenEdge {} plan", spec.name),
                    spec.stripe_price_id.map(|s| s.to_string()),
                )
            }
        };
    if amount == 0 && req.kind != "addon" {
        apply_plan_everywhere(&c, &t, &plan);
        let acc = c.billing.account(&t);
        c.audit.record(
            &t,
            "user",
            "plan_change",
            "billing",
            &plan,
            "switched to a free plan (no checkout needed)",
        );
        crate::persist::persist(&c);
        return Ok(Json(
            json!({ "url": "", "mock": false, "applied": true, "account": acc }),
        ));
    }
    if amount == 0 && req.kind == "addon" {
        let co = c.billing.open_checkout_full(
            &t,
            &req.kind,
            &plan,
            amount,
            crate::tencent_eip::SKU,
            &target,
        );
        let (co, _acc) = c.billing.confirm_checkout(&co.id).ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "checkout could not be confirmed".to_string(),
        ))?;
        match crate::tencent_eip::provision_from_checkout(&c, &co).await {
            Ok(alloc) => {
                c.audit.record(
                    &t,
                    "user",
                    "addon_purchase",
                    "billing",
                    &co.sku,
                    &format!("dedicated_ipv4 for {target} (testing price, $0)"),
                );
                crate::persist::persist(&c);
                return Ok(Json(
                    json!({ "url": "", "mock": false, "applied": true, "dedicated_ipv4": alloc }),
                ));
            }
            Err(e) => {
                tracing::error!(checkout = %co.id, tenant = %t, error = %e, "dedicated_ipv4 provisioning failed ($0 testing path)");
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("Dedicated IPv4 provisioning failed: {e}"),
                ));
            }
        }
    }
    let sku = if req.kind == "addon" {
        crate::tencent_eip::SKU
    } else {
        ""
    };
    let co = c
        .billing
        .open_checkout_full(&t, &req.kind, &plan, amount, sku, &target);

    if crate::billing::stripe_configured() {
        let base = std::env::var("PUBLIC_URL").unwrap_or_else(|_| "http://localhost:3000".into());
        let (success, cancel) = if req.kind == "addon" && !target.is_empty() {
            let enc =
                percent_encoding::utf8_percent_encode(&target, percent_encoding::NON_ALPHANUMERIC)
                    .to_string();
            (
                format!(
                    "{base}/projects/{enc}/settings/network?addon_success={}",
                    co.id
                ),
                format!("{base}/projects/{enc}/settings/network?addon_canceled=1"),
            )
        } else {
            (
                format!("{base}/billing?success={}", co.id),
                format!("{base}/billing?canceled=1"),
            )
        };
        match crate::billing::stripe_checkout(
            &c.http,
            price_id.as_deref(),
            amount,
            &label,
            &success,
            &cancel,
            &co.id,
        )
        .await
        {
            Ok((url, stripe_session_id)) => {
                c.billing.attach_stripe_session(&co.id, &stripe_session_id);
                return Ok(Json(json!({ "url": url, "mock": false, "session": co.id })));
            }
            Err(e) => tracing::warn!(error=%e, "stripe checkout failed; falling back to mock"),
        }
    }
    Ok(Json(
        json!({ "url": format!("/billing/checkout?session={}", co.id), "mock": true, "session": co.id }),
    ))
}

/// Reads a checkout by id. Checkouts live in an in-process map on whichever node opened them, and
/// `POST /v1/billing/checkout` is a mutation, so it always runs on the billing authority, while the
/// browser's follow-up GET (`/billing/checkout?session=<id>`) round-robins: a local miss therefore
/// proxies to the authority like the sibling billing reads, or the upgrade flow would dead-end with
/// "Checkout session not found".
pub(crate) async fn billing_checkout_get(
    State(c): State<Arc<CloudState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    if let Some(co) = c.billing.get_checkout(&id) {
        return Ok(Json(json!(co)));
    }
    if let Some(v) = proxy_billing_read(&c, &format!("/v1/billing/checkout/{id}"), "").await {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct ConfirmReq {
    session: String,
}

/// Confirms a checkout and applies its effect. Only the SAME tenant that opened a checkout may
/// confirm it (otherwise any authenticated caller who learns or guesses another tenant's checkout
/// id could force-complete or grief it). For a REAL Stripe checkout the payment is verified with
/// Stripe before anything is applied: the client hitting this endpoint (the redirect back from
/// Stripe, or a direct call) is not proof of payment, and without the check anyone could open a
/// checkout and confirm it without ever paying. `confirm_checkout` only moves the billing half; the
/// effect of a completed "plan"/"addon" checkout is dispatched here, never inside
/// `confirm_checkout` itself (the same single-writer split as the "credits" branch). A provisioning
/// failure after payment is surfaced as `addon_error` rather than failing the request: the payment
/// already succeeded and the checkout is consumed, so an error status would read as "you weren't
/// charged"; the caller can retry or contact support.
async fn billing_confirm(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<ConfirmReq>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let co = c
        .billing
        .get_checkout(&req.session)
        .ok_or(StatusCode::NOT_FOUND)?;
    if norm(&co.tenant) != t {
        return Err(StatusCode::FORBIDDEN);
    }
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
    let (co, acc) = c
        .billing
        .confirm_checkout(&req.session)
        .ok_or(StatusCode::NOT_FOUND)?;
    let mut addon_error: Option<String> = None;
    match co.kind.as_str() {
        "credits" => {}
        "addon" => {
            if let Err(e) = crate::tencent_eip::provision_from_checkout(&c, &co).await {
                tracing::error!(checkout = %co.id, tenant = %co.tenant, error = %e, "dedicated_ipv4 provisioning failed after payment confirmed");
                addon_error = Some(e);
            }
        }
        _ => apply_plan_everywhere(&c, &co.tenant, &co.plan),
    }
    if !stripe_customer.is_empty() || !stripe_subscription.is_empty() {
        c.billing
            .set_stripe_ids(&co.tenant, &stripe_customer, &stripe_subscription);
    }
    c.audit.record(
        &t,
        "user",
        "charge",
        "billing",
        &co.id,
        &format!(
            "checkout {} {} ${:.2}",
            co.kind,
            co.plan,
            co.amount_cents as f64 / 100.0
        ),
    );
    crate::persist::persist(&c);
    Ok(Json(
        json!({ "ok": true, "account": acc, "addon_error": addon_error }),
    ))
}

/// Stripe's server-to-server notification of billing events: the authoritative sync path (unlike
/// `billing_confirm`, which only fires when a user's browser happens to be present for the redirect
/// back). It handles the checkout completing (in case the user closes the tab before that redirect)
/// and subscription cancellation/renewal, so a tenant's plan stays correct even when nobody is
/// watching.
///
/// Auth: this route is in `auth::require_auth`'s `open` allowlist (Stripe cannot present a platform
/// JWT; it is authenticated by its own HMAC signature, verified below), and it deliberately FAILS
/// CLOSED when `STRIPE_WEBHOOK_SECRET` is unset, unlike `git_webhook`'s dev-open default: an
/// unverifiable delivery must never be allowed to mutate a tenant's billing state.
///
/// `checkout.session.completed` is the primary confirmation path and mirrors `billing_confirm`'s
/// apply logic, keyed by the `checkout_id` stamped into the session's metadata (see
/// `stripe_checkout`) rather than a tenant header, since Stripe is the caller; a checkout already
/// confirmed through `billing_confirm` (the common case, the user's browser is usually still
/// present) is a no-op, because `confirm_checkout` is idempotent (it removes on first success).
/// `customer.subscription.deleted` (immediate or end-of-period cancellation, depending on Stripe
/// settings) downgrades to Hobby, so quotas and business-locks re-engage even if nobody visits the
/// dashboard again. `customer.subscription.updated` only logs: a renewal failure (card declined
/// etc.) surfaces as the subscription moving to past_due/unpaid, and there is no automatic
/// downgrade on a transient failure (Stripe's own retry schedule and dunning emails run first, and
/// `customer.subscription.deleted` is still the terminal signal once retries are exhausted).
async fn billing_webhook(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    let secret = std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default();
    if secret.is_empty() {
        tracing::warn!("stripe webhook received but STRIPE_WEBHOOK_SECRET is not configured — rejecting (fail closed)");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !crate::billing::verify_webhook_signature(&body, sig, &secret) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let obj = v
        .get("data")
        .and_then(|d| d.get("object"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    match event_type {
        "checkout.session.completed" => {
            let checkout_id = obj
                .get("metadata")
                .and_then(|m| m.get("checkout_id"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if !checkout_id.is_empty() {
                if let Some((co, _acc)) = c.billing.confirm_checkout(checkout_id) {
                    match co.kind.as_str() {
                        "credits" => {}
                        "addon" => {
                            if let Err(e) =
                                crate::tencent_eip::provision_from_checkout(&c, &co).await
                            {
                                tracing::error!(checkout = %co.id, tenant = %co.tenant, error = %e, "dedicated_ipv4 provisioning failed (stripe webhook path)");
                            }
                        }
                        _ => apply_plan_everywhere(&c, &co.tenant, &co.plan),
                    }
                    let customer = obj.get("customer").and_then(|s| s.as_str()).unwrap_or("");
                    let subscription = obj
                        .get("subscription")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if !customer.is_empty() || !subscription.is_empty() {
                        c.billing.set_stripe_ids(&co.tenant, customer, subscription);
                    }
                    c.audit.record(
                        &co.tenant,
                        "system",
                        "charge",
                        "billing",
                        &co.id,
                        "stripe webhook: checkout.session.completed",
                    );
                    crate::persist::persist(&c);
                }
            }
        }
        "customer.subscription.deleted" => {
            let sub_id = obj.get("id").and_then(|s| s.as_str()).unwrap_or("");
            if let Some(tenant) = (!sub_id.is_empty())
                .then(|| c.billing.tenant_for_subscription(sub_id))
                .flatten()
            {
                apply_plan_everywhere(&c, &tenant, "hobby");
                c.audit.record(
                    &tenant,
                    "system",
                    "plan_change",
                    "billing",
                    sub_id,
                    "stripe webhook: subscription canceled -> downgraded to hobby",
                );
                crate::persist::persist(&c);
            }
        }
        "customer.subscription.updated" => {
            let sub_id = obj.get("id").and_then(|s| s.as_str()).unwrap_or("");
            let status = obj.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if let Some(tenant) = (!sub_id.is_empty())
                .then(|| c.billing.tenant_for_subscription(sub_id))
                .flatten()
            {
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

async fn billing_charge(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<ChargeReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let note = if req.note.is_empty() {
        "Compute usage".to_string()
    } else {
        req.note.clone()
    };
    match c.billing.charge(&t, req.cents, &note) {
        Ok(acc) => {
            c.audit.record(
                &t,
                "system",
                "charge",
                "billing",
                "compute",
                &format!("{} ¢ — {}", req.cents, note),
            );
            crate::persist::persist(&c);
            Ok(Json(json!({ "ok": true, "account": acc })))
        }
        Err(e) => Err((StatusCode::PAYMENT_REQUIRED, e)),
    }
}

#[derive(Deserialize)]
struct BillingGrantReq {
    tenant: String,
    /// Prepaid credit cents to add (0 = no credit change).
    #[serde(default)]
    credit_cents: u64,
    /// New plan id to switch to ("" = no plan change).
    #[serde(default)]
    plan: String,
    #[serde(default)]
    note: String,
    /// Pin this tenant's tier FLOOR so no automatic path can downgrade below it: `"enterprise"`
    /// locks at enterprise, `""` clears the lock, omitted leaves any existing lock untouched. A
    /// grant without this is not durable: a free-plan checkout silently reverted an owner grant of
    /// `enterprise` to `hobby`, and a remediation that passed `plan=""` skipped
    /// `apply_plan_everywhere` entirely (credits were added and the tier was never restored,
    /// leaving the account quota-locked).
    #[serde(default)]
    lock_tier: Option<String>,
}

/// Operator-only: comps an account with prepaid credits and/or switches its plan directly,
/// bypassing Stripe entirely: a support/goodwill grant (refunds, promotions, platform-owner
/// self-testing). Distinct from the self-service `/v1/billing/charge` path, which only ever acts on
/// the CALLER's own tenant and needs no operator gate since it cannot touch anyone else's account:
/// this one targets an explicit `tenant` chosen by the caller, so it is hard-gated to
/// `require_operator` like every other platform-wide admin mutation.
///
/// An unknown plan id is rejected up front: `plan_spec()` falls back to Hobby (`PLANS[0]`) for an
/// unrecognized id rather than erroring, so the resolved spec's own id is compared back against
/// what was asked for. The tier lock is applied AFTER any plan change, so a grant that both
/// switches the plan and locks it lands in the right order; `set_tier_lock` also raises the live
/// plan to the floor, which repairs an account whose stored plan was downgraded before the lock
/// existed, and the teams half is kept in step with it (the two halves must never disagree: that
/// split is what produced a tenant reading `enterprise` for features while quota-capped as
/// `hobby`).
async fn billing_grant(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<BillingGrantReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let t = req.tenant.trim();
    if t.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "tenant is required".into()));
    }
    if !req.plan.is_empty() && crate::billing::plan_spec(&req.plan).id != req.plan {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown plan '{}'", req.plan),
        ));
    }
    let note = if req.note.is_empty() {
        "Operator grant".to_string()
    } else {
        req.note.clone()
    };
    if !req.plan.is_empty() {
        apply_plan_everywhere(&c, t, &req.plan);
    }
    if let Some(lock) = req.lock_tier.as_deref() {
        let floor = lock.trim();
        if !floor.is_empty() && crate::billing::plan_spec(floor).id != floor {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown lock_tier '{floor}'"),
            ));
        }
        let floor_opt = if floor.is_empty() { None } else { Some(floor) };
        c.billing.set_tier_lock(t, floor_opt);
        if let Some(f) = floor_opt {
            apply_plan_everywhere(&c, t, f);
        }
    }
    let acc = if req.credit_cents > 0 {
        c.billing.add_credits(t, req.credit_cents, &note)
    } else {
        c.billing.account(t)
    };
    c.audit.record(
        t,
        "operator",
        "billing-grant",
        "billing",
        "grant",
        &format!(
            "plan={} credit_cents={} note={}",
            req.plan, req.credit_cents, note
        ),
    );
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true, "account": acc })))
}

#[derive(Deserialize, Default)]
struct BillingBackfillReq {
    /// Re-process tenants already marked migrated in `billing_backfill_state`
    /// instead of skipping them (default: skip — safe to re-POST after a
    /// partial run without re-touching tenants that already verified clean).
    #[serde(default)]
    force: bool,
}

/// ADMIN-ONLY, ONE-SHOT (idempotent) billing schema backfill: normalizes the pre-migration
/// JSON-blob billing rows (`billing_accounts.account_json`, `billing_ledger_snapshot.ledger_json`,
/// `billing_invoices_snapshot.invoices_json`) into the normalized per-row tables
/// (`billing_accounts`'s new scalar columns, `billing_ledger`, `billing_invoices` +
/// `billing_invoice_lines`). See `relational::backfill_billing_normalize` for the full write/verify
/// contract and the `billing_accounts` table-name collision it exists to handle.
///
/// NOT wired to run automatically anywhere: a platform operator must POST it explicitly, and only
/// after the separate production-data approval gate this migration's rollout plan calls for (it
/// performs REAL writes against REAL billing data the moment it is called and does not itself ask
/// for confirmation).
async fn billing_backfill_run(
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(body): Json<BillingBackfillReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    crate::relational::backfill_billing_normalize(body.force)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// All collections the platform stores, with row data, ordered for the UI: the persisted
/// `PlatformSnapshot` collections plus live in-memory stores, so the owner can query and explore
/// the state that rides the iroh/guardian-db replication mesh. Deployments are fleet-aggregated:
/// this operator data browser runs on ONE node, but the placement scheduler hosts a project's
/// deployments on peer nodes, so a bare `c.gw.list()` (locally hosted only) showed "0 deployments"
/// here while every deployment lived on a peer; the gossiped `peer_deployments` are merged (deduped
/// by id) exactly as the tenant-facing `dep_list` does, and the ops view spans ALL tenants
/// (operator-only). The databases collection is MASKED: this feeds the ops-console data browser,
/// and even an operator should not see raw credentials here (a real "reveal" affordance, if ever
/// added, should be its own explicit, individually audited action).
fn all_collections(c: &Arc<CloudState>) -> Vec<(&'static str, Vec<Value>)> {
    let mut projects: Vec<Value> = c
        .projects
        .snapshot()
        .into_iter()
        .map(|(k, v)| json!({ "project": k, "team": v.team, "domains": v.domains, "env_count": v.env.len(), "build": v.build, "functions": v.functions, "preview_protection": v.preview_protection }))
        .collect();
    projects.sort_by(|a, b| {
        a["project"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project"].as_str().unwrap_or(""))
    });

    let wf_runs: Vec<Value> = c.workflows.runs().into_iter().map(|r| json!(r)).collect();
    let wf_defs: Vec<Value> = c.workflows.defs().into_iter().map(|d| json!(d)).collect();

    vec![
        ("deployments", fleet_deployments_all(c)),
        ("projects", projects),
        ("orgs", c.identity.orgs().into_iter().map(|o| json!(o)).collect()),
        ("users", c.identity.users().into_iter().map(|u| json!(u)).collect()),
        ("databases", c.databases.list(None).into_iter().map(|d| json!(d)).collect()),
        ("domains", c.domains.snapshot().into_iter().map(|d| json!(d)).collect()),
        ("secure_links", c.securelinks.all().into_iter().map(|l| json!(l)).collect()),
        ("api_keys", c.apikeys.snapshot().into_iter().map(|k| json!(k)).collect()),
        ("builds", c.builds.list().into_iter().map(|b| json!({ "id": b.id, "project": b.project, "state": b.state, "branch": b.branch, "commit": b.commit, "commit_message": b.commit_message, "alias": b.alias, "started_ms": b.started_ms, "finished_ms": b.finished_ms, "log_lines": b.lines.len() })).collect()),
        ("workflow_defs", wf_defs),
        ("workflow_runs", wf_runs),
        ("incidents", c.incidents.snapshot().into_iter().map(|i| json!(i)).collect()),
        ("production_deployments", c.production_deployments.snapshot().into_iter().map(|r| json!(r)).collect()),
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
            let count = |k: &str| {
                doc.get(k)
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            };
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
    for (name, count) in c.docs.collections() {
        cols.push(json!({ "name": name, "count": count, "editable": true }));
    }
    Ok(Json(
        json!({ "collections": cols, "store": "guardian-db (iroh) · local snapshot" }),
    ))
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
    let typed = all_collections(&c)
        .into_iter()
        .find(|(name, _)| *name == collection)
        .map(|(_, r)| r);
    let editable = typed.is_none();
    let rows = typed
        .or_else(|| doc_rows(&c, &collection))
        .ok_or((StatusCode::NOT_FOUND, "no such collection".into()))?;
    let needle = q.q.unwrap_or_default().to_lowercase();
    let total = rows.len();
    let mut filtered: Vec<Value> = rows
        .into_iter()
        .filter(|r| needle.is_empty() || r.to_string().to_lowercase().contains(&needle))
        .collect();
    let matched = filtered.len();
    let limit = q.limit.unwrap_or(200).min(2000);
    filtered.truncate(limit);
    Ok(Json(
        json!({ "collection": collection, "total": total, "matched": matched, "rows": filtered, "editable": editable }),
    ))
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
    if all_collections(&c).iter().any(|(n, _)| *n == collection) {
        return Err((StatusCode::CONFLICT, format!("'{collection}' is a managed collection — create custom docs in a new collection name")));
    }
    let doc = c.docs.create(&collection, &t, body);
    c.audit
        .record(&t, "user", "create", "document", &doc.id, &collection);
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
    let doc = c
        .docs
        .patch(&id, body)
        .ok_or((StatusCode::NOT_FOUND, "no such document".into()))?;
    c.audit
        .record(&t, "user", "update", "document", &id, &collection);
    crate::persist::persist(&c);
    Ok(Json(json!(doc)))
}

/// Deletes a row: a custom document, or a typed entry via its owning store. A deployment row
/// releases the project's raw ports when it was the project's last deployment, as `dep_delete`
/// does; a database row purges its queue/vector namespaces under the RECORD's own owning team (not
/// the operator's tenant), so the purge targets the correct tenant.
async fn data_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((collection, id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.docs.delete(&id) {
        c.audit
            .record(&t, "user", "delete", "document", &id, &collection);
        crate::persist::persist(&c);
        return Ok(Json(json!({ "deleted": id })));
    }
    let ok = match collection.as_str() {
        "deployments" => match c.gw.remove(&id).await {
            Some(p) => {
                if !c.gw.list().iter().any(|d| d.project == p) {
                    let _ = crate::raw_ports::release_raw_ports_coordinated(&c, &p).await;
                }
                true
            }
            None => false,
        },
        "databases" => {
            let owner = c
                .databases
                .get_raw(&id)
                .map(|d| d.team)
                .unwrap_or_else(|| t.clone());
            c.databases.remove_db_and_purge_data(&id, &owner);
            true
        }
        "secure_links" => {
            c.securelinks.remove(&id);
            true
        }
        "webhooks" => {
            c.webhooks.remove(crate::webhooks::Scope::Operator, &id);
            true
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("'{collection}' rows are managed and can't be deleted here"),
            ))
        }
    };
    if !ok {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    c.audit.record(&t, "user", "delete", &collection, &id, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "deleted": id })))
}

/// Lists the known relational tables and their columns. The "view as PostgreSQL" data browser is an
/// alternative, READ-ONLY view of the relational mirror built on guardian-db's native SQL layer
/// (`relational.rs`): real typed tables (`project_teams`, `billing_*`) instead of the
/// document-collection JSON browsing above. It is deliberately a separate, narrower surface with no
/// PUT/POST/DELETE at all (see `relational::reject_unless_readonly`): mutations to this data still
/// only happen through the normal typed stores (ProjectStore/BillingStore), never through this
/// admin query box.
async fn sql_tables(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
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

/// Runs a read-only query against the relational mirror: either a caller-typed `sql`
/// (SELECT/read-only WITH only, enforced in `relational.rs`, not just here) or a `table` name for
/// the default `SELECT * FROM <table> LIMIT 200` the table-browser view uses. Exactly one of the
/// two is expected. A statement that does not finish inside the mirror's budget answers a retryable
/// 503 (it was accepted and validated), never a 400.
async fn sql_query(
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(body): Json<SqlQueryBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let sql = match (
        body.sql.as_deref().map(str::trim),
        body.table.as_deref().map(str::trim),
    ) {
        (Some(s), _) if !s.is_empty() => s.to_string(),
        (_, Some(t)) if !t.is_empty() => {
            if !crate::relational::known_tables()
                .iter()
                .any(|k| k.name == t)
            {
                return Err((StatusCode::BAD_REQUEST, format!("unknown table '{t}'")));
            }
            format!("SELECT * FROM {t} LIMIT 200")
        }
        _ => return Err((StatusCode::BAD_REQUEST, "provide 'sql' or 'table'".into())),
    };
    crate::relational::run_readonly_query(&sql)
        .await
        .map(Json)
        .map_err(|e| {
            if e.contains("timed out") {
                (StatusCode::SERVICE_UNAVAILABLE, e)
            } else {
                (StatusCode::BAD_REQUEST, e)
            }
        })
}

/// Preview metadata for a project's production deployment: a site thumbnail for frontends, or the
/// JSON/text body for backend services (fetched from "/" through the gateway). Another tenant's app
/// content is never rendered as a preview. `c.gw.list()` is LOCALLY-hosted deployments only and
/// most projects are placed on a peer, so with no local deployment it proxies to the hosting node
/// like the sibling deployment reads, or the project page would show a permanently empty preview
/// card on every node but the host.
pub(crate) async fn project_preview(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!({ "kind": "none" }));
    }
    let dep =
        c.gw.list()
            .into_iter()
            .find(|d| d.project == project && d.production)
            .or_else(|| c.gw.list().into_iter().find(|d| d.project == project));
    let Some(dep) = dep else {
        if let Some(node) = host_node_for_project(&c, &project) {
            if let Some(v) = fetch_from_host(
                &c,
                &node,
                &format!("/v1/projects/{}/preview", urlencode(&project)),
                &t,
            )
            .await
            {
                return Json(v);
            }
        }
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
    // The fetch targets this node's OWN gateway, but the app (sandboxed tenant code) controls the
    // RESPONSE, including any redirect: a bare client following it server-side would let a tenant's
    // own app hand the preview path a result read from wherever it points (loopback admin ports,
    // the VPC) with the body echoed back as "preview" to that caller. `same_origin_client` follows
    // only a redirect that stays on this exact origin.
    let base = c.public_base.clone();
    let resp = hive_outbound::same_origin_client()
        .get(format!("{base}/"))
        .header("host", &alias)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let ct = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            let trimmed: String = body.chars().take(4000).collect();
            let kind = if ct.contains("json")
                || trimmed.trim_start().starts_with('{')
                || trimmed.trim_start().starts_with('[')
            {
                "json"
            } else {
                "text"
            };
            Json(
                json!({ "kind": kind, "status": status, "content_type": ct, "body": trimmed, "alias": alias }),
            )
        }
        Err(e) => {
            Json(json!({ "kind": "text", "body": format!("(no response: {e})"), "alias": alias }))
        }
    }
}

pub(crate) fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

/// A PNG thumbnail of the deployed site, captured with headless Chrome and cached per deployment;
/// it falls back to a generated SVG card if capture fails. It mirrors `project_preview`'s ownership
/// check (another tenant's app content is never rendered as a preview; this handler independently
/// captures and serves the image bytes, and once had no check at all, which also bypassed
/// `preview_protection` for a password-gated project). Like the preview it only sees locally hosted
/// deployments and the PNG cache lives under this node's own data dir, so a remotely placed project
/// proxies the bytes from the hosting node instead of 404ing on every node but the host.
async fn project_thumbnail(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return (StatusCode::NOT_FOUND, "no deployment").into_response();
    }
    let dep =
        c.gw.list()
            .into_iter()
            .find(|d| d.project == project && d.production)
            .or_else(|| c.gw.list().into_iter().find(|d| d.project == project));
    let Some(dep) = dep else {
        if let Some(node) = host_node_for_project(&c, &project) {
            let path = format!("/v1/projects/{}/thumbnail", urlencode(&project));
            if let Some(b) = fetch_bytes_from_host(&c, &node, &path, &t).await {
                return (
                    [
                        (axum::http::header::CONTENT_TYPE, "image/png"),
                        (axum::http::header::CACHE_CONTROL, "public, max-age=60"),
                    ],
                    b,
                )
                    .into_response();
            }
        }
        return (StatusCode::NOT_FOUND, "no deployment").into_response();
    };
    let cache = crate::persist::data_dir().join("thumbnails");
    let _ = tokio::fs::create_dir_all(&cache).await;
    let png = cache.join(format!("{}.png", dep.id.as_str()));

    if !png.exists() {
        let _ = capture_thumbnail(&dep.alias, &png).await;
    }
    if let Ok(bytes) = tokio::fs::read(&png).await {
        return (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "public, max-age=60"),
            ],
            bytes,
        )
            .into_response();
    }
    let svg = thumbnail_placeholder(&project);
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
}

/// Gateway port the thumbnail capture dials when `HIVE_PUBLIC_PORT` is unset: the default of the
/// local mesh.
const THUMBNAIL_GATEWAY_PORT_DEFAULT: &str = "8787";

/// Capture a screenshot of `http://<alias>:<gateway-port>/` with headless Chrome.
async fn capture_thumbnail(alias: &str, out: &std::path::Path) -> anyhow::Result<()> {
    let port =
        std::env::var("HIVE_PUBLIC_PORT").unwrap_or_else(|_| THUMBNAIL_GATEWAY_PORT_DEFAULT.into());
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
    anyhow::ensure!(
        status.status.success() && out.exists(),
        "chrome screenshot failed"
    );
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
            /* auth_is_owner (the REAL, server-verified value for this caller) */
            Some(false),
            /* claimed_org_slug */ None,
            /* user_id */ "user_thoth_attacker",
            /* body_email_is_owner (forged — irrelevant once enforced) */ true,
        );
        assert!(
            !is_owner,
            "a non-owner caller must never be granted is_owner, no matter what the body claims"
        );
        assert_eq!(
            tenant, "u_user_thoth_attacker",
            "must be isolated under their own per-user namespace"
        );
        assert_eq!(org_slug, None);
        assert_ne!(
            tenant, "personal",
            "must never land in the owner's shared namespace"
        );
    }

    #[test]
    fn enforced_mode_grants_owner_only_via_the_verified_claim() {
        let (tenant, _org, is_owner) = resolve_identity_sync(
            true,
            Some("personal"),
            Some(true),
            None,
            "user_real_owner",
            false,
        );
        assert!(is_owner);
        assert_eq!(tenant, "personal");
    }

    #[test]
    fn enforced_mode_rejects_claiming_an_org_the_caller_is_not_verified_for() {
        // Authenticated as "acme" but the request body claims org "thoth-division"
        // (e.g. a stale/forged client payload) — must fall back to the caller's
        // OWN real tenant, never index or report the claimed org.
        let (tenant, org_slug, _owner) = resolve_identity_sync(
            true,
            Some("acme"),
            Some(false),
            Some("thoth-division"),
            "user_x",
            false,
        );
        assert_eq!(tenant, "acme");
        assert_eq!(
            org_slug, None,
            "an unverified org claim must never be indexed"
        );
    }

    #[test]
    fn enforced_mode_honors_an_org_claim_matching_the_verified_tenant() {
        let (tenant, org_slug, _owner) = resolve_identity_sync(
            true,
            Some("thoth-division"),
            Some(false),
            Some("thoth-division"),
            "user_x",
            false,
        );
        assert_eq!(tenant, "thoth-division");
        assert_eq!(org_slug.as_deref(), Some("thoth-division"));
    }

    #[test]
    fn enforced_mode_with_no_claims_never_falls_back_to_personal() {
        // Should be unreachable in practice (require_auth already rejects an
        // unauthenticated mutation before this function runs), but the pure
        // function must fail safe on its own regardless.
        let (tenant, org_slug, is_owner) =
            resolve_identity_sync(true, None, None, None, "user_x", true);
        assert_ne!(tenant, "personal");
        assert!(!is_owner);
        assert_eq!(org_slug, None);
    }

    #[test]
    fn unenforced_dev_mode_keeps_the_legacy_body_trusting_fallback() {
        // No JWT auth configured at all (local/dev) — no tenant boundary exists
        // to protect, so the historical behavior (trust the body) is fine and
        // intentionally preserved for zero-friction local development.
        let (tenant, _org, is_owner) =
            resolve_identity_sync(false, None, None, None, "user_x", true);
        assert!(is_owner);
        assert_eq!(tenant, "personal");

        let (tenant2, _org2, is_owner2) =
            resolve_identity_sync(false, None, None, None, "user_x", false);
        assert!(!is_owner2);
        assert_eq!(tenant2, "personal", "unenforced personal-scope default is still literal \"personal\" (pre-existing dev behavior)");
    }
}

#[cfg(test)]
mod project_delete_identity_tests {
    use super::has_explicit_caller_identity;

    /// REGRESSION TEST for a real, found (not yet confirmed exploited — live
    /// probing showed HIVE_JWT_SECRET/HIVE_PEER_TRUST both enforced on the
    /// nodes checked) vulnerability shape: on an unenforced node,
    /// `resolve_tenant` resolves a caller presenting NO credential at all to
    /// team "personal" — the platform owner's own namespace on a
    /// single-tenant deployment, which trivially "owns" every project. Before
    /// this fix, `project_delete` would happily treat that as authorized and
    /// cascade-delete the named project fleet-wide. This is the most likely
    /// mechanism behind the 2026-07-28 incident where 6 projects (including a
    /// production deployment, tokenhun) were deleted within 15 minutes with no
    /// traceable authenticated action.
    #[test]
    fn a_caller_with_zero_credentials_has_no_identity() {
        assert!(
            !has_explicit_caller_identity(false, None),
            "no JWT/API-key claims and no team header must never be treated as an identity"
        );
        assert!(
            !has_explicit_caller_identity(false, Some("")),
            "an empty team header is not an assertion of identity"
        );
        assert!(
            !has_explicit_caller_identity(false, Some("   ")),
            "a whitespace-only team header is not an assertion of identity"
        );
    }

    #[test]
    fn verified_claims_alone_are_a_valid_identity() {
        // The normal authenticated path: dashboard JWT cookie or a platform
        // API key, verified by require_auth before this handler ever runs.
        assert!(has_explicit_caller_identity(true, None));
    }

    #[test]
    fn an_explicit_team_header_is_a_valid_identity_even_unenforced() {
        // Dev-mode callers that deliberately set x-hive-team keep working —
        // this only closes the fully-implicit, zero-header default.
        assert!(has_explicit_caller_identity(false, Some("personal")));
        assert!(has_explicit_caller_identity(false, Some("acme")));
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
        assert!(recs
            .iter()
            .any(|r| r.kind == "A" && r.name.is_empty() && r.value == "76.76.21.21"));
        // www CNAME (trailing dot stripped)
        assert!(recs
            .iter()
            .any(|r| r.kind == "CNAME" && r.name == "www" && r.value == "app.example.com"));
        // MX with priority
        let mx = recs.iter().find(|r| r.kind == "MX").expect("mx");
        assert_eq!(mx.priority, Some(10));
        assert_eq!(mx.value, "mail.example.com");
        // TXT keeps content (quotes stripped)
        assert!(recs
            .iter()
            .any(|r| r.kind == "TXT" && r.value.contains("v=spf1")));
        // minimal "name TYPE value" form
        assert!(recs
            .iter()
            .any(|r| r.kind == "A" && r.name == "sub" && r.value == "9.9.9.9"));
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
        assert_eq!(
            resolved, "team-a",
            "JWT claim must win over x-hive-team header"
        );
    }

    #[test]
    fn dev_mode_falls_back_to_header_without_jwt() {
        // No JWT + not enforced (HIVE_JWT_SECRET unset) → the dashboard's header
        // is honored so unauthenticated local dev still works.
        let resolved = resolve_tenant(
            None, // no JWT claims
            None, // no API key
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
        assert_eq!(
            resolved,
            super::ANON_TENANT,
            "unauthenticated enforced request is anonymous, not the owner"
        );
        assert_ne!(resolved, "personal", "must not default to the owner tenant");
    }

    #[test]
    fn dev_mode_default_is_personal_when_no_header() {
        // Dev (unenforced), no header → the single local user is the owner.
        assert_eq!(resolve_tenant(None, None, None, false), "personal");
    }

    #[test]
    fn api_key_team_used_when_no_jwt() {
        let resolved = resolve_tenant(
            None,
            Some("key-team".to_string()),
            Some("team-b".to_string()),
            true,
        );
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
        assert!(
            !super::operator_allowed(None, true),
            "no claims + enforced must be denied"
        );
        assert!(
            super::operator_allowed(None, false),
            "dev mode (unenforced) stays open"
        );
    }

    #[test]
    fn ct_eq_matches_ordinary_equality_semantics() {
        assert!(super::ct_eq("same-secret", "same-secret"));
        assert!(!super::ct_eq("same-secret", "different"));
        assert!(!super::ct_eq("short", "shorter-value"));
        assert!(super::ct_eq("", ""));
    }
}
