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
use tokio::process::Command;

use crate::state::CloudState;

pub fn router(cloud: Arc<CloudState>) -> Router {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/overview", get(overview))
        .route("/v1/nodes", get(nodes))
        .route("/v1/serve-hosts", get(serve_hosts))
        .route("/v1/resources", get(resources_get))
        .route("/v1/leases", get(leases_get))
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
        .route("/v1/workflows", get(wf_list).post(wf_define))
        .route("/v1/workflows/summary", get(wf_summary))
        .route("/v1/workflows/runs", get(wf_runs))
        .route("/v1/workflows/runs/:id", get(wf_run_detail))
        .route("/v1/workflows/:id/run", post(wf_run))
        .route("/v1/sandbox", post(sandbox))
        .route("/deployments", get(dep_list).post(dep_create))
        .route("/v1/deployments/:id", delete(dep_delete))
        .route("/v1/deployments/:id/resources", get(deployment_resources))
        .route("/v1/deployments/:id/promote", post(dep_promote))
        .route("/v1/projects/:project", delete(project_delete))
        .route("/v1/projects/:project/redeploy", post(project_redeploy))
        .route("/v1/git/deploy", post(git_deploy))
        .route("/v1/fleet-deployments", get(fleet_deployments))
        .route("/v1/builds/:id", get(build_get))
        .route("/v1/buildcache/:key", get(buildcache_get))
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
        // ---- Webhooks ----
        .route("/v1/webhooks", get(webhooks_all).post(webhook_create_team))
        .route("/v1/webhooks/events", get(webhook_events))
        .route("/v1/webhooks/deliveries", get(webhook_deliveries))
        .route("/v1/webhooks/:id", delete(webhook_delete))
        .route("/v1/projects/:project/webhooks", get(webhooks_for_project).post(webhook_create))
        // ---- Databases / storage ----
        .route("/v1/databases", get(databases_list).post(database_create))
        .route("/v1/admin/databases", get(admin_databases_all))
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
        .route("/v1/admin/guardian", get(guardian_status))
        .route("/v1/identity/sync", post(identity_sync))
        // ---- Billing & compute credits ----
        .route("/v1/billing", get(billing_get))
        .route("/v1/billing/ledger", get(billing_ledger))
        .route("/v1/billing/checkout", post(billing_checkout))
        .route("/v1/billing/checkout/:id", get(billing_checkout_get))
        .route("/v1/billing/confirm", post(billing_confirm))
        .route("/v1/billing/charge", post(billing_charge))
        // ---- Deployment preview / thumbnail ----
        .route("/v1/projects/:project/preview", get(project_preview))
        .route("/v1/projects/:project/thumbnail", get(project_thumbnail))
        .route("/v1/incidents", get(incidents_list).post(incident_open))
        .route("/v1/incidents/:id/updates", post(incident_update))
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

/// The tier (hobby/pro/enterprise) of the team owning a project.
fn team_plan(c: &Arc<CloudState>, project: &str) -> String {
    let team = norm(&c.projects.team_of(project)).to_string();
    c.teams.get(&team).map(|t| t.plan).unwrap_or_else(|| "hobby".into())
}

async fn project_functions_put(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(mut f): Json<crate::project_settings::FunctionSettings>,
) -> Json<Value> {
    // Enforce plan limits: runtime cap (Enterprise = 1h) and Enterprise-only
    // automatic multi-region fail-over.
    let plan = team_plan(&c, &project);
    let max_dur = crate::billing::plan_max_duration_secs(&plan);
    f.default_max_duration_secs = f.default_max_duration_secs.clamp(1, max_dur);
    if !crate::billing::plan_allows_failover(&plan) {
        f.failover = false;
    }
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

async fn domains_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
    let pairs = c.projects.all_domains();
    Json(json!(pairs
        .into_iter()
        .filter(|(p, _)| norm(&c.projects.team_of(p)) == t)
        .map(|(p, d)| json!({ "project": p, "domain": d }))
        .collect::<Vec<_>>()))
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
async fn deployment_resources(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers);
    let Some(rec) = c.gw.deployment_records().into_iter().find(|r| r.id == id) else {
        // Not hosted locally — the placement scheduler may have put this deployment
        // on a peer. Proxy to the hosting node so its build outputs (functions +
        // static assets, which live on that node's filesystem) are returned.
        if let Some(admin) = host_admin_for_deployment(&c, &id) {
            if let Some(v) = proxy_get_json(&c, &admin, &format!("/v1/deployments/{id}/resources"), &t).await {
                return Json(v);
            }
        }
        return Json(json!({ "functions": [], "static_assets": [], "total_static": 0 }));
    };
    if norm(&c.projects.team_of(&rec.project)) != t {
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
async fn domain_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(domain): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers);
    // Connected projects: projects in this tenant whose domain ends with this one.
    let connected: Vec<Value> = c
        .projects
        .all_domains()
        .into_iter()
        .filter(|(p, d)| norm(&c.projects.team_of(p)) == t && (d == &domain || d.ends_with(&format!(".{domain}"))))
        .map(|(p, d)| json!({ "project": p, "domain": d }))
        .collect();
    Json(json!({ "domain": c.domains.ensure(&domain, &t), "connected": connected }))
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

async fn domain_add_record(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(domain): Path<String>, Json(r): Json<AddRecordReq>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    c.domains.ensure(&domain, &t);
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
    let added = c.domains.add_record(&domain, rec).ok_or(StatusCode::NOT_FOUND)?;
    c.audit.record(&t, "user", "create", "dns_record", &added.id, &format!("{} {} → {} ({domain})", added.kind, added.name, added.value));
    crate::persist::persist(&c);
    Ok(Json(json!(added)))
}

async fn domain_delete_record(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path((domain, id)): Path<(String, String)>) -> Json<Value> {
    let t = tenant(&c, &headers);
    let ok = c.domains.delete_record(&domain, &id);
    if ok {
        c.audit.record(&t, "user", "delete", "dns_record", &id, &domain);
        crate::persist::persist(&c);
    }
    Json(json!({ "deleted": ok }))
}

/// Edit an existing DNS record (system records are immutable).
async fn domain_update_record(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path((domain, id)): Path<(String, String)>,
    Json(r): Json<AddRecordReq>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    let updated = c
        .domains
        .update_record(&domain, &id, r.name, r.kind.to_uppercase(), r.value, r.ttl, r.priority, r.comment)
        .ok_or(StatusCode::NOT_FOUND)?;
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
    Path(domain): Path<String>,
    Json(req): Json<ImportReq>,
) -> Json<Value> {
    let t = tenant(&c, &headers);
    c.domains.ensure(&domain, &t);
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
    Json(json!({ "imported": added.len(), "records": added }))
}

/// Detect a domain's CURRENT public DNS records (via DNS-over-HTTPS) so a user can
/// migrate them into the console with one click. Best-effort: returns whatever
/// resolves. Records are NOT added — the client imports the ones it wants.
async fn domain_scan_dns(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(domain): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers);
    c.domains.ensure(&domain, &t);
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
    Json(json!({ "domain": domain, "records": found }))
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

async fn domain_set_nameservers(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(domain): Path<String>, Json(r): Json<NameserversReq>) -> Json<Value> {
    let t = tenant(&c, &headers);
    c.domains.ensure(&domain, &t);
    c.domains.set_nameservers(&domain, r.nameservers);
    c.audit.record(&t, "user", "update", "nameservers", &domain, "");
    crate::persist::persist(&c);
    Json(json!(c.domains.get(&domain)))
}

#[derive(Deserialize)]
struct AutoRenewReq {
    on: bool,
}

async fn domain_set_auto_renew(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(domain): Path<String>, Json(r): Json<AutoRenewReq>) -> Json<Value> {
    let t = tenant(&c, &headers);
    c.domains.ensure(&domain, &t);
    c.domains.set_auto_renew(&domain, r.on);
    crate::persist::persist(&c);
    Json(json!(c.domains.get(&domain)))
}

async fn domain_renew_ssl(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(domain): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers);
    c.domains.ensure(&domain, &t);
    let cert = c.domains.renew_ssl(&domain);
    c.audit.record(&t, "user", "update", "ssl_cert", &domain, "reissued free certificate");
    crate::persist::persist(&c);
    Json(json!(cert))
}

// ---- Git deploy (Import Git Repository) ----

/// Pick a globally-unique project name. Deployment aliases are `<project>.localhost`
/// (global), so two projects can't share a name. A genuine redeploy (same name +
/// same repo + same tenant) keeps its name; anything else gets a `-N` suffix.
fn unique_project_name(c: &Arc<CloudState>, desired: &str, repo_url: &str, tenant: &str) -> String {
    let existing = c.projects.snapshot();
    let base = if desired.trim().is_empty() {
        crate::git::project_name_from_url(repo_url)
    } else {
        desired.trim().to_string()
    };
    // Case-insensitive collision check: `<project>.localhost` aliases are global
    // and host routing is case-insensitive, so "Foo" and "foo" must not coexist.
    let existing_ci = |name: &str| existing.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned();
    let Some(hit) = existing_ci(&base) else {
        return base; // free — use it
    };
    // Redeploy of the same project (same repo + tenant) → keep the name.
    let cur = existing.get(&hit);
    let same_tenant = cur.map(|c| norm(&c.team) == tenant).unwrap_or(false);
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

async fn git_deploy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(mut req): Json<fluid_core::GitDeployRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Assign the (new) project to the requesting tenant so it shows under their
    // team only — with a globally-unique name (auto-generated when none given).
    let t = tenant(&c, &headers);
    let requested = req.project.clone().unwrap_or_default();
    let project = unique_project_name(&c, &requested, &req.repo_url, &t);
    // Reject an EXPLICIT name the user typed that's already taken by a different
    // project (Issue #4) — don't silently rename to `<name>-2`. Fanout deploys
    // (no_fanout) and auto-named deploys (empty requested) are exempt: those must
    // resolve to a concrete name without erroring.
    if !req.no_fanout && !requested.trim().is_empty() && project != requested.trim() {
        return Err((
            StatusCode::CONFLICT,
            format!("A project named \"{}\" already exists. Choose a different name.", requested.trim()),
        ));
    }
    req.project = Some(project.clone());
    c.projects.set_team(&project, &t);
    // Persist the subdirectory so future redeploys keep building it.
    if let Some(root) = req.root_dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        c.projects.set_root_dir(&project, root);
    }
    crate::persist::persist(&c);
    // Start the build asynchronously; the dashboard streams logs via /v1/builds/:id.
    let build_id = crate::git::start_build(c.clone(), req);
    Ok(Json(json!({ "build_id": build_id, "project": project })))
}

async fn build_get(
    State(c): State<Arc<CloudState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    c.builds.get(&id).map(|b| Json(json!(b))).ok_or(StatusCode::NOT_FOUND)
}

/// Publish the host subdomains this node serves + its gateway URL, so peers can
/// build their cross-node routing tables (the mesh routes requests to wherever a
/// deployment actually lives).
async fn serve_hosts(State(c): State<Arc<CloudState>>) -> Json<Value> {
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
async fn leases_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.leases.list()))
}

/// REAL cluster resource accounting: this node's live CPU/mem/disk/network usage
/// (sysinfo) plus cluster TOTALS = sum of every live node's capacity (gossiped via
/// NodeInfo). Answers "available compute / storage / bandwidth across the mesh".
async fn resources_get(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let nodes = c.registry.nodes();
    let cpu_cores: u64 = nodes.iter().map(|n| n.cpu_cores as u64).sum();
    let mem_total_mb: u64 = nodes.iter().map(|n| n.mem_total_mb).sum();
    let disk_total_gb: u64 = nodes.iter().map(|n| n.disk_total_gb).sum();
    let usage = crate::resources::live().await;
    Json(json!({
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
    }))
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
        Ok(bytes) => Ok(([(axum::http::header::CONTENT_TYPE, "application/x-tar")], bytes).into_response()),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
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
    // STRICT multi-tenant isolation: a request only ever sees the deployments for
    // its active tenant (the Clerk org slug / team via `x-hive-team`, or an API
    // key's team). Projects in other tenants are never returned — this is what
    // prevents data bleeding across accounts when switching teams.
    let t = tenant(&c, &headers);
    let mut list: Vec<_> = c
        .gw
        .list()
        .into_iter()
        .filter(|d| norm(&c.projects.team_of(&d.project)) == t)
        .collect();
    // Merge in deployments the placement scheduler placed on OTHER mesh nodes
    // (e.g. the default San-Jose placement), so the dashboard shows them too.
    // Dedup by id; tenant-filter on the remote-reported tenant.
    let mut seen: std::collections::HashSet<String> = list.iter().map(|d| d.id.to_string()).collect();
    for deps in c.peer_deployments.read().values() {
        for d in deps {
            if norm(&d.tenant) == t && seen.insert(d.id.to_string()) {
                list.push(d.clone());
            }
        }
    }
    list.sort_by_key(|d| std::cmp::Reverse(d.created_at_ms));
    Json(json!(list))
}

/// Node-to-node: this node's full deployment list (all tenants), for peers to
/// build a fleet-wide view. Consumed by the gossip loop into `peer_deployments`.
async fn fleet_deployments(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!({ "node": c.node_name, "deployments": c.gw.list() }))
}

async fn dep_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(req): Json<fluid_core::DeployRequest>,
) -> Json<Value> {
    // Tag the deployment (and the cells it spawns) with the caller's tenant so
    // compute is partitioned per team — mirrors gw.deploy()'s defaults otherwise.
    let t = tenant(&c, &headers);
    let info = c.gw.deploy_full(
        req.root,
        req.manifest,
        "you".into(),
        None,
        true,
        fluid_core::DeployState::Ready,
        t,
    );
    // Persist so the deployment survives a node restart (without this it lived
    // only in memory and was lost on reboot).
    crate::persist::persist(&c);
    Json(json!(info))
}

/// Roll back / promote: make an existing deployment the project's production.
async fn dep_promote(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    if let Some(info) = c.gw.promote(&id) {
        crate::persist::persist(&c);
        let ev = c.event(&c.region, "PROMOTE", &info.alias, "/", 200, "deploy", &format!("rolled back to {id}"));
        c.record(ev);
        crate::webhooks::dispatch(&c.webhooks, &info.project, "deployment.promoted",
            json!({ "id": id, "project": info.project, "url": format!("https://{}", info.alias) }));
        return Ok(Json(json!(info)));
    }
    // Not hosted locally — the placement scheduler put this deployment on a peer.
    // Proxy the promote to its host so instant rollback works cross-node.
    let t = tenant(&c, &headers);
    if let Some(admin) = host_admin_for_deployment(&c, &id) {
        if let Ok(r) = c
            .http
            .post(format!("{admin}/v1/deployments/{id}/promote"))
            .header("x-hive-team", t)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            if r.status().is_success() {
                if let Ok(v) = r.json::<Value>().await {
                    return Ok(Json(v));
                }
            }
        }
    }
    Err(StatusCode::NOT_FOUND)
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
        request_id: String::new(),
    });
}

/// Delete a single deployment (unregisters its functions).
async fn dep_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    // Ownership: a deployment belongs to its project's team. If it exists but
    // isn't ours, 404 (don't disclose another tenant's resource). If it doesn't
    // exist, fall through to the idempotent no-op remove (unchanged behavior).
    if let Some(d) = c.gw.list().into_iter().find(|d| d.id.0 == id) {
        if norm(&c.projects.team_of(&d.project)) != t {
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

/// Delete an entire project: all its deployments + settings. By default this
/// cascades across the mesh (removing the project from any peer node the
/// placement scheduler put it on); `?cascade=false` deletes only on this node
/// (used by the scheduler's relocate cleanup, which must NOT cascade back and
/// wipe the freshly-placed copy on another node).
async fn project_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Query(q): Query<CascadeQ>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    // Authorize against the local store if we have it, else against the gossiped
    // fleet — a project placed by the scheduler may live ONLY on a peer, so it's
    // absent from this node's project store (team_of would default to "personal"
    // and wrongly 404). Allow the delete if the requester's tenant owns the
    // project anywhere in the mesh.
    let authorized = c.projects.get_if_set(&project).map(|s| norm(&s.team) == t).unwrap_or(false)
        // a deployment of this project hosted locally under the requester's tenant
        || c.gw.list().iter().any(|d| d.project == project && norm(&d.tenant) == t)
        // …or hosted on a peer (project lives only on a scheduler-placed node)
        || c.peer_deployments.read().values().flatten().any(|d| d.project == project && norm(&d.tenant) == t);
    if !authorized {
        return Err(StatusCode::NOT_FOUND);
    }
    let ids = c.gw.remove_project(&project).await;
    // The placement scheduler may host this project on peer node(s); remove it
    // there too so a delete from the dashboard fully tears it down across the mesh.
    // Peers are told `cascade=false` so the teardown is a single hop (no loops).
    if q.cascade.unwrap_or(true) {
        let admins = c.node_admins.read().clone();
        let routes = c.peer_routes.read().clone();
        let mut hosting: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Nodes whose gossiped deployment list contains this project (most reliable
        // — covers errored/non-serving deployments that aren't in the route table).
        for (node, deps) in c.peer_deployments.read().iter() {
            if deps.iter().any(|d| d.project == project) {
                hosting.insert(node.clone());
            }
        }
        for (host, rs) in routes.iter() {
            let sub = host.split('.').next().unwrap_or(host);
            if sub == project || sub.starts_with(&format!("{project}-")) {
                for r in rs {
                    hosting.insert(r.node_id.clone());
                }
            }
        }
        for (node, admin) in admins.iter() {
            if hosting.contains(node) {
                let _ = c
                    .http
                    .delete(format!("{admin}/v1/projects/{project}?cascade=false"))
                    .header("x-hive-team", t.clone())
                    .timeout(std::time::Duration::from_secs(15))
                    .send()
                    .await;
            }
        }
    }
    record_event(&c, &project, "delete", &format!("deleted project {project} ({} deployment(s))", ids.len()));
    c.projects.remove(&project);
    crate::persist::persist(&c);
    crate::webhooks::dispatch(&c.webhooks, &project, "project.removed", json!({ "project": project, "deployments": ids.len() }));
    Ok(Json(json!({ "project": project, "removed_deployments": ids })))
}

/// Query for project_delete: `cascade=false` deletes only on this node (no mesh
/// fan-out). Defaults to cascade when absent.
#[derive(Deserialize, Default)]
struct CascadeQ {
    #[serde(default)]
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

/// Admin URL of the peer node hosting `project` (when the placement scheduler put
/// it on a peer rather than this node), for proxying per-project read views.
fn host_admin_for_project(c: &Arc<CloudState>, project: &str) -> Option<String> {
    let pd = c.peer_deployments.read();
    let admins = c.node_admins.read();
    for (node, deps) in pd.iter() {
        if deps.iter().any(|d| d.project == project) {
            if let Some(a) = admins.get(node) {
                return Some(a.clone());
            }
        }
    }
    None
}

/// Admin URL of the peer node hosting deployment `id`, for proxying per-deployment
/// read views (resources) to wherever the deployment actually lives.
fn host_admin_for_deployment(c: &Arc<CloudState>, id: &str) -> Option<String> {
    let pd = c.peer_deployments.read();
    let admins = c.node_admins.read();
    for (node, deps) in pd.iter() {
        if deps.iter().any(|d| d.id.to_string() == id) {
            if let Some(a) = admins.get(node) {
                return Some(a.clone());
            }
        }
    }
    None
}

/// Admin URLs of peer nodes that host ANY of the requester-tenant's projects —
/// the set to aggregate for fleet-wide views (the global Workflows tab). Empty
/// when nothing of this tenant is placed remotely.
fn peer_admins_for_tenant(c: &Arc<CloudState>, team: &str) -> Vec<String> {
    let pd = c.peer_deployments.read();
    let admins = c.node_admins.read();
    let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (node, deps) in pd.iter() {
        if deps.iter().any(|d| norm(&d.tenant) == team) {
            if let Some(a) = admins.get(node) {
                out.insert(a.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// GET `path` from a peer admin, forwarding the team header; returns parsed JSON.
async fn proxy_get_json(c: &Arc<CloudState>, admin: &str, path: &str, team: &str) -> Option<Value> {
    let resp = c
        .http
        .get(format!("{admin}{path}"))
        .header("x-hive-team", team)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    resp.json::<Value>().await.ok()
}

async fn project_redeploy(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
    Json(body): Json<RedeployBody>,
) -> Result<Json<Value>, StatusCode> {
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
    };
    let build_id = crate::git::start_build(c.clone(), req);
    Ok(Json(json!({ "build_id": build_id })))
}

// ---- GitOps ----

/// All projects owned by a tenant plus their settings + git source — the data the
/// dashboard serializes into the committed `openedge.yaml`.
async fn gitops_projects(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
    let mut out = Vec::new();
    for (project, settings) in c.projects.snapshot() {
        if norm(&settings.team) != t {
            continue;
        }
        let git = c.gw.git_for_project(&project);
        let prod = c.gw.list().into_iter().find(|d| d.project == project && d.production);
        out.push(json!({
            "project": project,
            "settings": c.projects.get_masked(&project),
            "git": git,
            "production": prod,
            "root_dir": settings.build.root_dir,
        }));
    }
    out.sort_by(|a, b| a["project"].as_str().unwrap_or("").cmp(b["project"].as_str().unwrap_or("")));
    Json(json!(out))
}

async fn gitops_get(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
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
    Json(b): Json<GitOpsLinkReq>,
) -> Json<Value> {
    let t = tenant(&c, &headers);
    let link = c.gitops.set_link(&t, &b.repo, &b.branch, &b.path, &b.scope);
    crate::persist::persist(&c);
    Json(json!(link))
}

async fn gitops_unlink(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
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
    Json(b): Json<GitOpsSynced>,
) -> Json<Value> {
    let t = tenant(&c, &headers);
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
        let Some(git) = c.gw.git_for_project(&project) else { continue };
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
    /// Internal: when set by a fleet-aggregation proxy, return ONLY this node's
    /// local events (no fan-out) — prevents proxy recursion.
    #[serde(default)]
    local: Option<bool>,
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
            if let Some(qq) = q.q.as_ref().filter(|s| !s.is_empty()) {
                s.push_str(&format!("&q={}", urlencode(qq)));
            }
            s
        };
        // A project filter targets just its host; otherwise pull from every peer
        // that hosts one of this tenant's projects.
        let admins: Vec<String> = match q.project.as_deref() {
            Some(p) if c.gw.git_for_project(p).is_none() => host_admin_for_project(&c, p).into_iter().collect(),
            Some(_) => Vec::new(),
            None => peer_admins_for_tenant(&c, &t),
        };
        for admin in admins {
            if let Some(v) = proxy_get_json(&c, &admin, &format!("/v1/logs?{qs}"), &t).await {
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

async fn rc_get(State(c): State<Arc<CloudState>>, Query(q): Query<RcKey>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match c.runtime_cache.get(&q.scope, &q.key) {
        Some(v) => ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn rc_put(
    State(c): State<Arc<CloudState>>,
    Query(q): Query<RcKey>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let tags: Vec<String> = q.tags.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect();
    match c.runtime_cache.set(&q.scope, &q.key, body.to_vec(), q.ttl, tags) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn rc_delete(State(c): State<Arc<CloudState>>, Query(q): Query<RcKey>) -> StatusCode {
    c.runtime_cache.delete(&q.scope, &q.key);
    StatusCode::NO_CONTENT
}

async fn rc_revalidate(State(c): State<Arc<CloudState>>, Query(q): Query<RcTag>) -> Json<Value> {
    let removed = c.runtime_cache.revalidate_tag(&q.scope, &q.tag);
    Json(json!({ "removed": removed }))
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

#[derive(Deserialize)]
struct WfQuery {
    /// Restrict to a single project.
    project: Option<String>,
    /// Internal: when set by a fleet-aggregation proxy call, return ONLY this
    /// node's local workflows (no further fan-out) — prevents proxy recursion.
    #[serde(default)]
    local: Option<bool>,
}

/// Does this workflow's project belong to the requesting team?
fn wf_in_team(c: &Arc<CloudState>, project: &str, team: &str) -> bool {
    norm(&c.projects.team_of(project)) == norm(team)
}

async fn wf_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, Query(q): Query<WfQuery>) -> Json<Value> {
    let team = tenant(&c, &headers);
    // Workflows are ingested on the node that HOSTS a deployment. If this project
    // was placed on a peer, proxy to that node so its workflows show up.
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() {
            if let Some(admin) = host_admin_for_project(&c, project) {
                if let Some(v) = proxy_get_json(&c, &admin, &format!("/v1/workflows?project={project}"), &team).await {
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
        for admin in peer_admins_for_tenant(&c, &team) {
            if let Some(v) = proxy_get_json(&c, &admin, "/v1/workflows?local=true", &team).await {
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

async fn wf_define(State(c): State<Arc<CloudState>>, Json(def): Json<WorkflowDef>) -> Json<Value> {
    c.workflows.define(def);
    Json(json!(c.workflows.defs()))
}

async fn wf_runs(State(c): State<Arc<CloudState>>, headers: HeaderMap, Query(q): Query<WfQuery>) -> Json<Value> {
    let team = tenant(&c, &headers);
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() {
            if let Some(admin) = host_admin_for_project(&c, project) {
                if let Some(v) = proxy_get_json(&c, &admin, &format!("/v1/workflows/runs?project={project}"), &team).await {
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
                    if norm(&c.projects.team_of(&d.project)) == team {
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
        for admin in peer_admins_for_tenant(&c, &team) {
            if let Some(v) = proxy_get_json(&c, &admin, "/v1/workflows/runs?local=true", &team).await {
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
    Json(json!(runs))
}

/// One run with full step detail (for the trace timeline / Gantt). Resolves from
/// our engine first, else from the project's WDK "world" store — proxied to the
/// hosting node when the project lives on a peer (the world env is decrypted
/// there). `?project=` is required to locate a world run.
async fn wf_run_detail(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<WfQuery>,
) -> Result<Json<Value>, StatusCode> {
    if let Some(r) = c.workflows.run(&id) {
        return Ok(Json(json!(r)));
    }
    let team = tenant(&c, &headers);
    let found = |v: &Value| v.get("run").map(|r| !r.is_null()).unwrap_or(false);
    // 1) Explicit project (proxy to its host if remote, else read locally).
    if let Some(project) = q.project.as_deref() {
        if c.gw.git_for_project(project).is_none() && !q.local.unwrap_or(false) {
            if let Some(admin) = host_admin_for_project(&c, project) {
                if let Some(v) = proxy_get_json(&c, &admin, &format!("/v1/workflows/runs/{id}?project={project}&local=true"), &team).await {
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
            if norm(&c.projects.team_of(&d.project)) == team {
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
    // 3) Fleet: ask peers hosting this tenant's projects to resolve it.
    if !q.local.unwrap_or(false) {
        for admin in peer_admins_for_tenant(&c, &team) {
            if let Some(v) = proxy_get_json(&c, &admin, &format!("/v1/workflows/runs/{id}?local=true"), &team).await {
                if found(&v) {
                    return Ok(Json(v));
                }
            }
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// Per-project rollup for the global "All Projects" workflows view.
async fn wf_summary(State(c): State<Arc<CloudState>>, headers: HeaderMap, Query(q): Query<WfQuery>) -> Json<Value> {
    use std::collections::BTreeMap;
    let team = tenant(&c, &headers);
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
      for admin in peer_admins_for_tenant(&c, &team) {
        if let Some(v) = proxy_get_json(&c, &admin, "/v1/workflows/summary?local=true", &team).await {
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
struct SetPlan {
    plan: String,
}

/// Change a team's tier (hobby | pro | enterprise). Keeps the billing account in
/// sync so the compute allowance + plan label update together.
async fn team_set_plan(
    State(c): State<Arc<CloudState>>,
    Path(slug): Path<String>,
    Json(b): Json<SetPlan>,
) -> Result<Json<Value>, (StatusCode, String)> {
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
    Path(slug): Path<String>,
    Json(b): Json<SetSso>,
) -> Result<Json<Value>, (StatusCode, String)> {
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

async fn webhooks_all(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    // Only the caller's own webhooks (the payload includes signing secrets, so
    // this must be tenant-scoped).
    let t = tenant(&c, &headers);
    let list: Vec<_> = c.webhooks.snapshot().into_iter().filter(|w| norm(&w.team) == t).collect();
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

async fn webhook_create_team(State(c): State<Arc<CloudState>>, headers: HeaderMap, Json(b): Json<CreateTeamWebhook>) -> Json<Value> {
    let t = tenant(&c, &headers);
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
    Json(json!(created))
}

async fn webhooks_for_project(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(project): Path<String>) -> Json<Value> {
    // Don't expose another tenant's project webhooks (incl. their secrets).
    let t = tenant(&c, &headers);
    if norm(&c.projects.team_of(&project)) != t {
        return Json(json!([]));
    }
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
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(b): Json<CreateWebhook>,
) -> Result<Json<Value>, StatusCode> {
    // A webhook may only be attached to a project the caller owns.
    let t = tenant(&c, &headers);
    if norm(&c.projects.team_of(&project)) != t {
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
    Ok(Json(json!(wh)))
}

async fn webhook_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    if let Some(w) = c.webhooks.snapshot().into_iter().find(|w| w.id == id) {
        if norm(&w.team) != t {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    c.webhooks.remove(&id);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
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

/// Platform-owner view: ALL databases across every tenant (the ops Database Fleet
/// is global, unlike the tenant-scoped `/v1/databases`).
async fn admin_databases_all(State(c): State<Arc<CloudState>>) -> Json<Value> {
    Json(json!(c.databases.list(None)))
}

async fn databases_for_project(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(project): Path<String>) -> Json<Value> {
    // Only expose databases for a project the caller's tenant actually owns.
    let t = tenant(&c, &headers);
    if norm(&c.projects.team_of(&project)) != t {
        return Json(json!([]));
    }
    Json(json!(c.databases.list(Some(&project))))
}

async fn database_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    let d = c.databases.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    if norm(&d.team) != t {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(d)))
}

async fn database_credentials(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    // Returns unmasked connection secrets — must be strictly tenant-scoped.
    let t = tenant(&c, &headers);
    let d = c.databases.get_raw(&id).ok_or(StatusCode::NOT_FOUND)?;
    if norm(&d.team) != t {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!(d)))
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

async fn database_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    if let Some(d) = c.databases.get_raw(&id) {
        if norm(&d.team) != t {
            return Err(StatusCode::NOT_FOUND);
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
    c.databases.remove_db(&id);
    crate::persist::persist(&c);
    Ok(Json(json!({ "removed": id })))
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

async fn securelink_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
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
        // Real cluster capacity = sum of every live node's host resources.
        "resources": {
            "cpu_cores": nodes.iter().map(|n| n.cpu_cores as u64).sum::<u64>(),
            "mem_total_mb": nodes.iter().map(|n| n.mem_total_mb).sum::<u64>(),
            "disk_total_gb": nodes.iter().map(|n| n.disk_total_gb).sum::<u64>(),
        },
    }))
}

async fn admin_audit(State(c): State<Arc<CloudState>>) -> Json<Value> {
    // The durable, append-only audit log of every state mutation (newest first).
    Json(json!(c.audit.recent(300, None)))
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
        if norm(&c.projects.team_of(&d.project)) != team {
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
        if norm(&c.projects.team_of(&b.project)) != team {
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
        if norm(&c.projects.team_of(&ev.project)) != team {
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

async fn notifications_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let team = tenant(&c, &headers);
    let items = build_notifications(&c, &team);
    let inbox = items.iter().filter(|n| !n.archived).count();
    let unread = items.iter().filter(|n| !n.archived && !n.read).count();
    Json(json!({ "unread": unread, "inbox": inbox, "items": items }))
}

async fn notification_archive(State(c): State<Arc<CloudState>>, Path(id): Path<String>) -> Json<Value> {
    c.notifications.archive(&id);
    Json(json!({ "archived": id }))
}

async fn notifications_archive_all(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let team = tenant(&c, &headers);
    let ids: Vec<String> = build_notifications(&c, &team)
        .into_iter()
        .filter(|n| !n.archived)
        .map(|n| n.id)
        .collect();
    c.notifications.archive_all(&ids);
    Json(json!({ "archived": ids.len() }))
}

async fn notifications_read(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let team = tenant(&c, &headers);
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
async fn identity_sync(State(c): State<Arc<CloudState>>, Json(req): Json<IdentitySyncReq>) -> Json<Value> {
    let (tenant, org_slug) = match &req.org {
        Some(o) => {
            let slug = if o.slug.is_empty() { o.id.clone() } else { o.slug.clone() };
            c.identity.upsert_org(&o.id, &slug, &o.name, &o.image_url);
            (slug.clone(), Some(slug))
        }
        None => ("personal".to_string(), None),
    };
    c.identity.upsert_user(
        &req.user.id,
        &req.user.email,
        &req.user.name,
        &req.user.image_url,
        &tenant,
        org_slug.as_deref(),
    );
    crate::persist::persist(&c);
    Json(json!({ "ok": true, "tenant": tenant }))
}

// ============================ Billing & compute credits ============================

async fn billing_get(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
    let acc = c.billing.account(&t);
    Json(json!({
        "account": acc,
        "plans": crate::billing::PLANS,
        "stripe": crate::billing::stripe_configured(),
    }))
}

async fn billing_ledger(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Json<Value> {
    let t = tenant(&c, &headers);
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

async fn billing_checkout(State(c): State<Arc<CloudState>>, headers: HeaderMap, Json(req): Json<CheckoutReq>) -> Json<Value> {
    let t = tenant(&c, &headers);
    let (plan, amount, label) = if req.kind == "credits" {
        let amt = req.amount_cents.unwrap_or(1000);
        ("".to_string(), amt, format!("OpenEdge credits (${:.2})", amt as f64 / 100.0))
    } else {
        let plan = req.plan.unwrap_or_else(|| "pro".into());
        let spec = crate::billing::plan_spec(&plan);
        (plan, spec.price_cents, format!("OpenEdge {} plan", spec.name))
    };
    let co = c.billing.open_checkout(&t, &req.kind, &plan, amount);

    // Real Stripe Checkout when configured; otherwise the local mock checkout.
    if crate::billing::stripe_configured() {
        let base = std::env::var("PUBLIC_URL").unwrap_or_else(|_| "http://localhost:3000".into());
        let success = format!("{base}/billing?success={}", co.id);
        let cancel = format!("{base}/billing?canceled=1");
        match crate::billing::stripe_checkout(&c.http, amount, &label, &success, &cancel).await {
            Ok(url) => return Json(json!({ "url": url, "mock": false, "session": co.id })),
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

async fn billing_confirm(State(c): State<Arc<CloudState>>, headers: HeaderMap, Json(req): Json<ConfirmReq>) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    let (co, acc) = c.billing.confirm_checkout(&req.session).ok_or(StatusCode::NOT_FOUND)?;
    c.audit.record(&t, "user", "charge", "billing", &co.id, &format!("checkout {} {} ${:.2}", co.kind, co.plan, co.amount_cents as f64 / 100.0));
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true, "account": acc })))
}

#[derive(Deserialize)]
struct ChargeReq {
    cents: u64,
    #[serde(default)]
    note: String,
}

async fn billing_charge(State(c): State<Arc<CloudState>>, headers: HeaderMap, Json(req): Json<ChargeReq>) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers);
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
        ("deployments", c.gw.list().into_iter().map(|d| json!(d)).collect()),
        ("projects", projects),
        ("orgs", c.identity.orgs().into_iter().map(|o| json!(o)).collect()),
        ("users", c.identity.users().into_iter().map(|u| json!(u)).collect()),
        ("databases", c.databases.snapshot().into_iter().map(|d| json!(d)).collect()),
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
async fn data_namespaces(State(c): State<Arc<CloudState>>) -> Json<Value> {
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
    Json(json!({ "namespaces": rows }))
}

/// Live status of the always-on GuardianDB durable store: the keys currently
/// held in the iroh-docs `hive-state` KV (one per tenant namespace) plus a
/// content sample proving data round-trips through the replicated store — not a
/// mock. `online` is true once the iroh-backed store has opened.
async fn guardian_status() -> Json<Value> {
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
    Json(json!({
        "store": "guardian-db",
        "engine": "iroh-docs (BLAKE3 · QUIC · Willow reconciliation)",
        "kv": "hive-state",
        "online": !keys.is_empty(),
        "key_count": keys.len(),
        "keys": keys,
        "sample": sample,
    }))
}

async fn data_collections(State(c): State<Arc<CloudState>>) -> Json<Value> {
    let mut cols: Vec<Value> = all_collections(&c)
        .into_iter()
        .map(|(name, rows)| json!({ "name": name, "count": rows.len(), "editable": false }))
        .collect();
    // Editable document collections (full CRUD).
    for (name, count) in c.docs.collections() {
        cols.push(json!({ "name": name, "count": count, "editable": true }));
    }
    Json(json!({ "collections": cols, "store": "guardian-db (iroh) · local snapshot" }))
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
    Path(collection): Path<String>,
    Query(q): Query<DataQ>,
) -> Result<Json<Value>, StatusCode> {
    let typed = all_collections(&c).into_iter().find(|(name, _)| *name == collection).map(|(_, r)| r);
    let editable = typed.is_none();
    let rows = typed.or_else(|| doc_rows(&c, &collection)).ok_or(StatusCode::NOT_FOUND)?;
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
    Path(collection): Path<String>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers);
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
    Path((collection, id)): Path<(String, String)>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, StatusCode> {
    let t = tenant(&c, &headers);
    let doc = c.docs.patch(&id, body).ok_or(StatusCode::NOT_FOUND)?;
    c.audit.record(&t, "user", "update", "document", &id, &collection);
    crate::persist::persist(&c);
    Ok(Json(json!(doc)))
}

/// Delete a row: a custom document, or a typed entry via its owning store.
async fn data_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path((collection, id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers);
    if c.docs.delete(&id) {
        c.audit.record(&t, "user", "delete", "document", &id, &collection);
        crate::persist::persist(&c);
        return Ok(Json(json!({ "deleted": id })));
    }
    let ok = match collection.as_str() {
        "deployments" => c.gw.remove(&id).await.is_some(),
        "databases" => { c.databases.remove_db(&id); true }
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

// ============================ Deployment preview / thumbnail ============================

/// Preview metadata for a project's production deployment: a site thumbnail for
/// frontends, or the JSON/text body for backend services.
async fn project_preview(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> Json<Value> {
    // Don't render another tenant's app content as a preview.
    let t = tenant(&c, &headers);
    if norm(&c.projects.team_of(&project)) != t {
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

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') { c.to_string() } else { format!("%{:02X}", c as u32) })
        .collect()
}

/// A PNG thumbnail of the deployed site, captured with headless Chrome and
/// cached per deployment. Falls back to a generated SVG card if capture fails.
async fn project_thumbnail(
    State(c): State<Arc<CloudState>>,
    Path(project): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
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
