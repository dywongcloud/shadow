//! HTTP surface for the enterprise feature suite ([`crate::enterprise`]).
//!
//! Every mutating endpoint is team-scoped (via [`crate::admin::tenant`]), plan-gated (via
//! `billing::plan_allows_*`) and role-gated ([`require_config_admin`]: owner and admin write, every
//! member reads). Secrets (SIEM/SCIM tokens, deployment passwords) are AEAD-encrypted
//! at rest and never returned in the clear: list/get responses redact them to booleans or masks.
//!
//! Two endpoint families are called by an external IdP, not by a tenant: SCIM 2.0 (`/v1/scim/v2/*`,
//! bearer = the per-team SCIM token) and the SAML ACS (`/v1/saml/:team/acs`, IdP form POST). SCIM is
//! on `auth::is_open_route`'s list (the platform gate would refuse an IdP's JWT-less POST with 401)
//! because every SCIM handler authenticates by its own bearer (`scim_auth`) and fails closed. The
//! SAML ACS is NOT: it verifies no assertion signature, so it stays behind the platform gate AND
//! answers only the team's own owner or admin (`saml_acs`): an anonymous caller is refused 401, a
//! caller from another team or a plain member of this one 403.

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
use crate::enterprise::{
    ConformancePolicy, ConformanceReport, ConformanceResult, MfeChild, MfeGroup, SamlConfig, RULES,
};
use crate::state::CloudState;
use crate::teams::{MemberRemoval, Role};
use hive_core::now_ms;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route(
            "/v1/enterprise/ip-blocks",
            get(ip_blocks_list).post(ip_block_add),
        )
        .route("/v1/enterprise/ip-blocks/:id", delete(ip_block_delete))
        .route("/v1/enterprise/siem", get(siem_get).put(siem_put))
        .route("/v1/enterprise/siem/test", post(siem_test))
        .route("/v1/enterprise/saml", get(saml_get).put(saml_put))
        .route("/v1/enterprise/saml/metadata", get(saml_metadata))
        .route("/v1/saml/:team/acs", post(saml_acs))
        .route(
            "/v1/enterprise/scim",
            get(scim_get).post(scim_enable).delete(scim_disable),
        )
        .route("/v1/scim/v2/ServiceProviderConfig", get(scim_spc))
        .route(
            "/v1/scim/v2/Users",
            get(scim_users_list).post(scim_users_create),
        )
        .route(
            "/v1/scim/v2/Users/:id",
            get(scim_user_get)
                .patch(scim_user_patch)
                .delete(scim_user_delete),
        )
        .route(
            "/v1/enterprise/projects/:project/protection",
            get(protection_get).put(protection_put),
        )
        .route(
            "/v1/enterprise/microfrontends",
            get(mfe_list).post(mfe_upsert),
        )
        .route("/v1/enterprise/microfrontends/:id", delete(mfe_delete))
        .route("/v1/enterprise/edge-export", get(edge_export_public))
        .route(
            "/v1/enterprise/conformance",
            get(conformance_get).put(conformance_put),
        )
        .route("/v1/enterprise/conformance/rules", get(conformance_rules))
        .route("/v1/enterprise/conformance/run", post(conformance_run))
}

/// The team's plan tier. The team record is authoritative (it is what
/// `team_set_plan`/`team_set_sso` gate on); fall back to the billing account, then "hobby".
fn plan_of(c: &Arc<CloudState>, team: &str) -> String {
    c.teams
        .get(team)
        .map(|t| t.plan)
        .unwrap_or_else(|| c.billing.account(team).plan)
}

fn forbid(feature: &str) -> (StatusCode, String) {
    (
        StatusCode::FORBIDDEN,
        format!("{feature} requires the Enterprise plan"),
    )
}

/// The roles that may WRITE a team's enterprise configuration (IP blocks, the SIEM sink and its
/// test event, SAML, SCIM, deployment protection): owner and admin. Every member may READ it.
/// Without a floor a viewer or member reconfigured the team's security: pointed the audit stream at
/// their own SIEM endpoint, added or deleted IP blocks, switched deployment protection off, or
/// minted the SCIM bearer (which provisions roster members and locks the IdP out). On an UNENFORCED
/// dev node there are no verified roles and the floor steps aside (`admin::require_team_floor`),
/// like every other role gate.
fn require_config_admin(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: &Claims,
    team: &str,
) -> Result<(), (StatusCode, String)> {
    crate::admin::require_team_floor(
        c,
        headers,
        claims.as_ref().map(|e| &e.0),
        team,
        &["owner", "admin"],
    )
    .map(|_| ())
}

async fn ip_blocks_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
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
    require_config_admin(&c, &headers, &claims, &t)?;
    if !crate::billing::plan_allows_ip_blocking(&plan_of(&c, &t)) {
        return Err(forbid("IP blocking"));
    }
    if b.prefix.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "prefix required".into()));
    }
    let block = c.enterprise.add_ip_block(&t, &b.prefix, &b.note);
    c.audit
        .record(&t, "user", "create", "ip-block", &block.id, &block.prefix);
    crate::persist::persist(&c);
    Ok(Json(json!(block)))
}

async fn ip_block_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_config_admin(&c, &headers, &claims, &t)?;
    c.enterprise.remove_ip_block(&t, &id);
    c.audit.record(&t, "user", "delete", "ip-block", &id, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true })))
}

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

async fn siem_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    match c.enterprise.siem(&t) {
        Some(cfg) => Json(siem_view(&cfg)),
        None => {
            Json(json!({ "format": "http", "endpoint": "", "enabled": false, "has_token": false }))
        }
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
    require_config_admin(&c, &headers, &claims, &t)?;
    if !crate::billing::plan_allows_siem(&plan_of(&c, &t)) {
        return Err(forbid("SIEM log streaming"));
    }
    if !matches!(b.format.as_str(), "http" | "datadog" | "splunk-hec") {
        return Err((
            StatusCode::BAD_REQUEST,
            "format must be http|datadog|splunk-hec".into(),
        ));
    }
    let cfg = c
        .enterprise
        .set_siem(&t, &b.format, &b.endpoint, b.token.as_deref(), b.enabled)
        .map_err(|refused| (StatusCode::BAD_REQUEST, refused.to_string()))?;
    c.audit.record(
        &t,
        "user",
        "update",
        "siem",
        &t,
        &format!("{} {}", cfg.format, cfg.endpoint),
    );
    crate::persist::persist(&c);
    Ok(Json(siem_view(&cfg)))
}

/// Fire a synthetic audit entry through the SIEM streamer so operators can
/// confirm delivery from the UI.
async fn siem_test(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_config_admin(&c, &headers, &claims, &t)?;
    if c.enterprise
        .siem(&t)
        .map(|s| !s.enabled || s.endpoint.is_empty())
        .unwrap_or(true)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "SIEM not configured/enabled".into(),
        ));
    }
    c.audit
        .record(&t, "system", "test", "siem", &t, "siem connectivity test");
    Ok(Json(
        json!({ "ok": true, "note": "test event emitted to the SIEM stream" }),
    ))
}

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

async fn saml_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
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

/// Stores the team's SAML config (Enterprise plan) and reflects `enabled` into the team's SSO flag
/// (`teams.set_sso`), so the rest of the platform sees SSO on.
async fn saml_put(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<SamlReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_config_admin(&c, &headers, &claims, &t)?;
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
    c.teams.set_sso(&t, cfg.enabled);
    c.audit
        .record(&t, "user", "update", "saml", &t, &cfg.idp_entity_id);
    crate::persist::persist(&c);
    Ok(Json(saml_view(&cfg)))
}

/// SP metadata for this team — hand to the IdP to configure the connection.
async fn saml_metadata(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Response {
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

/// SAML Assertion Consumer Service: the IdP POSTs the (base64) SAML response here after the user
/// authenticates. It decodes the response, extracts the NameID (email), optionally auto-provisions
/// the team member (`auto_provision`), then bounces to the dashboard (`HIVE_DASHBOARD_URL`, else
/// the API base) at `RelayState`, reduced to a same-origin path (`auth::same_origin_path`: an
/// absolute, protocol-relative or `@host` RelayState lands on `/`, never on another host). It
/// establishes NO session: it federates directory membership, and the dashboard's own auth signs
/// the user in.
///
/// SECURITY GAP: the assertion's XML-DSig signature is NOT verified against the IdP `x509_cert`;
/// the handler checks only that SAML is enabled for the team and extracts the subject, so it trusts
/// any `NameID` posted to it (with `auto_provision`, that email becomes a team Member). Signature
/// verification is the open hardening item, so until it exists the ACS is not a public endpoint:
/// the platform gate refuses an anonymous POST and `require_team_floor` lets only the team's own
/// owner or admin (who can already add members) through, so a signed-in caller of ANOTHER team, or
/// a plain member of this one, can no longer forge an assertion into the roster. On an UNENFORCED
/// dev node (no `HIVE_JWT_SECRET`, so no verified roles) the floor steps aside like every other
/// role gate. Consequence: an IdP's browser POST (no platform JWT) cannot complete a sign-in on an
/// enforced platform.
async fn saml_acs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(team): Path<String>,
    axum::extract::Form(form): axum::extract::Form<AcsForm>,
) -> Response {
    if let Err((status, message)) = crate::admin::require_team_floor(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &team,
        &["owner", "admin"],
    ) {
        return (status, message).into_response();
    }
    let cfg = c.enterprise.saml(&team);
    if !cfg.enabled {
        return (StatusCode::FORBIDDEN, "SAML not enabled for this team").into_response();
    }
    let Some(email) = decode_saml_nameid(&form.saml_response) else {
        return (StatusCode::BAD_REQUEST, "could not parse SAML assertion").into_response();
    };
    let provisioned = cfg.auto_provision
        && c.teams
            .add_member_if_absent(&team, &email, Role::Member)
            .is_some_and(|(_, added)| added);
    if provisioned {
        c.enterprise.scim_upsert_user(&team, &email, &email, true);
        c.audit.record(
            &team,
            "saml",
            "provision",
            "member",
            &email,
            "auto-provisioned via SAML",
        );
        crate::persist::persist(&c);
    } else {
        c.audit
            .record(&team, "saml", "login", "member", &email, "SAML sign-in");
    }
    let dash = std::env::var("HIVE_DASHBOARD_URL").unwrap_or_else(|_| c.api_base());
    let next = crate::auth::same_origin_path(&form.relay_state, "/");
    axum::response::Redirect::temporary(&format!("{}{}", dash.trim_end_matches('/'), next))
        .into_response()
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

async fn scim_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
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

/// Enable SCIM + (re)issue a bearer token. Returns the plaintext token ONCE. Owner or admin only
/// (the bearer is the directory's whole authority over the roster, and a re-mint invalidates the
/// IdP's current token).
async fn scim_enable(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_config_admin(&c, &headers, &claims, &t)?;
    if !crate::billing::plan_allows_scim(&plan_of(&c, &t)) {
        return Err(forbid("SCIM directory sync"));
    }
    let token = c.enterprise.scim_enable(&t);
    let base = c.api_base();
    c.audit
        .record(&t, "user", "enable", "scim", &t, "issued SCIM bearer token");
    crate::persist::persist(&c);
    Ok(Json(
        json!({ "token": token, "endpoint": format!("{base}/v1/scim/v2"), "note": "store this token now — it is not shown again" }),
    ))
}

/// Disables the team's SCIM connection. Owner or admin only, like enabling it.
async fn scim_disable(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    require_config_admin(&c, &headers, &claims, &t)?;
    c.enterprise.scim_disable(&t);
    c.audit.record(&t, "user", "disable", "scim", &t, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true })))
}

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

/// Creates (or updates) a directory user and, when active, adds them to the team roster as a
/// Member, so the directory drives real membership. Only when they are NOT on the roster yet
/// (`add_member_if_absent`): the directory never changes an existing member's role, so provisioning
/// an owner's, an admin's or a viewer's email is a no-op for the roster.
async fn scim_users_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(b): Json<ScimCreate>,
) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    let display = if b.display_name.is_empty() {
        b.user_name.clone()
    } else {
        b.display_name
    };
    let u = c
        .enterprise
        .scim_upsert_user(&team, &b.user_name, &display, b.active);
    if b.active {
        c.teams
            .add_member_if_absent(&team, &b.user_name, Role::Member);
    }
    c.audit.record(
        &team,
        "scim",
        "create",
        "member",
        &u.user_name,
        "provisioned via SCIM",
    );
    crate::persist::persist(&c);
    (StatusCode::CREATED, Json(scim_user_json(&u))).into_response()
}

async fn scim_user_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    match c.enterprise.scim_user(&team, &id) {
        Some(u) => Json(scim_user_json(&u)).into_response(),
        None => (StatusCode::NOT_FOUND, "no such user").into_response(),
    }
}

/// The answer to a deprovisioning (PATCH `active:false`, DELETE) of a user who is a team Owner or
/// Admin: 403 in SCIM's error envelope, and NOTHING changed (the directory record and the roster
/// both stay as they were, so the IdP sees an error rather than a deprovisioning that did not
/// happen). Removing the people who run a team is a dashboard action by a person
/// (`team_remove_member`), never a directory sync.
fn scim_privileged_refusal(user_name: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:Error"],
            "status": "403",
            "detail": format!(
                "{user_name} is a team owner or admin: directory sync never removes one, remove them from the team in the dashboard"
            ),
        })),
    )
        .into_response()
}

/// SCIM PATCH — Okta/Azure use this to deactivate (`active:false`) on offboard.
///
/// Only `active:false` is honored, found in any operation either as `path: "active"` with a `false`
/// value or as a `value` object carrying `active: false`; every other operation (re-activation
/// included) is ignored and the current user is returned. Deactivating a user who is a team Owner
/// or Admin is refused ([`scim_privileged_refusal`]); a plain member is removed from the roster.
async fn scim_user_patch(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
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
        if let Some(u) = c.enterprise.scim_user(&team, &id) {
            if c.teams.remove_plain_member(&team, &u.user_name) == Some(MemberRemoval::Privileged) {
                return scim_privileged_refusal(&u.user_name);
            }
            c.enterprise.scim_deactivate_user(&team, &id);
            c.audit.record(
                &team,
                "scim",
                "deactivate",
                "member",
                &u.user_name,
                "deprovisioned via SCIM",
            );
        }
        crate::persist::persist(&c);
    }
    match c.enterprise.scim_user(&team, &id) {
        Some(u) => Json(scim_user_json(&u)).into_response(),
        None => (StatusCode::NOT_FOUND, "no such user").into_response(),
    }
}

/// Erases the user (`scim_purge_user`, not a soft deactivate: SCIM DELETE is deprovisioning, often
/// an offboarding or a data-subject erasure request) and removes them from the team roster, unless
/// they are a team Owner or Admin ([`scim_privileged_refusal`]: nothing is erased or removed). The
/// audit trail still records WHO was removed (the `user_name` at deletion time).
async fn scim_user_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(team) = scim_auth(&c, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid SCIM token").into_response();
    };
    if let Some(u) = c.enterprise.scim_user(&team, &id) {
        if c.teams.remove_plain_member(&team, &u.user_name) == Some(MemberRemoval::Privileged) {
            return scim_privileged_refusal(&u.user_name);
        }
        c.enterprise.scim_purge_user(&team, &id);
        c.audit.record(
            &team,
            "scim",
            "delete",
            "member",
            &u.user_name,
            "deprovisioned + purged via SCIM",
        );
        crate::persist::persist(&c);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Node-to-node: this node's locally authored edge-enforcement config (IP blocks, deployment
/// protection, microfrontend groups; no secrets, the password field is a one-way salted hash).
/// Peers ingest it every gossip round so the gate works wherever a request enters or a deployment
/// is hosted. Peer-authenticated QUIC transport.
async fn edge_export(State(c): State<Arc<CloudState>>) -> Json<crate::enterprise::EdgeExport> {
    Json(c.enterprise.edge_export())
}

/// Public HTTP mount for [`edge_export`]. CWE-306: the export carries every tenant's
/// edge-enforcement config (IP blocks, deployment protection), so it must not answer an anonymous
/// caller (`admin::require_fleet_read`). The gossip loop calls `edge_export` directly, so the gate
/// is on this route wrapper, never inside the shared handler.
async fn edge_export_public(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<crate::enterprise::EdgeExport>, (StatusCode, String)> {
    crate::admin::require_fleet_read(claims.as_ref().map(|e| &e.0))?;
    Ok(edge_export(State(c)).await)
}

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
    require_config_admin(&c, &headers, &claims, &t)?;
    if !crate::billing::plan_allows_deploy_protection(&plan_of(&c, &t)) {
        return Err((
            StatusCode::FORBIDDEN,
            "Deployment protection requires the Pro or Enterprise plan".into(),
        ));
    }
    if !matches!(b.mode.as_str(), "off" | "standard" | "password") {
        return Err((
            StatusCode::BAD_REQUEST,
            "mode must be off|standard|password".into(),
        ));
    }
    if !matches!(b.scope.as_str(), "preview" | "all" | "production") {
        return Err((
            StatusCode::BAD_REQUEST,
            "scope must be preview|all|production".into(),
        ));
    }
    if b.mode == "password"
        && b.password.as_deref().unwrap_or("").is_empty()
        && c.enterprise.protection(&project).password_hash.is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "password required for password mode".into(),
        ));
    }
    let p = c
        .enterprise
        .set_protection(&project, &b.mode, &b.scope, b.password.as_deref());
    c.audit.record(
        &t,
        "user",
        "update",
        "deploy-protection",
        &project,
        &format!("{}/{}", p.mode, p.scope),
    );
    crate::persist::persist(&c);
    Ok(Json(protection_view(&p)))
}

async fn mfe_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
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

/// Creates or updates a microfrontend group (Pro or Enterprise plan). Every referenced project,
/// host and children, must belong to THIS team so a group never composes another tenant's
/// deployments; the check is fleet-aware (`project_owned_by`) because settings rows are node-local,
/// so a remotely placed project's ownership is judged from its deployment tenant tags. The request
/// arrives in the legacy `children` shape and `set_mfe_group` normalizes it into the rich `members`
/// model (default member plus child members with derived routing).
async fn mfe_upsert(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<MfeReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if !crate::billing::plan_allows_microfrontends(&plan_of(&c, &t)) {
        return Err((
            StatusCode::FORBIDDEN,
            "Microfrontends require the Pro or Enterprise plan".into(),
        ));
    }
    if b.name.trim().is_empty() || b.host_project.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "name and host_project required".into(),
        ));
    }
    for p in std::iter::once(&b.host_project).chain(b.children.iter().map(|ch| &ch.project)) {
        if !crate::admin::project_owned_by(&c, p, &t) {
            return Err((
                StatusCode::FORBIDDEN,
                format!("project '{p}' belongs to a different team"),
            ));
        }
    }
    let id = if b.id.is_empty() {
        format!("mfe_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
    } else {
        b.id
    };
    let mut g = MfeGroup::new(id, b.name, b.host_project, now_ms());
    g.members.clear();
    g.children = b
        .children
        .into_iter()
        .map(|ch| MfeChild {
            project: ch.project,
            path_prefix: ch.path_prefix,
        })
        .collect();
    let saved = c.enterprise.set_mfe_group(&t, g);
    c.audit.record(
        &t,
        "user",
        "update",
        "microfrontend",
        &saved.id,
        &saved.name,
    );
    crate::persist::persist(&c);
    Ok(Json(json!(saved)))
}

async fn mfe_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(id): Path<String>,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    c.enterprise.remove_mfe_group(&t, &id);
    c.audit
        .record(&t, "user", "delete", "microfrontend", &id, "");
    crate::persist::persist(&c);
    Json(json!({ "ok": true }))
}

async fn conformance_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
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
    c.audit.record(
        &t,
        "user",
        "update",
        "conformance",
        &t,
        &format!("{} required rules", p.required.len()),
    );
    crate::persist::persist(&c);
    Ok(Json(json!(p)))
}

#[derive(Deserialize)]
struct RunReq {
    project: String,
}

/// Evaluate the built-in conformance rules against a project's latest deployment,
/// using the service graph + project settings as evidence. Deterministic; no AI.
///
/// Evidence comes from THIS node's gateway (`c.gw`), so a deployment hosted only on another node is
/// not seen. `env-no-plaintext-secrets` requires every env var whose name looks secret-ish
/// (`SECRET`, `TOKEN`, `PASSWORD`, `APIKEY` or a `_KEY` suffix) to be flagged `sensitive` (masked
/// and encrypted).
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

    let deployments = c.gw.list();
    let dep = deployments
        .iter()
        .filter(|d| d.project == b.project)
        .max_by_key(|d| d.created_at_ms)
        .cloned();
    let dep_id = dep.as_ref().map(|d| d.id.to_string()).unwrap_or_default();
    let has_production = deployments
        .iter()
        .any(|d| d.project == b.project && d.production);
    let settings = c.projects.get_masked(&b.project);
    let fw = settings.build.framework.clone();
    let waf_rules = c.waf.rules().len();
    let preview_protected = c.projects.preview_protected(&b.project)
        || c.enterprise.protection(&b.project).mode != "off";
    let env_sensitive_ok = settings.env.iter().all(|e| {
        let k = e.key.to_uppercase();
        let secretish = k.contains("SECRET")
            || k.contains("TOKEN")
            || k.contains("PASSWORD")
            || k.ends_with("_KEY")
            || k.contains("APIKEY");
        !secretish || e.sensitive
    });

    let eval = |rule: &str| -> (bool, String) {
        match rule {
            "https-only" => (
                true,
                "All deployments served over HTTPS with HSTS by the gateway".into(),
            ),
            "env-no-plaintext-secrets" => (
                env_sensitive_ok,
                if env_sensitive_ok {
                    "Secret env values are masked/encrypted".into()
                } else {
                    "Plaintext secret values detected in env".into()
                },
            ),
            "firewall-enabled" => (waf_rules > 0, format!("{waf_rules} WAF rule(s) active")),
            "preview-protected" => (
                preview_protected,
                if preview_protected {
                    "Preview/deployment access protection enabled".into()
                } else {
                    "No deployment access protection".into()
                },
            ),
            "has-production" => (
                has_production,
                if has_production {
                    "Healthy production deployment present".into()
                } else {
                    "No production deployment".into()
                },
            ),
            "pinned-runtime" => (
                !fw.is_empty() && fw != "unknown",
                if !fw.is_empty() && fw != "unknown" {
                    format!("Framework pinned: {fw}")
                } else {
                    "Framework/runtime not detected".into()
                },
            ),
            _ => (false, "unknown rule".into()),
        }
    };

    let results: Vec<ConformanceResult> = RULES
        .iter()
        .map(|(id, title)| {
            let (passed, detail) = eval(id);
            ConformanceResult {
                rule: id.to_string(),
                title: title.to_string(),
                passed,
                required: required(id),
                detail,
            }
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
    c.audit.record(
        &t,
        "user",
        "run",
        "conformance",
        &b.project,
        &format!("score {}", report.score),
    );
    Ok(Json(json!(report)))
}
