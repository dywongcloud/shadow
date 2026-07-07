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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,    // user id
    pub tenant: String, // org/team id (multi-tenancy)
    pub role: String,   // TENANT-scoped role: owner | member | service (never a platform-operator signal)
    pub exp: usize,     // expiry (unix seconds)
    pub iat: usize,
    /// Whether the subject is a genuine PLATFORM operator (the backend's own
    /// `HIVE_OWNER_EMAIL`), independent of `role`. `role: "owner"` only ever
    /// means "owner of `tenant`" — every user is the owner of their own personal
    /// namespace, so it must never be treated as platform-wide authority.
    /// `require_operator()` gates on THIS field, not `role`. `#[serde(default)]`
    /// so any already-issued/short-lived legacy token without this field
    /// deserializes to `false` (fail closed), never `true`.
    #[serde(default)]
    pub platform_admin: bool,
}

fn secret() -> Option<String> {
    std::env::var("HIVE_JWT_SECRET").ok().filter(|s| !s.is_empty())
}

pub fn enforced() -> bool {
    secret().is_some()
}

/// Issue a signed token for a subject/tenant valid for `ttl_secs`. `platform_admin`
/// must be independently derived by the caller (matching the backend's own
/// `owner_email` against the verified account email) — never trust a
/// client-supplied boolean for this field.
pub fn issue(sub: &str, tenant: &str, role: &str, platform_admin: bool, ttl_secs: i64) -> anyhow::Result<String> {
    let key = secret().ok_or_else(|| anyhow::anyhow!("HIVE_JWT_SECRET not set"))?;
    issue_with_secret(sub, tenant, role, platform_admin, ttl_secs, &key)
}

/// `issue` with an explicit signing secret (no env read) — the deterministic core,
/// used by `issue` and by tests that must not touch global process env.
pub fn issue_with_secret(
    sub: &str, tenant: &str, role: &str, platform_admin: bool, ttl_secs: i64, key: &str,
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
    let token = encode(&Header::default(), &claims, &EncodingKey::from_secret(key.as_bytes()))?;
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
    // Parse the `hive_jwt` cookie out of the Cookie header.
    let cookie = headers.get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok())?;
    cookie.split(';').find_map(|kv| {
        let kv = kv.trim();
        kv.strip_prefix("hive_jwt=").map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    })
}

/// Middleware: when a JWT secret is configured, require a valid bearer token on
/// mutating requests (POST/PUT/DELETE). Reads are always allowed. With no secret
/// configured it is a pass-through (dev mode).
///
/// Tenancy: whenever a valid platform JWT is presented (read OR write), the
/// verified [`Claims`] are inserted into the request's extensions so downstream
/// handlers can resolve the tenant from the cryptographically-bound `tenant`
/// claim — never from a client-supplied `x-hive-team` header, which is spoofable.
/// Extensions are in-process only and cannot be set by the client.
pub async fn require_auth(mut req: Request, next: Next) -> Response {
    if !enforced() {
        return next.run(req).await;
    }
    // Verify the bearer token (if any) up front and bind its claims to the
    // request. This runs for reads too, so read handlers also get the
    // authoritative tenant.
    let claims = extract_token(req.headers()).and_then(|t| verify(&t).ok());
    let authed = claims.is_some();
    if let Some(c) = claims {
        req.extensions_mut().insert(c);
    }
    let method = req.method().clone();
    let is_mutation = matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    // Allow the token-mint + health endpoints unauthenticated.
    let path = req.uri().path();
    // GitHub/Stripe webhooks can't present a platform JWT — they are
    // authenticated by their own HMAC signature inside the handler
    // (GITHUB_WEBHOOK_SECRET / STRIPE_WEBHOOK_SECRET respectively).
    let open = path == "/healthz" || path == "/v1/token" || path == "/v1/git/webhook" || path == "/v1/billing/webhook";
    if !is_mutation || open {
        return next.run(req).await;
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
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer abc.def.ghi"));
        assert_eq!(extract_token(&h).as_deref(), Some("abc.def.ghi"));

        // No bearer, hive_jwt cookie among others → cookie value used.
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("foo=1; hive_jwt=tok123; bar=2"));
        assert_eq!(extract_token(&h).as_deref(), Some("tok123"));

        // Bearer wins over cookie.
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer BEAR"));
        h.insert(header::COOKIE, HeaderValue::from_static("hive_jwt=COOK"));
        assert_eq!(extract_token(&h).as_deref(), Some("BEAR"));

        // Neither → None. Empty bearer → None (falls through, no cookie).
        assert_eq!(extract_token(&HeaderMap::new()), None);
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(extract_token(&h), None);
    }
}
