//! The node's admin/control API — everything the dashboard talks to.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
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
        .route("/v1/tasks/health", get(tasks_health))
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
        .route("/v1/gpu-pools", get(gpu_pools))
        .route("/v1/inference", get(inference_endpoints))
        .route("/v1/dns/stats", get(dns_stats))
        .route("/v1/mesh/discovery", get(mesh_discovery))
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
        // Runtime Cache (regional data cache for tenant functions). Loopback-only
        // (admin port), reached by local function cells via the injected
        // HIVE_RUNTIME_CACHE_URL. Scope = "<project>:<environment>".
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
        // Run operations (the upstream console's 3-dots menu): cancel, replay
        // (recreate), reenqueue, wakeup (cancel active sleeps). Each mutates the
        // project's world on its HOST node (env decrypts there) — host-routed.
        .route("/v1/workflows/runs/:id/cancel", post(wf_run_cancel))
        .route("/v1/workflows/runs/:id/replay", post(wf_run_replay))
        .route("/v1/workflows/runs/:id/reenqueue", post(wf_run_reenqueue))
        .route("/v1/workflows/runs/:id/wakeup", post(wf_run_wakeup))
        // Operator-only manual recovery primitives, conformant with the real
        // `@workflow/world` spec's generic `events.create` /
        // `experimentalSetAttributes` — see `require_operator_or_internal`.
        .route("/v1/workflows/runs/:id/events", post(wf_run_add_event))
        // `.post(..)` too: `post_body_to_host` (internal node-to-node forward,
        // shared with cancel/replay/reenqueue/wakeup) always issues a POST —
        // the public method stays PATCH, matching the spec's merge semantics.
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
        .route(
            "/v1/deployments/:id/service-graph",
            get(deployment_service_graph),
        )
        .route("/v1/deployments/:id/promote", post(dep_promote))
        .route("/v1/projects/:project", delete(project_delete))
        .route(
            "/v1/project-delete/capabilities",
            get(project_delete_capabilities),
        )
        .route("/v1/projects/:project/redeploy", post(project_redeploy))
        // Versioned receiver for artifact/workdir semantics. New coordinators use
        // this path so an old node returns 404/NO_HANDLER instead of accepting a
        // deployment it would launch from the wrong cwd.
        .route("/v1/runtime-artifact/v1/git/deploy", post(git_deploy))
        .route("/v1/git/deploy", post(git_deploy_public))
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
        .route("/v1/builds/:id/cancel", post(build_cancel))
        .route("/v1/buildcache/:key", get(buildcache_get))
        .route("/v1/build/frameworks", get(build_frameworks))
        .route("/v1/nodes/announce", post(node_announce))
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
        // ---- Teams ----
        .route("/v1/teams", get(teams_list).post(team_create))
        .route("/v1/teams/:slug", get(team_get).delete(team_delete))
        .route("/v1/teams/:slug/members", post(team_add_member))
        .route("/v1/teams/:slug/members/:email", delete(team_remove_member))
        .route("/v1/teams/:slug/plan", put(team_set_plan))
        .route("/v1/teams/:slug/sso", put(team_set_sso))
        // ---- GitOps (config repo link + inbound CI webhook) ----
        .route(
            "/v1/gitops",
            get(gitops_get).put(gitops_put).delete(gitops_unlink),
        )
        .route("/v1/gitops/synced", post(gitops_synced))
        .route("/v1/gitops/projects", get(gitops_projects))
        .route("/v1/git/webhook", post(git_webhook))
        // ---- API keys (tenant-scoped platform tokens) ----
        .route("/v1/apikeys", get(apikeys_list).post(apikey_create))
        .route("/v1/apikeys/:id", delete(apikey_revoke))
        // ---- Connected integrations (consumable via the Vercel-compatible SDK) ----
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
        // ---- Webhooks ----
        .route("/v1/webhooks", get(webhooks_all).post(webhook_create_team))
        .route("/v1/webhooks/events", get(webhook_events))
        .route("/v1/webhooks/deliveries", get(webhook_deliveries))
        .route("/v1/webhooks/:id", delete(webhook_delete))
        .route(
            "/v1/projects/:project/webhooks",
            get(webhooks_for_project).post(webhook_create),
        )
        // ---- Databases / storage ----
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
        // Mesh-internal: register/remove a cross-region replica of a database.
        .route("/v1/databases/replica", post(database_replica))
        // Functional storage REST surface (Blob / Queue / Vector).
        .route("/v1/storage/blob/:bucket", get(blob_list_keys))
        .route("/v1/storage/blob/:bucket/:key", get(blob_get).put(blob_put))
        .route(
            "/v1/storage/queue/:queue",
            get(queue_depth).post(queue_push),
        )
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
        .route(
            "/v1/securelinks",
            get(securelinks_list).post(securelink_create),
        )
        .route("/v1/securelinks/:id", delete(securelink_delete))
        // ---- Notifications (inbox bell) ----
        .route("/v1/notifications", get(notifications_list))
        .route("/v1/notifications/read", post(notifications_read))
        .route(
            "/v1/notifications/archive-all",
            post(notifications_archive_all),
        )
        .route("/v1/notifications/:id/archive", post(notification_archive))
        // ---- Push delivery (web push + SMS) ----
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
        // ---- Monitoring ----
        .route("/v1/metrics", get(metrics_get))
        .route("/v1/speed-insights", get(speed_insights_get))
        // ---- Owner / ops dashboard ----
        .route("/v1/admin/overview", get(admin_overview))
        .route("/v1/admin/audit", get(admin_audit))
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
        // ---- Billing & compute credits ----
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
        // ---- Deployment preview / thumbnail ----
        .route("/v1/projects/:project/preview", get(project_preview))
        .route("/v1/projects/:project/thumbnail", get(project_thumbnail))
        .route("/v1/incidents", get(incidents_list).post(incident_open))
        .route("/v1/incidents/:id", axum::routing::delete(incident_delete))
        .route("/v1/incidents/:id/updates", post(incident_update))
        // ---- Low-trust browser serving admissions ----
        .merge(crate::browser_admission::routes())
        // ---- Browser function artifact bytes (content-addressed, owner-proxied) ----
        .merge(crate::browser_artifacts::routes())
        // ---- Immutable runtime artifact transfer (bounded, durable, mesh-internal) ----
        .merge(crate::runtime_artifact_transfer::routes())
        // ---- Browser-replicated database exchange (fleet-initiated pull) ----
        .merge(crate::browser_db::routes())
        // ---- browser_db libsql/Hrana + Upstash REST surface (owner-proxied) ----
        .merge(crate::browser_db_rest::routes())
        // ---- Coarse browser presence (constellation satellites) ----
        .merge(crate::browser_presence::routes())
        // ---- Per-tenant browser relay byte metering (operator; metering only, never bills) ----
        .merge(crate::browser_metering::routes())
        // ---- Enterprise feature suite (IP blocking, SIEM, SAML, SCIM,
        //      deployment protection, microfrontends, conformance) ----
        .merge(crate::enterprise_api::routes())
        // ---- Platform-native Microfrontends (project-scoped groups/members/config) ----
        .merge(crate::microfrontends_api::routes())
        // ---- Platform-native Sandboxes (isolated on-demand Linux environments) ----
        .merge(crate::sandboxes_api::routes())
        // ---- Storage broker: Firecracker cell data-image snapshots ----
        .merge(crate::storage_api::routes())
        // ---- Managed SQLite over libsql/Hrana (per-database bearer, owner-proxied) ----
        .merge(crate::hrana::routes())
        .merge(crate::db_rest::routes())
        // ---- shadw drive v1: REST + WebDAV (relational metadata, owner-routed bytes) ----
        .merge(crate::drive_api::routes())
        .merge(crate::drive_webdav::routes())
        .with_state(cloud.clone());
    // EXPERIMENT: anonymous team/role membership (only with `--features zkauth`).
    #[cfg(feature = "zkauth")]
    let app = app.merge(crate::zkauth::routes(cloud.clone()));
    let _ = &cloud;
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
fn default_sub() -> String {
    "user".into()
}
fn default_tenant() -> String {
    "default".into()
}

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

/// HTTP(S) fallback distribution path for TLS cert bundles — the same payload
/// `acme::bundle_for_mesh` serves over the iroh mesh, but reachable via a
/// plain admin URL (e.g. https://api.shadw.cloud). EXISTS BECAUSE of a
/// bootstrap deadlock in the iroh path: a peer whose relay cert is stale can
/// have no working relay fallback to the leader (the relay's own hostname is
/// missing from the very cert it needs to fetch), and direct cloud-VM↔cloud-VM
/// QUIC is unreliable (NAT/MTU, see the fleet mesh-reliability finding) — so
/// `mesh_fetch` can spin forever while the fix it needs is one HTTPS GET away.
///
/// AUTH: serves a DECRYPTED private key, so this fails CLOSED unconditionally:
/// a non-empty `HIVE_INTERNAL_TOKEN` must be configured AND presented via
/// `x-hive-internal` (constant-time compare) — stricter than `mint_allowed`
/// (no dev-mode open case), and rate-limited by the same tight mint limiter.
async fn tls_bundle_mesh(
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
    let bytes = crate::acme::bundle_for_mesh(name);
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
    // Real TCP peer address — no reverse proxy sits in front of the admin
    // listener on any fleet node, so a client-supplied `x-forwarded-for` (the
    // previous source here) is pure attacker-controlled input that lets a
    // caller reset their own rate-limit bucket at will just by changing it.
    let ip = peer.ip().to_string();
    if !mint_rate_limiter().check(&ip, hive_core::now_ms()) {
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
    // Independently derive platform-operator status from THIS backend's own
    // admin_emails set (owner_email + HIVE_ADMIN_EMAILS) — never trust a
    // client-supplied claim for it. Case-insensitive membership, mirroring the
    // owner check's eq_ignore_ascii_case.
    let email = req.email.trim();
    let platform_admin =
        !email.is_empty() && c.admin_emails.iter().any(|a| email.eq_ignore_ascii_case(a));
    // Admins are always enterprise (feature: "make all admins automatically
    // always enterprise plan no matter what"). Set an unbypassable tier FLOOR
    // on the admin's tenant via the existing lock — the same mechanism
    // `account()`/`set_plan()` honor at read AND write time, so it survives
    // every automatic downgrade path (free checkout, Stripe
    // subscription.deleted). Two guards keep this safe inside the mint path:
    //   * LEADER-ONLY — /v1/token is exempt from leader-forwarding precisely
    //     because it writes no state, so a follower writing teams/billing here
    //     would race the leader's replicated snapshot. The leader is on the
    //     same round-robin and the dashboard re-mints hourly, so the lock
    //     lands (and replicates) the first time an admin's mint hits the
    //     leader.
    //   * IDEMPOTENT — only write when the floor is not already enterprise, so
    //     the hourly re-mint does not spam the billing ledger with no-op
    //     plan_change entries.
    if platform_admin && !req.tenant.trim().is_empty() && c.is_control_plane_leader() {
        let already = c
            .billing
            .account(req.tenant.trim())
            .tier_locked_plan
            .as_deref()
            == Some("enterprise");
        if !already {
            apply_admin_enterprise(&c, req.tenant.trim());
        }
    }
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
    // Not bound by middleware (e.g. called directly, bypassing the layer) —
    // best-effort resolve either credential so the endpoint is still honest.
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

// ---- Project settings (env vars, build config, function settings) ----

/// The function-region catalog is built **from the live mesh** — the actual
/// regions in which P2P nodes report their longitude/latitude. Each region is
/// auto-assigned to its real continent (from lat/lon), so a node in Los Angeles
/// appears under "North America". No hard-coded region table.
///
/// `c.registry.nodes()` is node-LOCAL gossip state (see AGENTS.md's
/// round-robin-reads-vs-leader-forwarded-writes note): a node with a poorer
/// mesh view (e.g. one still converging, or isolated from part of the fleet)
/// would otherwise answer this GET with an incomplete region list depending
/// purely on which node round-robin DNS happened to route to. Proxy to the
/// CP leader first (its view is the fleet's best-connected), falling back to
/// the local view only if that proxy fails — never a silent empty catalog.
fn build_region_catalog(c: &Arc<CloudState>) -> Value {
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
    json!(out)
}

async fn region_catalog(State(c): State<Arc<CloudState>>) -> Json<Value> {
    if !c.is_control_plane_leader() {
        let leader = c.control_plane_leader();
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
    // READ-YOUR-WRITES: env/build/settings MUTATIONS are forwarded to the
    // control-plane leader (admin_ingress), so the leader's copy is always the
    // freshest. A non-leader serving its own copy is subject to replication
    // lag — and the dashboard reads through round-robin DNS, so an add on the
    // leader followed by a read on a lagging follower showed the OLD list
    // ("I added a var and nothing appeared"). A non-leader therefore fetches
    // the leader's authoritative copy first, falling back to local only when
    // the leader is unreachable (never fail a read that local state can
    // answer). The leader itself serves local (no self-loop: it IS the
    // leader, so this branch is skipped there).
    let path = format!("/v1/projects/{project}/settings");
    if !c.is_control_plane_leader() {
        let leader = c.control_plane_leader();
        if let Some(v) = fetch_from_host(&c, &leader, &path, &t).await {
            return Ok(Json(v));
        }
        // Leader unreachable: for a project this node never hosted (untagged
        // local row), try the host node before giving up to a possibly-empty
        // local default.
        if crate::admin::record_tenant(&c.projects.team_of(&project)) == UNTAGGED_TENANT {
            if let Some(node) = host_node_for_project(&c, &project) {
                if let Some(v) = fetch_from_host(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
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
    // See project_git_ci_put's comment: a settings PUT on a project name that
    // has no row yet materializes one via set_build's underlying
    // .entry().or_default(), defaulting to UNTAGGED_TENANT unless claimed
    // here with the tenant require_project already verified.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &t);
    // The dashboard round-trips the whole BuildConfig; a save where the user
    // did NOT change the framework must not promote an auto-detected slug to
    // an explicit override (which would freeze re-detection). Only an actual
    // framework CHANGE through this PUT is an explicit user choice.
    let mut build = build;
    // `framework_auto` also marks the command/output tuple written by older
    // builders as generated. Preserve that provenance only when the dashboard
    // round-trips the entire tuple unchanged; editing any member turns the
    // framework + command set into an explicit user configuration.
    build.framework_auto = false;
    if let Some(cur) = c.projects.get_if_set(&project).map(|s| s.build) {
        if cur.framework_auto
            && build.framework.trim() == cur.framework.trim()
            && build.install_command.trim() == cur.install_command.trim()
            && build.build_command.trim() == cur.build_command.trim()
            && build.output_dir.trim() == cur.output_dir.trim()
        {
            build.framework_auto = true;
        }
    }
    c.projects.set_build(&project, build);
    crate::persist::persist(&c);
    Ok(Json(json!(c.projects.get_masked(&project))))
}

/// The tier (hobby/pro/enterprise) of the team owning a project.
pub(crate) fn team_plan(c: &Arc<CloudState>, project: &str) -> String {
    let team = norm(&c.projects.team_of(project)).to_string();
    // Fall back to the BILLING plan when there is no team row — a personal
    // namespace (`u_<uid>`, `personal`) has a billing account but no team, so
    // `teams.get` returns None there. Without this an admin whose tenant is a
    // personal namespace read "hobby" for feature-gating (duration cap, etc.)
    // even while billing correctly held enterprise via the tier lock — the two
    // halves disagreeing is exactly the drift `apply_plan_everywhere` exists to
    // prevent. `account()` already applies the enterprise floor, so this
    // reflects the admin-always-enterprise lock too.
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
    // See project_git_ci_put's comment: claim an unowned row rather than
    // leaving it UNTAGGED_TENANT once require_project has verified the caller.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &t);
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

/// `PUT /v1/projects/:project/browser-db` — the Storages page's "Deploy a
/// replicated SQLite database" flow. Writes ONLY the dashboard-managed
/// settings mirror (`ProjectSettings::browser_db`); it does not touch
/// fluid.json/the repo and does not itself rebuild anything — same
/// "settings apply going forward" contract as `project_functions_put`'s
/// vcpus/memory/gpu fields (see that handler's plan-limit comment). The
/// caller is expected to follow a successful save with the existing
/// `/v1/projects/:project/redeploy` so the block actually lands in a Ready
/// deployment's manifest (`git.rs`'s merge, right next to the `inference`
/// sync).
async fn project_browser_db_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(policy): Json<fluid_core::BrowserDbPolicy>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // See project_git_ci_put's comment: claim an unowned row rather than
    // leaving it UNTAGGED_TENANT once require_project has verified the caller.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &t);
    // Validate table identifiers at save time so a typo 400s right here.
    // `BrowserDbPolicy::resolve` clamps/drops invalid entries instead of
    // failing (correct for a REPLICATED spec another binary might read with
    // stricter/looser rules), which would otherwise hide a fresh typo from
    // the person who just typed it into this form.
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

/// `DELETE /v1/projects/:project/browser-db` — clears the dashboard-managed
/// opt-in. Like the PUT side, this alone changes nothing already deployed;
/// it stops the NEXT deploy from carrying the block (unless fluid.json
/// itself still declares one, which always wins).
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

/// `PUT /v1/projects/:project/container` — dashboard-managed container config
/// (port/protocol/memory/cpus/pids/volume mount path) for a project running
/// the `container` runtime. Same "settings apply going forward" contract as
/// `project_browser_db_put`: this alone changes nothing already deployed —
/// git.rs's produce_manifest merges it (fluid.json wins per-field) on the
/// NEXT deploy; the caller follows a successful save with
/// `/v1/projects/:project/redeploy` to apply it immediately, same as the
/// browser-db flow.
async fn project_container_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(spec): Json<crate::project_settings::ContainerSettings>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // See project_git_ci_put's comment: claim an unowned row rather than
    // leaving it UNTAGGED_TENANT once require_project has verified the caller.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &t);
    // A deploy-input boundary: an unparseable protocol string must 400 here
    // with a clear message rather than silently falling back to "http" at
    // build time, which is exactly the class of mistake that broke the
    // 'minecwaft' project in production (protocol=udp on a TCP-only port,
    // accepted with no complaint because nothing ever validated it).
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

/// `DELETE /v1/projects/:project/container` — clears the dashboard-managed
/// config. Like `project_browser_db_delete`, this alone changes nothing
/// already deployed; it stops the NEXT deploy from carrying it (unless
/// fluid.json itself still declares a `container` block, which always wins
/// per-field).
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

/// One user-editable port row in `PUT /v1/projects/:project/network`.
/// `protocol` arrives as its wire string and is parsed with the STRICT
/// `FromStr` — this is a deploy-input boundary, so a malformed value must 400
/// with a clear message, never silently coerce to http (the lenient serde
/// impl is reserved for already-stored state).
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

/// Edit a project's exposed ports/protocol WITHOUT a redeploy — the write
/// side of the settings Network page. Rewrites the PRIMARY function's (first
/// entry — the shape every single-service deploy path produces) `ports` list
/// on the project's production (else newest Ready) deployment record, syncs
/// `FunctionConfig::protocol` to the first spec, stamps a public port for
/// every raw (tcp/udp/grpc) spec via the leader-coordinated allocator
/// (stable claim keys ⇒ an unchanged spec keeps its public port across
/// edits, exactly like redeploys do), and persists the record in place
/// (`Gateway::update_manifest` — same id, no rebuild). The raw proxy/UDP
/// relay reconcile listeners from the record's stamped bindings within their
/// ~5s loop, and the updated `DeploymentInfo.raw_ports` gossips fleet-wide
/// with the deployment list. Claims are released only when the edit leaves
/// the project with NO ports declared and NO stamped binding on any function
/// — releasing while any binding survives would free live ports.
///
/// Records are node-local: a project placed on a peer node has no local
/// record here, so the validated edit is FORWARDED (body and all) to that
/// project's actual hosting node over HTTP admin or the iroh mesh — mirroring
/// `dep_promote`'s host-node proxy for `/promote` — instead of 404ing (see
/// `put_to_host` and the matching `/network` arm in `gossip::dispatch`).
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
    // A Settings→Network save rebuilds the spec list from the FORM, which has
    // no notion of compose publish requests — leaving `preferred_public_port`
    // blank here silently UNPUBLISHED every compose-published port fleet-wide
    // within one reconcile tick (the spec drops out of `raw_targets`, loses
    // its stamp, and every raw-proxy listener closes). The publish request
    // belongs to the compose file, not this form: carry it over from the
    // current record's matching spec.
    {
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
        // Not hosted locally — the placement scheduler put this project on a
        // peer node. Forward the already-validated edit (body and all) to
        // that node instead of 404ing; it holds the real deployment record,
        // so there is no re-proxy loop (mirrors the delete-cascade /
        // read-view proxy arms in `gossip::dispatch`).
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

/// Records the outcome of the auto-CI install attempted right after a
/// project's first git import (`/api/gitops/project-ci`). Called by the
/// dashboard immediately after that fetch resolves — previously the result
/// was discarded entirely (fire-and-forget), so a project imported without a
/// completed GitHub OAuth connection silently got no webhook AND no Actions
/// fallback installed, with no future push ever auto-deploying and no
/// visible error anywhere. See [`crate::project_settings::GitCiStatus`].
async fn project_git_ci_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(mut status): Json<crate::project_settings::GitCiStatus>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    status.checked_ms = hive_core::now_ms();
    // `set_git_ci`'s underlying `.entry(project).or_default()` materializes a
    // row for a project that doesn't have one yet -- and this handler fires
    // right after a project's FIRST git import, potentially before the
    // deploy path (which is what normally calls set_team) ever runs for it.
    // Left unclaimed, that row defaults to UNTAGGED_TENANT and stays that
    // way forever if the import never leads to a completed deploy (e.g. no
    // GitHub OAuth connection, as `set_git_ci`'s own doc comment already
    // describes). require_project already resolved the caller's real tenant
    // (optimistically trusting it for an unknown project) — claim ownership
    // with it, the same way the deploy path does, instead of discarding it.
    // A no-op when the row is already correctly owned.
    c.projects.set_team(&project, &t);
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
    // See project_git_ci_put's comment: claim an unowned row rather than
    // leaving it UNTAGGED_TENANT once require_project has verified the caller.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &t);
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
    let team = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let domain = b.domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || !domain.contains('.') {
        return Err((StatusCode::BAD_REQUEST, "not a valid domain".into()));
    }
    // A DomainRecord belongs to its tenant from first verified attach: an
    // existing record owned by another team 403s here, before any challenge
    // is issued or state touched (adversarial finding: first-come claims let
    // a squatter steer a delegated zone's contents — and Seer is the public
    // resolver for delegated zones, so zone contents ARE the proof channel).
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    // Ownership proof is the gate: the challenge is issued now, and the alias
    // only activates once `_hive-verify.<domain>` answers this exact TXT
    // (checked independently at every activation, never from a caller's say-
    // so). Attachments that predate this gate are grandfathered and keep
    // routing; new attaches route only after proof.
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

/// DoH TXT lookup: does `_hive-verify.<domain>` currently answer `want`?
/// Returns (matched, human-readable outcome for the record's last_probe).
/// Cloudflare's public resolver, same client shape as `domain_scan_dns`.
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

/// Pin a delegated zone's apex to the CURRENT healthy edge set (returns true
/// when the set actually changed — `set_system_address_records` damps no-op
/// refreshes). Shared by verification (first pin) and the watcher's refresh
/// sweep (membership drift).
fn pin_zone_addresses(c: &Arc<CloudState>, domain: &str) -> bool {
    let nodes = c.registry.nodes();
    c.domains.set_system_address_records(
        domain,
        crate::dnsserver::lb_records_strings(&nodes, 1),
        crate::dnsserver::lb_records_strings(&nodes, 28),
    )
}

/// Mark the domain verified (rebinding the record to the proving team) AND
/// pin the delegated zone's apex to the live healthy edge set — one helper
/// for every activation path (local apply, forward ack, owner arm, watcher)
/// so they can never drift: without the records write, a domain DELEGATED to
/// the platform verified clean but its zone served no address records at
/// all, so it resolved nowhere (adversarial finding).
fn mark_domain_verified(c: &Arc<CloudState>, domain: &str, project: &str) {
    c.domains
        .mark_verified_as(domain, Some(&verify_project_team(c, project)));
    pin_zone_addresses(c, domain);
}

/// Activate an attached domain's routing alias — self-verifying: the DoH
/// proof is re-checked HERE, so activation can never be asserted by a caller,
/// only proven. Runs the alias insert locally when this node hosts the
/// project, else forwards to the owner's activate arm (the
/// `project_domain_add` not-local pattern). On a match the leader's
/// DomainStore marks verified (replicating the status fleet-wide); the
/// owning node applies the alias and persists.
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
    // Proof good at THIS node — now route. The alias applies on the node
    // that HOSTS the project, and the verified mark lands only after the
    // apply is confirmed (locally or by the owner arm's ack): a failed apply
    // must leave the record pending so the watcher keeps retrying instead of
    // wedging in verified-but-unrouted (adversarial finding). Everywhere
    // else learns the route via the host's `serve_hosts` gossip (the
    // `smsrelay.shadw.app` mechanism).
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
        // Kick the custom-cert pass now (internally leader-gated) so TLS is
        // issued within seconds instead of at the next loop cadence.
        let kick = c.clone();
        tokio::spawn(async move { crate::acme::custom_cert_pass(&kick).await });
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
            // The owner arm applied the alias and marked its own copy; mark
            // the leader's (this store wholesale-replicates from here).
            mark_domain_verified(c, domain, project);
            crate::persist::persist(&c);
            let kick = c.clone();
            tokio::spawn(async move { crate::acme::custom_cert_pass(&kick).await });
        }
        return Ok(Json(v));
    }
    Err((
        StatusCode::BAD_GATEWAY,
        format!("project '{project}' is hosted on node '{node}' but the domain activation forward failed"),
    ))
}

/// Owner-side activation arm (`POST /v1/projects/:p/domains/:domain/activate`).
/// Applies the alias ONLY after independently re-proving the TXT — a
/// forwarded activation carries no authority of its own.
/// The local half of domain activation, shared by the HTTP arm and the
/// gossip arm: re-prove the TXT at THIS node, apply the alias, and only then
/// mark verified — a failed apply must leave the record pending so the
/// watcher keeps retrying (the stuck-verified-unrouted state the adversarial
/// review found).
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
    if verify.status == "verified" {
        // Already proven — this is a re-assert (the watcher re-applying
        // routing after a cross-node redeploy/relocation, or a verify-now
        // click). The TXT may legitimately be GONE now: path-A tenants remove
        // it after success and the Seer arm stops serving it once verified,
        // so re-demanding the DoH proof would wedge a verified domain
        // (adversarial finding: a cross-node redeploy darkened the domain
        // until the new host happened to restart). The proof stands
        // server-side; only the alias needs re-applying. Ownership is checked
        // by every caller before this point.
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
    let kick = c.clone();
    tokio::spawn(async move { crate::acme::custom_cert_pass(&kick).await });
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

/// The proving attach's team for the tenant rebind on verification — the
/// project tag when known, never invented (personal namespaces have no team
/// row; a rebind to "" would orphan the record, so fall back to the record's
/// own tenant then).
fn verify_project_team(c: &Arc<CloudState>, project: &str) -> String {
    let t = c.projects.team_of(project);
    if !t.is_empty() {
        return t;
    }
    String::new()
}

/// Owner-side activation arm (`POST /v1/projects/:p/domains/:domain/activate`).
/// Applies the alias ONLY after independently re-proving the TXT — a
/// forwarded activation carries no authority of its own.
async fn domain_activate(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((project, domain)): Path<(String, String)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    // The domain being activated must be the caller's AND the stored
    // challenge must belong to THIS attach: activation applies routing for
    // whatever challenge is stored, so without the binding a caller could
    // drive another tenant's proof onto their own project (adversarial
    // finding — cross-tenant domain theft).
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
    // Round-robin rule (AGENTS.md): this arm answers on WHATEVER node the
    // api host's DNS picked, but the alias applies only where the project
    // is hosted. A local no-deployment park on a non-host node is not an
    // answer — the gossiped fleet view knows the real host, so forward
    // there (the same owner-proxy `activate_domain_alias` already does for
    // the attach flow). Witnessed live: numo.gg parked "pending" on
    // fc-sanjose-2 while fc-sanjose-gpu-3 verified the identical call.
    let parked_no_deployment = local.get("attached").and_then(|a| a.as_bool()) == Some(false)
        && local.get("reason").and_then(|r| r.as_str()) == Some("no_local_deployment");
    // `?local=true` is the no-re-forward guard (the `fetch_artifact_from_host`
    // precedent): a forwarded activate answers locally, honestly, even if its
    // own gossip view is stale — two nodes with mutually-stale views must
    // never bounce a request between each other forever.
    if parked_no_deployment && q.get("local").map(|s| s.as_str()) != Some("true") {
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
                        let kick = c.clone();
                        tokio::spawn(async move { crate::acme::custom_cert_pass(&kick).await });
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

/// `GET /v1/domains-nameserver-roster` — the nameserver hostnames a tenant
/// points their registrar at when delegating DNS to the platform (the same
/// roster `dnsserver::apex_ns_names` computes, so the wizard and the served
/// zone can never disagree). Unauthenticated-ish (tenant-scoped like every
/// dashboard read): the set is public DNS data anyway.
/// `GET /v1/domains-nameserver-roster` — the nameserver hostnames a tenant
/// points their registrar at when delegating DNS to the platform. ONLY
/// peer-ATTESTED nameservers are named, with the platform's own ≥2 floor:
/// "advertise only what peers have PROVEN, never what a node claims" — the
/// rule whose violation once put two dead nameservers into a live
/// delegation, here applied to the tenant-facing roster the wizard shows
/// (adversarial finding: it previously fed declared-only names). Below the
/// floor the response says so honestly instead of inventing a pair.
async fn domain_ns_roster(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // The attested set + live edge addresses double as a real-time
    // health/attestation signal for the fleet — not public data (the old
    // "tenant-scoped like every dashboard read" comment was simply false:
    // GETs pass require_auth). Claims are required whenever auth is
    // enforced; the nameserver hostnames themselves remain public DNS data.
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
    // The address set a tenant points A/AAAA records at when they keep their
    // own DNS (path A of the wizard): exactly the set this node's DNS would
    // answer for the apps wildcard right now (`lb_records` — healthy +
    // public-address, latency-ordered, capped), so the guidance and the wire
    // can never disagree.
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

/// `DELETE /v1/projects/:p/domains/:domain` — detach: drop the routing alias
/// (on the owner, via the same not-local forward as attach) and the settings
/// entry. The DomainStore record stays (the user may still manage DNS); the
/// verification state clears so a later re-attach proves again.
async fn project_domain_detach(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path((project, domain)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let team = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    // The DOMAIN is the revoked resource, so the domain — not just the
    // project in the path — must be the caller's (adversarial finding: any
    // tenant could wipe a foreign domain's verification and its TLS
    // fleet-wide by naming it in a detach against their own project).
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    // Only a domain ACTUALLY attached to this project may be detached here —
    // another project may hold the live verified attach (ensure_verify
    // re-issues cross-project pendings), and clearing THAT would kill its
    // routing + TLS (adversarial finding).
    let attached = c
        .projects
        .all_domains()
        .into_iter()
        .any(|(p, d)| p == project && d == domain);
    if !attached {
        return Err((
            StatusCode::NOT_FOUND,
            format!("'{domain}' is not attached to project '{project}'"),
        ));
    }
    c.projects.remove_domain(&project, &domain);
    // Detach is a revocation: it must reach EVERY node hosting the project,
    // not the first registry entry (adversarial finding: fanout/static
    // projects kept serving on the other hosts while the UI said
    // "detached"). Local first, then every hosting peer; the route dies
    // everywhere `serve_hosts` stops carrying it. The alias dies whenever it
    // names a deployment of the DETACHED project — the rebind case, where
    // "no project still lists the domain" is false but the previous project's
    // binding must still go (live witness: numo0's stale binding kept
    // serving the old version on bkk after the re-attach to numo). A second
    // project's live attach survives: the alias then names THAT project's
    // deployment, and the watcher re-asserts it within one tick regardless.
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
        // Full detach revokes both proof and the certificate claim atomically
        // with the project-side removal. A second live attachment keeps both:
        // cert-sync still wants and serves the same domain bundle for it.
        if c.domains
            .verify_of(&domain)
            .is_some_and(|v| v.project == project)
        {
            c.domains.clear_verify(&domain);
        }
        // A fully-detached domain's zone stops claiming the platform edge —
        // the pinned system records (apex A/AAAA + www CNAME) would
        // otherwise keep resolving to the fleet forever, serving 404s on a
        // domain the tenant believes is detached.
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
            // Best-effort per peer: an unreachable peer stops publishing the host
            // at its own next serve_hosts tick, and the alias is gone there too
            // since its table is rebuilt from the same settings + heal rules.
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
    // This route is reachable directly, not only via the internal forward —
    // the same domain-side guards as the tenant-facing detach apply (the
    // domain must be the caller's, the attach must be this project's, and a
    // second project's live attach must survive).
    require_domain_owner_if_exists(&c, &headers, claims.as_ref().map(|e| &e.0), &domain)?;
    let attached = c
        .projects
        .all_domains()
        .into_iter()
        .any(|(p, d)| p == project && d == domain);
    if !attached {
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
    // The alias dies whenever it names a deployment of the DETACHED project —
    // "no other project lists the domain" alone misses the rebind case (the
    // domain was just re-attached elsewhere, and the previous project's
    // binding kept serving the OLD version on every node that held it; live
    // witness: numo0's stale binding on bkk). The watcher re-asserts the
    // alias for any project still attached within one tick.
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

/// Leader-only watcher for pending custom-domain verifications (45s tick).
/// For each pending challenge it asks the attach's OWNING node to activate —
/// the owner arm re-proves the TXT itself, so this loop never asserts a
/// proof, it just polls for one. `HIVE_DOMAIN_VERIFY_SECS` overrides the
/// cadence; `0` disables (rollout/debug).
pub fn spawn_domain_verify_loop(cloud: Arc<CloudState>) {
    let secs = std::env::var("HIVE_DOMAIN_VERIFY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(45);
    if secs == 0 {
        return;
    }
    crate::supervise::spawn_supervised("domain-verify", move || {
        let cloud = cloud.clone();
        async move {
            loop {
                crate::supervise::beat("domain-verify");
                tokio::time::sleep(Duration::from_secs(secs)).await;
                let isolated = cloud.mesh_health().isolated;
                if cloud.control_plane_leader() != cloud.node_name || isolated {
                    continue;
                }
                // Refresh the pinned apex set for verified attachments:
                // fleet membership moves under a static zone record (a node
                // leaves, a closer edge joins), and a delegated domain must
                // not keep resolving to a dead address. Damped — no store
                // churn, and no persist, when every set is unchanged.
                let verified = cloud.domains.verified_attachments();
                let mut addrs_changed = false;
                for (domain, _) in &verified {
                    addrs_changed |= pin_zone_addresses(&cloud, domain);
                }
                if addrs_changed {
                    crate::persist::persist(&cloud);
                }
                // Re-assert routing for every verified attachment: a
                // cross-node redeploy/relocation drops the alias on the old
                // host and nothing re-applies it on the new one until a
                // restart (adversarial finding). The owner arm's verified
                // fast-path re-applies the alias idempotently WITHOUT
                // re-demanding the (possibly removed) TXT; the proof already
                // stands server-side.
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

/// Body-carrying POST forward to a specific node's admin surface — the POST
/// counterpart of `put_to_host` (PUT), same node_admins-then-iroh-mesh
/// fallback shape.
async fn post_to_host_json(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
    body: &Value,
) -> Option<Value> {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        // The forwarded call must authenticate like any cross-node mutation:
        // x-hive-team alone got bounced by the receiver's loopback-forwarder
        // (adversarial finding: every activate/detach forward returned 401/403
        // or looped back to the leader). Attach the internal marker and, under
        // enforced auth, a short service JWT — the
        // `dispatch_project_delete_with` shape, proven on this same fleet.
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
            if let Ok(token) = crate::auth::issue("mesh-internal", team, "service", false, 60) {
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
            // The verify status rides every row (fanned-out peers answer from
            // their own replicated DomainStore, so the badge is consistent
            // whichever node serves the list).
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

// ---- Deployment resources (functions + static assets, build artifacts) ----

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
        // Not hosted locally — the placement scheduler may have put this deployment
        // on a peer. Proxy to the hosting node so its build outputs (functions +
        // static assets, which live on that node's filesystem) are returned.
        if let Some(node) = host_node_for_deployment(&c, &id) {
            if let Some(v) =
                fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/resources"), &t).await
            {
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
                // Resolve through the canonical `Runtime` (config value wins,
                // else argv basename) instead of comparing raw strings with an
                // `else "Node"` catch-all — that catch-all reported every
                // Python and Wasmer function to the dashboard as "Node", a
                // WRONG label rather than a missing one, on exactly the read
                // path an operator uses to diagnose why a function will not
                // start. `Command` keeps the historical label for a bare
                // binary, which is what the old `else` arm actually meant.
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
    // Walk the build OUTPUT, not the source tree: an explicit static_dir, else a
    // recognized framework output dir. Avoids listing source files + tool caches.
    let base = match rec.manifest.static_dir.as_deref() {
        Some(sd) if sd != "." && !sd.is_empty() => Some(root.join(sd)),
        _ => [
            ".vercel/output/static",
            ".next/static",
            "dist",
            "build",
            "out",
            "public",
        ]
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
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // A GET must never CREATE a DomainRecord — first-come claiming via a
    // read endpoint is how squatters pre-seeded zones (adversarial finding).
    // Records come into existence through the verified-attach flow alone.
    let Some(rec) = c.domains.get(&domain) else {
        return Err((StatusCode::NOT_FOUND, "no such domain".into()));
    };
    if norm(&rec.tenant) != norm(&t) {
        return Err((
            StatusCode::FORBIDDEN,
            "domain belongs to a different team".into(),
        ));
    }
    // Connected projects: projects in this tenant whose domain ends with this one.
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

/// Value length bound for any zone record: an unbounded TXT made a tenant
/// zone a UDP amplifier (adversarial finding — Seer shipped whatever the
/// store held in one datagram).
const MAX_RECORD_VALUE_BYTES: usize = 4096;
/// Per-(name, kind) RR bound — the amplifier needs many answers at one name.
const MAX_RECORDS_PER_NAME_KIND: usize = 32;

/// Is this record name platform-managed? `_hive-verify` (the ownership
/// proof) and `_acme-challenge` (LE DNS-01) are answered by the platform
/// itself — the proof channel MUST NOT be tenant-writable, or a zone holder
/// could self-answer their own challenge (the circular-verification break).
fn reserved_record_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n == "_hive-verify"
        || n.starts_with("_hive-verify.")
        || n == "_acme-challenge"
        || n.starts_with("_acme-challenge.")
}

/// Shared write-side validation for zone records (add/update/import):
/// reserved names, MX/SRV/CAA shapes (the encoder must never emit a wrong
/// answer), value length and per-(name,kind) count (the amplifier bounds).
/// `replacing_id` excludes the record being updated from the count.
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
    // Numeric/shape validation at write time (the encoder must never emit a
    // wrong answer): MX/SRV priority fits u16; SRV value is
    // `<weight> <port> <target>`; CAA is `<flags> <tag> "<value>"`.
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
    // The update path enforces the same write-side rules as add/import —
    // renaming a record to a platform-reserved name or an invalid shape was
    // previously possible here and only here (adversarial finding).
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
    // The `_hive-verify`/`_acme-challenge` reservation and the amplifier
    // bounds apply to bulk imports exactly as to single adds (see
    // validate_record_write) — over-cap or over-length records are SKIPPED
    // and named in the response, never silently stored or silently dropped.
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
    // Per-(name,kind) bound, counting what the zone ALREADY holds — a bulk
    // import is exactly the vector an amplifier set would arrive by.
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
                continue; // skip e.g. CNAME chains returned for an A query
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
            // MX (and SRV) prefix a numeric priority in the data field.
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
    const TYPES: &[&str] = &[
        "A", "AAAA", "CNAME", "ALIAS", "MX", "TXT", "CAA", "NS", "SRV",
    ];
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim(); // strip ; comments
        if line.is_empty() {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        // Find the record TYPE token; everything before is name/ttl/class, after is value.
        let Some(ti) = toks
            .iter()
            .position(|t| TYPES.contains(&t.to_uppercase().as_str()))
        else {
            continue;
        };
        let kind = toks[ti].to_uppercase();
        // name = first token if it isn't a ttl/class keyword, else apex.
        let mut name = String::new();
        if ti > 0 {
            let first = toks[0];
            if !first.eq_ignore_ascii_case("IN") && first.parse::<u32>().is_err() {
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
        // ttl = a numeric token before the type, if any.
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
    // Kick the REAL issuance pass (leader-gated internally); renew_ssl only
    // reads the current block now — it must never fabricate an issued state.
    let kick = c.clone();
    tokio::spawn(async move { crate::acme::custom_cert_pass(&kick).await });
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
    let same_repo =
        c.gw.git_for_project(&hit)
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

fn mesh_deploy_authority(claims: Option<&crate::auth::Claims>) -> bool {
    claims.is_some_and(|claims| claims.sub == "mesh-internal" && claims.role == "service")
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
    // Rolling compatibility: an older coordinator can still send its signed
    // internal request to the legacy path. Its service claim, not the path or a
    // caller-controlled request flag, is the authority boundary.
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

    // These fields are internal orchestration data, never public authority.
    req.project_incarnation = None;
    req.no_fanout = false;
    req.fanout_secondary = false;
    let t = tenant(&c, &headers, claims);
    start_named_deploy(&c, &t, req, None).await
}

/// Shared deploy entry: assign the project to the tenant (unique name, conflict
/// check, root-dir persist) then kick off the async build. Used by BOTH the git
/// deploy (`/v1/git/deploy`) and the zip upload (`/v1/deploy/zip`) so naming +
/// placement behave identically regardless of source.
pub(crate) async fn start_named_deploy(
    c: &Arc<CloudState>,
    t: &str,
    mut req: fluid_core::GitDeployRequest,
    expected_incarnation: Option<fluid_core::ProjectIncarnation>,
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
    if req.redeploy && expected_incarnation.is_none() && !redeploy_existing {
        return Err((
            StatusCode::NOT_FOUND,
            "the project no longer exists; start a new deployment instead of redeploying it".into(),
        ));
    }
    let project = if (req.no_fanout || redeploy_existing) && !requested.trim().is_empty() {
        c.projects
            .find_key_ci(requested.trim())
            .unwrap_or_else(|| requested.trim().to_string())
    } else {
        unique_project_name(c, &requested, &req.repo_url, &t)
    };
    // Reject an EXPLICIT name the user typed that's already taken by a different
    // project (Issue #4) — don't silently rename to `<name>-2`. Fanout deploys
    // (no_fanout), redeploys of an existing project, and auto-named deploys (empty
    // requested) are exempt: those must resolve to a concrete name without erroring.
    if !req.no_fanout
        && !redeploy_existing
        && !requested.trim().is_empty()
        && project != requested.trim()
    {
        // Say WHOSE name it is, honestly. Project names are unique across the
        // PLATFORM, not per tenant, because the name becomes a real identifier
        // downstream: the podman volume `hive-vol-{sanitize_tag(project)}`, the
        // deploy checkout dir, `hive-browserdb-{tag}.db`. So a name can be taken
        // by a project the caller cannot see, and the old wording ("already
        // exists. Choose a different name.") read as "you already have this",
        // sending the owner hunting for a project that was never in their list.
        // Witnessed: `smsrelay` was orphaned and invisible, so the owner made
        // `smsrelay2`.
        let taken_by_caller = c
            .projects
            .find_key_ci(requested.trim())
            .map(|k| project_owned_by(c, &k, &t))
            .unwrap_or(false);
        return Err((
            StatusCode::CONFLICT,
            if taken_by_caller {
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
    // silently reassigned here. Capture or upgrade the server-issued incarnation
    // while holding the lifecycle writer; deletion after this point tombstones
    // that exact identity, so build admission fails instead of recreating it.
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
    req.project = Some(project.clone());
    let build_id = crate::git::start_build(c.clone(), req, admitted_incarnation, Some(t.clone()))
        .await
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
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
        return Err((
            StatusCode::BAD_REQUEST,
            "Empty upload — choose a .zip file.".into(),
        ));
    }
    if body.len() > MAX_ZIP {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Zip too large (max 10 MB). Upload your SOURCE only — dependencies install during the build.".into(),
        ));
    }
    // Cheap sanity check that this is actually a zip (PK\x03\x04 / empty-archive magic).
    if !(body.starts_with(b"PK\x03\x04") || body.starts_with(b"PK\x05\x06")) {
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
        /// Same semantics as the git path's `redeploy`: the upload targets an
        /// EXISTING same-tenant project, so its name is kept verbatim instead
        /// of 409ing. Without this flag a same-named re-upload could never
        /// satisfy the conflict guard (zip has no git index, so `same_repo`
        /// is always false and both escape hatches were hardcoded off).
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
    let redeploy = meta.redeploy.unwrap_or(false);
    let req = fluid_core::GitDeployRequest {
        source_deployment_ids: Vec::new(),
        repo_url: format!("upload://{filename}"),
        branch: None,
        commit: None,        // zip upload: no git history, nothing to pin to
        head_repo_url: None, // zip upload has no git clone, let alone a fork
        project: meta.project,
        project_incarnation: None,
        creator: Some("you".into()),
        production: meta.production.unwrap_or(true),
        target: None,
        use_cache: true,
        root_dir: None,
        env: meta.env,
        no_fanout: false, // coordinator deploy → schedule + fanout (ships the zip to the region node)
        fanout_secondary: false, // coordinator-originated: fanout_remote stamps this per target
        build_config: None,
        function_settings: None,
        redeploy,
        zip_b64: Some(zip_b64),
        image_ref: None,
        image_port: None,
        image_protocol: None,
        image_memory: None, // zip upload has no image_ref, so no container override to carry
        image_cpus: None,
        image_pids: None,
        image_ports: None,
        git_token: None, // zip upload has no git clone
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
    /// Explicit protocol override (Railway-style; see `fluid_core::ServiceProtocol`).
    /// Independent of `port` — either may be given without the other. Needed for any
    /// image whose auto-detected default (http) is wrong, and REQUIRED for a
    /// UDP-only image (e.g. Minecraft Bedrock, port 19132/udp, no TCP port at all —
    /// there is no TCP port for auto-detection to find, let alone default to http).
    #[serde(default)]
    protocol: Option<fluid_core::ServiceProtocol>,
    /// Memory ceiling override for the container, e.g. "4g", "2048m", "512" —
    /// same format/semantics as a Dockerfile-build project's fluid.json
    /// `container.memory`. Omitted/None = the node's generous env-tunable
    /// default. Always clamped to a fleet-wide ceiling
    /// (`ContainerLimits::for_container`) so a request can never remove the
    /// ceiling entirely — gives registry-image deploys (e.g.
    /// `itzg/minecraft-server`) the same resource-override parity the
    /// Dockerfile-build path already has via fluid.json.
    #[serde(default)]
    memory: Option<String>,
    /// CPU quota override for the container, e.g. "2.0", "0.5" — same format as
    /// fluid.json `container.cpus`. Omitted/None = the node's default. Clamped
    /// fleet-wide.
    #[serde(default)]
    cpus: Option<String>,
    /// Max-PIDs override for the container's cgroup (fork-bomb guard) — same as
    /// fluid.json `container.pids`. Omitted/None = the node's default. Clamped
    /// fleet-wide.
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
        return Err((
            StatusCode::BAD_REQUEST,
            "Provide an image reference, e.g. fruitbox12/simplifi:latest".into(),
        ));
    }
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let req = fluid_core::GitDeployRequest {
        source_deployment_ids: Vec::new(),
        // A synthetic source URL so the deployment record reads sensibly; the actual
        // source is `image_ref` (no clone happens).
        repo_url: format!("image://{image}"),
        branch: None,
        commit: None,        // prebuilt image deploy: no git clone, nothing to pin to
        head_repo_url: None, // prebuilt image deploy has no git clone, let alone a fork
        project: body.project,
        project_incarnation: None,
        creator: Some("you".into()),
        production: body.production.unwrap_or(true),
        target: None,
        use_cache: true,
        root_dir: None,
        env: body.env,
        no_fanout: false, // coordinator deploy → schedule + fanout to a container node
        fanout_secondary: false, // coordinator-originated: fanout_remote stamps this per target
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
        git_token: None, // prebuilt image deploy has no git clone
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
            // Not built here — a deploy mutation (POST /v1/git/deploy) is always
            // leader-forwarded (admin_ingress), so a fresh build this node never
            // ran locally almost always lives on the CURRENT control-plane leader
            // instead. Reads are NOT leader-forwarded ("best-effort local" —
            // admin_ingress's own comment), so without this fallback a dashboard
            // poll landing on a non-leader node 404'd forever: the deploy
            // genuinely succeeded (on the leader), but its status could never be
            // read back from whichever node the browser happened to hit — the
            // exact "Deployment started 0s ago…, 0 lines, Waiting for logs…"
            // stuck-forever bug. Mirrors deployment_build's identical fallback
            // (see its comment) for the sibling /v1/deployments/:id/build route.
            if !c.is_control_plane_leader() {
                let leader = c.control_plane_leader();
                if let Some(v) = fetch_from_host(&c, &leader, &format!("/v1/builds/{id}"), &t).await
                {
                    return Ok(Json(v));
                }
            }
            return Err(StatusCode::NOT_FOUND);
        }
    };
    // Team-scoped — this route previously had no ownership check at all, so
    // any caller (even unauthenticated, since GET bypasses the JWT gate) who
    // knew/guessed a build id could read another tenant's full build log.
    if !build_owned_by(&c, &b, &t) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(b)))
}

/// Whether tenant `t` owns build `b` — the project row is node-local (never
/// gossiped) and `Build` carries no tenant tag of its own, so `team_of` alone
/// 404'd a member's OWN build log/cancel on any node whose row was GC'd/never
/// synced. Fall back to the build's deployment record tenant tag (local `gw`
/// record or gossiped peer copy) — the same authority `deployment_build`/
/// `dep_list` trust. Shared by `build_get` and `build_cancel`.
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

/// Cancel an in-flight build: stop the ACTUAL OS process (git clone / npm
/// install / npm run build / a remote fanout target's own build) — not just
/// relabel the record while the process keeps running — and mark it
/// `Cancelled`. A build already in a terminal state is a no-op (idempotent:
/// returns the current record rather than erroring, so a UI double-click or a
/// race with the build finishing naturally is harmless). Not found locally →
/// best-effort proxy to the current control-plane leader, mirroring
/// `build_get`'s read fallback (mutations aren't leader-forwarded for an id
/// this node has never heard of; the leader is the next-most-likely holder).
pub(crate) async fn build_cancel(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let Some(b) = c.builds.get(&id) else {
        if !c.is_control_plane_leader() {
            let leader = c.control_plane_leader();
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
    if !matches!(
        b.state,
        fluid_core::DeployState::Queued | fluid_core::DeployState::Building
    ) {
        // Already terminal (ready/error/already-cancelled) — nothing to stop.
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
    // Kill the real process (local process group and/or a mirrored remote
    // build's own host) — see `BuildCancelRegistry::cancel`. The record above
    // is already updated regardless of how this resolves, so the dashboard
    // reflects the cancel immediately even if the kill itself is still racing
    // the build's own natural completion.
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
        &b.project,
        // Matches `webhooks::ALL_EVENTS`'s catalogued spelling exactly — a
        // webhook's subscription list is built from that catalog, so any
        // other spelling here would silently match zero subscribers forever.
        "deployment.canceled",
        json!({ "id": id, "project": b.project }),
    );
    Ok(Json(json!(c.builds.get(&id).unwrap_or(b))))
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
        if let Some(v) =
            fetch_from_host(&c, &node, &format!("/v1/deployments/{id}/build"), &t).await
        {
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
        Err((
            StatusCode::FORBIDDEN,
            "project belongs to a different team".into(),
        ))
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
        return Err((
            StatusCode::FORBIDDEN,
            "domain belongs to a different team".into(),
        ));
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
        // Only a VERIFIED record locks to its tenant (the `ensure_verify`
        // rule): an unproven claim — pending or DNS-only — confers no
        // ownership and must never 403 the real owner's attach, which takes
        // the record over and lets DNS proof settle ownership (adversarial
        // finding: first-come pending claims locked owners out permanently).
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

/// Like [`require_operator`], but also honors the internal node-to-node
/// forward trust already used elsewhere in this file (`x-hive-internal` ==
/// `HIVE_INTERNAL_TOKEN`, constant-time compare — see `mint_allowed` /
/// `tls_bundle_mesh`). Needed because, unlike every other `require_operator`
/// call site (global infra mutations executed on whichever node receives
/// them), the two run-mutation endpoints that use this guard forward to a
/// run's project's HOST node exactly like `cancel`/`replay`/`wakeup` do (env
/// decrypts locally) — and `post_body_to_host` carries no Authorization
/// header across that hop, only `x-hive-team`. A bare `require_operator`
/// would 403 every legitimately-forwarded internal hop the instant auth is
/// enforced, breaking these ops for any run not hosted on the node the
/// operator happened to hit.
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

/// Authenticated-READ guard for the topology views the dashboard shows every
/// signed-in user (network page: nodes/cluster/overview). Any verified claims
/// pass — the handler then decides between the full operator payload and the
/// sanitized tenant view. Missing claims while enforced is 401, not 403: the
/// dashboard's transparent cookie re-mint keys on 401, and "no session" is an
/// authentication failure, not an authorization decision.
pub(crate) fn require_auth_read(
    claims: Option<&crate::auth::Claims>,
) -> Result<(), (StatusCode, String)> {
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

async fn dep_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    // STRICT multi-tenant isolation: a request only ever sees the deployments for
    // its active tenant (the Clerk org slug / team via `x-hive-team`, or an API
    // key's team). Projects in other tenants are never returned — this is what
    // prevents data bleeding across accounts when switching teams.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // node name -> region, to resolve where each deployment ACTUALLY runs.
    let node_region: std::collections::HashMap<String, String> = c
        .registry
        .nodes()
        .into_iter()
        .map(|n| (n.name, n.region))
        .collect();
    // deployment id -> the SET of regions it's actually hosted in (multi-region aware).
    let mut host_regions: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();

    let mut list: Vec<_> =
        c.gw.list()
            .into_iter()
            .filter(|d| {
                // Trust the deployment's OWN tenant tag first — it's authoritative
                // for who actually built/owns it (matches the peer branch below).
                // `ProjectStore.team_of` is REPLICATED (store_sync merges rows
                // with per-row updated_ms + tombstones) but converges on a sync
                // cadence and repairs on the tenancy-reconcile loop, so on a
                // node other than the one that ran `set_team` it can miss even for
                // a correctly-tagged deployment; only consult it as a fallback when
                // the record itself was never tagged, and even then fail CLOSED
                // (never silently adopt into the owner's personal view).
                let own = record_tenant(&d.tenant);
                let effective = if own == UNTAGGED_TENANT {
                    record_tenant(&c.projects.team_of(&d.project)).to_string()
                } else {
                    own.to_string()
                };
                effective == t
            })
            .collect();
    // Locally-hosted deployments run in THIS node's region.
    for d in &list {
        host_regions
            .entry(d.id.to_string())
            .or_default()
            .insert(c.region.clone());
    }
    // Merge in deployments the placement scheduler placed on OTHER mesh nodes, and
    // record each one's actual host region(s). Collect regions from EVERY peer that
    // hosts it (multi-region); push to the list only once (dedup by id).
    let mut seen: std::collections::HashSet<String> =
        list.iter().map(|d| d.id.to_string()).collect();
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
    // Tag the deployment (and the cells it spawns) with the caller's tenant so
    // compute is partitioned per team — mirrors gw.deploy()'s defaults otherwise.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Honor the request's environment + git metadata (previously both were forced —
    // `production=true`, `git=None` — which made preview deployments and
    // commit-scoped routing, e.g. microfrontend fallback resolution, unreachable via
    // this route). A project's first deploy still claims the production alias inside
    // `deploy_full` even when `production=false`, so URLs resolve.
    let creator = req.creator.clone().unwrap_or_else(|| "you".into());
    // Public raw-port allocation: same stable-keyed allocator the git build
    // path uses (`raw_ports::allocate_raw_ports`) — a direct-API deployment
    // declaring raw-protocol PortSpecs gets its public ports stamped before
    // the record is registered/persisted. No-op for HTTP-only manifests.
    let mut manifest = req.manifest;
    let project = manifest.project.clone();
    // Same per-project lifecycle writer gate `run_build`'s finalization
    // section holds (`project_settings::lifecycle_write`) — this is also a
    // "final production/preview deployment-registration path", and without
    // this it could interleave with a concurrent `delete_project_local`
    // exactly like the git-build race the gate exists to close. Held for the
    // rest of this handler so registration below cannot straddle a delete's
    // tombstone application.
    let _lifecycle = crate::project_settings::lifecycle_write(&project).await;
    // `DeployRequest.project_incarnation` is rollout metadata exposed on a
    // public schema, never authority. Resolve or mint the identity exclusively
    // from the server-owned ProjectStore while the lifecycle writer is held.
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
    // Browser-function contract enforcement (bn-launch-blocked-assets-never-deployed):
    // this route runs NO bundle/persist step, so a manifest carrying a browser
    // policy would present as loudly-ineligible forever, and a client-supplied
    // `browser_artifact` descriptor would admit donors whose artifact fetch
    // then 404s on every node. Mirror the git path's rule — an opt-in that
    // cannot produce real bytes fails loudly — by rejecting browser policies
    // here and stripping any descriptor: the ONLY legitimate descriptor
    // source is the git build pipeline.
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
    // Persist so the deployment survives a node restart (without this it lived
    // only in memory and was lost on reboot).
    causal_stamp_new_deployment(&c, &project, &info.id.0);
    crate::persist::persist(&c);
    Ok(Json(json!(info)))
}

/// Whether a just-registered deployment record is still authorized to exist.
/// A captured incarnation is real proof and must still match the project's
/// CURRENT active one (mirrors `persist::restore`'s own `Some` arm exactly).
/// The legacy `None` case has no captured fact to re-check — LIVE-witnessed
/// 2026-08-22: mirroring `persist::restore`'s `None` arm exactly (reject
/// whenever the project was EVER tombstoned and no settings row has since
/// been re-touched) false-positive-rejected an ordinary same-request redeploy
/// of a just-deleted project name, because `dep_create` never happens to
/// touch `ProjectSettings` — ordinary git builds usually do (via
/// `set_production_branch`/`set_build`/etc.), so the boot-restore version of
/// this bug is rare, but on the live path it fires on literally the very
/// next request. So `None` is authorized UNLESS the current row proves a
/// DIFFERENT, REAL incarnation is now active — i.e. a genuine
/// incarnation-tracked recreation already exists and this legacy record
/// would collide with it. Mere past deletion, or a row with no incarnation
/// of its own, is never by itself proof of staleness for this case;
/// preventing the actual interleaving race is `project_settings::
/// lifecycle_write`'s job (held across `delete_project_local` and every
/// gated registration path), not this heuristic's.
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

/// Floor a fresh deployment's causal stamp at the project tombstone +1 (see
/// `ProjectStore::tombstone_of`) — but only once the record has proven it is
/// still authorized to exist. Every registration path (direct
/// `POST /deployments`, git.rs's `run_build`, the first-deploy "Building…"
/// placeholder) calls this immediately after `gw.deploy_full[_with_runtime]`,
/// which makes it the ONE chokepoint every registration funnels through —
/// deliberately kept a plain sync `fn` with this exact 3-arg signature so
/// git.rs's existing call sites need no edit.
///
/// The race this closes (adversarial finding 2026-08-22): `run_build`'s own
/// "still current" check can pass BEFORE a concurrent `delete_project_local`
/// tombstones the project, and the build can still finish and register
/// AFTER — `run_build` never re-reads the project's incarnation at that
/// point, so without this check the registration got floored above the
/// tombstone exactly like a genuine post-delete redeploy, resurrecting a
/// project whose databases/volumes/source may already be purged underneath
/// it. Rejected here means torn back down completely, detached (this fn
/// cannot become `async` without editing every caller): no deployment, no
/// alias, no causal stamp survives a delete the registration lost its race
/// against.
pub(crate) fn legacy_deployment_ids_for_owner(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
) -> Vec<String> {
    let team = norm(team).to_string();
    // Local records (`DeployRecord`) and gossiped-peer records
    // (`DeploymentInfo`) are different types with different `id` shapes
    // (`String` vs `DeploymentId`) — map both onto one common
    // (id, project, incarnation, tenant) tuple before chaining, rather than
    // trying to chain incompatible iterator item types directly.
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

/// Persist and apply the exact-id migration authority for records observed
/// before a live legacy settings row acquired its first incarnation. The caller
/// holds the project's lifecycle writer. A fresh or tombstone-ambiguous row
/// returns an empty allow-set and therefore adopts nothing.
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
                        // Authority recovered before this serialized callback ran.
                        // Apply the same causal floor the synchronous fast path
                        // would have applied; retaining an authorized record
                        // without restamping it lets an older project tombstone
                        // erase a genuine recreation on the next merge/restart.
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
            &info.project,
            "deployment.promoted",
            json!({ "id": id, "project": info.project, "url": c.deploy_url(&info.alias) }),
        );
        return Ok(Json(json!(info)));
    }
    // Not hosted locally — the placement scheduler put this deployment on a peer.
    // Proxy the promote to its host NODE over iroh (FC nodes have no HTTP admin) so
    // instant rollback works cross-node.
    if let Some(node) = host_node_for_deployment(&c, &id) {
        if let Some(v) =
            post_to_host(&c, &node, &format!("/v1/deployments/{id}/promote"), &t0).await
        {
            // The host node updated its alias, but THIS node (the leader) still
            // holds the old project→deployment alias in its LOCAL gateway state.
            // Without this local update, the leader serves the previous deployment
            // for the project hostname forever, and every other node that round-
            // robins the project hostname locally does the same — the promote only
            // landed on one node. Update the local alias here so the leader and
            // any node that proxies through it also route correctly.
            //
            // The host node already owns the deployment record; this node only
            // needs the alias to point at it so mesh routing can forward requests
            // to the host. The deployment record itself is gossiped fleet-wide via
            // peer_deployments, so the alias is the only missing piece.
            if let Some(project) = v.get("project").and_then(|p| p.as_str()) {
                c.gw.set_project_alias(project, &id);
                crate::persist::persist(&c);
            }
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
        if let Some(b) =
            crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &[], 20).await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// DELETE (no body) to a host node by NAME — same two-tier HTTP-then-mesh
/// shape as `post_to_host`. `hive_p2p` defines only GOSSIP_GET/GOSSIP_POST
/// (no delete-shaped verb), so the mesh fallback rides GOSSIP_POST like every
/// other delete-shaped mutation already dispatched over the mesh (e.g. the
/// `/v1/projects/*/delete` arm in `gossip::dispatch`) — the receiving arm
/// tells create from delete by PATH SHAPE (a trailing snapshot-id segment),
/// not by verb. Used by `storage_api`'s snapshot delete to reach a
/// deployment's actual hosting node.
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
        if let Some(b) =
            crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &[], 20).await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// PUT (with a JSON body) to a host node by NAME: HTTP admin PUT if we know
/// its URL — matching the real route's registered verb, since unlike
/// `/promote` the `/network` route is only registered as `put(...)` and a
/// `.post()` there would 405 — else over the iroh mesh (dispatched there as
/// `GOSSIP_POST`; the mesh transport has no HTTP-verb distinction, only
/// path/body — see the matching `/network` arm in `gossip::dispatch`). The
/// body-carrying counterpart of `post_to_host`, used to forward
/// `PUT /v1/projects/:project/network` to a project's actual hosting node
/// when the record isn't local (see `project_network_put`).
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

/// Delete a single deployment under the same project lifecycle writer used by
/// build finalization, promotion, and full project deletion. The owned guard is
/// moved into a task before the first cancellable await: dropping the HTTP
/// request drops only the waiter, never the serialized deletion itself.
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
    crate::git::checkout_prefixes(project)
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

    // Grow-only settings claims are server-derived exact facts for volumes whose
    // deployment record may already have been reaped. Add the exact names parsed
    // from every still-live run configuration, but never invent a current base or
    // service suffix from a project/function prefix.
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

    // Legacy names carry no incarnation and are never deletion authority. Exact
    // candidates are retained and reported if present so a prefix-sharing tenant
    // survives byte-identically while an operator can resolve the residue.
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

    // Database catalog rows predate an incarnation field. Admission now records
    // every newly minted platform id in the incarnation's grow-only settings
    // lineage while holding the lifecycle writer. Only that exact id set is
    // deletion authority; legacy/unclaimed rows are retained rather than guessed
    // from names, teams, or timestamps.
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

/// Run one destructive container/volume command with a hard per-attempt budget.
/// `kill_on_drop` makes a timeout real: no child survives to mutate the host
/// after the lifecycle writer has returned an incomplete-cleanup result.
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

/// Reclaim only the immutable inventory captured for one exact incarnation.
/// The caller owns the project lifecycle writer for this whole await chain, so
/// database creation and deployment registration cannot enter a new lifetime
/// between proof and removal.
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
            // The authoritative last-reference proof is here. The concrete
            // backend is trait-erased behind `Fluid`, and the released
            // lifecycle surfaces expose no safe downcast/forwarding seam; do
            // not emulate `LiteboxBackend::retire_image_reference` with direct
            // filesystem deletion. Surface the unresolved retirement instead.
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

/// Enumerate a backend first, then remove only incarnation-qualified names that
/// are actually present. Legacy names are byte-ambiguous (`app` service `v2`
/// and project `app-v2` base both map to `hive-vol-app-v2`), so a present legacy
/// candidate is retained and returned as an observable residual.
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

/// Parse one current/legacy checkout, placeholder, or retained-source name.
/// Syntax only identifies the embedded build id; deletion additionally checks
/// that id against the immutable incarnation lineage.
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

/// Re-stat with `symlink_metadata` on every bounded attempt so a symlink is
/// unlinked rather than traversed. Files and symlinks use `remove_file`, real
/// directories use `remove_dir_all`, and unsupported special files are kept
/// and reported.
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

/// Remove only current/legacy source entries whose platform-issued build id is
/// in the exact deleted-incarnation lineage. Enumeration, metadata, special
/// types, and removal failures remain observable in the returned residual set.
async fn purge_project_source_dirs(
    project: &str,
    build_ids: &std::collections::BTreeSet<String>,
) -> Vec<ProjectCleanupResidual> {
    let prefixes = crate::git::checkout_prefixes(project);
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

/// Whether a `project_delete` caller has asserted SOME identity at all —
/// verified claims (JWT or platform API key), or an explicit `x-hive-team`
/// header. Pure core of the identity gate below, unit-testable without a
/// `HeaderMap`/`CloudState`. A blank/whitespace-only team header counts as no
/// identity (matches `resolve_tenant`'s own header-trimming behavior).
fn has_explicit_caller_identity(claims_present: bool, team_header: Option<&str>) -> bool {
    claims_present || team_header.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

fn delete_forward_is_internal(headers: &HeaderMap, claims: Option<&crate::auth::Claims>) -> bool {
    let service_claim = claims.is_some_and(|cl| cl.sub == "mesh-internal" && cl.role == "service");
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

/// A delete generation may lead the receiver's clock only by the skew the
/// origin intentionally adds plus the opposite-direction skew of this receiver.
/// Older generations remain valid forever for idempotent retries.
pub(crate) const MAX_DELETE_GENERATION_FUTURE_MS: u64 =
    hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS * 2;

pub(crate) fn valid_delete_generation(delete_ms: u64) -> bool {
    delete_ms > 0 && delete_ms <= now_ms().saturating_add(MAX_DELETE_GENERATION_FUTURE_MS)
}

/// Newest causal version this coordinator can see for `project`. A fresh
/// deletion generation is issued strictly above this floor so it dominates
/// settings and deployment records already visible anywhere in the fleet.
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

/// Delete an entire project: all its deployments + settings. By default this
/// cascades across the mesh. `?cascade=false` is the internal single-hop form
/// used by a cascade receiver; relocation uses the separate, narrowly-scoped
/// `/reap-deployments` arm and never calls project deletion.
async fn project_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Query(q): Query<CascadeQ>,
) -> Result<Json<Value>, StatusCode> {
    // A destructive, fleet-cascading operation must never authorize purely off
    // resolve_tenant's dev-mode "personal" default: on an unenforced node, a
    // caller presenting NO credential at all (no JWT, no API key, no explicit
    // x-hive-team header) still resolves to team "personal" — the platform
    // owner's own namespace on a single-tenant deployment — which trivially
    // "owns" every project. That turns every unenforced node's admin port into
    // an anonymous delete-any-project-by-name endpoint. Require the caller to
    // have asserted SOME identity (verified claims, or an explicit team header)
    // before even resolving a tenant to check ownership against — this is the
    // one endpoint on this router where the implicit personal-mode default is
    // not an acceptable substitute for real authorization.
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
    // Authorize against the local store if we have it, else against the gossiped
    // fleet — a project placed by the scheduler may live ONLY on a peer, so it's
    // absent from this node's project store (team_of would default to "personal"
    // and wrongly 404). Allow the delete if the requester's tenant owns the
    // project anywhere in the mesh.
    let authorized = c.projects.get_if_set(&project).map(|s| norm(&s.team) == t).unwrap_or(false)
        // a deployment of this project hosted locally under the requester's tenant
        || c.gw.list().iter().any(|d| d.project == project && record_tenant(&d.tenant) == t)
        // …or hosted on a peer (project lives only on a scheduler-placed node)
        || c.peer_deployments.read().values().flatten().any(|d| d.project == project && record_tenant(&d.tenant) == t)
        // …or known ONLY to the fleet-replicated relational mirror. This tier
        // exists in the LISTING (`projects`' third fallback, added for a project
        // created but never successfully deployed) and had no counterpart here,
        // so a project the dashboard happily showed could never be authorized
        // for deletion: every delete fell to the not-hosted-here branch, no peer
        // could accept a cascade for a project none of them held either, and the
        // handler answered 502 forever. Live-witnessed on `gpu-smoke` — zero
        // deployments and zero settings rows fleet-wide, one `project_teams`
        // row, undeletable from both the dashboard home and its own page.
        // Anything we are willing to LIST for a tenant must be deletable by it.
        || crate::relational::projects_for_team(&t).await.iter().any(|p| p == &project);
    // Not locally verifiable ≠ deniable: after a partial delete (or right after a
    // restart) this node may hold NO trace of a project that still serves on a
    // peer, and the gossip view can be sparse — a hard 404 here left ORPHANS
    // undeletable. Every receiving node re-checks ownership against ITS OWN store
    // before acting, so the safe behavior is: skip local teardown, but still
    // broadcast the single-hop cascade and let owners tear down their copies.
    if !authorized {
        if cascade {
            // The coordinator cannot create a project-name tombstone until an
            // owner has authenticated the tenant: on a sparse observer, doing
            // so before one peer accepts could poison another tenant's project
            // with the same name. Issue a candidate generation, let every
            // receiver authorize locally, then retain it here only after the
            // first confirmed acceptance. The small future allowance dominates
            // an ordinary in-flight write on a peer whose clock is a few seconds
            // ahead without letting retries ratchet the generation forever.
            let delete_ms = supplied_delete_ms.unwrap_or_else(|| {
                now_ms().saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS)
            });
            // AWAIT the broadcast rather than spawning it and answering 200. This node
            // holds no copy of the project, so a 200 here asserted a deletion it never
            // performed and never observed anyone else perform — and the dashboard,
            // which treats any 2xx as success, showed the user nothing at all while the
            // project kept serving. That is the "deleting a project does nothing" bug.
            // Deletes are rare and each hop is small, so paying the round trip to be
            // able to tell the truth is the right trade.
            //
            // Broadcast to EVERY peer, not just healthy ones: health is a per-observer
            // verdict, and a peer this node currently calls unhealthy is exactly the one
            // most likely to still be serving the copy nobody could reach.
            let peers: Vec<String> = c
                .registry
                .nodes()
                .into_iter()
                .filter(|n| !n.is_self)
                .map(|n| n.name)
                .collect();
            let n_peers = peers.len();
            // Quick single-attempt pass, hard-capped per peer: the user is waiting,
            // and the leader-forward client that proxied this request times out at
            // 30s — an answer slower than that reads as failure even when the
            // cascade later succeeds. Peers the quick pass could not reach get the
            // full retry budget DETACHED below.
            let results: Vec<bool> = futures::future::join_all(peers.iter().map(|node| {
                let c = c.clone();
                let project = project.clone();
                let t = t.clone();
                async move {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(20),
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
                // Nobody confirmed. Either the project does not exist under this tenant,
                // or every node that could own it is unreachable. Both are honest
                // failures the user must see; neither is a deletion. Crucially, do
                // not leave detached retries running after this 502: a later success
                // would perform a deletion the coordinator never recorded.
                tracing::warn!(
                    project, tenant = %t, peers = n_peers,
                    "project delete: not hosted locally and no peer accepted the cascade"
                );
                return Err(StatusCode::BAD_GATEWAY);
            }
            // Authorization is now witnessed by at least one owner. Persist the
            // same generation locally BEFORE scheduling slower retries, so every
            // later acceptance is tracked by a durable tombstone on this
            // coordinator even if the original HTTP request disappears.
            c.projects.apply_delete(&project, delete_ms);
            c.git_index.remove_project(&project);
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
    // The exact local lifecycle owner issues a fresh generation for a user
    // request, or applies the immutable supplied generation on an internal
    // single-hop retry. It retains the owned writer until bounded physical
    // cleanup has either completed or produced explicit residuals.
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

    // Background cascade (cascade=false → single hop, no loops): BROADCAST the
    // delete to EVERY peer instead of a gossip-derived "hosting" set —
    // peer_deployments/peer_routes can be sparse right after restarts, which made
    // the cascade silently dispatch to NOBODY and leave remote copies serving.
    // The receiving arm is team-checked and idempotent, and retries carry this
    // exact generation so they can never erase a causal recreation.
    if cascade {
        let c2 = c.clone();
        let project2 = project.clone();
        let team2 = t.clone();
        tokio::spawn(async move {
            // Every peer, not only the ones this node currently calls healthy: health is
            // a per-OBSERVER verdict, so an "unhealthy" peer is precisely the one most
            // likely to still be serving a copy nobody could reach — and a delete it
            // never receives is a project that serves forever. Concurrently, so one slow
            // peer cannot delay the rest.
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

/// LOCAL-ONLY relocation reap (the receiving side of
/// [`dispatch_deployments_reap`]): remove THIS node's production-lane
/// deployment records for the project and nothing else — previews stay,
/// ProjectSettings stay (team tag, production_branch, env), the relational row
/// stays, no resource purge. The full-teardown primitive
/// ([`delete_project_local`]) was reused for relocation reaping and its
/// settings wipe is what made projects vanish from accounts (the team tag is
/// how every listing filter finds them) and made push-classified builds treat
/// preview branches as production (production_branch erased).
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

/// Relocation-reap dispatch to ONE peer over the iroh mesh. Deliberately a NEW
/// arm rather than a parameter on the delete arm: a pre-upgrade peer answers
/// NO_HANDLER (a safe no-op — its stale copies linger until it upgrades)
/// instead of an old binary ignoring the scope parameter and running the FULL
/// teardown, which is the destroy-by-omission this replaces.
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
        if let Some(body) =
            crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &path, &[], 20).await
        {
            let v: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
            if v.as_ref()
                .map(|v| v.get("reaped").is_some())
                .unwrap_or(false)
            {
                return true;
            }
            // NO_HANDLER (pre-upgrade peer) or a refusal — both terminal.
            return false;
        }
    }
    false
}

/// Tear down a project on ONE peer node, over whatever control-plane transport
/// reaches it: the HTTP admin URL when known (local tunnel peers), else the iroh
/// mesh (Firecracker fleet nodes — `node_admins` is EMPTY for them since the SSH
/// tunnels were cut, which is exactly why HTTP-only cascade deletes silently
/// never reached the hosting nodes and "deleting a project didn't work").
/// `cascade=false` semantics on the receiving side: single hop, no loops.
/// Returns `true` when a peer ACCEPTED the delete over some transport. The caller
/// on the not-hosted-here path reports that outcome to the user instead of
/// claiming success it never observed.
pub(crate) async fn dispatch_project_delete(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    team: &str,
    delete_ms: u64,
) -> bool {
    dispatch_project_delete_with(c, node, project, team, delete_ms, 3).await
}

/// [`dispatch_project_delete`] with an explicit retry budget. The AWAITED
/// cascade (the not-hosted-here delete path) uses `attempts = 1`: with the full
/// 3-attempt budget a single dead peer holds the user's request for over a
/// minute — longer than the leader-forward client's own 30s timeout, so the
/// user would be told the delete failed while it went on to succeed. Reliability
/// is not lost: the caller spawns the full-budget variant DETACHED for any peer
/// the quick pass could not reach.
pub(crate) async fn dispatch_project_delete_with(
    c: &Arc<CloudState>,
    node: &str,
    project: &str,
    team: &str,
    delete_ms: u64,
    attempts: u32,
) -> bool {
    if node == c.node_name {
        return true; // local copy already handled by the caller
    }
    // A delete is a ONE-SHOT broadcast — unlike a database write mirror
    // (`db_replicate::send_mirrored`), there is no "next delete" to naturally
    // retry a dropped attempt, so a single failed hop here left the peer
    // serving the deleted project FOREVER. Try HTTP admin first (when known),
    // fall back to the iroh mesh on failure instead of the two being mutually
    // exclusive by transport availability, and retry the whole pair a few
    // times with backoff so one transient blip on both transports in the same
    // instant doesn't strand the peer either.
    let mut http_generation_v1 = false;
    let mut mesh_generation_v1 = false;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(750 * attempt as u64)).await;
        }
        // Bind out of the lock FIRST so no parking_lot guard is held across the
        // await (the spawned cascade future must be `Send`).
        let admin = c.node_admins.read().get(node).cloned();
        if let Some(admin) = &admin {
            if !http_generation_v1 {
                http_generation_v1 = match c
                    .http
                    .get(format!("{admin}/v1/project-delete/capabilities"))
                    .timeout(std::time::Duration::from_secs(5))
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
            // Never send a generation-bearing delete to a pre-upgrade HTTP
            // receiver. An old handler ignores `delete_ms` and can erase a
            // causal recreation before its ambiguous legacy response is rejected.
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
                    if let Ok(token) =
                        crate::auth::issue("mesh-internal", team, "service", false, 60)
                    {
                        request = request.bearer_auth(token);
                    }
                }
                match request
                    .timeout(std::time::Duration::from_secs(15))
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
        // iroh mesh path: resolve the peer's cryptographic identity + address from
        // the registry and dispatch the delete as a gossip POST (team rides as
        // `?team=`). Tried even when an admin URL exists — a node reachable over
        // one transport but not the other should not be abandoned.
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
                    10,
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
            // See `fetch_from_host`: bumped from 15s for discovery-fallback headroom.
            if let Some(body) =
                crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &path, &[], 20)
                    .await
            {
                // The mesh protocol carries no status code, so a peer's REFUSAL
                // ("not owner") arrives as a perfectly well-delivered body.
                // Treating any reply as acceptance fabricated `accepted_by`
                // counts for projects that exist nowhere — on the fleet, where
                // `node_admins` is empty and iroh is the only transport, EVERY
                // peer's refusal was counted as a confirmed deletion. Accepted
                // means the arm actually ran `delete_project_local` (it answers
                // `{"removed": n}`); an `error` body is an authoritative no, so
                // further transports/retries are pointless for this peer.
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
                // Unparseable reply: fall through to the retry loop.
            }
        } else if admin.is_none() {
            tracing::warn!(
                node,
                project,
                "project delete: no route to hosting node (no admin URL, no iroh identity)"
            );
            return false; // no route exists at all; retrying won't help until routes change
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
        // No uniquely-authoritative incarnation survives locally. Mint a fresh
        // server identity for the logical deletion, but do not adopt any of the
        // ambiguous records into it; route removal will make them unreachable
        // and the residual set will retain their storage for operator review.
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

    // Snapshot checkout ownership before cancellation: Drop-owned guards may
    // disappear while the build unwinds, but their exact paths remain lineage
    // evidence for the subsequent bounded source sweep.
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
    // Inventory only after cancellation/drain has settled. An already-admitted
    // build can create exact-incarnation workflow storage while the delete is
    // acquiring this writer; snapshotting before the drain would miss that new
    // database and then falsely report cleanup complete. The pre-cancel checkout
    // paths above remain source-lineage evidence even after their guards drop.
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
    let preserved_domains: std::collections::HashSet<String> = c
        .projects
        .all_domains()
        .into_iter()
        .map(|(_, domain)| domain)
        .collect();
    c.domains
        .clear_verify_for_project(&project, &preserved_domains);
    // Persist the exact logical tombstone before any async runtime/resource
    // teardown. A process death after this point may leave residual bytes, but
    // it can never resurrect routing or the project lifetime.
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

/// LOCAL-ONLY project teardown used by HTTP and mesh receivers. The owned
/// lifecycle guard moves into a spawned task before any await that request
/// cancellation can drop; the task owns cancellation, tombstoning, exact route
/// removal, and bounded physical cleanup to completion.
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

/// Mixed-version mesh/main call seam: propagated deletes always carry a
/// generation. User-originated deletes call `delete_project_local_requested`
/// with `None` so the exact owner issues the generation under its writer.
pub(crate) async fn delete_project_local(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    delete_ms: u64,
) -> ProjectDeleteOutcome {
    delete_project_local_requested(c, project, team, Some(delete_ms)).await
}

/// Query for project_delete: `cascade=false` deletes only on this node (no mesh
/// fan-out). `delete_ms` is the immutable generation issued by the originating
/// delete and reused by every retry; absent on a user-originated request and on
/// pre-upgrade peers. Defaults to cascade when absent.
#[derive(Deserialize, Default)]
struct CascadeQ {
    #[serde(default, deserialize_with = "de_lenient_bool")]
    cascade: Option<bool>,
    #[serde(default)]
    delete_ms: Option<u64>,
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
    /// GitHub token for a private-repo redeploy, attached server-side by the
    /// dashboard's /api/projects/:project/redeploy route (never sent by the browser).
    #[serde(default)]
    git_token: Option<String>,
}

/// Redeploy a project's newest git source (create a fresh deployment).
/// Find the latest git source for a project — from this node's gateway, OR (when
/// the placement scheduler put the project on a peer) from the gossiped fleet
/// deployments. Lets redeploy work even when the project is hosted remotely.
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

/// The newest deployment's SOURCE for `project`, fleet-aware and INCLUDING the
/// non-real-git pseudo-sources (`upload://` zip, `image://` prebuilt image). Unlike
/// `git_for_project_fleet` (which filters `is_real_git`, powering GitHub-webhook
/// lookups), this powers REDEPLOY, which must reconstruct a build for zip- and
/// image-based projects too — those return `None` from the git-only helper and were
/// the exact cause of redeploy 404ing for every non-git project.
/// Returns the source plus whether it came from the PRODUCTION lane. A redeploy
/// must rebuild from the newest PRODUCTION deployment when one exists — picking
/// the newest record across ALL environments meant any preview (PR branch) that
/// landed after the last production deploy hijacked "Redeploy": the rebuild
/// cloned the preview's branch, branch-vs-production_branch classification then
/// filed the result as a preview, and the project's production alias kept
/// serving whatever was there before (or a failed preview).
fn source_for_project_fleet(
    c: &Arc<CloudState>,
    project: &str,
) -> Option<(fluid_core::GitSource, bool, Vec<String>)> {
    // The third element is the selected upload source's platform-issued BUILD
    // id (`GitSource.commit`). Checkout dirs and retained archives are keyed by
    // build id, not by the independently-minted deployment id.
    // The LIVE promoted deployment (production == true) outranks the lane
    // heuristic entirely: `target` is immutable while promote/rollback flips
    // only `production`, so after promoting a preview the current production
    // deployment is stamped target="preview" — sourcing the newest
    // target="production" record instead would rebuild (and re-promote) the
    // very deployment the operator just rolled back from.
    let mut best_live: Option<(u64, fluid_core::GitSource)> = None;
    let mut best_prod: Option<(u64, fluid_core::GitSource)> = None;
    let mut best_any: Option<(u64, fluid_core::GitSource)> = None;
    let mut consider = |ts: u64, g: fluid_core::GitSource, target: &str, production: bool| {
        if production {
            if best_live.as_ref().map_or(true, |(t, _)| ts >= *t) {
                best_live = Some((ts, g.clone()));
            }
        }
        // `target` is immutable ("production"/"preview"); empty = pre-target
        // snapshot, fall back to the live promoted flag.
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

/// The port + protocol the newest deployment for `project` was actually running with
/// (its `web` function's declared port, whatever `container_manifest` baked in at
/// deploy time — an explicit override or the auto-detected value) — LOCAL only (this
/// node's own `deployment_records`; a project placed entirely on a peer has no
/// manifest available here, since `peer_deployments`'/`DeploymentInfo` carries no
/// port info, only `GitSource` — same fleet-locality gap `source_for_project_fleet`
/// itself only partially closes). Feeds `redeploy_request` so an image redeploy
/// restores the ORIGINAL port/protocol instead of blindly re-running auto-detection
/// (which silently drops a manual override, and can't even land on the right answer
/// for a multi-port image). `None` when there's no prior deployment with a declared
/// port (no deploy yet, non-container deployment, or the record lives only on a peer).
fn image_port_spec_for_project_fleet(
    c: &Arc<CloudState>,
    project: &str,
) -> Option<fluid_core::PortSpec> {
    fn spec_from(f: &fluid_core::FunctionConfig) -> Option<fluid_core::PortSpec> {
        // Post-fix records carry the real spec directly.
        if let Some(p) = f.ports.first() {
            return Some(fluid_core::PortSpec::single(p.container_port, p.protocol));
        }
        // Back-compat: a record written before `FunctionConfig.ports` existed —
        // recover the port from the `__container__` structured marker
        // (start_cmd = ["__container__", image, port, run_cfg_json], see
        // `container_manifest`/`image_container_manifest` in git.rs) with whatever
        // protocol was baked in (pre-fix that was always `http`, so this only ever
        // recovers a plain TCP/HTTP port — the only kind that could exist before
        // this patch).
        if f.runtime == "container"
            && f.start_cmd.first().map(String::as_str) == Some("__container__")
        {
            let port: u16 = f.start_cmd.get(2)?.parse().ok()?;
            return Some(fluid_core::PortSpec::single(port, f.protocol));
        }
        None
    }
    // Mirror source_for_project_fleet's lane preference: the redeploy sources
    // the live/production record, so restoring the port spec from a NEWER
    // preview record would pair a production source with a preview's ports.
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

/// Build a placement `Target` addressing a specific node by NAME: self (both None),
/// its HTTP admin URL when known, else its iroh mesh route. `None` when the node is
/// unknown/unreachable. Used to pin a zip redeploy to the node holding the source.
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

/// One deterministic node hosting `project`, preferring the live production
/// record and then the newest record. Project-level owner proxies use this when
/// public round-robin ingress lands on a node with no local deployment state.
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
/// Byte-oriented sibling of [`fetch_from_host`], for reads whose payload isn't
/// JSON (blob objects). Kept deliberately simple — HTTP admin URL only — since
/// its callers all fall back to their existing not-found behaviour when it
/// returns `None`, so it can never make a read worse than it is today.
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
        if let Ok(tok) = crate::auth::issue("mesh-internal", team, "service", false, 60) {
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
pub(crate) fn host_node_for_deployment(c: &Arc<CloudState>, id: &str) -> Option<String> {
    c.peer_deployments
        .read()
        .iter()
        .find(|(_, deps)| deps.iter().any(|d| d.id.to_string() == id))
        .map(|(node, _)| node.clone())
}

/// Host node NAME for the selected upload source lineage. Matching the
/// platform-issued build id stamped in `GitSource.commit` is load-bearing: a
/// host carrying only a newer preview checkout cannot rebuild production.
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

/// Every deployment across the WHOLE fleet, ALL tenants (operator scope): the
/// locally-hosted set plus every gossiped `peer_deployments` entry, deduped by id.
/// The ops overview + data browser run on one node, but the placement scheduler
/// hosts deployments on peers — a bare `c.gw.list()` returns only what this node
/// hosts (often nothing on a coordinator), which is why the ops console showed
/// "0 deployments" while peers served every project. This mirrors `dep_list`'s
/// aggregation but WITHOUT the tenant filter (operator-only callers).
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

/// GET a read-view from a host node by NAME: HTTP admin if we have one, else over the
/// iroh mesh (resolve the node's id+addr from the gossip registry). Team rides as a
/// query param on the iroh path (no HTTP headers over that transport).
/// Aggregate per-function usage stats across the WHOLE fleet: the local node's
/// live stats plus each peer's `/v1/functions` (over HTTP admin or iroh gossip).
/// The billing meter loop uses this so a NAT'd coordinator still bills compute that
/// actually ran on the Firecracker nodes. Each `FunctionStats` carries its `tenant`.
pub async fn fleet_function_stats(c: &Arc<CloudState>) -> Vec<fluid_compute::FunctionStats> {
    let mut out: Vec<fluid_compute::FunctionStats> = c.fluid.stats();
    // `local=true` is REQUIRED now that `/v1/functions` fans out itself —
    // without it each peer would re-fan to every other peer.
    for v in fan_out_peers(c, &all_healthy_peers(c), "", "/v1/functions?local=true").await {
        if let Ok(mut stats) = serde_json::from_value::<Vec<fluid_compute::FunctionStats>>(v) {
            out.append(&mut stats);
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

pub(crate) async fn fetch_from_host(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
) -> Option<Value> {
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
        if let Some(b) =
            crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_GET, &p, &[], 20).await
        {
            return serde_json::from_slice(&b).ok();
        }
    }
    None
}

/// Fan out `path` (identical for every peer — only the target host varies) to
/// every node in `peers` CONCURRENTLY, returning each reachable peer's parsed
/// JSON response (unreachable/malformed peers are silently absent — never
/// fail the whole read over one bad node).
///
/// Replaces the hand-rolled `for node in <peer list> { ...fetch_from_host
/// (...).await... }` sequential loop that used to live in every one of these
/// "merge every peer's view" handlers (metrics_get, wf_list, wf_runs,
/// wf_summary, cron_list, fleet_function_stats, admin_overview). Each hop can
/// carry up to `fetch_from_host`'s own 20s timeout, so N sequential hops could
/// cost up to N×20s in the worst case (live-witnessed as the dashboard's
/// slowest reads); `join_all` bounds the total added latency to the SLOWEST
/// single hop instead of the sum of every hop — with a 10-node fleet this is
/// up to a ~9x cut. Callers keep their own per-endpoint merge logic (it's
/// cheap, in-memory, never the bottleneck) — only the network fan-out itself
/// changes shape. `peers` is caller-supplied (not always "every healthy fleet
/// node" — `peer_nodes_for_tenant` narrows to only nodes hosting the tenant's
/// projects for the workflow endpoints).
async fn fan_out_peers(
    c: &Arc<CloudState>,
    peers: &[String],
    team: &str,
    path: &str,
) -> Vec<Value> {
    // PER-PEER budget, at the one chokepoint every dashboard fan-out funnels
    // through. `fetch_from_host`'s own budgets (10s HTTP admin, then 20s iroh)
    // are DIAL-RESILIENCE budgets — right for one-shot control operations,
    // structurally wrong for polled reads: one registry-healthy peer whose
    // trunk is cold made `/v1/functions` and `/v1/metrics` take a flat ~20s
    // per poll (measured live), which the 3-5s dashboard polling then piled
    // onto until the usage page rendered nothing at all. The fetches run
    // concurrently, so the whole fan-out now costs at most one budget; a peer
    // that misses it contributes to the next poll instead (the wf_runs
    // PHASE_BUDGET precedent, applied at the shared chokepoint).
    const PER_PEER_BUDGET: Duration = Duration::from_secs(8);
    futures::future::join_all(peers.iter().map(|name| async move {
        tokio::time::timeout(PER_PEER_BUDGET, fetch_from_host(c, name, path, team))
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

/// Assemble the deploy request for a redeploy from the resolved source. `no_fanout`
/// pins a zip redeploy to the node holding the retained source (see below); git/image
/// redeploys leave it false so the normal placement scheduler runs. `image_port_spec`
/// is the ORIGINAL port/protocol the project's newest deployment ran with (see
/// `image_port_spec_for_project_fleet`) — restored onto `image_port`/`image_protocol`
/// so an image redeploy doesn't silently discard an explicit override (or a
/// UDP-only image's only possible port) and blindly re-detect from scratch every
/// time. Harmless to set for a non-image (git/zip) redeploy: `image_ref` stays
/// `None` there, so `produce_manifest` never even looks at these two fields.
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
        // Manual redeploy has no specific webhook-notified commit — build
        // whatever the branch currently points to, same as before this field
        // existed.
        commit: None,
        // A redeploy always clones the project's own configured repo — there is
        // no webhook PR payload here to have identified a fork in the first place.
        head_repo_url: None,
        project: Some(project.to_string()),
        project_incarnation: None,
        creator: Some("you".into()),
        production: true,
        target,
        use_cache,
        root_dir,
        env: None, // redeploy: existing project env is read from the store at build time
        no_fanout,
        // A pinned zip redeploy (`no_fanout: true`) targets exactly ONE node (the
        // retained-source holder) — never one-of-many — so it is never a secondary.
        fanout_secondary: false,
        build_config: None, // coordinator reads its own store; fanout fills these per-target
        function_settings: None,
        redeploy: false, // goes straight to start_build (bypasses git_deploy naming)
        zip_b64: None,
        image_ref: None,
        image_port: image_port_spec.as_ref().map(|p| p.container_port),
        image_protocol: image_port_spec.as_ref().map(|p| p.protocol),
        // Resource overrides (unlike port/protocol above) aren't restored from
        // the project's deployment history — a redeploy takes the node's
        // default until the caller re-specifies one.
        image_memory: None,
        image_cpus: None,
        image_pids: None,
        // A multi-port declaration isn't restored from history on redeploy
        // (only the single primary image_port_spec is) — a genuinely narrower,
        // separately-tracked residual of this restore path, not a regression:
        // the single-port restore this field sits beside has the exact same
        // shape it always did.
        image_ports: None,
        git_token,
    }
}

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

    // Environment chosen in the modal: "production" | "preview". When absent the
    // branch decides (Vercel's classification).
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

    // Resolve the newest deployment's SOURCE including the non-git pseudo-sources
    // (`upload://` zip, `image://` image). The old handler used `git_for_project_fleet`
    // which filters `is_real_git`, so it returned None — and thus 404 — for EVERY
    // zip-uploaded or image-based project (the reported bug).
    let Some((src, prod_lane, source_ids)) = source_for_project_fleet(&c, &project) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Project '{project}' has no deployment to redeploy yet."),
        ));
    };
    // No explicit environment in the modal: a redeploy rebuilds the deployment
    // it sourced from into that deployment's OWN environment (Vercel's model).
    // Leaving `target: None` fell through to branch-vs-production_branch
    // guessing at build time, which misfiled production redeploys as previews
    // whenever the sourced branch string didn't exactly match.
    let target = target.or_else(|| Some(if prod_lane { "production" } else { "preview" }.into()));
    // Original port/protocol to restore on an image redeploy (see `redeploy_request`'s
    // doc comment) — resolved once, reused across every branch below.
    let image_spec = image_port_spec_for_project_fleet(&c, &project);

    // Zip-uploaded project: there is no re-fetchable remote — rebuild from the RETAINED
    // source, which lives on the node that built it. Build locally (no re-placement)
    // when this node holds it, else dispatch the redeploy to the host node so it
    // rebuilds from its own source. (Git/image sources are re-fetchable anywhere and
    // fall through to the normal placement path below.)
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
                .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
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
                    .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
                    return Ok(Json(json!({ "build_id": build_id })));
                }
            }
        }
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("The uploaded source for '{project}' is not available on a reachable node — re-upload the archive to redeploy."),
        ));
    }

    // Prebuilt-image project: reconstruct the image ref and redeploy as an image.
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
        req.repo_url = String::new();
        req.branch = None;
        req.image_ref = Some(image_ref);
        let build_id = crate::git::start_build(c.clone(), req, Some(incarnation), None)
            .await
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
        return Ok(Json(json!({ "build_id": build_id })));
    }

    // Real git source: re-clone + rebuild through the normal placement/fanout path.
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
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
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
async fn gitops_projects(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
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
    out.sort_by(|a, b| {
        a["project"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project"].as_str().unwrap_or(""))
    });
    Json(json!(out))
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

/// Inbound GitHub webhook: on a push (or merged/updated PR) to a repo that backs
/// one or more existing projects, trigger a fresh production build+deploy from the
/// pushed commit — repos become deployable workflows (taubyte-style GitOps CI).
///
/// Auth: this route is in the `open` allowlist (GitHub can't present a platform
/// JWT). When `GITHUB_WEBHOOK_SECRET` is set the HMAC-SHA256 signature is
/// verified. When it is unset, the safe default is to REJECT the delivery
/// (401) rather than silently accepting anything unsigned — that permissive
/// behavior must now be deliberately opted into via
/// `GITHUB_WEBHOOK_ALLOW_UNSIGNED=1`, intended for local/dev convenience only
/// (never set it in production once the secret is fleet-provisioned).
async fn git_webhook(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Signature policy: a SIGNED delivery is always verified when a secret is
    // configured (a bad/forged signature is rejected). An UNSIGNED delivery is
    // accepted ONLY with the explicit `GITHUB_WEBHOOK_ALLOW_UNSIGNED` opt-in —
    // this restores auto-deploy for hooks that were installed before the UI
    // was provisioned with the signing secret (they carry no signature and
    // would otherwise 401 forever), without dropping verification of the
    // hooks that DO sign. Re-signing those legacy hooks (reconnect GitHub) lets
    // this flag be removed again for strict-everywhere verification.
    let secret = std::env::var("GITHUB_WEBHOOK_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // The opt-in is honored ONLY while no secret is configured. Its documented
    // purpose is legacy hooks installed before the signing secret existed, which
    // carry no signature — that is a pre-secret condition by construction. Left
    // ungated it did something far larger: with a secret configured and this flag
    // set, an unsigned delivery was accepted, so an attacker did not need to forge
    // a signature at all — merely OMIT the header — and signature verification
    // became unreachable for anyone who bothered.
    //
    // That was live, not theoretical: an ansible-invisible drop-in
    // (`hive-node.service.d/webhook-unsigned.conf`) set it on 10 of 14 fleet
    // nodes, including the control-plane leader, while the main unit correctly
    // configured a secret. A public unsigned POST to `/v1/git/webhook` on the
    // round-robin host returned 200 while the same request WITH a bogus signature
    // correctly returned 401 — i.e. presenting no credential beat presenting a
    // wrong one, and the webhook clones and deploys attacker-named source into a
    // victim project WITH that project's env injected.
    //
    // Gating on `secret.is_none()` keeps the escape hatch working for the case it
    // was written for (genuine local/dev with no secret provisioned) and makes it
    // inert everywhere the fleet has one. A legacy unsigned hook on a
    // secret-configured node is now rejected with the existing message, which
    // already names the remedy: reconnect GitHub to re-sign it.
    let allow_unsigned = secret.is_none()
        && std::env::var("GITHUB_WEBHOOK_ALLOW_UNSIGNED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if !sig.is_empty() {
        // Signed: verify against the secret. A signed delivery with no secret
        // configured can't be verified — accept only under the opt-in.
        match &secret {
            Some(s) => {
                if !verify_github_sig(s.as_bytes(), &body, sig) {
                    // WARN, because this rejection used to be completely silent
                    // and it has two very different causes that look identical
                    // from outside: a forged/replayed delivery (correctly
                    // refused, nothing to do) or a SECRET MISMATCH between the
                    // UI that installed the hook and the backend that verifies
                    // it — under which every real GitHub delivery is dropped and
                    // auto-deploy is dead platform-wide, with no log, no event
                    // and no incident to notice it by. The repo/event context is
                    // what lets an operator tell the two apart: a mismatch shows
                    // a steady stream from THEIR OWN repos.
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
        // Unsigned and not opted in → reject (the strict default).
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

    // Extract the repo + branch + head commit, supporting `push` and
    // `pull_request` (opened/synchronize/reopened/closed) events. The deploy
    // TARGET follows Vercel's model: a PR always builds a PREVIEW; a push is
    // classified per-project from its branch (None => the production branch wins).
    let repo_full = payload["repository"]["full_name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let mut target: Option<String> = None;
    let mut pr_number: Option<u64> = None;
    // `head_repo_url`: Some only for a PR opened FROM A FORK — its clone URL, used
    // to override the clone SOURCE further down (never the project-matching repo,
    // which always stays `repo_full`/the base repo — see `want` below).
    let (branch, commit, head_repo_url) = match event.as_str() {
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or("");
            // A closed PR has nothing to (re)build. A merge fires a separate push
            // to the base branch, which is what produces the production deployment.
            if action == "closed" {
                return Ok(Json(json!({ "ignored": "pr closed" })));
            }
            // opened / synchronize / reopened / ready_for_review -> preview.
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
            // `head.repo` is the repo the PR branch actually lives on — null/absent
            // for a same-repo PR (GitHub omits it in that case for some payload
            // shapes) as well as for a PR opened from a since-deleted/inaccessible
            // fork (nothing left to clone; falls through to the base-repo clone,
            // which will then fail downstream with a clear git error, same as
            // today). Only when it's PRESENT and differs from the base repo is
            // this a genuine fork PR whose branch/commit live somewhere the base
            // repo's remote has never heard of.
            let head_repo_full = payload["pull_request"]["head"]["repo"]["full_name"]
                .as_str()
                .map(str::to_string);
            let is_fork = match head_repo_full.as_deref() {
                Some(h) => crate::gitops::norm_repo(h) != crate::gitops::norm_repo(&repo_full),
                None => false,
            };
            let head_repo_url = if is_fork {
                // Prefer GitHub's own clone_url when present; fall back to
                // constructing it from full_name (covers hand-built test payloads
                // and any delivery shape that omits clone_url).
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
            // push event -> classified from the branch at build time.
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
    // PROJECT MATCHING always uses the BASE repo's full_name — a fork-originated
    // PR still belongs to the base project; only the clone SOURCE (below) forks.
    let want = crate::gitops::norm_repo(&repo_full);
    tracing::info!(event = %event, repo = %repo_full, branch = %branch, commit = %commit, pr = ?pr_number, fork = ?head_repo_url, "git_webhook: received");

    // Private-repo credential for every deploy this delivery triggers below,
    // resolved ONCE (every triggered project shares the same `repo_full`).
    // FIRST CHOICE: a GitHub App installation access token, minted server-side
    // with no user session required (see `github_app_auth`) — this is what
    // lets a repo that was only ever connected via a user's interactive
    // GitHub App session (no node-wide GITHUB_TOKEN configured) still
    // auto-deploy on push. `None` here (App not configured, App not
    // installed on this repo, or a mint failure) is NOT fatal: `git.rs`'s
    // clone path transparently falls back to the node-wide GITHUB_TOKEN env
    // var exactly as it did before this existed.
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
                        // Never log `e`'s source token/key content — the error
                        // variants in `github_app_auth` never carry credential
                        // material, only status codes / parse failures.
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

    // Candidate projects for this repo: O(1) via the reverse index
    // (`gitops::GitRepoIndex`) in the normal case, instead of an O(projects)
    // fleet-wide scan on every single webhook delivery. An index that has NEVER
    // been populated at all (fresh boot before the first rebuild, or a bug) is
    // NOT the same as "this repo genuinely has zero projects" — that specific
    // case falls back to the original full scan defensively (not the normal
    // path). See `GitRepoIndex`'s doc comment for what keeps it in sync.
    let candidates: Vec<String> = if !c.git_index.is_empty() {
        c.git_index.projects_for(&want)
    } else {
        tracing::warn!("git_webhook: reverse index is empty/uninitialized — falling back to a full project scan");
        c.projects.snapshot().into_keys().collect()
    };

    // Deploy every project pointing at this repo for the pushed/PR branch. We do
    // NOT filter by branch: a push to a non-production branch (or a PR) is exactly
    // how preview deployments are created — the branch decides production vs
    // preview at build time (or `target` forces a preview for PRs).
    let mut triggered = Vec::new();
    for project in candidates {
        // Fleet-aware git lookup: a project placed on a peer node (the common case —
        // most projects run on Firecracker nodes, not this coordinator) has no LOCAL
        // gateway git source, so `gw.git_for_project` returns None and the webhook
        // would silently trigger NOTHING. Use the gossiped fleet view so pushes/PRs
        // to remotely-placed projects still create deployments.
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

        // Already-building guard, matching `git_poll_cycle`'s (git.rs) — a
        // webhook delivery firing near-simultaneously with a poll tick (or a
        // redelivered/duplicate webhook) for the same commit would otherwise
        // start a full duplicate build: production safety is already covered
        // by `deploy_full`'s superseded-by veto (only the newest-started build
        // goes live), but the loser still burns a full clone+install+build and
        // fires a spurious `deployment.error` webhook when it's vetoed. Only
        // guards when the commit is actually known at receipt time (every real
        // `push`/`pull_request` payload carries one; a malformed/edge-case
        // delivery with an empty SHA can't be deduped this way and falls
        // through to start a build as before — an honest under-cover, not a
        // silent one).
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
            // Pin to the EXACT commit GitHub notified us about — without this the
            // clone just builds "whatever the branch currently points to", which
            // races a rapid double-push into building the wrong commit.
            commit: Some(commit.clone()).filter(|s| !s.is_empty()),
            // Fork-originated PR: clone/fetch from the FORK's repo, not the base
            // repo `git.repo_url` — the fork's branch (and, without SHA-fetch
            // support, its commits) don't exist on the base repo's remote at all.
            // None for same-repo PRs and every push — clones `git.repo_url` as before.
            head_repo_url: head_repo_url.clone(),
            project: Some(project.clone()),
            project_incarnation: None,
            creator: Some("github".into()),
            production: true, // legacy field; classification uses `target`/branch
            target: target.clone(),
            use_cache: true, // git push redeploy: reuse the warm dependency cache
            root_dir,
            env: None,               // git push redeploy: env comes from the project store
            no_fanout: false,        // gitops redeploy is a coordinator deploy → schedule + fanout
            fanout_secondary: false, // coordinator-originated: fanout_remote stamps this per target
            build_config: None,
            function_settings: None,
            redeploy: false, // webhook push → start_build directly (bypasses git_deploy naming)
            zip_b64: None,
            image_ref: None,
            image_port: None,
            image_protocol: None,
            image_memory: None, // git push deploy has no image_ref, so no container override to carry
            image_cpus: None,
            image_pids: None,
            image_ports: None,
            // webhook auto-deploy: GitHub App installation token (first choice,
            // resolved once above) else falls back to node GITHUB_TOKEN in git.rs
            git_token: webhook_git_token.clone(),
        };
        // Webhook push: not a fanout receiver and no pre-resolved incarnation
        // (`req.project_incarnation` above is `None` too) — `start_build`
        // determines/mints the current one itself under its own
        // `lifecycle_write`. The project's own recorded team authenticates
        // legacy-deployment adoption the same way `team_of` gates ownership
        // elsewhere in this handler.
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
        // The single most common "preview build never starts" shape: a
        // webhook/Actions-fallback delivery arrived and was accepted (no auth/
        // signature error), but matched zero locally-known-or-fleet-gossiped
        // projects for `want` — invisible before this line, since a 200 with
        // `triggered: 0` looks identical to a genuine no-op from the GitHub
        // delivery's own "Recent Deliveries" view.
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
    // The announcing node is describing ITSELF — the authoritative copy that
    // may rename it past a stale registry entry (upsert_peer_self_report).
    c.registry.upsert_peer_self_report(node);
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
    Ok(Json(
        json!({ "roster": roster, "trusted_peer_count": trusted }),
    ))
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

/// Unauthenticated, liveness-adjacent mesh-membership probe (`/v1/mesh`) — no
/// JWT, matching `/healthz`, since the watchdog/monitoring polling it has none.
/// Deliberately separate from `/healthz`: liveness ("is the process serving
/// HTTP") tells you nothing about mesh membership. See `MeshHealth`'s doc.
async fn mesh_health(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.mesh_health()))
}

/// THIS node's supervised background loops: restart counts + heartbeat age.
/// Operator-only, and deliberately NODE-LOCAL (no leader proxy): each node
/// reports its OWN loops — a dead reconciler on node X is only visible by
/// asking X, so the round-robin read split is the semantics here, not a bug.
/// Answers "is the world reconciler / lock sweep / anti-entropy loop actually
/// alive on this node?" — the question that previously required inferring from
/// which log lines had STOPPED appearing.
async fn tasks_health(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(
        json!({ "tasks": crate::supervise::snapshot(), "memory": crate::supervise::memory_pressure() }),
    ))
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
    // Typed connectivity, derived from signals the node already tracks — the
    // boolean `healthy` collapses three distinct situations the operator needs
    // to tell apart, and the dashboard rendered them all as one gray:
    //  - "healthy":  gossip-alive and this observer's probes pass.
    //  - "degraded": gossip-alive and probes pass, but THIS observer's trunk
    //    to it recently failed (`health::is_cold`) — an observer-local
    //    transport fault, not a sick node; requests route around it while the
    //    trunk re-forms.
    //  - "suspect":  gossip-alive but this observer's probes fail repeatedly
    //    (withdrawn from placement/DNS by `health::demote`'s guarded path).
    //  - absent from the list entirely = no gossip for 30s = offline; the
    //    registry already drops those, so the dashboard's "unreachable" rows
    //    come from its own roster diff, not from this endpoint.
    // Per-observer by design (AGENTS.md: a health verdict is per-OBSERVER) —
    // reading this through the ops proxy answers with the LEADER's view.
    let connectivity = |n: &hive_edge::region::NodeInfo| -> &'static str {
        if n.is_self {
            "self"
        } else if !n.healthy {
            "suspect"
        } else if crate::health::is_cold(&c.registry, &n.name) {
            "degraded"
        } else {
            "healthy"
        }
    };
    // Operators (and the mesh-internal gossip path) get the full record —
    // peer sync depends on iroh_addr/peer_id being present.
    if operator_allowed(cl, crate::auth::enforced()) {
        let full: Vec<Value> = list
            .iter()
            .map(|n| {
                let mut v = json!(n);
                v["connectivity"] = json!(connectivity(n));
                v
            })
            .collect();
        return Ok(Json(json!(full)));
    }
    // Every other signed-in user gets the sanitized topology: enough to render
    // the network page (mesh map, health, capacity totals), none of the
    // mesh-internal addressing (iroh_addr/peer_id/public_ip/public_url/
    // relay_url/guardian_iroh_addr — all dialable endpoints, held back from
    // non-operators same as public_url). GPU capacity fields are NOT
    // addresses (they're the same class of hardware descriptor as
    // cpu_cores/mem_total_mb/disk_total_gb, already included below) — omitting
    // them used to blank the regions map's GPU coloring/badge
    // (`isGpuNode`/`nodeColor` in ui/app/regions/page.tsx) for every node, for
    // every non-operator signed-in viewer, fleet-wide.
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
                "connectivity": connectivity(&n),
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

/// TENANT-SCOPED function stats (Usage page's cost breakdown, the Functions
/// list page, service-graph/deployment-canvas overlays). Previously gated
/// behind `require_operator` — a hard 401/403 for every non-owner user, so
/// all four dashboard consumers silently showed zero invocations/CPU/memory
/// (and, on the Usage page, near-zero computed dollar charges) for the
/// overwhelming majority of real users. No admin-only page ever depended on
/// this endpoint's previous all-tenants shape — fixed the same way
/// `databases_list` already scopes `/v1/databases`.
pub async fn functions(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Query(q): axum::extract::Query<LocalQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // `fleet_function_stats` (the billing meter) reads this endpoint on every
    // peer over a signed node-to-node delegation with an EMPTY tenant, which
    // `resolve_tenant` maps to "personal" — so the meter was only ever counting
    // each peer's personal-tenant functions and silently UNDER-COUNTING org
    // compute. A verified `service` delegation asking for its own local slice is
    // the internal aggregation path, so it gets the unfiltered view; everything
    // else stays tenant-scoped exactly as before.
    let internal = claims
        .as_ref()
        .map(|e| e.0.role == "service")
        .unwrap_or(false);
    let mut list: Vec<Value> = c
        .fluid
        .stats()
        .into_iter()
        .filter(|f| (internal && q.local == Some(true)) || norm(&f.tenant) == t)
        .map(|f| json!(f))
        .collect();
    // `c.fluid` is THIS node's in-process runtime, but functions run wherever
    // the placement scheduler put them — so the dashboard, polling through the
    // round-robin, kept landing on a node hosting none of the tenant's
    // functions and rendering an empty Functions page and zero usage. The
    // `local=true` guard is required: `fleet_function_stats` also reads this
    // endpoint on every peer, and without it the fan-out would recurse.
    if q.local != Some(true) {
        let peers = all_healthy_peers(&c);
        for v in fan_out_peers(&c, &peers, &t, "/v1/functions?local=true").await {
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
    }
    Json(json!(list))
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
                    "per_name_holds": crate::vercel_dns::STATS.per_name_holds.load(std::sync::atomic::Ordering::Relaxed),
                    // Host labels currently pinned straight to their owning node
                    // (deployment affinity) rather than falling through to the
                    // all-nodes wildcard — 0 means every request is still a coin
                    // flip across the fleet.
                    "affinity_records": crate::vercel_dns::STATS.affinity_records.load(std::sync::atomic::Ordering::Relaxed),
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

/// Serverless GPU pool snapshot (operator-only, same guard as `/v1/tunnels` /
/// `/v1/relay`): every healthy `gpu_count > 0` node grouped into a named pool
/// by region, with live aggregate + per-node free VRAM. See `gpu_pool`'s
/// module doc for the allocate/release discipline and the fan-out rationale
/// (per AGENTS.md's round-robin doc — live instance counts are node-local).
async fn gpu_pools(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let regions = crate::gpu_pool::snapshot(&c).await;
    Ok(Json(json!(regions)))
}

/// Managed-inference endpoint listing (operator): every project with an
/// inference spec, its deterministic coordinator/port/URL, plus THIS node's
/// own live server statuses (authoritative only for endpoints coordinated
/// here — the listing names the coordinator so an operator knows where the
/// authoritative status lives).
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

/// Live memory accounting (operator). The measurement that was MISSING during
/// the 2026-08 fc-hongkong OOM hunt: the heap profile said live sampled heap was
/// ~80 MB while RSS was ~1.09 GB, and there was no way to read jemalloc's own
/// `stats.allocated/active/resident/retained` to tell a leak from dirty-page
/// fragmentation — `stats_print:true` only writes at exit, and the process is
/// SIGKILLed, so the atexit hook never runs.
///
/// Unlike `/v1/debug/heap` this is CHEAP (no sampling, no dump) and safe to
/// poll, so it is also the right thing for a monitor to scrape. It reports, on
/// one document: the OS view (RSS, thread count), the allocator view, and every
/// tenant-driven growth bound this node enforces as a live gauge — a bound
/// nobody can read is indistinguishable from no bound at all.
///
/// NODE-LOCAL, like `/v1/dns/stats`: `/proc/self` and this process's own
/// allocator. Through the dashboard's `/ops/*` proxy you are reading the
/// LEADER's memory, not the page-serving node's.
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

/// Restart audit (operator): did this node die, when, and why.
///
/// The question that had no answer. A cgroup OOM kill is SIGKILL — the dying
/// process cannot log it — and systemd has the node back in under a second, so
/// the platform's own view showed only "a node that is up". A node cycling
/// every 2-3h was investigated for a whole session as "random unhealthy nodes",
/// because every symptom (probe failures, dropped trunks, placement gaps) was
/// visible and the cause was not.
///
/// Returns this boot's verdict with the EVIDENCE it was reached from (kernel
/// line, cgroup counter, boot id, last-heartbeat RSS against the memory
/// ceiling), the bounded restart history with each previous process's uptime —
/// the list that makes a 2-3h cycle self-evident rather than an inference —
/// and the live 24h counters.
///
/// NODE-LOCAL, like `/v1/dns/stats`: the marker file, the history file and
/// `/proc` all belong to the node answering. Through the dashboard's `/ops/*`
/// proxy you are reading the LEADER's restarts; the fleet view is the gossiped
/// `NodeInfo::started_ms` / `oom_restarts_24h` / `last_oom_ms` on `/v1/nodes`.
async fn node_restarts(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    Ok(Json(crate::restart_audit::snapshot(&c.node_name)))
}

/// Health-demotion guard (operator): how often THIS observer withdrew a peer
/// from DNS/placement, how often it declined to because the peer was still
/// gossiping, and which peers are currently in a local routing penalty.
///
/// `refused_gossip_alive` is the number to read during an incident: a large
/// value means this node's transports are failing against peers that are
/// demonstrably alive, i.e. the fault is local — and before the guard existed
/// every one of those would have been a fleet-visible withdrawal (see
/// health.rs). NODE-LOCAL, like `/v1/dns/stats`: a health verdict is
/// per-observer, so this is deliberately one observer's counters.
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

/// Live heap profile (operator). Answers "which allocation site is growing?"
/// on a node that is ALREADY misbehaving, which is the question nothing on the
/// fleet could answer during the 2026-07 fc-sanjose OOM (RSS ~12.9GB anon
/// before the kernel killed it, never root-caused).
///
/// `GET /v1/debug/heap` returns a jemalloc heap profile in jeprof's native
/// format — analyze with the `jeprof` that ships alongside jemalloc, e.g.
/// `jeprof --show_bytes --pdf /path/to/hive-cloud heap.prof`, or diff two with
/// `jeprof --base=first.prof <binary> second.prof`. Sampling is OFF at boot
/// (`prof_active:false` in main.rs's malloc_conf) so there is no steady-state
/// cost; this handler turns it on for the process on first call and leaves it
/// on, since a leak hunt needs the allocations that happen AFTER activation.
///
/// Intended use is a diff, not a single dump: call once to start sampling,
/// wait while RSS climbs, call again, and compare the two profiles — the growth
/// is what identifies the leak, whereas one snapshot mostly shows normal
/// steady-state usage. `?deactivate=1` turns sampling back off.
///
/// Linux-only: the fleet is Linux and jemalloc profiling is unavailable on
/// macOS, where this returns 501 rather than failing the build.
#[cfg(target_os = "linux")]
async fn heap_profile(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;
    require_operator(claims.as_ref().map(|e| &e.0))?;

    let want_off = q.get("deactivate").is_some_and(|v| v == "1" || v == "true");
    // `prof.active` is a bool mallctl; `prof.dump` takes a NUL-terminated path.
    // Both are only present when the binary was built against a jemalloc with
    // profiling enabled (see main.rs's malloc_conf) -- on a build without it,
    // these return ENOENT, which is reported rather than silently ignored.
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

    // Dump to a unique path so concurrent operator calls can't clobber each
    // other, then read it back and delete it -- the profile is returned in the
    // response body, never left on disk.
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

/// Geo-DNS observability (operator): live Seer query counters, the
/// tailored-vs-generic split that actually tells you whether proximity routing
/// is working, the local geo table's identity and hit rate (plus the optional
/// remote fallback's memo state and its on-disk persistence counters), the
/// delegation record count the DNS reconciler published, and the per-node
/// histogram of which address each answer handed out first.
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
            // The local prefix table: which copy is loaded and how big it is.
            // `source: "none"` means it failed validation and every answer here
            // is generic — the one state worth alerting on.
            "table_source": table_source,
            "table_rows_v4": table_v4,
            "table_rows_v6": table_v6,
            "local_hits": geo.local_hits,
            "local_misses": geo.local_misses,
            // The optional remote fallback. Disabled unless an operator set
            // HIVE_DNS_GEO_ENDPOINT, in which case these count its memo.
            "remote_enabled": geo.remote_enabled,
            "remote_known": geo.remote_known,
            "remote_pending": geo.remote_pending,
            "remote_unlocatable": geo.remote_unlocatable,
            // Durability of the remote memo: how many prefixes came back off
            // disk at boot (0 after a first-ever boot OR a wiped/corrupt file —
            // the signal that this node is re-warming from scratch) and how
            // many debounced saves have run since.
            "cache_loaded_at_boot": loaded_at_boot,
            "cache_writes": cache_writes,
            "cache_file": crate::dns_geo::cache_path().display().to_string(),
        },
        "delegation_records": crate::vercel_dns::STATS.geo_delegation_records.load(Ordering::Relaxed),
        // Prove-before-advertise, made visible. `nameservers` is the SAME
        // verdict the DNS reconciler publishes from (`validate_nameservers`
        // over the same gossiped registry) — an operator asking "why is this
        // node not in the NS set?" gets the actual reason, plus who attested
        // it and from which regions, rather than having to infer it. `probes`
        // is this node's OWN raw evidence, which is what makes a disagreement
        // between vantages diagnosable instead of mysterious.
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
            // Derived from the verdicts computed FRESH in this response, never
            // the reconciler-written counters: those are stamped only inside
            // the DNS reconcile loop, so on a non-leader node (or on the
            // leader between passes / right after a restart) they lag and
            // CONTRADICT the per-node verdicts in the same payload —
            // live-witnessed as `proven: 0` above eight `validated: true`
            // rows. The counters stay exported below as the reconciler's own
            // last-pass view (`reconciler_last_pass_*`), clearly named so a
            // stale number can no longer masquerade as live truth.
            "proven": verdicts.iter().filter(|v| v.validated).count(),
            "unproven": verdicts.iter().filter(|v| v.declared && !v.validated).count(),
            "reconciler_last_pass_proven": crate::vercel_dns::STATS.geo_ns_validated.load(Ordering::Relaxed),
            "reconciler_last_pass_unproven": crate::vercel_dns::STATS.geo_ns_unproven.load(Ordering::Relaxed),
            "delegation_holds": crate::vercel_dns::STATS.geo_delegation_holds.load(Ordering::Relaxed),
            "api_delegation_holds": crate::vercel_dns::STATS.api_delegation_holds.load(Ordering::Relaxed),
            "create_circuit_skips": crate::vercel_dns::STATS.create_circuit_skips.load(Ordering::Relaxed),
            // Never-dark cutover telemetry: cutovers completed, rollbacks
            // (a climbing rollback count = a delegation RETRYING, never
            // stranding), and ACME orphan challenges swept.
            "delegation_cutovers": crate::vercel_dns::STATS.delegation_cutovers.load(Ordering::Relaxed),
            "delegation_cutover_rollbacks": crate::vercel_dns::STATS.delegation_cutover_rollbacks.load(Ordering::Relaxed),
            "acme_orphans_swept": crate::vercel_dns::STATS.acme_orphans_swept.load(Ordering::Relaxed),
            "min_attester_regions": crate::dns_probe::MIN_ATTESTER_REGIONS,
            "failed_rounds_before_withdraw": crate::dns_probe::FAILED_ROUNDS_BEFORE_WITHDRAW,
        },
        "answer_first_histogram": answers,
    })))
}

/// Mesh address-lookup observability (operator): which address SOURCES this
/// node actually registered, and what the public mainline-DHT provider has
/// done since boot.
///
/// This exists because "the DHT is enabled" is a config claim while "41
/// resolves, 12 hits, 3 publishes requested" is evidence — without it the
/// mechanism is unfalsifiable in production. It is also the fastest way to see
/// the condition that motivated the DHT in the first place: a node whose
/// `seed_providers`, `pkarr_providers` and `n0_enabled` are all zero/false has
/// NO address source at all, so `PeerPool::dial_fresh` has nothing to consult
/// and burns its budget for nothing.
///
/// **Node-local by construction, like `/v1/dns/stats`.** Every field is read
/// from this process's own counters; there is no fleet-wide answer and none is
/// implied. Note the consequence for the dashboard: `/ops/*` forwards to the
/// control-plane LEADER, so a browser reading this through the dashboard sees
/// the leader's numbers, not those of the node serving the page. Query a
/// specific node's admin port directly to ask about that node.
async fn mesh_discovery(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let s = hive_p2p::dht::stats();
    let sources = usize::from(s.n0_enabled) + s.seed_providers + s.pkarr_providers;
    Ok(Json(json!({
        "node": c.node_name,
        // This node's own 64-hex id: the argument another host passes to
        // `--dht-probe` to prove it can find THIS node from cold.
        "endpoint_id": c.iroh.read().as_ref().map(|e| e.id().to_string()),
        // The pre-DHT sources, so "this node had nowhere to look" is a reading
        // rather than an inference.
        "sources": {
            "bootstrap_seeds": s.seed_providers,
            "pkarr_relays": s.pkarr_providers,
            "n0_public": s.n0_enabled,
            "mainline_dht": s.dht_registered,
            // Sources OTHER than the DHT. 0 is the state that made a wiped
            // node permanently dark on 12 of 14 fleet hosts.
            "non_dht_total": sources,
        },
        "dht": {
            "registered": s.dht_registered,
            // Populated only while NOT registered — names the exact reason
            // (disabled by env, bootstrap unresolvable, socket conflict), so a
            // silently-inert feature is impossible to mistake for a live one.
            "skip_reason": s.dht_skip_reason,
            "bootstrap_nodes": s.dht_bootstrap_nodes,
            // 0 = the crate default (try 6881, fall back to an ephemeral port).
            "port": s.dht_port,
            "publish": s.dht_publish,
            // false ⇒ only this node's relay URL is on the public DHT; true ⇒
            // its globally-routable direct address is too (never its RFC1918
            // ones — see hive_p2p::dht's publish filter).
            "publish_direct": s.dht_publish_direct,
            // "requested", not "succeeded": AddressLookup::publish is
            // fire-and-forget by contract, so a success count here would be a
            // claim with no witness.
            "publishes_requested": s.publishes_requested,
            "last_publish_request_ms": s.last_publish_request_ms,
            "resolves": s.resolves,
            "resolve_hits": s.resolve_hits,
            "resolve_misses": s.resolve_misses,
            "resolve_errors": s.resolve_errors,
        },
        // The budget dial_fresh gives the whole address-lookup set. Measured
        // steady-state DHT resolve is 2.0-4.6s on top of a cold-routing-table
        // warm-up, so a value at or below ~4s means the DHT provider is
        // registered but is being cancelled before it can answer.
        "discovery_budget_ms": hive_p2p::dial_discovery_budget().as_millis() as u64,
        "dial_fallback_ceiling_ms": hive_p2p::dial_fallback_ceiling().as_millis() as u64,
    })))
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

pub(crate) async fn logs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<LimitQ>,
) -> Json<Value> {
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
        let hosts: std::collections::HashSet<String> =
            c.gw.list()
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
            hosts.iter().any(|ph| {
                h == *ph || h.starts_with(&format!("{ph}.")) || ph.starts_with(&format!("{h}."))
            })
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
            let label = h
                .split(':')
                .next()
                .unwrap_or(&h)
                .split('.')
                .next()
                .unwrap_or(&h);
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
            b.get("ts_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                .cmp(&a.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0))
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
    Ok(Json(
        json!({ "managed": c.waf.managed_enabled(), "rules": c.waf.rules() }),
    ))
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
    Ok(Json(
        json!({ "hits": hits, "misses": misses, "stale": stale, "entries": entries, "hit_ratio": ratio }),
    ))
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

// ---- Cron ----

pub(crate) async fn cron_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<LocalQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Tenant-scoped: only this team's jobs. Legacy jobs (no tenant recorded) are
    // attributed by their target project's owning team.
    // Dedup by id here too (`seen`): defends the merged view against a store
    // that still holds pre-fix duplicate jobs (same id) until every node has
    // restarted onto the deduping restore path — without it a node's own
    // duplicates would show while a peer fanning them in would collapse them,
    // leaving counts that never converge across the fleet.
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
    // FLEET FAN-OUT (read-only): cron jobs are node-local — a `vercel.json` cron
    // is registered on the node that BUILT the deployment (git.rs), and a manual
    // job on the control-plane leader; neither replicates. So a dashboard read
    // that lands on any single node shows only that node's slice. Merge every
    // healthy peer's local list (dedup by job id) so the operator sees the whole
    // fleet's schedule. Deliberately a READ merge, NOT store replication +
    // leader-only execution: each node keeps firing exactly its own jobs, so
    // this can never double-fire or drop a follower-hosted deployment's cron.
    // `?local=true` (the internal fan-out marker) short-circuits the recursion.
    if !q.local.unwrap_or(false) {
        // `seen` already holds this node's own (deduped) job ids from above.
        for v in fan_out_peers(&c, &all_healthy_peers(&c), &t, "/v1/cron?local=true").await {
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
    // Stamp the caller's tenant (never trust a tenant in the body) and require
    // the target project to belong to them.
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
    // See project_git_ci_put's comment: claim an unowned row rather than
    // leaving it UNTAGGED_TENANT once require_project has verified the caller.
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    c.projects.set_team(&project, &t);
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

/// Shared dispatch for the four run operations. Resolves the project (given or
/// auto-scanned), checks tenant ownership, then runs the op on the project's
/// HOST node (env decrypts locally): local when we host it, else forwarded over
/// the iroh mesh with `local=1` so the host runs it without re-forwarding.
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

    // 1) Resolve the project that holds this run.
    let project = if let Some(p) = body.project.clone() {
        p
    } else {
        // Auto-scan: which of this tenant's world-backed projects has the run?
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
            // Not on this node — fan the op out to peers hosting this tenant's projects.
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
    // 2) Ownership.
    if !wf_in_team(c, &project, &team) {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    // 3) If this project isn't hosted locally, forward to its host node.
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
    // 4) Run it locally against the project's world.
    match crate::world::run_op(c, &project, id, op, cancel_reason.as_deref()).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// POST a JSON body to a host node by NAME: HTTP admin POST if we know its URL,
/// else the iroh mesh (`GOSSIP_POST` — same body/path dispatch). The
/// body-carrying, POST-verb counterpart of `post_to_host`/`put_to_host`, used
/// to forward run operations to a project's hosting node.
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
        // Carry the internal node-to-node trust token, when configured, so a
        // handler gated by `require_operator_or_internal` (run events/
        // attributes) still passes on this forwarded hop, which never carries
        // the caller's Authorization header. Inert extra header for every
        // other forwarded route, which doesn't check it.
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

/// Body for the operator-only generic run-event append, matching the real
/// `@workflow/world` spec's `events.create(runId, data, params)` shape:
/// `eventType` = the event name (required — this is what makes the endpoint
/// GENERIC, unlike the three hardcoded call sites in [`crate::world::run_op`]),
/// `eventData` = its payload (spec's `data`), `correlationId` = spec's
/// `params.correlationId`.
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

/// Body for the operator-only run-attributes merge, matching the real spec's
/// `experimentalSetAttributes(runId, changes, options)`: `attributes` = the
/// spec's `changes`, merged (top-level keys, overwriting) into the run's
/// stored `attributes` object.
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

/// Shared dispatch for the operator-only "append arbitrary event to a run"
/// op. Mirrors [`wf_run_op_dispatch`]'s project-resolution + host-forward
/// shape exactly (this is the same node-local world data cancel/replay/
/// wakeup mutate — see AGENTS.md's round-robin-reads-vs-leader-forwarded-
/// writes section), but gated by [`require_operator_or_internal`] instead of
/// tenant ownership alone: this is a manual incident-recovery primitive for
/// platform operators, not a tenant self-service action like the 3-dots menu.
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

    // 1) Resolve the project that holds this run — identical shape to
    // `wf_run_op_dispatch`'s auto-scan/fan-out.
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
    // 2) Ownership.
    if !wf_in_team(c, &project, &team) {
        return Err((StatusCode::NOT_FOUND, "run not found".into()));
    }
    // 3) If this project isn't hosted locally, forward to its host node.
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
    // 4) Run it locally against the project's world.
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

/// Shared dispatch for the operator-only "merge run attributes" op — same
/// shape as [`wf_run_event_dispatch`] (and [`wf_run_op_dispatch`]), routed to
/// [`crate::world::merge_run_attributes`] instead.
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

pub(crate) async fn wf_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Workflows are ingested on the node that HOSTS a deployment. If this project
    // was placed on a peer, proxy to that node so its workflows show up.
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
    // Global view (no project filter): merge in workflows registered on peer nodes
    // that host this tenant's projects — they're ingested on the HOST, so the
    // coordinator otherwise shows none for scheduler-placed projects.
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
        for v in fan_out_peers(
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
    // The workflow's project must belong to the caller's team.
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

pub(crate) async fn wf_runs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Cache this endpoint's result for a couple seconds — it's "the most
    // expensive read on the dashboard" per the comments below (fleet fan-out
    // PLUS per-project Upstash world reads on every call), so collapsing
    // concurrent/rapid requests from multiple tabs/team members matters more
    // here than almost anywhere else. Never cache the inner `local=true` hop.
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
        // Inner `local=true` hop: run inline, never cached, never coalesced —
        // its caller (a peer's own aggregation) carries the budget.
        return Json(wf_runs_collect(c, team, q).await);
    }
    // Top-level: the aggregation runs on a DETACHED task, coalesced per cache
    // key. Two reasons, both live-witnessed as "Error loading runs" on the
    // dashboard: (1) the dashboard's own world proxy aborts this request at
    // 20s, and axum then DROPS this handler future mid-await — with the
    // caches written only at handler completion, an aborted poll starved
    // `resp_cache` AND the last-known-good belt forever, so EVERY subsequent
    // poll re-ran the full slow path and aborted identically (steady-state
    // failure, not a transient). Detaching means the computation always
    // finishes and always writes the caches — one slow poll instead of an
    // endless series. (2) Coalescing means five aborted-and-retried polls
    // spawn ONE aggregation, not five (the dc8286f amplification rule).
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
                    // The map entry MUST clear even if the collector panics:
                    // a leaked entry's dead sender makes every later poll fall
                    // through to the uncached inline path forever — the exact
                    // steady-state starvation this task exists to prevent.
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
    // STALE-WHILE-REVALIDATE: with a computation already in flight, a recent
    // last-known-good answer beats blocking this poll on the aggregation — the
    // dashboard polls every few seconds, so it picks the fresh rows up on the
    // next tick from the cache the detached task writes. Only the genuinely
    // FIRST poll (nothing known yet) waits out the computation.
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
            // Compute task died (panic) — degrade to the collector inline.
            return Json(wf_runs_collect(c, team, q).await);
        }
    }
}

/// The actual aggregation `wf_runs` serves — local workflow store + per-project
/// Upstash world reads + peer fan-out, each slow phase under its OWN budget so
/// the worst case stays inside the dashboard's 20s client abort (the phases'
/// dial-resilience budgets — 10s HTTP + 20s iroh per peer, 20s per world read —
/// are correct for one-shot control operations and structurally wrong for a
/// polled read: this endpoint is re-asked every few seconds, so a phase that
/// misses one poll's budget simply contributes to the next poll). Writes the
/// LAST-KNOWN-GOOD belt (and consults it for empties) itself, so completion —
/// not the HTTP caller's patience — is what populates it.
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
    // Scoped request for a project with a locally-readable, tenant-owned WORLD
    // (env is gossiped, so this is true on EVERY node): never depend on the
    // mesh forward for the world rows — the forward path returned a false-[]
    // on any fan-out hiccup, live-witnessed as the project tab's runs table
    // flickering to empty mid-poll even after the unscoped fix. The direct
    // local world read below covers it; a best-effort host merge (dedup'd)
    // still picks up the host's engine-local rows when reachable.
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
    // Append the REAL runs the deployed app executed, read from its WDK "world"
    // store. Only for projects HOSTED on THIS node (env_map decrypts locally); the
    // coordinator gets these via the per-project / `local=true` proxy paths above.
    {
        let mut locals: Vec<String> = match q.project.as_deref() {
            // Direct world read whenever this node CAN (local deployment
            // record OR gossiped world env + ownership) — see
            // scoped_world_local above for why the mesh must not be the only
            // source of a scoped table's rows.
            Some(p) if c.gw.git_for_project(p).is_some() || scoped_world_local => {
                vec![p.to_string()]
            }
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
                // ALSO every tenant-owned project from the GOSSIPED settings
                // store whose env carries a world config. World reads go over
                // the project's own REST URL, which works from ANY node — but
                // deriving the list from node-local deployment records alone
                // meant only the node(s) that ran the deploy read the world
                // directly; every other node answered the console's poll from
                // the iroh fan-out, and one mesh hiccup turned a populated
                // runs table into a false-[] on the next poll. Live-witnessed
                // as the "rows load then flicker and disappear" report:
                // local=true returned 8 rows on the leader and 0 on all six
                // other public-serving nodes.
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
        locals.retain(|p| crate::world::has_world(&c, p));
        // Per-project Upstash world reads run CONCURRENTLY — a tenant with many
        // projects previously paid one Upstash round-trip per project, one at a
        // time (this function's own doc/comment history calls it "the most
        // expensive read on the dashboard"). join_all bounds the added latency to
        // the slowest single project's read instead of the sum of every project.
        // Budgeted: a single slow/hung Upstash world must cost this POLL its
        // rows for that project, never the whole endpoint its deadline.
        if let Ok(all) = tokio::time::timeout(
            PHASE_BUDGET,
            futures::future::join_all(locals.iter().map(|p| crate::world::list_runs(&c, p, 100))),
        )
        .await
        {
            for wruns in all.into_iter().flatten() {
                runs.extend(wruns);
            }
        } else if !locals.is_empty() {
            tracing::warn!(
                team,
                projects = locals.len(),
                "wf_runs: world reads exceeded the phase budget — serving without them this poll"
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
        // Budgeted PER PEER, never as one timeout over the whole join: a
        // single dark peer used to trip the outer budget and silently discard
        // every HEALTHY peer's rows too — fast but permanently incomplete,
        // and the incomplete (non-empty) result then overwrote last-known-
        // good. Per-peer budgets bound the wall clock to one budget (the
        // fetches run concurrently) while each healthy peer's rows land.
        let peer_results: Vec<Value> = futures::future::join_all(peers.iter().map(|node| {
            let c = c.clone();
            let team = team.clone();
            async move {
                tokio::time::timeout(
                    PHASE_BUDGET,
                    fetch_from_host(&c, node, "/v1/workflows/runs?local=true", &team),
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
        // Scoped read served by the DIRECT local world (scoped_world_local)
        // on a node with no local deployment record: the host node's
        // engine-local rows for this project aren't in `runs` yet. Merge them
        // best-effort (dedup'd) — a mesh miss here degrades to world-rows-only
        // (still populated), never to an empty table, which is the whole
        // point of the scoped-flicker fix.
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
    // LAST-KNOWN-GOOD belt: the console polls this endpoint and REPLACES its
    // table with whatever comes back, so a single transiently-failed world
    // read (downstash hiccup, mesh fan-out miss, provider timeout — all
    // swallowed into an empty vec upstream) rendered as rows-flicker-then-
    // disappear. If THIS poll came up empty but a recent poll for the same
    // cache key had rows, serve those instead: a transient outage degrades to
    // seconds-stale, never to false-empty. Genuine emptiness still wins once
    // the hold expires (WF_LAST_GOOD_TTL), so a truly-cleared world shows
    // empty within a minute rather than pinning stale rows forever.
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
    // Candidate hosts queried CONCURRENTLY — a project hosted on the LAST
    // candidate used to cost one sequential hop per candidate before this fix.
    if let Some(project) = q.project.as_deref() {
        if !q.local.unwrap_or(false) {
            let hosts = host_nodes_for_project(&c, project);
            let path = format!("/v1/workflows/runs/{id}?project={project}&local=true");
            if let Some(v) = fan_out_peers(&c, &hosts, &team, &path)
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
    let world_locals: Vec<String> = locals
        .into_iter()
        .filter(|p| crate::world::has_world(&c, p))
        .collect();
    // Concurrent per-project Upstash world reads (was one sequential hop per
    // locally-hosted project — same "most expensive read" class as wf_runs).
    let details = futures::future::join_all(
        world_locals
            .iter()
            .map(|p| crate::world::run_detail(&c, p, &id)),
    )
    .await;
    if let Some(detail) = details.into_iter().flatten().find(|d| found(d)) {
        return Ok(Json(detail));
    }
    // 3) Fleet: ask peers hosting this tenant's projects to resolve it (over iroh), concurrently.
    if !q.local.unwrap_or(false) {
        let peers = peer_nodes_for_tenant(&c, &team);
        let path = format!("/v1/workflows/runs/{id}?local=true");
        if let Some(v) = fan_out_peers(&c, &peers, &team, &path)
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
        e.0 += 1; // created
        match r.status {
            hive_edge::workflows::RunStatus::Succeeded => e.1 += 1,
            hive_edge::workflows::RunStatus::Failed => e.2 += 1,
            hive_edge::workflows::RunStatus::Running | hive_edge::workflows::RunStatus::Pending => {
                e.3 += 1
            }
            _ => {}
        }
    }
    // Merge peer rollups (the tenant's projects placed on other nodes). A project
    // lives on one host, so per-project rows don't overlap; sum defensively.
    if !q.local.unwrap_or(false) {
        let peers = peer_nodes_for_tenant(&c, &team);
        for v in fan_out_peers(&c, &peers, &team, "/v1/workflows/summary?local=true").await {
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

/// List workflow HOOKS for the observability console's Hooks tab. Hooks live ONLY
/// in the deployed app's WDK "world" store (there's no engine-side source the way
/// runs have one), so this reads them from every project HOSTED on this node and
/// fans out to the peers hosting this tenant's other projects — the same
/// tenant-scoping / project-filtering / `?local=` fleet-merge shape as `wf_runs`.
/// `?runId=` scopes the read to a single run.
pub(crate) async fn wf_hooks(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<WfQuery>,
) -> Json<Value> {
    let team = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Same short-TTL collapse as wf_runs (fleet fan-out + per-project Upstash world
    // reads). Never cache the inner `local=true` hop.
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
    // Cross-region project: proxy to the node that HOSTS it — its world env is
    // decrypted there (same per-project `local=true` proxy path as wf_runs).
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
    // Hooks from the WDK world of each project hosted on THIS node (env_map decrypts
    // locally); the coordinator gets remote ones via the proxy / fan-out paths.
    let mut hooks: Vec<Value> = Vec::new();
    {
        let locals: Vec<String> = match q.project.as_deref() {
            Some(p) if c.gw.git_for_project(p).is_some() => vec![p.to_string()],
            Some(_) => vec![],
            None => {
                let mut s: std::collections::HashSet<String> = std::collections::HashSet::new();
                for d in c.gw.list() {
                    // Authoritative deployment-record tag, not the node-local project row.
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
        // Per-project Upstash world reads run CONCURRENTLY (same "most expensive
        // read" class as wf_runs) — bound the added latency to the slowest project.
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
        for v in fan_out_peers(&c, &peers, &team, &path).await {
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
    // Newest-first across the merged fleet view.
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
    // Only the owning team may trigger a workflow run.
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

async fn sandbox(
    State(c): State<Arc<CloudState>>,
    Json(req): Json<SandboxReq>,
) -> Json<SandboxResp> {
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
    // An exact-slug creation targets a specific pre-existing tenant id (e.g. one
    // already used by real projects/billing) — that's an operator-level action,
    // not the normal self-serve "create my own org" flow every signed-in user can
    // do, so gate it the same way every other cross-tenant admin write is gated.
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
    // Tier lives in both TeamStore and BillingStore. Creation is not a special
    // third write path: initialize the billing half through the same helper every
    // later upgrade/downgrade uses.
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
    // Only an Owner-rank caller may grant Owner to someone else — an Admin
    // adding/promoting a member to Owner would be a lateral privilege
    // escalation within the team.
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
    // Seat quota (business locking): block adding a NEW member past the plan's seat
    // limit. Updating an existing member's role is always allowed.
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
    // Revoke the ex-member's push/SMS registrations for THIS team so they stop
    // receiving its notifications immediately (rows capture the tenant at
    // registration and the dispatcher would otherwise keep fanning to them
    // forever — see push.rs). Resolve email -> Clerk user id via the identity
    // mirror (push rows are keyed by user id, members by email).
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
    // Commit the membership and notification purge before the first await.
    // Client cancellation while relay revocation is in flight must not restore
    // the removed member on the next process restart.
    crate::persist::persist(&c);
    // TeamStore membership is email-keyed while browser grants are subject-id
    // keyed. Revoke the whole team fail-closed; remaining members renew from a
    // fresh platform session, while the removed member loses service now.
    crate::browser_admission::revoke_team(&c, &slug).await;
    // Revocation mutates its own persisted admission state after the first save.
    crate::persist::persist(&c);
    Ok(Json(json!(t)))
}

/// Apply a tier change to a tenant EVERYWHERE it is stored.
///
/// A tenant's tier lives in two places that are read for different things:
/// `c.teams` (feature gating via `team_plan()`) and `c.billing` (project/seat
/// quotas, the `can_deploy` credit lock, and everything the billing UI shows).
/// Four separate call sites used to write only the billing half — the free-plan
/// checkout shortcut, Stripe `customer.subscription.deleted`, the operator
/// grant, and checkout confirmation — so the two drifted apart silently and a
/// tenant could read Enterprise for features while being quota-limited as
/// Hobby. Every tier change goes through here so that cannot happen again.
///
/// `teams.set_plan` returning `None` is normal, not an error: personal
/// namespaces (`personal`, `u_<uid>`) have a billing account but no team row.
pub(crate) fn apply_plan_everywhere(c: &Arc<CloudState>, tenant: &str, plan: &str) {
    c.teams.set_plan(tenant, plan);
    c.billing.set_plan(tenant, plan);
    // Downgrades drop Enterprise-only SSO, matching `team_set_plan`.
    if !crate::billing::plan_allows_sso(plan) {
        c.teams.set_sso(tenant, false);
    }
}

/// Lock a platform-admin's tenant at enterprise, permanently. Wraps
/// `apply_plan_everywhere` (both halves) with an unbypassable tier FLOOR via
/// `set_tier_lock` so no automatic downgrade path (free-plan checkout, Stripe
/// `customer.subscription.deleted`) can drop it back below enterprise — the
/// "no matter what" the admin-always-enterprise feature requires. Called from
/// `mint_token`, leader-only and idempotent (see its guards).
pub(crate) fn apply_admin_enterprise(c: &Arc<CloudState>, tenant: &str) {
    apply_plan_everywhere(c, tenant, "enterprise");
    // The FLOOR is what makes it durable: set_plan alone is overwritten by the
    // next downgrade, the lock is not (billing.rs set_plan honors it).
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

/// Delete a team. Owner-only, and deliberately refuses in the cases where
/// deleting would strand resources or break the caller's own identity:
///
/// * a PERSONAL namespace (`personal`, `u_<uid>`) is a user's own space, not a
///   team someone chose to create — removing it would leave that user with no
///   tenant at all;
/// * a team that still owns projects, because project records carry the team
///   slug and would be orphaned (delete or move the projects first).
///
/// The billing account is cleared alongside the team record so a deleted team
/// cannot leave a stray tier/quota row behind — the same
/// both-halves-or-neither rule `apply_plan_everywhere` exists to enforce.
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
    // The row+tombstone and billing removal are the authoritative deletion.
    // Persist before relay revocation can suspend and the request be cancelled.
    crate::persist::persist(&c);
    crate::browser_admission::revoke_team(&c, &s).await;
    c.audit
        .record(&s, "user", "team_delete", "team", &s, "team deleted");
    // Capture the admission revocation and audit append as a second generation.
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

/// Whether `project` belongs to NOBODY: its own tag is `__untagged__` AND no
/// deployment record anywhere names a real tenant either.
///
/// This state is a trap, not a curiosity. `require_project` matches the owner
/// tag against the caller's tenant and `__untagged__` matches none, while
/// `project_owned_by`'s optimistic fallback only applies when the project has NO
/// deployments at all — so an untagged project that HAS untagged deployments is
/// claimable by no account in existence. It stays invisible in every project
/// list while still holding its name platform-wide, which is exactly how
/// creating a project can fail on a duplicate name its owner cannot find
/// anywhere. Six projects sat in that state on 2026-08-08 (ctest,
/// internet-structure, mien-kamp, silly-cat-test, smsrelay, tokenhun) — one of
/// them proven the owner's by a database tagged `personal` under it.
///
/// Deliberately strict: if ANY deployment names a real tenant, that tenant owns
/// the project and this returns false, so adoption can never take a project away
/// from a live account.
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

async fn project_team_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<ProjectTeam>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Only the CURRENT owning team may move a project (or change its protection)
    // — EXCEPT that an UNOWNED project may be adopted by a platform operator.
    // Without that exception an orphaned project is frozen forever: no tenant
    // matches `__untagged__`, so no caller can pass the check that would let
    // them fix it. See `project_unowned`, which refuses the moment any
    // deployment names a real tenant.
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

// ============================ API keys ============================

#[derive(Deserialize)]
struct CreateApiKey {
    name: String,
    #[serde(default)]
    role: String,
}

async fn apikeys_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!(c
        .apikeys
        .list(&t)
        .iter()
        .map(|k| k.public())
        .collect::<Vec<_>>()))
}

async fn apikey_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<CreateApiKey>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let (key, token) = c.apikeys.create(&b.name, &t, &b.role);
    crate::persist::persist(&c);
    // The plaintext token is returned exactly once.
    let mut v = key.public();
    v["token"] = json!(token);
    Json(v)
}

async fn apikey_revoke(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Json<Value> {
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

// ---- Intelligent service graph (Issue #2) ----
//
// The algorithmically-derived graph of services actually consumed by a deployment
// (app/framework, bundled front+back nodes, workspace packages, databases). Scanned
// async on build AND lazily on-demand (below) so EXISTING + FAILED deployments —
// which predate the async scan — are backfilled the first time they're viewed.

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
    // Try records newest-first, scanning the first whose checkout still exists on
    // disk (a failed/newer redeploy may have a GC'd root while an older LIVE one
    // is still present + serving).
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
    // built the project. Fan out to every OTHER reachable peer CONCURRENTLY and
    // return the first that has it (each answers from its LOCAL store/scan +
    // team-checks → no re-proxy loop, no cross-tenant leak). Registry is the
    // reliable peer set (trunk-warmed). Concurrent rather than the old
    // first-match-wins sequential loop — a project hosted on the LAST-checked
    // node used to cost N sequential hops; now it costs one concurrent round.
    let self_name = c.node_name.clone();
    let peers: Vec<String> = c
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.name != self_name && n.iroh_addr.is_some())
        .map(|n| n.name)
        .collect();
    let path = format!("/v1/projects/{project}/service-graph");
    if let Some(v) = fan_out_peers(&c, &peers, &t, &path)
        .await
        .into_iter()
        .next()
    {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

// ============================ Webhooks ============================

async fn webhook_events() -> Json<Value> {
    Json(json!(crate::webhooks::ALL_EVENTS))
}

async fn webhooks_all(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    // Only the caller's own webhooks (the payload includes signing secrets, so
    // this must be tenant-scoped).
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let list: Vec<_> = c
        .webhooks
        .snapshot()
        .into_iter()
        .filter(|w| norm(&w.team) == t)
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
) -> Json<Value> {
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

async fn webhooks_for_project(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
) -> Json<Value> {
    // Don't expose another tenant's project webhooks (incl. their secrets).
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !project_owned_by(&c, &project, &t) {
        return Json(json!([]));
    }
    let list: Vec<_> = c
        .webhooks
        .list(Some(&project))
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

async fn webhook_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
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
    axum::extract::Query(q): axum::extract::Query<LocalQ>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_operator(claims.as_ref().map(|e| &e.0))?;
    let mut all: Vec<Value> = c
        .webhooks
        .deliveries(100)
        .into_iter()
        .map(|d| json!(d))
        .collect();
    // The delivery ring is in-process and NOT replicated, and dispatches fire
    // both from the leader (promote/delete/database/incident mutations) and
    // from whichever node ran a build (git.rs). So a single node's view is a
    // slice: the operator saw an empty panel and concluded webhooks were
    // broken when they had in fact delivered. `local=true` stops the recursion.
    if q.local != Some(true) {
        let peers = all_healthy_peers(&c);
        for v in fan_out_peers(&c, &peers, "", "/v1/webhooks/deliveries?local=true").await {
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

// ============================ Databases ============================

/// The leader's answer for a tenant-scoped databases path, or `None` when this
/// node IS the leader (so there is nothing above it to ask, and no recursion).
///
/// Database records are replicated, but replication is neither instantaneous nor
/// — measured live — always complete. A SQLite database created through the
/// leader was present on 6 of 11 nodes and ABSENT on the other 5 well after
/// creation; the five missing it were the nodes that had been restarting or
/// OOM-thrashing, and nothing backfilled them. Because `admin_ingress` serves
/// every GET **locally** while the public host is round-robin DNS, that made a
/// real, ready database 404 on roughly half of all requests — including its
/// `/credentials`, i.e. the connection string was simply unobtainable about half
/// the time. That is exactly the round-robin-reads-vs-leader-writes split
/// AGENTS.md names, and this is its documented owner/leader fallback.
async fn databases_from_leader(c: &Arc<CloudState>, path: &str, team: &str) -> Option<Value> {
    if c.is_control_plane_leader() {
        return None;
    }
    let leader = c.control_plane_leader();
    if leader.is_empty() || leader == c.node_name {
        return None;
    }
    fetch_from_host(c, &leader, path, team).await
}

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

    // MERGE the leader's view rather than replacing or short-circuiting on
    // "local is non-empty": a node holding SOME of the tenant's databases would
    // otherwise silently serve a short list, which reads as "my database was
    // deleted" rather than as the replication gap it is.
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
    // Deterministic order so a re-poll never reshuffles the rendered list.
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
    // Only expose databases for a project the caller's tenant actually owns.
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
    // Absent HERE is not absent everywhere — see `databases_from_leader`. The
    // leader re-runs its own tenant check, so this can never widen access.
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
    // Returns unmasked connection secrets — must be strictly tenant-scoped.
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some(d) = c.databases.credentials(&id) {
        if record_tenant(&d.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
        return Ok(Json(json!(d)));
    }
    // Same fallback as `database_get`, and it matters most here: this is the ONLY
    // way to obtain a database's connection string, so a replication gap made the
    // DSN unobtainable on any node that had not caught up. The leader applies the
    // identical tenant check before answering — the `proxy_to_owner` re-check
    // precedent — so proxying cannot leak another tenant's secrets.
    if let Some(v) = databases_from_leader(&c, &format!("/v1/databases/{id}/credentials"), &t).await
    {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

/// One-click "Open Studio" for a managed Supabase database: mint a short-lived,
/// nonce-bearing login ticket. The ticket is returned only in a no-store JSON
/// body; the dashboard POSTs it to the reserved database-host exchange path, so
/// no bearer enters a URL, history, referrer, or upstream Studio request.
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
            // Replication-gap fallback: the leader applied the identical
            // fail-closed tenant check before returning the record.
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

/// The env-var (KEY, VALUE, sensitive) triples a database injects into its owning
/// project so deployments reach it automatically — canonical names (DATABASE_URL,
/// REDIS_URL, …) for the common single-DB case PLUS name-prefixed copies that are
/// collision-free when a project has several DBs of the same kind. Pure — no side
/// effects — so create (apply) and delete (remove by key) agree on the key set.
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

const DATABASE_INCARNATION_PROVIDER_PREFIX: &str = "__hive_internal_project_incarnation_v1__:";

/// Carry lifecycle authority through the existing `ProvisionReq` mesh ABI.
///
/// The iroh dispatch arm deserializes the body into `ProvisionReq` before it
/// calls this module, so an additive top-level field would be discarded there.
/// `provider` is cosmetic and optional; a capability-gated internal sender
/// temporarily wraps its original value here, and the receiver restores it
/// before the record is created. Public callers can never make this marker
/// authoritative: only the internal provision handler decodes it, after its
/// peer/operator gate has passed.
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
    let project = req.project.clone();
    // Database admission is a project-lifecycle mutation: mint/resolve the
    // lifetime and insert the provisioning record while holding the same writer
    // project deletion owns. An empty project is deliberately unbound storage
    // and has no project lifetime to serialize against.
    let _lifecycle = if project.is_empty() {
        None
    } else {
        Some(crate::project_settings::lifecycle_write(&project).await)
    };
    // Only enforce project ownership when the project is ALREADY registered
    // (has a team of record) — provisioning a database for a brand-new
    // project name before its first deploy is a normal flow and must keep
    // working. An EXISTING project owned by a different tenant is rejected.
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
        // The internal target must be able to validate this exact identity even
        // if the request is cancelled before provisioning finishes.
        crate::persist::persist(&c);
    }
    // Idempotency: provision() itself always mints a fresh id with no dedup
    // check, so a client double-submit (double-click, a retry racing a slow
    // response) silently created exact duplicate databases — witnessed live,
    // three identical blob databases under one project, ~90s apart. A
    // same-tenant, same-project, same-name, same-kind, non-error database
    // already existing means this create is a retry of one that already
    // landed: return that database (unmasked, so the caller still gets
    // usable connection credentials) instead of minting another.
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
    // Region-aware placement: `provision()` used to always run on THIS node
    // (whichever one the admin request happened to land on after leader-
    // forwarding), so a tenant requesting `region=sao-paulo` from anywhere
    // got a database really provisioned wherever the control-plane leader
    // currently was — measured live: every Supabase database on the fleet
    // landed on `fc-sanjose` regardless of its requested region. Reuses
    // `schedule::place`'s existing health/region/reachability picker (the
    // same one function deploys already go through). Only overrides when a
    // region was actually requested and a healthy, reachable, public
    // candidate exists there; any miss (unknown region, nothing eligible)
    // falls through to self — this must never turn an empty placement into
    // a failed database create, only an occasionally-suboptimal one.
    let host_node = req
        .region
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .and_then(|region| {
            let regions = [region.to_string()];
            crate::schedule::place(&c, &regions, true, true, false, false, false, false)
                .into_iter()
                .next()
        })
        .map(|t| t.node)
        .unwrap_or_else(|| c.node_name.clone());

    if host_node != c.node_name {
        // The chosen node is a PEER, never this node: dispatch the actual
        // provisioning work there (podman containers only ever run where the
        // process that spawns them lives — relabeling `host_node` alone
        // without also routing the work there would create a record
        // claiming to live on a node that never actually got any
        // containers). Mirrors the `post_body_to_host` pattern this file
        // already uses for run-event forwarding. The target's own
        // `database_provision_local` handler does provisioning +
        // egress/replication/persist/webhooks locally and its result
        // replicates back fleet-wide through the existing `databases`
        // store-sync path (the same one that already carries every other
        // node's database records into this node's own `state.json`).
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
                // Adopt the authoritative record into THIS node's own local store
                // immediately, rather than waiting on `store_sync`'s replication
                // tick — otherwise a delete that arrives back on THIS node (the one
                // the client actually talks to) moments later finds nothing in
                // `c.databases` yet and silently no-ops while still reporting
                // `removed: true`, leaking the remote container. Witnessed live:
                // create dispatched sj->bkk, immediate delete issued on sj, bkk
                // container survived with no warn logged at all — the outer lookup
                // itself was returning `None`. `upsert_replica` is the existing
                // insert-or-replace-by-id primitive for exactly this shape.
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
        // Dispatch failed (node unreachable over both HTTP admin and mesh) —
        // fall through to local provisioning rather than failing the create
        // outright. A database on the WRONG node beats no database at all;
        // the same "admit and let the floor catch it" direction placement
        // already takes elsewhere in this codebase (AGENTS.md's disk-floor
        // section).
        tracing::warn!(
            node = %host_node,
            "database create: chosen region node unreachable, provisioning locally instead"
        );
    }

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
        &db.project,
        "database.created",
        json!({ "id": db.id, "name": db.name, "kind": db.kind }),
    );
    Ok(Json(json!(db)))
}

/// Internal-only mirror of [`database_create`]'s local-provisioning tail,
/// reached via [`post_body_to_host`] when that handler's own region-aware
/// placement chose a DIFFERENT node than the one that received the client's
/// request. Runs the exact same `provision()` + egress/replication/persist/
/// webhooks sequence, just always locally (never re-dispatches — the caller
/// already resolved placement, and re-resolving here on a `req.region` this
/// node might interpret differently would risk a dispatch loop). Gated by
/// [`require_operator_or_internal`]: the forwarded hop carries
/// `x-hive-internal`, never the original caller's Authorization header,
/// mirroring every other internal node-to-node forward in this file.
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

    // A cancelled coordinator request can be retried after this node already
    // admitted the first copy. Deduplicate under the same lifecycle writer so
    // the retry cannot mint a second database in this incarnation.
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
        &db.project,
        "database.created",
        json!({ "id": db.id, "name": db.name, "kind": db.kind }),
    );
    Ok(Json(json!(db)))
}

/// Best-effort teardown of a database's backing containers/volume/SQLite
/// file — the part of delete that only means anything ON THE NODE THAT
/// ACTUALLY HOSTS THEM. Split out of `database_delete` so it can run either
/// locally (the common case: this node IS `d.host_node`) or be dispatched to
/// the real owner over the mesh (`database_teardown_local`) when it isn't —
/// the exact gap `database_create`'s region-aware placement opened: before
/// that change every database's `host_node` was always the node handling the
/// request, so local-only teardown was correct by construction; a database
/// now genuinely living on a peer node would otherwise leak its containers
/// and (for Supabase) its data volume forever, since a podman `rm` for a
/// name that doesn't exist locally just silently no-ops — witnessed live the
/// first time this fix was tested: a deleted database's container kept
/// running on its real host node with no error anywhere.
async fn teardown_db_backing(d: &crate::databases::Database) {
    let id = &d.id;
    crate::databases::note_teardown_request(id);
    // Managed SQLite: no container, so the teardown is its own. Streams
    // first (each holds a pooled connection, and one mid-`BEGIN` holds the
    // file's write lock), then the pool, then the file — in that order, or
    // the unlink races a live writer.
    if d.kind == crate::databases::DbKind::Sqlite {
        crate::hrana::close_streams(id);
        crate::sqlite_pool::close(id);
        if let Some(path) = crate::databases::sqlite_file_path(id) {
            for suffix in ["", "-wal", "-shm"] {
                let p = if suffix.is_empty() {
                    path.clone()
                } else {
                    std::path::PathBuf::from(format!("{}{suffix}", path.display()))
                };
                if let Err(e) = std::fs::remove_file(&p) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(db = %id, path = %p.display(), error = %e, "could not remove SQLite database file");
                    }
                }
            }
        }
    }
    // Best-effort teardown of the backing container(s): the record's
    // comma-joined `container` field when set (a Supabase record carries its
    // whole stack db,meta,studio), else the platform-templated names by kind —
    // a delete that races an IN-FLIGHT provision reads `container` while it is
    // still None, and skipping there leaks the container the provision starts
    // seconds later (witnessed live: redis on fc-bangkok outlived its catalog
    // entry). `-v` is the podman lock-pool rule (anonymous volumes leak locks
    // forever); the stack's NAMED data volume is removed explicitly below.
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
        // Deleting the database IS deleting its data — the stack's named
        // volumes (engine + storage-api buckets) are platform-templated
        // (`hive-vol-supa[st]-<id8>`), never tenant-named, so this cannot
        // touch another volume.
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

/// Record-free teardown by platform template: kind is unknowable without the
/// catalog record, so cover both naming families (single-container
/// `hive-db-<id8>` and the Supabase trio + its named volume). Every removal is
/// idempotent best-effort; a name that never existed is a no-op.
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

/// Internal-only: run [`teardown_db_backing`] for `id` on THIS node. Reached
/// via [`post_body_to_host`] from `database_delete` when the record's
/// `host_node` is a peer. Gated by [`require_operator_or_internal`], same as
/// [`database_provision_local`].
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
        // The catalog delete's store_sync can beat this dispatch to the peer —
        // no record here means the old arm removed NOTHING and still answered
        // `torn_down`, which is exactly how the witness container kept running
        // after its catalog entry vanished. The backing names are platform
        // templates, so a missing record does not blind the teardown.
        tracing::warn!(
            db = %id,
            "database teardown-local: no local record (catalog removal replicated first) — tearing down by platform template"
        );
        teardown_db_backing_blind(&id).await;
    }
    Ok(Json(json!({ "torn_down": id })))
}

async fn database_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // Replication-lag guard, same reasoning as the inner `host_node`-empty
    // retry below but for the OUTER lookup itself: a database created via
    // `database_provision_local` on a PEER (this node's own region-aware
    // placement dispatch) has not necessarily replicated into THIS node's
    // local `databases` store yet by the time a fast-following delete
    // arrives — witnessed live: `get_raw` returned `None` here, so the
    // whole teardown block below (including its own host_node retry) never
    // ran at all, `remove_db_and_purge_data` no-op'd on an id it had never
    // heard of, and the API still answered `{"removed": true}` while the
    // real container kept running on the peer forever. A few hundred ms of
    // retry closes this for any delete within a couple of store-sync
    // rounds of the create; a delete on a database that never existed
    // still 404s after these attempts exhaust, unchanged from before.
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
        // A replication-lag guard for the delete-moments-after-create case:
        // if this node's own copy of the record has no `host_node` yet
        // (created on a peer via `database_provision_local`, whose write
        // hasn't finished propagating back here through the ordinary
        // `databases` store-sync path), briefly re-fetch rather than
        // immediately concluding "unknown, give up" — closes the race for
        // any delete landing within a couple of store-sync rounds of the
        // create, which is the overwhelming common case (a delete seconds
        // or minutes later already has a fully-replicated `host_node`).
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
            // NEVER treat an unset `host_node` as "must be local" — that is
            // exactly the trap that leaked a real container live: a delete
            // landing on a node whose replicated copy of the record hasn't
            // finished catching up from wherever `database_provision_local`
            // actually created it reads `host_node` as its zero-value, not
            // as the true owner, and the container silently outlives the
            // catalog entry with no error anywhere. Loud and honest instead:
            // this node genuinely does not know where the backing container
            // lives, so it cannot tear it down — surfaced so an operator can
            // find it via the orphan-reap sweep rather than trusting a
            // "removed: true" response that only removed the catalog row.
            tracing::warn!(
                db = %id,
                "database delete: host_node unknown on this node (stale replication?) — \
                 backing container/volume was NOT torn down; catalog entry removed regardless"
            );
        } else {
            // The containers/volume/file live on a PEER — dispatch there
            // rather than silently no-op'ing a local `podman rm` against a
            // name that was never created on this host. Best-effort: an
            // unreachable peer must not block the catalog delete below (the
            // customer asked for this database gone; a stranded container
            // on an unreachable node is a leak to catch via the normal
            // orphan-reap sweep, not a reason to refuse the delete).
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
    // Purges the actual queue/vector/blob PAYLOAD data too (keyed by a
    // team-prefixed connection name, not the db id) — `remove_db` alone only
    // ever removed the catalog entry, leaving customer data (which may
    // include PII in vector metadata or queue message bodies) orphaned in
    // memory/on-disk forever, re-persisted in every future snapshot.
    c.databases.remove_db_and_purge_data(&id, &t);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
}

/// Regenerate a Blob database's access credentials without touching the
/// underlying data or bucket identity -- the fix for a leaked credential
/// (log, transcript, accidental print) that doesn't warrant losing the data.
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
    // Refresh the egress env with the new credentials so deployments pick
    // up the rotated values on their next restart instead of the stale ones.
    if !rotated.project.is_empty() {
        apply_db_egress(&c, &rotated);
    }
    crate::persist::persist(&c);
    Ok(Json(json!(rotated)))
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
    let mut db: crate::databases::Database =
        serde_json::from_value(body.get("db").cloned().unwrap_or(Value::Null))
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
            if record_tenant(&existing.team) != norm(&db.team) {
                return Err((
                    StatusCode::FORBIDDEN,
                    "db belongs to a different team".into(),
                ));
            }
        }
    }
    match op {
        "register" => {
            // Default to replica role / this node's region, but let the caller
            // override either (the primary uses this same endpoint to re-register
            // a corrected record without code changes).
            if db.role.is_empty() {
                db.role = "replica".into();
            }
            if db.region.is_empty() {
                db.region = c.region.clone();
            }
            // Platform-native kinds serve from THIS node's in-process store (data
            // arrives via write-mirroring); rewrite endpoint host to our API base.
            // Postgres/Redis get a real local backing container so reads are local.
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
        // Realtime rooms ride the same broker as pub/sub topics. Without this
        // arm a fanned-out room frame was accepted and then dropped, so two
        // browsers in the same room but on different nodes never saw each other.
        "realtime" => {
            if let Some(room) = segs.get(3) {
                if let Ok(v) = serde_json::from_slice::<Value>(body) {
                    if let Some(msg) = v.get("message") {
                        // Room frames are raw text on the wire; keep them raw so a
                        // mirrored frame is byte-identical to a locally-published one.
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

// ---- Functional storage REST (Blob / Queue / Vector) ----
//
// MULTI-TENANT: every namespace name (bucket/queue/index/topic/room) coming from
// the request path is tenant-prefixed (`<tenant>::<name>`) before touching the
// store, so two teams using the same name get DISJOINT storage. Responses echo
// the caller's un-prefixed name.

fn ns(
    c: &Arc<CloudState>,
    h: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
    name: &str,
) -> String {
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
            // Blob bytes live on node-local disk, but `admin_ingress` forwards
            // mutations to the control-plane leader while serving reads
            // LOCALLY. `BLOB_ENDPOINT` handed to every app points at the
            // round-robin api host, so a PUT lands on the leader and the
            // matching GET lands anywhere — 404 forever, with no sync path that
            // ever heals it. Same leader fallback `build_get` already uses.
            if !c.is_control_plane_leader() {
                let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
                let leader = c.control_plane_leader();
                let path = format!("/v1/storage/blob/{bucket}/{key}");
                if let Some(b) = fetch_bytes_from_host(&c, &leader, &path, &t).await {
                    return Ok(b.into_response());
                }
            }
            Err(StatusCode::NOT_FOUND)
        }
    }
}

async fn blob_list_keys(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(bucket): Path<String>,
) -> Json<Value> {
    let nsb = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &bucket);
    let keys = c.databases.blob_list(&nsb);
    // An empty listing on a non-leader almost always means "the objects were
    // PUT on the leader" rather than "the bucket is empty" — see `blob_get`.
    if keys.is_empty() && !c.is_control_plane_leader() {
        let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
        let leader = c.control_plane_leader();
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

async fn queue_depth(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(queue): Path<String>,
) -> Json<Value> {
    let nsq = ns(&c, &headers, claims.as_ref().map(|e| &e.0), &queue);
    let depth = c.databases.queue_depth(&nsq);
    // `queue_push`/`queue_pop` are mutations and so run on the leader, while
    // this read serves locally — so every non-leader node reports 0 no matter
    // how deep the real queue is.
    if depth == 0 && !c.is_control_plane_leader() {
        let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
        let leader = c.control_plane_leader();
        // NOTE: depth is served by GET on the queue route itself
        // (`/v1/storage/queue/:queue`) — there is no `/depth` sub-route.
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
    let id = c.world_queue.enqueue(
        t,
        b.target_url,
        b.headers,
        b.payload,
        b.delay_seconds,
        b.max_attempts,
    );
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
    // Fan to EVERY node, not just replica regions: the broker is per-process and
    // subscribers sit on whichever node their WebSocket landed on, so anything
    // less silently delivers to a subset (usually just the leader's own).
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
                    // Bidirectional: a client message is broadcast to the whole room.
                    Some(Ok(Message::Text(t))) => {
                        db.publish(&room, t.clone());
                        // Subscribers sit on whichever node their socket landed
                        // on, so a room frame has to reach every node — locally
                        // only, two browsers in the same room never see each other.
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

// ====================== Secure compute (WireGuard) ======================

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
    let rec = c
        .securelinks
        .provision(req, &region)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Wire the function datapath: inject the connector's local address as a
    // project env var, so deployed functions reach the private backend through
    // the encrypted tunnel transparently.
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
async fn metrics_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<MetricsQ>,
) -> Json<Value> {
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
    // Cache the fleet-fan-out result (never the inner `local=true` hop) for a
    // few seconds — collapses redundant fan-outs from multiple tabs/team
    // members viewing the same tenant's metrics within the window into one
    // real cross-node round.
    let is_top_level = !q.local.unwrap_or(false);
    let cache_key = format!(
        "metrics:{t}:{minutes}:{}:{}",
        q.gran.as_deref().unwrap_or("minute"),
        project.unwrap_or("")
    );
    if is_top_level {
        if let Some(v) = c.resp_cache.get(&cache_key, Duration::from_secs(3)) {
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
        for v in fan_out_peers(&c, &all_healthy_peers(&c), &t, &path).await {
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
    /// Set by the fleet fan-out's own inner call — same single-hop rationale
    /// as `metrics_get`'s `MetricsQ.local` (RUM samples are node-local: a
    /// tenant's visitors can land on any node the routing mesh sends them to).
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

/// Real User Monitoring summary for the Speed Insights page — p75/p90/p95/p99
/// per Core Web Vital, a computed Real Experience Score, real top routes by
/// beacon count, and the true sample count. See `fluid_gateway::RumStore` for
/// where the underlying beacon data actually lives (this endpoint was
/// entirely missing before — the beacon fired and was silently 202-accepted
/// with no storage, and the dashboard's Speed Insights page was a hardcoded
/// empty stub waiting for exactly this).
async fn speed_insights_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Query(q): Query<SpeedInsightsQ>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let minutes = q.minutes.unwrap_or(10_080); // default "Last 7 Days"
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
    for v in fan_out_peers(&c, &all_healthy_peers(&c), &t, &path).await {
        if let Ok(peer_raw) = serde_json::from_value::<RumRaw>(v) {
            raw.merge(&peer_raw);
        }
    }
    let result = json!(raw.summarize());
    c.resp_cache.set(cache_key, result.clone());
    Json(result)
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
    let series = c.metrics.series(
        crate::metrics::Granularity::Minute,
        30,
        now_ms(),
        None,
        None,
    );
    let mut req30: u64 = series.iter().map(|b| b.requests).sum();
    let mut err30: u64 = series.iter().map(|b| b.errors).sum();
    // FLEET-AGGREGATE: `c.counters()`/`c.metrics` are node-local (same confirmed
    // defect as `metrics_get` — see its doc comment). Without this, the ops
    // console's headline request/blocked/error-rate tiles reflect only whichever
    // node happens to serve the request, understating real fleet traffic by up
    // to 8x. `local=true` on the fan-out call stops peer recursion (one hop).
    if !q.map(|Query(q)| q.local.unwrap_or(false)).unwrap_or(false) {
        for v in fan_out_peers(
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

/// Leader-forward for mutations of leader→follower REGISTRY-synced stores
/// (incidents, push): a follower-side write returns 200, persists locally,
/// then gets silently CLOBBERED by the next sync cycle's wholesale snapshot
/// adoption (live-hit 2026-07-21: an incident created on fc-lax vanished from
/// fc-lax itself minutes later). Mutations therefore run ONLY on the leader —
/// a non-leader node forwards the request there (carrying the caller's own
/// bearer, so the leader re-verifies the same JWT against the fleet-shared
/// secret) and returns the leader's response, or fails LOUDLY with 502. It
/// must never fall back to the local write: accepting a write the sync loop
/// will erase is strictly worse than refusing it.
///
/// Deliberately NO iroh-mesh fallback (unlike `put_to_host`): mesh delegation
/// tokens are stripped of `platform_admin` on verification (`gossip::
/// team_claims` — a compromised trusted peer could otherwise forge operator
/// authority fleet-wide; closing that needs per-node signing keys), so an
/// operator-gated mutation cannot ride the mesh under the current threat
/// model. In practice the dashboard's `/ops` proxy already talks to the
/// leader directly, so this 502 only surfaces on direct API calls against a
/// follower whose HTTP admin map lacks the leader — which previously LOST the
/// write silently and now names the fix instead.
/// Egress relay for Textbelt sends (see `push::send_sms`): runs the DIRECT
/// send from THIS node so a regional key is presented from an NA-classified
/// IP. Reached via `put_to_host` (HTTP admin in dev, gossip arm on the mesh).
async fn push_sms_relay(State(c): State<Arc<CloudState>>, Json(v): Json<Value>) -> Json<Value> {
    Json(crate::push::sms_relay_exec(&c, v).await)
}

/// Direct-MX carrier-gateway SMS relay: run the direct-MX blast on THIS node
/// (chosen by the caller for its open outbound :25). Same node-to-node relay
/// shape as `/v1/push/sms-relay`; the leader forwards here to an NA-egress peer.
async fn push_sms_direct_mx(
    State(_c): State<Arc<CloudState>>,
    Json(v): Json<Value>,
) -> Json<Value> {
    Json(crate::push::sms_direct_mx_exec(v).await)
}

async fn forward_mutation_to_leader(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    method: reqwest::Method,
    path: &str,
    body: &Value,
) -> Result<Json<Value>, (StatusCode, String)> {
    let leader = c.control_plane_leader();
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
    // Forward EVERY credential/tenant-context header the leader re-derives auth
    // and tenant from: Authorization (bearer), Cookie (the dashboard's hive_jwt
    // is a cookie, not a bearer — dropping it 401'd cookie-auth mutations), and
    // x-hive-team (the tenant selector `tenant()` reads in unenforced mode —
    // dropping it silently mis-stored the row under "personal").
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
    let inc = c.incidents.open(req);
    crate::persist::persist(&c);
    crate::webhooks::dispatch(
        &c.webhooks,
        "*",
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
        crate::webhooks::dispatch(
            &c.webhooks,
            "*",
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
    crate::webhooks::dispatch(
        &c.webhooks,
        "*",
        "incident.deleted",
        json!({ "id": inc.id, "title": inc.title }),
    );
    Ok(Json(json!({ "ok": true, "deleted": inc.id })))
}

// ============================ Push delivery (web push + SMS) ============================

/// Caller's user id from verified claims (`sub` = Clerk user id via the mint);
/// "local" in unenforced dev mode. Push/SMS rows are keyed by this so
/// `push_settings` can never return a teammate's devices or phone number.
fn push_user_id(claims: Option<&crate::auth::Claims>) -> String {
    claims
        .map(|cl| cl.sub.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// Public VAPID key for `pushManager.subscribe` — requires a signed-in session
/// (the key itself is public by design; the gate just keeps the surface
/// consistent with every other dashboard read).
async fn push_vapid_key(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0))?;
    // Generate ONLY on the leader (a follower minting its own key forks the
    // fleet keypair, stranding every subscription made against the other key).
    crate::push::ensure_vapid_on_leader(&c);
    let keys = c.push.vapid();
    if keys.public_b64.is_empty() {
        // Follower before the leader's key has synced in — transient, retryable.
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

/// Register (or re-register) this browser's push subscription under the
/// caller's VERIFIED tenant — the tenant comes from `tenant()` (JWT claim
/// first, never a bare spoofable header under enforcement), which is the
/// entire cross-tenant isolation story for delivery: the dispatcher only fans
/// a tenant's notifications to rows stored under that exact tenant.
async fn push_subscribe(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(b): Json<PushSubscribeBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Validate BEFORE forwarding: endpoint host must be a recognized push
    // service (stops our VAPID-signed server-side POST from being aimed at an
    // arbitrary attacker URL) and the subscriber keys must be well-formed
    // (garbage keys create rows that fail encryption on every tick forever).
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

/// The CALLER's own delivery config for the current tenant — never anyone
/// else's (rows are filtered by the verified user id AND tenant).
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
    // `verified` gates delivery; `sms_quota` is served from a short-TTL cache so
    // this read never blocks on a live Textbelt round trip.
    let sms = c
        .push
        .sms_for(&user, &team)
        .map(|s| json!({ "phone": s.phone, "enabled": s.enabled, "verified": s.verified }));
    let quota = crate::push::sms_quota_cached(&c).await;
    // Key state for the operator UI: which source is live and a masked tail —
    // never the key itself.
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

/// Operator-set Textbelt key (see `PushState::sms_key_override`): refilling
/// Textbelt is a purchase that funds a specific key, and requiring per-node
/// env surgery to activate it left SMS dead even after payment. Platform-admin
/// gated (it changes billing-bearing behavior for the whole platform);
/// leader-forwarded so the store-sync registry replicates it fleet-wide
/// (including the NA-egress relay peers) instead of a follower write being
/// clobbered. Responds with the masked state + the NEW key's live quota so
/// the dashboard confirms the paste worked in one round trip.
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

/// Set (or change) the SMS number. This does NOT enable delivery: it texts a
/// verification code to the number and stores it PENDING. Delivery starts only
/// after `push_sms_verify` confirms the code — so a caller can never point
/// notifications at a phone number they don't control (anti-SMS-bombing).
/// Toggling an already-verified number on/off skips re-verification.
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

    // Fast path: toggling an already-verified, unchanged number needs no text.
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
        // Within resend cooldown — do not spend another SMS.
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

/// Send a real test notification through the REAL delivery pipeline to the
/// caller's own registered devices/SMS for the current tenant. Deliberately
/// NOT leader-gated: it mutates nothing (no store writes), and running it on
/// any node is exactly what makes dev/non-leader verification possible. Rate-
/// limited per user; the SMS leg only ever reaches the caller's OWN verified
/// number.
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

    // Rate-limit per (user, tenant) — cheap in-memory cooldown.
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
            // A 404/410 means the subscription is permanently gone (the browser
            // profile closed / unsubscribed). Don't report it as a scary
            // failure — prune it so the test self-heals and the next one is
            // clean, exactly like the background dispatcher does.
            Ok(false) => dead.push(d.endpoint.clone()),
            Err(e) => errors.push(format!("{}: {e}", d.label)),
        }
    }
    // Prune the dead subscriptions durably: locally if we're the leader, else
    // via the leader-forwarded DELETE (the PushStore is REGISTRY-synced, so a
    // follower-local remove would be clobbered by the next sync).
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

    // SMS test only ever targets the caller's OWN verified+enabled number.
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

// ---- Notifications (inbox bell) ----

/// Compute the live notification list for a team from real platform signals
/// (failed deploys, 5xx error anomalies, blocked-traffic usage anomalies),
/// applying the user's read/archived state. Archived items keep their `archived`
/// flag so the client can render an Archive tab.
pub(crate) fn build_notifications(
    c: &Arc<CloudState>,
    team: &str,
) -> Vec<crate::notifications::Notification> {
    use crate::notifications::Notification;
    use std::collections::HashMap;
    let team = norm(team).to_string();
    let mut out: Vec<Notification> = Vec::new();

    // Newest event timestamp per project, across BOTH deployments and builds --
    // a failed deploy/build notification is only live while it's still the most
    // recent thing that happened to its project. Without this, a failure from
    // weeks ago kept notifying forever even after the project redeployed
    // successfully since (the "old stale notifs" bug: build_notifications had no
    // time window and no superseded-by-a-later-event check at all, so every
    // Error-state deployment/build that ever existed showed up indefinitely
    // unless the user explicitly archived it).
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

    // 1) Failed deployments.
    for d in c.gw.list() {
        // Authoritative record tag (present on the local record), not the
        // node-local project row which is UNTAGGED on a non-hosting node.
        if record_tenant(&d.tenant) != team {
            continue;
        }
        // Superseded by a later deployment or build for the same project (e.g.
        // a subsequent successful redeploy) -- the failure is moot, stop notifying.
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

    // 1b) Failed builds (git deploys that errored before going live).
    for b in c.builds.list() {
        // Fleet-aware ownership (project rows are node-local): a failed-build
        // alert would otherwise silently drop on any node whose project row is
        // absent, hiding the failure from the owner.
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
    // Every ITEM is derived from node-local sources (`gw.list`, `builds.list`,
    // `recent_events`) even though the read/archived flags are replicated. A
    // deploy that fails on the FC node hosting it produced no notification on
    // the other nodes, so the bell's 8s poll made the badge appear and vanish
    // depending on which node answered — and a failed production deploy could
    // go unseen. Merge peers by the already-deterministic notification id.
    let mut merged: Vec<Value> = items.iter().map(|n| json!(n)).collect();
    let peers = peer_nodes_for_tenant(&c, &team);
    for v in fan_out_peers(&c, &peers, &team, "/v1/notifications?local=true").await {
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
    // Live quota usage (business-locking gauges) + the metered rate card + this
    // period's draft invoice, so the UI shows limits/usage/invoicing in one place.
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

/// Invoices for the tenant (finalized periods + current draft, newest first).
/// Prefers the LOCAL fleet-replicated relational mirror (fastest — no network
/// hop, and correct even if the billing leader is briefly unreachable),
/// falling back to the HTTP proxy-to-leader, falling back to this node's own
/// (possibly stale) local BillingStore.
pub(crate) async fn billing_invoices(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if let Some((_, _, Some(invoices_json))) = crate::relational::billing_snapshot(&t).await {
        if let Ok(Value::Array(mut arr)) = serde_json::from_str::<Value>(&invoices_json) {
            // The mirror only ever holds FINALIZED invoices (drafts are never
            // persisted — see `relational::upsert_billing`'s doc comment).
            // Append the current in-progress period's draft here so this
            // fast local-mirror path matches the fallback below
            // (`BillingStore::invoices` always includes the draft), then
            // re-sort newest-first the same way `invoices()` does.
            arr.push(json!(c.billing.current_invoice(&t)));
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
            return Json(Value::Array(arr));
        }
    }
    if let Some(v) = proxy_billing_read(&c, "/v1/billing/invoices", &t).await {
        return Json(v);
    }
    Json(json!(c.billing.invoices(&t)))
}

pub(crate) async fn billing_ledger(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Json<Value> {
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

/// Which purchasable addons this fleet can actually deliver, and when it
/// cannot, WHY. The dashboard reads this to render a disabled control with the
/// operator's remedy instead of a Buy button that answers with an HTTP error
/// only after the user clicks it. Tenant-authed (same read bar as billing).
async fn billing_addons(
    State(_c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_auth_read(claims.as_ref().map(|e| &e.0)).map_err(|e| (e.0, e.1))?;
    let (available, reason) = match crate::tencent_eip::preflight() {
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

async fn billing_checkout(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Json(req): Json<CheckoutReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Errors carry their REASON. Every arm below used to return a bare
    // StatusCode, so the dashboard's Buy button could only render
    // "POST /v1/billing/checkout -> 502" — indistinguishable from a platform
    // bug, when the actual cause was usually "this fleet has no Tencent
    // credentials configured". `apiSend` surfaces the response body verbatim,
    // so a sentence here is a sentence in the UI.
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
                // PREFLIGHT before any checkout record exists: discovering
                // missing config after confirming a checkout left a confirmed
                // purchase with no address, and told the buyer nothing.
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
    // A free plan (Hobby, or any future $0 tier) has nothing to charge —
    // Stripe Checkout rejects a $0 payment/subscription outright, and there's
    // no reason to round-trip it at all. Apply immediately, same as the mock
    // path always did for this case.
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
    // Same $0 shortcut for an addon (currently `tencent_eip::PRICE_CENTS == 0`,
    // an operator-set testing price, never assumed permanent): route through
    // the SAME `open_checkout_full` -> `confirm_checkout` -> `provision_from_checkout`
    // sequence the paid path uses below, just without the Stripe session that
    // a $0 amount could never produce anyway. `provision_from_checkout` is
    // already idempotent on `ProjectSettings::dedicated_ipv4` (see its own doc
    // comment), so this is safe to hit more than once for the same project.
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

    // Real Stripe Checkout when configured; otherwise the local mock checkout.
    if crate::billing::stripe_configured() {
        let base = std::env::var("PUBLIC_URL").unwrap_or_else(|_| "http://localhost:3000".into());
        // An addon purchase is scoped to a project (e.g. dedicated_ipv4) --
        // land the buyer back where they bought it, not on the unrelated
        // billing overview page, which has nothing to show for it.
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

pub(crate) async fn billing_checkout_get(
    State(c): State<Arc<CloudState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    if let Some(co) = c.billing.get_checkout(&id) {
        return Ok(Json(json!(co)));
    }
    // Checkouts live in an in-process map on whichever node opened them, and
    // `POST /v1/billing/checkout` is a mutation so it always runs on the billing
    // authority. The browser then navigates to /billing/checkout?session=<id>,
    // whose GET round-robins — landing anywhere else it 404'd and the upgrade
    // flow dead-ended with "Checkout session not found". The sibling billing
    // reads already proxy this way; this one was missed.
    if let Some(v) = proxy_billing_read(&c, &format!("/v1/billing/checkout/{id}"), "").await {
        return Ok(Json(v));
    }
    Err(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct ConfirmReq {
    session: String,
}

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
    let (co, acc) = c
        .billing
        .confirm_checkout(&req.session)
        .ok_or(StatusCode::NOT_FOUND)?;
    // `confirm_checkout` only moves the billing half; the effect for a
    // completed "plan"/"addon" checkout is dispatched here — never inside
    // `confirm_checkout` itself, same single-writer split as the "credits"
    // branch it already draws.
    let mut addon_error: Option<String> = None;
    match co.kind.as_str() {
        "credits" => {}
        "addon" => {
            if let Err(e) = crate::tencent_eip::provision_from_checkout(&c, &co).await {
                // Payment already succeeded (verified above) and the checkout
                // is already consumed — never fail this request over a
                // provisioning error, which would read as "you weren't
                // charged." Surface it instead so the caller can retry/support.
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
        // Primary confirmation path — mirrors `billing_confirm`'s apply logic,
        // keyed by the `checkout_id` we stamped into the session's metadata
        // (see `stripe_checkout`) rather than a tenant header, since Stripe is
        // the caller here, not an authenticated user.
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
        // A renewal failure (card declined etc.) surfaces as the subscription
        // moving to "past_due"/"unpaid" — logged for now (no automatic
        // downgrade on a transient failure; Stripe's own retry schedule and
        // dunning emails run first, and `customer.subscription.deleted` is
        // still the terminal signal once retries are exhausted).
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
    /// Pin this tenant's tier FLOOR so no automatic path can downgrade below it.
    /// `"enterprise"` locks at enterprise; `""` clears the lock; omitted leaves
    /// any existing lock untouched.
    ///
    /// A grant without this is not durable, and that is not hypothetical: the
    /// 2026-07-18 owner grant of `enterprise` was silently reverted to `hobby`
    /// by a user-initiated free-plan checkout on 2026-07-25, and the 2026-07-29
    /// remediation passed `plan=""` so the `!req.plan.is_empty()` guard below
    /// skipped `apply_plan_everywhere` entirely — credits were added and the
    /// tier was never restored, leaving the account quota-locked.
    #[serde(default)]
    lock_tier: Option<String>,
}

/// Operator-only: comp an account with prepaid credits and/or switch its plan
/// directly, bypassing Stripe entirely — a support/goodwill grant (refunds,
/// promotions, platform-owner self-testing). Distinct from the self-service
/// `/v1/billing/charge` path (which only ever acts on the CALLER's own tenant,
/// no operator gate needed since it can't touch anyone else's account): this
/// targets an explicit `tenant` chosen by the caller, so it is hard-gated to
/// `require_operator` the same way every other platform-wide admin mutation is.
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
    // plan_spec() falls back to Hobby (PLANS[0]) for an unrecognized id rather
    // than erroring, so an unknown id must be caught here by comparing the
    // resolved spec's own id back against what was asked for.
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
    // Apply the tier lock AFTER any plan change, so a grant that both switches
    // the plan and locks it lands in the right order. `set_tier_lock` also
    // raises the live plan to the floor, which is what repairs an account whose
    // stored plan was downgraded before the lock existed.
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
        // Keep the teams half in step: the lock raises the billing plan, and
        // the two halves must never disagree (that split is what produced a
        // tenant reading `enterprise` for features while quota-capped as
        // `hobby`).
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

/// ADMIN-ONLY, ONE-SHOT (idempotent) billing schema backfill: normalize the
/// pre-migration JSON-blob billing rows (`billing_accounts.account_json` /
/// `billing_ledger_snapshot.ledger_json` / `billing_invoices_snapshot.invoices_json`)
/// into the new normalized per-row tables (`billing_accounts`'s new scalar
/// columns, `billing_ledger`, `billing_invoices` + `billing_invoice_lines`).
/// See `relational::backfill_billing_normalize`'s doc comment for the full
/// write/verify contract and the critical `billing_accounts` table-name-
/// collision finding it exists to handle.
///
/// NOT wired to run automatically anywhere — must be explicitly POSTed by a
/// platform operator, and even then only after the separate production-data
/// approval gate this migration's rollout plan calls for (this endpoint
/// performs REAL writes against REAL billing data the moment it's called; it
/// does not itself ask for confirmation — that gate lives outside this code).
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
    projects.sort_by(|a, b| {
        a["project"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project"].as_str().unwrap_or(""))
    });

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
    // Don't let custom docs shadow a typed platform collection.
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
        c.audit
            .record(&t, "user", "delete", "document", &id, &collection);
        crate::persist::persist(&c);
        return Ok(Json(json!({ "deleted": id })));
    }
    let ok = match collection.as_str() {
        "deployments" => match c.gw.remove(&id).await {
            Some(p) => {
                // Same last-deployment raw-port release as `dep_delete`.
                if !c.gw.list().iter().any(|d| d.project == p) {
                    let _ = crate::raw_ports::release_raw_ports_coordinated(&c, &p).await;
                }
                true
            }
            None => false,
        },
        "databases" => {
            // Use the RECORD's own owning team (not the operator's `t`) so the
            // queue/vector namespace purge targets the correct tenant.
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
            c.webhooks.remove(&id);
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

// ============================ Data Browser: "view as PostgreSQL" ============================
// An alternative, READ-ONLY view of the relational mirror built on guardian-db's
// native SQL layer (crates/hive-cloud/src/relational.rs) — real typed tables
// (project_teams, billing_*) instead of the document-collection JSON browsing
// above. Deliberately a separate, narrower surface: no PUT/POST/DELETE here at
// all (see relational::reject_unless_readonly) — mutations to this data still
// only ever happen through the normal typed stores (ProjectStore/BillingStore),
// never through this admin query box.

/// List the known relational tables + their columns.
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

/// Run a read-only query against the relational mirror: either a caller-typed
/// `sql` (SELECT/read-only WITH only — enforced in relational.rs, not just
/// here), or a `table` name for the default `SELECT * FROM <table> LIMIT 200`
/// the table-browser view uses. Exactly one of the two is expected.
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
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

// ============================ Deployment preview / thumbnail ============================

/// Preview metadata for a project's production deployment: a site thumbnail for
/// frontends, or the JSON/text body for backend services.
pub(crate) async fn project_preview(
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
    let dep =
        c.gw.list()
            .into_iter()
            .find(|d| d.project == project && d.production)
            .or_else(|| c.gw.list().into_iter().find(|d| d.project == project));
    let Some(dep) = dep else {
        // `c.gw.list()` is LOCALLY-hosted deployments only. Most projects are
        // placed on a peer, so without this the project page showed a
        // permanently empty preview card on every node but the host. Sibling
        // deployment reads here already proxy the same way.
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
    // Backend service: fetch "/" through the gateway and return the body.
    let base = c.public_base.clone();
    let resp = c
        .http
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
    let dep =
        c.gw.list()
            .into_iter()
            .find(|d| d.project == project && d.production)
            .or_else(|| c.gw.list().into_iter().find(|d| d.project == project));
    let Some(dep) = dep else {
        // Locally-hosted deployments only (see `project_preview`), and the PNG
        // cache below lives under this node's own data dir — so a remotely
        // placed project 404'd its thumbnail on every node but the host.
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
