//! JWT signing + verification for the admin/control API.
//!
//! * Service tokens are HS256-signed with `HIVE_JWT_SECRET`.
//! * Tenancy: the `tenant` claim (Clerk org id when Clerk is enabled, else a
//!   default) scopes a token to a team/account.
//! * **Dev-open default:** if `HIVE_JWT_SECRET` is unset, verification is skipped
//!   so local development (and the dashboard with no auth wired) keeps working.
//!   Set the secret to enforce signed requests.

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// TTL (seconds) for a `mesh-internal` delegation token minted to authenticate one node-to-node
/// forwarded call (`gossip::team_claims`'s `?tok=`, `fetch_from_host`/`proxy_get_json`'s
/// `Authorization: Bearer`). It must clear the mesh's own dial budget with real headroom:
/// `fetch_from_host` gives `gossip::request_to` up to 20 s because `PeerPool::acquire` can fall
/// back to a fresh-discovery dial when its cached hint is stale, and a token minted just before
/// that fallback, plus receiver-side queuing under load, must not land on the verifying node
/// already expired (jsonwebtoken's `ExpiredSignature`, logged as 'mesh delegation token present but
/// INVALID'). 90 s clears the 20 s budget with a wide margin without meaningfully widening the
/// replay window of a single-purpose, mesh-peer-trust-gated token.
pub const MESH_DELEGATION_TOKEN_TTL_SECS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the user id, or `key:<id>` for a dashboard API key.
    pub sub: String,
    /// Tenant: the org/team id that scopes the token (multi-tenancy).
    pub tenant: String,
    /// TENANT-scoped role: `owner` | `member` | `service`; never a platform-operator signal (see
    /// `platform_admin`).
    pub role: String,
    /// Expiry, unix seconds.
    pub exp: usize,
    pub iat: usize,
    /// Whether the subject is a genuine PLATFORM operator (an email in `HIVE_OWNER_EMAIL` or
    /// `HIVE_ADMIN_EMAILS`), independent of `role`. `role: "owner"` only ever means "owner of
    /// `tenant`": every user is the owner of their own personal namespace, so it must never be
    /// treated as platform-wide authority. `require_operator()` gates on THIS field, not `role`.
    /// `#[serde(default)]` so any already-issued/short-lived legacy token without this field
    /// deserializes to `false` (fail closed), never `true`.
    #[serde(default)]
    pub platform_admin: bool,
}

/// Subject of a mesh delegation token: a fleet node acting on the mesh's behalf, minted through
/// `issue` with [`MESH_SERVICE_ROLE`].
pub const MESH_INTERNAL_SUB: &str = "mesh-internal";
/// Role reserved for [`MESH_INTERNAL_SUB`] delegations. A tenant never carries it: `tenant_role`
/// refuses it on an API key and `api_key_claims` never emits it.
pub const MESH_SERVICE_ROLE: &str = "service";

/// THE predicate for "this caller is a fleet node", never a tenant: subject AND role of a mesh
/// delegation. The role alone is not proof (a role string is data a tenant can store), so every
/// fleet-internal gate asks this one question.
pub fn is_mesh_internal(claims: &Claims) -> bool {
    claims.sub == MESH_INTERNAL_SUB && claims.role == MESH_SERVICE_ROLE
}

fn secret() -> Option<String> {
    std::env::var("HIVE_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn enforced() -> bool {
    secret().is_some()
}

/// Issue a signed token for a subject/tenant valid for `ttl_secs`. `platform_admin`
/// must be independently derived by the caller (matching the backend's own
/// `owner_email` against the verified account email) — never trust a
/// client-supplied boolean for this field.
pub fn issue(
    sub: &str,
    tenant: &str,
    role: &str,
    platform_admin: bool,
    ttl_secs: i64,
) -> anyhow::Result<String> {
    let key = secret().ok_or_else(|| anyhow::anyhow!("HIVE_JWT_SECRET not set"))?;
    issue_with_secret(sub, tenant, role, platform_admin, ttl_secs, &key)
}

/// `issue` with an explicit signing secret (no env read) — the deterministic core,
/// used by `issue` and by tests that must not touch global process env.
pub fn issue_with_secret(
    sub: &str,
    tenant: &str,
    role: &str,
    platform_admin: bool,
    ttl_secs: i64,
    key: &str,
) -> anyhow::Result<String> {
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: sub.into(),
        tenant: tenant.into(),
        role: role.into(),
        iat: now,
        exp: now + ttl_secs.max(1) as usize,
        platform_admin,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(key.as_bytes()),
    )?;
    Ok(token)
}

/// Verify a token and return its claims.
pub fn verify(token: &str) -> anyhow::Result<Claims> {
    let key = secret().ok_or_else(|| anyhow::anyhow!("HIVE_JWT_SECRET not set"))?;
    verify_with_secret(token, &key)
}

/// `verify` with an explicit secret (no env read) — the deterministic core.
pub fn verify_with_secret(token: &str, key: &str) -> anyhow::Result<Claims> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(key.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}

/// Extract a platform JWT from a request: `Authorization: Bearer <jwt>` first,
/// else the `hive_jwt` cookie. The cookie lets the self-hosted dashboard carry
/// the token as an httpOnly cookie over the same-origin `/cloud` rewrite (no
/// token in browser JS, no CORS), while Bearer still works for API/SDK clients.
pub fn extract_token(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Some(bearer);
    }
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())?;
    cookie.split(';').find_map(|kv| {
        let kv = kv.trim();
        kv.strip_prefix("hive_jwt=")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// Resolve a bearer that is a dashboard-issued API key (`hive_…`, see `apikeys.rs`) into
/// synthesized tenant-scoped [`Claims`]. API keys are the documented CLI/SDK credential, so they
/// must clear the same mutation gates a platform JWT does. `platform_admin` is always false: a key
/// scopes to its team, never to platform-operator authority.
pub fn api_key_claims(c: &crate::state::CloudState, token: &str) -> Option<Claims> {
    if !token.starts_with("hive_") {
        return None;
    }
    let k = c.apikeys.verify(token)?;
    let now = chrono::Utc::now().timestamp() as usize;
    Some(Claims {
        sub: format!("key:{}", k.id),
        tenant: k.team,
        role: k.role,
        iat: now,
        exp: now + 3600,
        platform_admin: false,
    })
}

/// Auth middleware. When a JWT secret is configured, a mutation (POST/PUT/DELETE/PATCH) needs a
/// valid bearer token and reads always pass; with no secret nothing is enforced (dev mode) and the
/// middleware only binds claims.
///
/// Tenancy: whenever a valid platform JWT, or a valid dashboard API key, is presented (read OR
/// write), the verified [`Claims`] are inserted into the request's extensions so downstream
/// handlers can resolve the tenant from the cryptographically-bound `tenant` claim, never from a
/// client-supplied `x-hive-team` header, which is spoofable. Extensions are in-process only and
/// cannot be set by the client. A JWT wins over an API key (tenant-scoped claims, never
/// `platform_admin`). API-key claims are bound even in dev (no `HIVE_JWT_SECRET`): key verification
/// is a self-contained hash lookup, and an identity endpoint must not report a valid key as
/// unauthenticated just because JWTs are not enforced; only the JWT half depends on the secret.
///
/// Routes that carry their OWN credential skip the mutation gate (`open`), and each fails closed in
/// its own handler:
///
/// * `/healthz` and `/v1/token` (the token mint);
/// * `/v1/git/webhook` and `/v1/billing/webhook`: GitHub and Stripe cannot present a platform JWT,
///   so each handler verifies its own HMAC signature (`GITHUB_WEBHOOK_SECRET` /
///   `STRIPE_WEBHOOK_SECRET`);
/// * `/v1/zkauth/register`, `/v1/zkauth/preview-proof` and `/v1/zkauth/project-team`: called
///   server-to-server by the dashboard's preview-unlock route with ONLY a shared `x-hive-internal`
///   secret (`zkauth::internal_ok`), never a platform JWT, since the caller is authenticating a
///   browser tab that has no session on this node at all;
/// * `/v1/browser/admissions` (this exact collection POST): `admit()` fails closed through
///   `fresh_user_claims`, so a missing or expired browser session gets the same structured
///   `{error:{code,retryable}}` contract as every other admission denial; the item DELETE/accept
///   routes stay behind this gate;
/// * `/v1/sqlite/`: libsql/Hrana pipeline POSTs carry the DATABASE's own bearer, which the Hrana
///   handler (`hrana.rs`) checks through `db_rest::credential_matches`. This is the posture of the
///   `<slug>.{db_domain}` REST surface, which is served off the edge pipeline and never reaches
///   this middleware;
/// * `/v1/browser-db/`: the project's own `browser_db_rest` bearer, checked through
///   `browser_db_rest::credential_scope` (the `/v1/sqlite/` posture);
/// * `/v1/drive/webdav/`: WebDAV clients speak only HTTP Basic/Digest, never a bearer JWT, so
///   `drive_webdav.rs` verifies the HTTP Basic credential itself
///   (`drive_api::verify_webdav_credentials`) and answers 401 before any filesystem call reaches
///   `dav_server`.
///
/// A GET/HEAD is never rejected for missing auth, but an unauthenticated one on a non-exempt route
/// resolves to `admin::ANON_TENANT` (`resolve_tenant`'s fail-closed scope: a namespace that owns
/// nothing). That is a real 200 with a wrongly-scoped empty body, indistinguishable from 'you
/// really have zero X' and never the 401/403 that `ui/lib/api.ts`'s `fetchWithTimeout` treats as
/// 'the session is gone: re-mint and retry'. Such a response is therefore stamped `x-hive-anon: 1`,
/// which the client treats like a 401/403, so a session cookie that expired mid-session (or a mint
/// race on a fresh load) cannot leave a permanently empty list with no error, while the read path
/// stays reachable.
pub async fn require_auth(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::state::CloudState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let claims = extract_token(req.headers())
        .and_then(|t| verify(&t).ok().or_else(|| api_key_claims(&state, &t)));
    let authed = claims.is_some();
    if let Some(c) = claims {
        req.extensions_mut().insert(c);
    }
    if !enforced() {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let is_mutation = matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    let path = req.uri().path();
    let open = path == "/healthz"
        || path == "/v1/token"
        || path == "/v1/git/webhook"
        || path == "/v1/billing/webhook"
        || path == "/v1/zkauth/register"
        || path == "/v1/zkauth/preview-proof"
        || path == "/v1/zkauth/project-team"
        || path == "/v1/browser/admissions"
        || path.starts_with("/v1/sqlite/")
        || path.starts_with("/v1/browser-db/")
        || path.starts_with("/v1/drive/webdav/");
    if !is_mutation || open {
        let mut resp = next.run(req).await;
        if !authed && !open {
            resp.headers_mut()
                .insert("x-hive-anon", axum::http::HeaderValue::from_static("1"));
        }
        return resp;
    }
    if authed {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::extract_token;
    use axum::http::{header, HeaderMap, HeaderValue};

    #[test]
    fn extracts_bearer_then_cookie() {
        // Bearer present → used.
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc.def.ghi"),
        );
        assert_eq!(extract_token(&h).as_deref(), Some("abc.def.ghi"));

        // No bearer, hive_jwt cookie among others → cookie value used.
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("foo=1; hive_jwt=tok123; bar=2"),
        );
        assert_eq!(extract_token(&h).as_deref(), Some("tok123"));

        // Bearer wins over cookie.
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer BEAR"),
        );
        h.insert(header::COOKIE, HeaderValue::from_static("hive_jwt=COOK"));
        assert_eq!(extract_token(&h).as_deref(), Some("BEAR"));

        // Neither → None. Empty bearer → None (falls through, no cookie).
        assert_eq!(extract_token(&HeaderMap::new()), None);
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(extract_token(&h), None);
    }
}
