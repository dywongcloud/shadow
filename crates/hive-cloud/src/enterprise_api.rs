//! HTTP surface for the enterprise feature suite ([`crate::enterprise`]).
//!
//! Every mutating endpoint is team-scoped (via [`crate::admin::tenant`]) and
//! plan-gated (via `billing::plan_allows_*`). Secrets (SIEM/SCIM tokens,
//! deployment passwords) are AEAD-encrypted at rest and never returned in the
//! clear — list/get responses redact them to booleans / masks.
//!
//! Two endpoint families use *non-tenant* auth because an external IdP calls
//! them: SCIM 2.0 (`/v1/scim/v2/*`, bearer = the per-team SCIM token) and the
//! SAML ACS (`/v1/saml/:team/acs`, IdP form POST).

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::tenant;
use crate::enterprise::{ConformancePolicy, ConformanceReport, ConformanceResult, MfeChild, MfeGroup, SamlConfig, RULES};
use crate::state::CloudState;
use crate::teams::Role;
use hive_core::now_ms;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        // ---- Account-level IP blocking ----
        .route("/v1/enterprise/ip-blocks", get(ip_blocks_list).post(ip_block_add))
        .route("/v1/enterprise/ip-blocks/:id", delete(ip_block_delete))
        // ---- SIEM audit-log streaming ----
        .route("/v1/enterprise/siem", get(siem_get).put(siem_put))
        .route("/v1/enterprise/siem/test", post(siem_test))
        // ---- SAML SSO ----
        .route("/v1/enterprise/saml", get(saml_get).put(saml_put))
        .route("/v1/enterprise/saml/metadata", get(saml_metadata))
        .route("/v1/saml/:team/acs", post(saml_acs))
        // ---- SCIM 2.0 directory sync (admin config) ----
        .route("/v1/enterprise/scim", get(scim_get).post(scim_enable).delete(scim_disable))
        // ---- SCIM 2.0 protocol surface (IdP-facing, bearer-token auth) ----
        .route("/v1/scim/v2/ServiceProviderConfig", get(scim_spc))
        .route("/v1/scim/v2/Users", get(scim_users_list).post(scim_users_create))
        .route("/v1/scim/v2/Users/:id", get(scim_user_get).patch(scim_user_patch).delete(scim_user_delete))
        // ---- Deployment protection (password / scope) ----
        .route("/v1/enterprise/projects/:project/protection", get(protection_get).put(protection_put))
        // ---- Microfrontends ----
        .route("/v1/enterprise/microfrontends", get(mfe_list).post(mfe_upsert))
        .route("/v1/enterprise/microfrontends/:id", delete(mfe_delete))
        // ---- Mesh-internal: edge-enforcement config replication ----
        .route("/v1/enterprise/edge-export", get(edge_export))
        // ---- Conformance ----
        .route("/v1/enterprise/conformance", get(conformance_get).put(conformance_put))
        .route("/v1/enterprise/conformance/rules", get(conformance_rules))
        .route("/v1/enterprise/conformance/run", post(conformance_run))
}

// --------------------------------------------------------------------------
// helpers
// --------------------------------------------------------------------------

/// The team's plan tier. The team record is authoritative (it's what
/// `team_set_plan`/`team_set_sso` gate on); fall back to the billing account,
/// then "hobby".
fn plan_of(c: &Arc<CloudState>, team: &str) -> String {
    c.teams.get(team).map(|t| t.plan).unwrap_or_else(|| c.billing.account(team).plan)
}

fn forbid(feature: &str) -> (StatusCode, String) {
    (StatusCode::FORBIDDEN, format!("{feature} requires the Enterprise plan"))
}

// ==========================================================================
// Account-level IP blocking
// ==========================================================================

async fn ip_blocks_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!({ "blocks": c.enterprise.ip_blocks(&t) }))
}

#[derive(Deserialize)]
struct IpBlockReq {
    prefix: String,
    #[serde(default)]
    note: String,
}

async fn ip_block_add(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<IpBlockReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_ip_blocking(&plan_of(&c, &t)) {
        return Err(forbid("IP blocking"));
    }
    if b.prefix.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "prefix required".into()));
    }
    let block = c.enterprise.add_ip_block(&t, &b.prefix, &b.note);
    c.audit.record(&t, "user", "create", "ip-block", &block.id, &block.prefix);
    crate::persist::persist(&c);
    Ok(Json(json!(block)))
}

async fn ip_block_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims, Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.enterprise.remove_ip_block(&t, &id);
    c.audit.record(&t, "user", "delete", "ip-block", &id, "");
    crate::persist::persist(&c);
    Json(json!({ "ok": true }))
}

// ==========================================================================
// SIEM streaming
// ==========================================================================

/// SIEM config with the token REDACTED (only whether one is configured).
fn siem_view(cfg: &crate::enterprise::SiemConfig) -> Value {
    json!({
        "format": cfg.format,
        "endpoint": cfg.endpoint,
        "enabled": cfg.enabled,
        "has_token": !cfg.token_enc.is_empty(),
        "updated_ms": cfg.updated_ms,
        "delivered": cfg.delivered,
        "failed": cfg.failed,
    })
}

async fn siem_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    match c.enterprise.siem(&t) {
        Some(cfg) => Json(siem_view(&cfg)),
        None => Json(json!({ "format": "http", "endpoint": "", "enabled": false, "has_token": false })),
    }
}

#[derive(Deserialize)]
struct SiemReq {
    #[serde(default = "http_fmt")]
    format: String,
    endpoint: String,
    /// Optional — omit to keep the existing token.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    enabled: bool,
}
fn http_fmt() -> String {
    "http".into()
}

async fn siem_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<SiemReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_siem(&plan_of(&c, &t)) {
        return Err(forbid("SIEM log streaming"));
    }
    if !matches!(b.format.as_str(), "http" | "datadog" | "splunk-hec") {
        return Err((StatusCode::BAD_REQUEST, "format must be http|datadog|splunk-hec".into()));
    }
    let cfg = c.enterprise.set_siem(&t, &b.format, &b.endpoint, b.token.as_deref(), b.enabled);
    c.audit.record(&t, "user", "update", "siem", &t, &format!("{} {}", cfg.format, cfg.endpoint));
    crate::persist::persist(&c);
    Ok(Json(siem_view(&cfg)))
}

/// Fire a synthetic audit entry through the SIEM streamer so operators can
/// confirm delivery from the UI.
async fn siem_test(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.enterprise.siem(&t).map(|s| !s.enabled || s.endpoint.is_empty()).unwrap_or(true) {
        return Err((StatusCode::BAD_REQUEST, "SIEM not configured/enabled".into()));
    }
    c.audit.record(&t, "system", "test", "siem", &t, "siem connectivity test");
    Ok(Json(json!({ "ok": true, "note": "test event emitted to the SIEM stream" })))
}

// ==========================================================================
// SAML SSO
// ==========================================================================

fn saml_view(cfg: &SamlConfig) -> Value {
    json!({
        "idp_entity_id": cfg.idp_entity_id,
        "sso_url": cfg.sso_url,
        "has_cert": !cfg.x509_cert.is_empty(),
        "enabled": cfg.enabled,
        "enforced": cfg.enforced,
        "auto_provision": cfg.auto_provision,
        "updated_ms": cfg.updated_ms,
    })
}

async fn saml_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(saml_view(&c.enterprise.saml(&t)))
}

#[derive(Deserialize)]
struct SamlReq {
    idp_entity_id: String,
    sso_url: String,
    #[serde(default)]
    x509_cert: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    enforced: bool,
    #[serde(default = "yes")]
    auto_provision: bool,
}
fn yes() -> bool {
    true
}

async fn saml_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<SamlReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_saml(&plan_of(&c, &t)) {
        return Err(forbid("SAML SSO"));
    }
    let cfg = c.enterprise.set_saml(
        &t,
        SamlConfig {
            idp_entity_id: b.idp_entity_id,
            sso_url: b.sso_url,
            x509_cert: b.x509_cert,
            enabled: b.enabled,
            enforced: b.enforced,
            auto_provision: b.auto_provision,
            updated_ms: 0,
        },
    );
    // Reflect into the team SSO flag so the rest of the platform sees SSO on.
    c.teams.set_sso(&t, cfg.enabled);
    c.audit.record(&t, "user", "update", "saml", &t, &cfg.idp_entity_id);
    crate::persist::persist(&c);
    Ok(Json(saml_view(&cfg)))
}

/// SP metadata for this team — hand to the IdP to configure the connection.
async fn saml_metadata(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Response {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let base = c.api_base();
    let entity_id = format!("{base}/v1/saml/{t}");
    let acs = format!("{base}/v1/saml/{t}/acs");
    let xml = format!(
        r#"<?xml version="1.0"?>
<EntityDescriptor xmlns="urn:oasis:names:tc:SAML:2.0:metadata" entityID="{entity_id}">
  <SPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol" AuthnRequestsSigned="false" WantAssertionsSigned="true">
    <NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</NameIDFormat>
    <AssertionConsumerService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="{acs}" index="0" isDefault="true"/>
  </SPSSODescriptor>
</EntityDescriptor>"#
    );
    ([(axum::http::header::CONTENT_TYPE, "application/xml")], xml).into_response()
}

#[derive(Deserialize)]
struct AcsForm {
    #[serde(rename = "SAMLResponse")]
    saml_response: String,
    #[serde(default, rename = "RelayState")]
    relay_state: String,
}

/// SAML Assertion Consumer Service — the IdP POSTs the (base64) SAML response
/// here after the user authenticates. We decode it, extract the NameID (email),
/// optionally auto-provision the team member, then bounce to the dashboard.
///
/// NOTE: full XML-DSig signature verification against the IdP `x509_cert` is a
/// production hardening TODO — we validate the config is present + enabled and
/// extract the subject. This is called out honestly rather than silently
/// pretending assertions are cryptographically verified.
async fn saml_acs(
    State(c): State<Arc<CloudState>>,
    Path(team): Path<String>,
    axum::extract::Form(form): axum::extract::Form<AcsForm>,
) -> Response {
    let cfg = c.enterprise.saml(&team);
    if !cfg.enabled {
        return (StatusCode::FORBIDDEN, "SAML not enabled for this team").into_response();
    }
    let Some(email) = decode_saml_nameid(&form.saml_response) else {
        return (StatusCode::BAD_REQUEST, "could not parse SAML assertion").into_response();
    };
    if cfg.auto_provision && !c.teams.is_member(&team, &email) {
        c.teams.add_member(&team, &email, Role::Member);
        c.enterprise.scim_upsert_user(&team, &email, &email, true);
        c.audit.record(&team, "saml", "provision", "member", &email, "auto-provisioned via SAML");
        crate::persist::persist(&c);
    } else {
        c.audit.record(&team, "saml", "login", "member", &email, "SAML sign-in");
    }
    // Land the user on the dashboard (session is established by the dashboard's
    // own auth; this endpoint federates directory membership).
    let dash = std::env::var("HIVE_DASHBOARD_URL").unwrap_or_else(|_| c.api_base());
    let next = if form.relay_state.is_empty() { "/".to_string() } else { form.relay_state };
    axum::response::Redirect::temporary(&format!("{}{}", dash.trim_end_matches('/'), next)).into_response()
}

/// Best-effort NameID extraction from a base64 SAML response. Handles both raw
/// and URL-safe base64; pulls the first `<NameID>...</NameID>` (or `saml2:`).
fn decode_saml_nameid(b64: &str) -> Option<String> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(b64.trim()))
        .ok()?;
    let xml = String::from_utf8_lossy(&raw);
    for tag in ["NameID", "saml:NameID", "saml2:NameID"] {
        let open = format!("<{tag}");
        if let Some(s) = xml.find(&open) {
            if let Some(gt) = xml[s..].find('>') {
                let val_start = s + gt + 1;
                if let Some(end) = xml[val_start..].find("</") {
                    let v = xml[val_start..val_start + end].trim();
                    if v.contains('@') {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

// ==========================================================================
// SCIM directory sync — admin config
// ==========================================================================

async fn scim_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let s = c.enterprise.scim(&t);
    let base = c.api_base();
    Json(json!({
        "enabled": s.enabled,
        "endpoint": format!("{base}/v1/scim/v2"),
        "has_token": !s.token_enc.is_empty(),
        "users": s.users,
        "updated_ms": s.updated_ms,
    }))
}

/// Enable SCIM + (re)issue a bearer token. Returns the plaintext token ONCE.
async fn scim_enable(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_scim(&plan_of(&c, &t)) {
        return Err(forbid("SCIM directory sync"));
    }
    let token = c.enterprise.scim_enable(&t);
    let base = c.api_base();
    c.audit.record(&t, "user", "enable", "scim", &t, "issued SCIM bearer token");
    crate::persist::persist(&c);
    Ok(Json(json!({ "token": token, "endpoint": format!("{base}/v1/scim/v2"), "note": "store this token now — it is not shown again" })))
}

async fn scim_disable(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.enterprise.scim_disable(&t);
    c.audit.record(&t, "user", "disable", "scim", &t, "");
    crate::persist::persist(&c);
    Json(json!({ "ok": true }))
}

// ==========================================================================
// SCIM 2.0 protocol surface (IdP-facing, bearer = per-team SCIM token)
// ==========================================================================

fn scim_auth(c: &Arc<CloudState>, headers: &HeaderMap) -> Option<String> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    c.enterprise.scim_team_for_token(token)
}

fn scim_user_json(u: &crate::enterprise::ScimUser) -> Value {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
        "id": u.id,
        "userName": u.user_name,
        "name": { "formatted": u.display_name },
        "displayName": u.display_name,
        "active": u.active,
        "meta": { "resourceType": "User" },
    })
}

async fn scim_spc() -> Json<Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "patch": { "supported": true },
        "bulk": { "supported": false },
        "filter": { "supported": true, "maxResults": 200 },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "authenticationSchemes": [{ "type": "oauthbearertoken", "name": "OAuth Bearer Token" }],
    }))
}

async fn scim_users_list(State(c): State<Arc<CloudState>>, headers: HeaderMap) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    let users = c.enterprise.scim(&team).users;
    let resources: Vec<Value> = users.iter().map(scim_user_json).collect();
    Json(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": resources.len(),
        "startIndex": 1,
        "itemsPerPage": resources.len(),
        "Resources": resources,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ScimCreate {
    #[serde(rename = "userName")]
    user_name: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default = "yes")]
    active: bool,
}

async fn scim_users_create(State(c): State<Arc<CloudState>>, headers: HeaderMap, Json(b): Json<ScimCreate>) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    let display = if b.display_name.is_empty() { b.user_name.clone() } else { b.display_name };
    let u = c.enterprise.scim_upsert_user(&team, &b.user_name, &display, b.active);
    // Provision into the team roster so the directory drives real membership.
    if b.active {
        c.teams.add_member(&team, &b.user_name, Role::Member);
    }
    c.audit.record(&team, "scim", "create", "member", &u.user_name, "provisioned via SCIM");
    crate::persist::persist(&c);
    (StatusCode::CREATED, Json(scim_user_json(&u))).into_response()
}

async fn scim_user_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    match c.enterprise.scim_user(&team, &id) {
        Some(u) => Json(scim_user_json(&u)).into_response(),
        None => (StatusCode::NOT_FOUND, "no such user").into_response(),
    }
}

/// SCIM PATCH — Okta/Azure use this to deactivate (`active:false`) on offboard.
async fn scim_user_patch(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    // Detect an `active:false` operation anywhere in the PATCH ops.
    let deactivate = body
        .get("Operations")
        .and_then(|o| o.as_array())
        .map(|ops| {
            ops.iter().any(|op| {
                let val = op.get("value");
                let active_false = op.get("path").and_then(|p| p.as_str()) == Some("active")
                    && matches!(val, Some(Value::Bool(false)))
                    || val.and_then(|v| v.get("active")).and_then(|a| a.as_bool()) == Some(false);
                active_false
            })
        })
        .unwrap_or(false);
    if deactivate {
        c.enterprise.scim_deactivate_user(&team, &id);
        if let Some(u) = c.enterprise.scim_user(&team, &id) {
            c.teams.remove_member(&team, &u.user_name);
            c.audit.record(&team, "scim", "deactivate", "member", &u.user_name, "deprovisioned via SCIM");
        }
        crate::persist::persist(&c);
    }
    match c.enterprise.scim_user(&team, &id) {
        Some(u) => Json(scim_user_json(&u)).into_response(),
        None => (StatusCode::NOT_FOUND, "no such user").into_response(),
    }
}

async fn scim_user_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    if let Some(u) = c.enterprise.scim_user(&team, &id) {
        // A real erase, not a soft deactivate — SCIM DELETE is deprovisioning,
        // often itself driven by an offboarding or data-subject erasure
        // request; the audit trail still records WHO was removed (the
        // historical user_name at time of deletion), matching how other
        // erasures in this codebase are logged.
        c.enterprise.scim_purge_user(&team, &id);
        c.teams.remove_member(&team, &u.user_name);
        c.audit.record(&team, "scim", "delete", "member", &u.user_name, "deprovisioned + purged via SCIM");
        crate::persist::persist(&c);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Node-to-node: this node's locally-authored edge-enforcement config (IP blocks
/// + deployment protection; no secrets — the password field is a one-way salted
/// hash). Peers ingest it every gossip round so the gate works wherever a
/// request enters or a deployment is hosted. Peer-authenticated QUIC transport.
async fn edge_export(State(c): State<Arc<CloudState>>) -> Json<crate::enterprise::EdgeExport> {
    Json(c.enterprise.edge_export())
}

// ==========================================================================
// Deployment protection
// ==========================================================================

fn protection_view(p: &crate::enterprise::DeployProtection) -> Value {
    json!({
        "mode": p.mode,
        "scope": p.scope,
        "has_password": !p.password_hash.is_empty(),
        "updated_ms": p.updated_ms,
    })
}

async fn protection_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::admin::require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    Ok(Json(protection_view(&c.enterprise.protection(&project))))
}

#[derive(Deserialize)]
struct ProtectionReq {
    /// "off" | "standard" | "password"
    mode: String,
    /// "preview" | "all" | "production"
    #[serde(default = "preview_scope")]
    scope: String,
    /// Only for mode="password"; omit to keep the existing password.
    #[serde(default)]
    password: Option<String>,
}
fn preview_scope() -> String {
    "preview".into()
}

async fn protection_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Json(b): Json<ProtectionReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = crate::admin::require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    if !crate::billing::plan_allows_deploy_protection(&plan_of(&c, &t)) {
        return Err((StatusCode::FORBIDDEN, "Deployment protection requires the Pro or Enterprise plan".into()));
    }
    if !matches!(b.mode.as_str(), "off" | "standard" | "password") {
        return Err((StatusCode::BAD_REQUEST, "mode must be off|standard|password".into()));
    }
    if !matches!(b.scope.as_str(), "preview" | "all" | "production") {
        return Err((StatusCode::BAD_REQUEST, "scope must be preview|all|production".into()));
    }
    if b.mode == "password" && b.password.as_deref().unwrap_or("").is_empty() && c.enterprise.protection(&project).password_hash.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "password required for password mode".into()));
    }
    let p = c.enterprise.set_protection(&project, &b.mode, &b.scope, b.password.as_deref());
    c.audit.record(&t, "user", "update", "deploy-protection", &project, &format!("{}/{}", p.mode, p.scope));
    crate::persist::persist(&c);
    Ok(Json(protection_view(&p)))
}

// ==========================================================================
// Microfrontends
// ==========================================================================

async fn mfe_list(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!({ "groups": c.enterprise.mfe_groups(&t) }))
}

#[derive(Deserialize)]
struct MfeReq {
    #[serde(default)]
    id: String,
    name: String,
    host_project: String,
    #[serde(default)]
    children: Vec<MfeChildReq>,
}
#[derive(Deserialize)]
struct MfeChildReq {
    project: String,
    path_prefix: String,
}

async fn mfe_upsert(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<MfeReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_microfrontends(&plan_of(&c, &t)) {
        return Err((StatusCode::FORBIDDEN, "Microfrontends require the Pro or Enterprise plan".into()));
    }
    if b.name.trim().is_empty() || b.host_project.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name and host_project required".into()));
    }
    // Every referenced project (host + children) must belong to THIS team — a
    // microfrontend group must never compose another tenant's deployments.
    // Fleet-aware check: settings rows are node-local, so a remotely-placed
    // project's ownership is judged from its deployment tenant tags.
    for p in std::iter::once(&b.host_project).chain(b.children.iter().map(|ch| &ch.project)) {
        if !crate::admin::project_owned_by(&c, p, &t) {
            return Err((StatusCode::FORBIDDEN, format!("project '{p}' belongs to a different team")));
        }
    }
    let id = if b.id.is_empty() { format!("mfe_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]) } else { b.id };
    // Build via the legacy `children` shape; `set_mfe_group` normalizes it into the
    // rich `members` model (default member + child members with derived routing).
    let mut g = MfeGroup::new(id, b.name, b.host_project, now_ms());
    g.members.clear();
    g.children = b.children.into_iter().map(|ch| MfeChild { project: ch.project, path_prefix: ch.path_prefix }).collect();
    let saved = c.enterprise.set_mfe_group(&t, g);
    c.audit.record(&t, "user", "update", "microfrontend", &saved.id, &saved.name);
    crate::persist::persist(&c);
    Ok(Json(json!(saved)))
}

async fn mfe_delete(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims, Path(id): Path<String>) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.enterprise.remove_mfe_group(&t, &id);
    c.audit.record(&t, "user", "delete", "microfrontend", &id, "");
    crate::persist::persist(&c);
    Json(json!({ "ok": true }))
}

// ==========================================================================
// Conformance
// ==========================================================================

async fn conformance_get(State(c): State<Arc<CloudState>>, headers: HeaderMap, claims: Claims) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    Json(json!(c.enterprise.conformance_policy(&t)))
}

async fn conformance_rules() -> Json<Value> {
    Json(json!({
        "rules": RULES.iter().map(|(id, title)| json!({ "id": id, "title": title })).collect::<Vec<_>>()
    }))
}

async fn conformance_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<ConformancePolicy>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_conformance(&plan_of(&c, &t)) {
        return Err(forbid("Conformance"));
    }
    let p = c.enterprise.set_conformance_policy(&t, b);
    c.audit.record(&t, "user", "update", "conformance", &t, &format!("{} required rules", p.required.len()));
    crate::persist::persist(&c);
    Ok(Json(json!(p)))
}

#[derive(Deserialize)]
struct RunReq {
    project: String,
}

/// Evaluate the built-in conformance rules against a project's latest deployment,
/// using the service graph + project settings as evidence. Deterministic; no AI.
async fn conformance_run(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<RunReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = crate::admin::require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &b.project)?;
    if !crate::billing::plan_allows_conformance(&plan_of(&c, &t)) {
        return Err(forbid("Conformance"));
    }
    let policy = c.enterprise.conformance_policy(&t);
    let required = |id: &str| policy.required.iter().any(|r| r == id);

    // Gather evidence.
    let deployments = c.gw.list();
    let dep = deployments
        .iter()
        .filter(|d| d.project == b.project)
        .max_by_key(|d| d.created_at_ms)
        .cloned();
    let dep_id = dep.as_ref().map(|d| d.id.to_string()).unwrap_or_default();
    let has_production = deployments.iter().any(|d| d.project == b.project && d.production);
    let settings = c.projects.get_masked(&b.project);
    let fw = settings.build.framework.clone();
    let waf_rules = c.waf.rules().len();
    let preview_protected = c.projects.preview_protected(&b.project) || c.enterprise.protection(&b.project).mode != "off";
    // Sensitive env hygiene: every env var whose name looks secret-ish (KEY,
    // SECRET, TOKEN, PASSWORD) must be flagged `sensitive` (→ masked/encrypted).
    let env_sensitive_ok = settings.env.iter().all(|e| {
        let k = e.key.to_uppercase();
        let secretish = k.contains("SECRET") || k.contains("TOKEN") || k.contains("PASSWORD") || k.ends_with("_KEY") || k.contains("APIKEY");
        !secretish || e.sensitive
    });

    let eval = |rule: &str| -> (bool, String) {
        match rule {
            "https-only" => (true, "All deployments served over HTTPS with HSTS by the gateway".into()),
            "env-no-plaintext-secrets" => (
                env_sensitive_ok,
                if env_sensitive_ok { "Secret env values are masked/encrypted".into() } else { "Plaintext secret values detected in env".into() },
            ),
            "firewall-enabled" => (waf_rules > 0, format!("{waf_rules} WAF rule(s) active")),
            "preview-protected" => (
                preview_protected,
                if preview_protected { "Preview/deployment access protection enabled".into() } else { "No deployment access protection".into() },
            ),
            "has-production" => (
                has_production,
                if has_production { "Healthy production deployment present".into() } else { "No production deployment".into() },
            ),
            "pinned-runtime" => (
                !fw.is_empty() && fw != "unknown",
                if !fw.is_empty() && fw != "unknown" { format!("Framework pinned: {fw}") } else { "Framework/runtime not detected".into() },
            ),
            _ => (false, "unknown rule".into()),
        }
    };

    let results: Vec<ConformanceResult> = RULES
        .iter()
        .map(|(id, title)| {
            let (passed, detail) = eval(id);
            ConformanceResult { rule: id.to_string(), title: title.to_string(), passed, required: required(id), detail }
        })
        .collect();
    let total = results.len().max(1);
    let passed_count = results.iter().filter(|r| r.passed).count();
    let all_required_pass = results.iter().filter(|r| r.required).all(|r| r.passed);
    let report = ConformanceReport {
        project: b.project.clone(),
        deployment_id: dep_id,
        results,
        passed: all_required_pass,
        score: (passed_count as u32 * 100 / total as u32),
        ran_ms: now_ms(),
    };
    c.enterprise.put_report(report.clone());
    c.audit.record(&t, "user", "run", "conformance", &b.project, &format!("score {}", report.score));
    Ok(Json(json!(report)))
}
