//! JWT signing and verification for the admin/control API. Service tokens are HS256-signed with
//! `HIVE_JWT_SECRET`; the `tenant` claim (Clerk org id when Clerk is enabled, else a default)
//! scopes a token to a team. Dev-open default: with `HIVE_JWT_SECRET` unset verification is
//! skipped, and setting it enforces signed requests.

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// TTL (seconds) of a `mesh-internal` delegation token for one node-to-node forwarded call. It must
/// clear the mesh dial budget with headroom (`fetch_from_host` lets `gossip::request_to` run up to
/// 20 s, and a stale-hint fallback dial plus receiver queuing must not land an already-expired
/// token) without widening the replay window of a single-purpose token.
pub const MESH_DELEGATION_TOKEN_TTL_SECS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the user id, or `key:<id>` for a dashboard API key.
    pub sub: String,
    /// Tenant: the org/team id that scopes the token (multi-tenancy).
    pub tenant: String,
    /// TENANT-scoped role: `owner` | `admin` | `member` | `viewer` (`apikeys::MINTABLE_ROLES`);
    /// `service` only on a mesh delegation token ([`is_mesh_internal`]). Never a platform-operator
    /// signal (see `platform_admin`).
    pub role: String,
    /// Expiry, unix seconds.
    pub exp: usize,
    pub iat: usize,
    /// Whether the subject is a genuine PLATFORM operator (an email in `HIVE_OWNER_EMAIL` or
    /// `HIVE_ADMIN_EMAILS`), independent of `role`: `role: "owner"` only means owner of `tenant`
    /// (every user owns their personal namespace). `require_operator()` gates on THIS field; legacy
    /// tokens without it deserialize to `false` (fail closed).
    #[serde(default)]
    pub platform_admin: bool,
}

/// Subject of a mesh delegation token: a fleet node acting on the mesh's behalf, minted through
/// `issue` with [`MESH_SERVICE_ROLE`].
pub const MESH_INTERNAL_SUB: &str = "mesh-internal";
/// Role reserved for [`MESH_INTERNAL_SUB`] delegations. A tenant never carries it:
/// `apikeys::check_new_role` refuses to mint it and `api_key_claims` demotes a stored one through
/// `apikeys::effective_role`.
pub const MESH_SERVICE_ROLE: &str = "service";

/// THE predicate for "this caller is a fleet node", never a tenant: subject AND role of a mesh
/// delegation. The role alone is not proof (a role string is data a tenant can store), so every
/// fleet-internal gate asks this one question.
pub fn is_mesh_internal(claims: &Claims) -> bool {
    claims.sub == MESH_INTERNAL_SUB && claims.role == MESH_SERVICE_ROLE
}

/// `raw` when it is a path on THIS origin, otherwise `fallback`: the only way a caller-supplied
/// return path may become a `Location` (the unlock and SSO flows redirect back to one).
///
/// A same-origin path is one leading `/` followed by neither `/` nor `\`, with no control character
/// anywhere. A `starts_with("//")` check is not enough: the WHATWG URL parser strips ASCII tab and
/// newline from ANY position and reads `\` as `/`, so `/\evil.example` and `/<TAB>/evil.example`
/// both resolve to `https://evil.example/`. Twin of `ui/lib/same-origin-path.ts`
/// (`sameOriginPath`): change them together.
pub fn same_origin_path<'a>(raw: &'a str, fallback: &'a str) -> &'a str {
    let mut chars = raw.chars();
    let path_absolute = chars.next() == Some('/') && !matches!(chars.next(), Some('/' | '\\'));
    if path_absolute && !raw.chars().any(char::is_control) {
        raw
    } else {
        fallback
    }
}

fn secret() -> Option<String> {
    std::env::var("HIVE_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn enforced() -> bool {
    secret().is_some()
}

/// Issue a signed token valid for `ttl_secs`. `platform_admin` must be derived independently by the
/// caller (the backend's `owner_email` against the verified account email), never from a
/// client-supplied boolean.
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

/// Extract a platform JWT: `Authorization: Bearer` first, else the `hive_jwt` cookie, which lets
/// the self-hosted dashboard carry the token httpOnly over the same-origin `/cloud` rewrite (no
/// token in browser JS, no CORS).
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
/// synthesized tenant-scoped [`Claims`], so the CLI/SDK credential clears the same mutation gates
/// as a platform JWT. `platform_admin` is always false: a key scopes to its team, never to operator
/// authority, and its role is `apikeys::effective_role` of the stored one, so a key can never
/// authenticate as the mesh's `service` role whatever was stored.
pub fn api_key_claims(c: &crate::state::CloudState, token: &str) -> Option<Claims> {
    if !token.starts_with("hive_") {
        return None;
    }
    let k = c.apikeys.verify(token)?;
    let now = chrono::Utc::now().timestamp() as usize;
    Some(Claims {
        sub: format!("key:{}", k.id),
        tenant: k.team,
        role: crate::apikeys::effective_role(&k.role).to_string(),
        iat: now,
        exp: now + 3600,
        platform_admin: false,
    })
}

/// Auth middleware. With a JWT secret configured a mutation (POST/PUT/DELETE/PATCH) needs a valid
/// bearer and reads always pass; without one nothing is enforced (dev mode) and the middleware only
/// binds claims.
///
/// Whenever a valid platform JWT or dashboard API key is presented (read or write) its verified
/// [`Claims`] go into the request extensions (in-process only, never client-settable), so handlers
/// resolve the tenant from the cryptographically-bound `tenant` claim, never from the spoofable
/// `x-hive-team` header. A JWT wins over an API key (tenant-scoped, never `platform_admin`); key
/// claims bind even in dev, since key verification is a self-contained hash lookup.
///
/// Routes carrying their OWN credential skip the mutation gate (`open`) and each fails closed in
/// its own handler: `/healthz`; `/v1/token` (the mint); the GitHub and Stripe webhooks (their own
/// HMAC signature); `/v1/zkauth/{register,preview-proof,project-team}` (the dashboard's
/// server-to-server `x-hive-internal` secret, `zkauth::internal_ok`); `/v1/browser/admissions`
/// (`admit()` through `fresh_user_claims`; the item routes stay gated); `/v1/sqlite/` (the
/// database's own bearer, checked in `hrana.rs`); `/v1/browser-db/` (the project's
/// `browser_db_rest` bearer); `/v1/drive/webdav/` (HTTP Basic, verified by `drive_webdav.rs` before
/// any filesystem call).
///
/// A GET/HEAD is never rejected for missing auth, but an unauthenticated one on a non-exempt route
/// resolves to `admin::ANON_TENANT` (a namespace that owns nothing) and is stamped `x-hive-anon:
/// 1`, which `ui/lib/api.ts` treats like a 401/403: a real 200 with an empty body would otherwise
/// read as "you have zero X" after the session expired.
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
