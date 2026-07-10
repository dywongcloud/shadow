//! Project-scoped Microfrontends HTTP API — group + membership CRUD, config
//! generation, and the per-project settings surface backing the dashboard's
//! Project Settings → Microfrontends tab.
//!
//! Storage is [`crate::enterprise::EnterpriseStore`] (`mfe`), the single source of
//! truth (persisted + gossiped). Every mutation is validated with
//! [`crate::microfrontends::validate_group`] BEFORE it is committed (validate the
//! candidate group, then `set_mfe_group`), so the store never holds an invalid
//! group. All writes enforce tenant ownership of every referenced project.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::admin::{require_project, tenant};
use crate::microfrontends::{validate_group, MfeDevelopment, MfeError, MfeGroup, MfeMembership, MfeRoute};
use crate::state::CloudState;
use hive_core::now_ms;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        // Per-project settings surface (dashboard Project Settings > Microfrontends).
        .route(
            "/v1/projects/:project/settings/microfrontends",
            get(project_mfe_get).put(project_mfe_put),
        )
        // Group CRUD.
        .route("/v1/microfrontends/groups", get(groups_list).post(group_create))
        .route(
            "/v1/microfrontends/groups/:groupId",
            get(group_get).patch(group_patch).delete(group_delete),
        )
        .route("/v1/microfrontends/groups/:groupId/config", get(group_config))
        // Membership CRUD.
        .route("/v1/microfrontends/groups/:groupId/members", post(member_add))
        .route(
            "/v1/microfrontends/groups/:groupId/members/:projectId",
            axum::routing::patch(member_patch).delete(member_remove),
        )
}

// ---------------------------------------------------------------------------
// Error mapping — MfeError -> HTTP status + {code, error} JSON body
// ---------------------------------------------------------------------------

fn mfe_err(e: MfeError) -> (StatusCode, String) {
    let status = match &e {
        MfeError::GroupNotFound(_) | MfeError::UnknownMember(_) | MfeError::TargetDeploymentNotFound { .. } => {
            StatusCode::NOT_FOUND
        }
        MfeError::Unauthorized(_) => StatusCode::FORBIDDEN,
        MfeError::ProjectAlreadyInGroup { .. } => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, json!({ "code": e.code(), "error": e.message() }).to_string())
}

fn bad(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, json!({ "error": msg }).to_string())
}

/// Every referenced project (default + members) must belong to `team`, else the
/// group could compose another tenant's deployments.
fn ensure_team_owns(c: &Arc<CloudState>, team: &str, projects: impl IntoIterator<Item = String>) -> Result<(), (StatusCode, String)> {
    for p in projects {
        // Fleet-aware ownership (settings rows are node-local; remotely-placed
        // projects are judged from their deployment tenant tags).
        if !crate::admin::project_owned_by(c, &p, team) {
            return Err(mfe_err(MfeError::Unauthorized(format!("project '{p}' belongs to a different team"))));
        }
    }
    Ok(())
}

/// Plan gate for MFE writes (Pro or Enterprise).
fn require_plan(c: &Arc<CloudState>, team: &str) -> Result<(), (StatusCode, String)> {
    let plan = c.teams.get(team).map(|t| t.plan).unwrap_or_else(|| c.billing.account(team).plan);
    if !crate::billing::plan_allows_microfrontends(&plan) {
        return Err((StatusCode::FORBIDDEN, json!({ "error": "Microfrontends require the Pro or Enterprise plan" }).to_string()));
    }
    Ok(())
}

/// Validate a candidate group, then commit it (upsert + persist + audit). The
/// store only ever sees valid groups.
fn commit_group(c: &Arc<CloudState>, team: &str, mut g: MfeGroup, action: &str) -> Result<MfeGroup, (StatusCode, String)> {
    g.updated_ms = now_ms();
    let normalized = g.clone().normalized();
    validate_group(&normalized).map_err(mfe_err)?;
    let saved = c.enterprise.set_mfe_group(team, g);
    c.audit.record(team, "user", action, "microfrontend_group", &saved.id, &saved.name);
    let ev = c.event(&c.region, "PUT", "", &format!("/microfrontends/{}", saved.id), 200, "mfe-config", action);
    c.record(ev);
    crate::persist::persist(c);
    Ok(saved)
}

// ---------------------------------------------------------------------------
// Group CRUD
// ---------------------------------------------------------------------------

async fn groups_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!({ "groups": c.enterprise.mfe_groups(&t) }))
}

#[derive(Deserialize)]
struct GroupCreateReq {
    name: String,
    /// The default (host) project id.
    #[serde(default, alias = "defaultProjectId")]
    default_project: String,
}

async fn group_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<GroupCreateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_plan(&c, &t)?;
    if b.name.trim().is_empty() || b.default_project.trim().is_empty() {
        return Err(bad("name and defaultProjectId are required"));
    }
    ensure_team_owns(&c, &t, [b.default_project.clone()])?;
    // First-implementation constraint: a project belongs to at most one group.
    if let Some(existing) = c.enterprise.mfe_group_of_project(&t, &b.default_project) {
        return Err(mfe_err(MfeError::ProjectAlreadyInGroup { project: b.default_project, group: existing.id }));
    }
    let id = format!("mfe_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let g = MfeGroup::new(id, b.name.trim().to_string(), b.default_project.trim().to_string(), now_ms());
    let saved = commit_group(&c, &t, g, "create")?;
    Ok(Json(json!(saved)))
}

async fn group_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(group_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.enterprise.mfe_group(&t, &group_id).map(|g| Json(json!(g))).ok_or_else(|| mfe_err(MfeError::GroupNotFound(group_id)))
}

async fn group_config(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(group_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let g = c.enterprise.mfe_group(&t, &group_id).ok_or_else(|| mfe_err(MfeError::GroupNotFound(group_id)))?;
    Ok(Json(crate::microfrontends::to_vercel_config(&g)))
}

#[derive(Deserialize)]
struct GroupPatchReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, alias = "defaultProjectId")]
    default_project: Option<String>,
    #[serde(default, alias = "fallbackEnvironment")]
    fallback_environment: Option<String>,
    #[serde(default, alias = "customFallbackEnvironmentName")]
    custom_fallback_environment_name: Option<String>,
    #[serde(default, alias = "disableOverrides")]
    disable_overrides: Option<bool>,
    #[serde(default, alias = "localProxyPort")]
    local_proxy_port: Option<u32>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn group_patch(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(group_id): Path<String>,
    Json(b): Json<GroupPatchReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_plan(&c, &t)?;
    let mut g = c.enterprise.mfe_group(&t, &group_id).ok_or_else(|| mfe_err(MfeError::GroupNotFound(group_id.clone())))?;
    if let Some(n) = b.name.filter(|s| !s.trim().is_empty()) {
        g.name = n.trim().to_string();
    }
    if let Some(dp) = b.default_project.filter(|s| !s.trim().is_empty()) {
        let dp = dp.trim().to_string();
        // Promoting a different member to default: it must already be a member.
        if g.member(&dp).is_none() {
            return Err(mfe_err(MfeError::UnknownMember(dp)));
        }
        g.host_project = dp;
    }
    if let Some(fe) = b.fallback_environment {
        g.fallback_environment = fe;
    }
    if let Some(name) = b.custom_fallback_environment_name {
        g.custom_fallback_environment_name = if name.trim().is_empty() { None } else { Some(name.trim().to_string()) };
    }
    if let Some(d) = b.disable_overrides {
        g.disable_overrides = d;
    }
    if let Some(p) = b.local_proxy_port {
        g.local_proxy_port = if p == 0 { None } else { Some(p) };
    }
    if let Some(e) = b.enabled {
        g.enabled = e;
    }
    let saved = commit_group(&c, &t, g, "update")?;
    Ok(Json(json!(saved)))
}

async fn group_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(group_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_plan(&c, &t)?;
    // Only the OWNING node may authoritatively delete (local groups); a gossiped
    // copy is read-only here.
    if c.enterprise.mfe_groups_local(&t).iter().all(|g| g.id != group_id) {
        return Err(mfe_err(MfeError::GroupNotFound(group_id)));
    }
    c.enterprise.remove_mfe_group(&t, &group_id);
    c.audit.record(&t, "user", "delete", "microfrontend_group", &group_id, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Membership CRUD
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MemberAddReq {
    project: String,
    /// "child" (default) | "default". Adding as default promotes it.
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    routing: Option<Vec<RouteReq>>,
}

#[derive(Deserialize, Default)]
struct RouteReq {
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    flag: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
}

impl RouteReq {
    fn into_route(self) -> MfeRoute {
        MfeRoute { group: self.group.filter(|s| !s.trim().is_empty()), flag: self.flag.filter(|s| !s.trim().is_empty()), paths: self.paths }
    }
}

async fn member_add(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(group_id): Path<String>,
    Json(b): Json<MemberAddReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_plan(&c, &t)?;
    let project = b.project.trim().to_string();
    if project.is_empty() {
        return Err(bad("project is required"));
    }
    ensure_team_owns(&c, &t, [project.clone()])?;
    let mut g = c.enterprise.mfe_group(&t, &group_id).ok_or_else(|| mfe_err(MfeError::GroupNotFound(group_id.clone())))?;
    // At most one group per project (across the team).
    if let Some(existing) = c.enterprise.mfe_group_of_project(&t, &project) {
        if existing.id != g.id {
            return Err(mfe_err(MfeError::ProjectAlreadyInGroup { project, group: existing.id }));
        }
    }
    if g.member(&project).is_some() {
        return Err(mfe_err(MfeError::ProjectAlreadyInGroup { project, group: g.id }));
    }
    let now = now_ms();
    let make_default = b.role.as_deref() == Some("default");
    let routing: Vec<MfeRoute> = b.routing.unwrap_or_default().into_iter().map(RouteReq::into_route).collect();
    g.members.push(MfeMembership {
        project: project.clone(),
        role: if make_default { "default".into() } else { "child".into() },
        routing,
        default_route: None,
        package_name: None,
        asset_prefix: None,
        observability_routing: "default_application".into(),
        development: None,
        created_ms: now,
        updated_ms: now,
    });
    if make_default {
        g.host_project = project;
    }
    let saved = commit_group(&c, &t, g, "add_member")?;
    Ok(Json(json!(saved)))
}

#[derive(Deserialize)]
struct MemberPatchReq {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    routing: Option<Vec<RouteReq>>,
    #[serde(default, alias = "defaultRoute")]
    default_route: Option<String>,
    #[serde(default, alias = "packageName")]
    package_name: Option<String>,
    #[serde(default, alias = "assetPrefix")]
    asset_prefix: Option<String>,
    #[serde(default, alias = "observabilityRouting")]
    observability_routing: Option<String>,
    #[serde(default)]
    development: Option<DevelopmentReq>,
}

#[derive(Deserialize, Default)]
struct DevelopmentReq {
    #[serde(default)]
    local: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    fallback: Option<String>,
}

/// Apply the membership patch to `g`'s member `project` in place.
fn apply_member_patch(g: &mut MfeGroup, project: &str, b: MemberPatchReq, now: u64) -> Result<(), (StatusCode, String)> {
    // Promotion to default rewires the group's host_project.
    if b.role.as_deref() == Some("default") {
        g.host_project = project.to_string();
    }
    let Some(m) = g.members.iter_mut().find(|m| m.project == project) else {
        return Err(mfe_err(MfeError::UnknownMember(project.to_string())));
    };
    if let Some(routing) = b.routing {
        m.routing = routing.into_iter().map(RouteReq::into_route).collect();
    }
    if let Some(dr) = b.default_route {
        m.default_route = if dr.trim().is_empty() { None } else { Some(dr.trim().to_string()) };
    }
    if let Some(pn) = b.package_name {
        m.package_name = if pn.trim().is_empty() { None } else { Some(pn.trim().to_string()) };
    }
    if let Some(ap) = b.asset_prefix {
        m.asset_prefix = if ap.trim().is_empty() { None } else { Some(ap.trim().trim_matches('/').to_string()) };
    }
    if let Some(obs) = b.observability_routing {
        m.observability_routing = if obs == "this_project" { "this_project".into() } else { "default_application".into() };
    }
    if let Some(dev) = b.development {
        let d = MfeDevelopment {
            local: dev.local.filter(|s| !s.trim().is_empty()),
            task: dev.task.filter(|s| !s.trim().is_empty()),
            fallback: dev.fallback.filter(|s| !s.trim().is_empty()),
        };
        m.development = if d == MfeDevelopment::default() { None } else { Some(d) };
    }
    m.updated_ms = now;
    Ok(())
}

async fn member_patch(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((group_id, project)): Path<(String, String)>,
    Json(b): Json<MemberPatchReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_plan(&c, &t)?;
    let mut g = c.enterprise.mfe_group(&t, &group_id).ok_or_else(|| mfe_err(MfeError::GroupNotFound(group_id.clone())))?;
    apply_member_patch(&mut g, &project, b, now_ms())?;
    let saved = commit_group(&c, &t, g, "update_member")?;
    Ok(Json(json!(saved)))
}

async fn member_remove(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((group_id, project)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_plan(&c, &t)?;
    let mut g = c.enterprise.mfe_group(&t, &group_id).ok_or_else(|| mfe_err(MfeError::GroupNotFound(group_id.clone())))?;
    // The default app cannot be removed while children remain (pure guard).
    crate::microfrontends::can_remove_member(&g, &project).map_err(|e| match e {
        MfeError::MissingDefaultApp => (
            StatusCode::CONFLICT,
            json!({
                "code": "MICROFRONTENDS_MISSING_DEFAULT_APP",
                "error": "cannot remove the default application while child projects remain; remove the children or promote a new default first"
            })
            .to_string(),
        ),
        other => mfe_err(other),
    })?;
    g.members.retain(|m| m.project != project);
    if g.members.is_empty() {
        // Emptied the group: delete it outright rather than persist a defaultless shell.
        c.enterprise.remove_mfe_group(&t, &group_id);
        c.audit.record(&t, "user", "delete", "microfrontend_group", &group_id, "");
        crate::persist::persist(&c);
        return Ok(Json(json!({ "ok": true, "deleted_group": true })));
    }
    let saved = commit_group(&c, &t, g, "remove_member")?;
    Ok(Json(json!(saved)))
}

// ---------------------------------------------------------------------------
// Per-project settings surface
// ---------------------------------------------------------------------------

/// GET the current project's microfrontends state: whether it is in a group, its
/// role, and the group (if any) — the payload the settings tab renders.
async fn project_mfe_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let group = c.enterprise.mfe_group_of_project(&t, &project);
    let role = group.as_ref().and_then(|g| g.member(&project)).map(|m| m.role.clone());
    // Sibling projects in the team available to add to a group (not already in one).
    Ok(Json(json!({
        "project": project,
        "in_group": group.is_some(),
        "role": role,
        "group": group,
        "groups": c.enterprise.mfe_groups(&t),
    })))
}

/// PUT the current project's membership config within its group (routing,
/// defaultRoute, assetPrefix, observability, development). This is the save action
/// on the project settings page; it edits ONLY this project's membership.
async fn project_mfe_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Json(b): Json<MemberPatchReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    require_plan(&c, &t)?;
    let mut g = c
        .enterprise
        .mfe_group_of_project(&t, &project)
        .ok_or_else(|| bad("project is not a member of any microfrontend group"))?;
    apply_member_patch(&mut g, &project, b, now_ms())?;
    let saved = commit_group(&c, &t, g, "update_member")?;
    Ok(Json(json!(saved)))
}
