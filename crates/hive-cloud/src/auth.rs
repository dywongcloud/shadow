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

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,    // user id
    pub tenant: String, // org/team id (multi-tenancy)
    pub role: String,   // owner | member | service
    pub exp: usize,     // expiry (unix seconds)
    pub iat: usize,
}

fn secret() -> Option<String> {
    std::env::var("HIVE_JWT_SECRET").ok().filter(|s| !s.is_empty())
}

pub fn enforced() -> bool {
    secret().is_some()
}

/// Issue a signed token for a subject/tenant valid for `ttl_secs`.
pub fn issue(sub: &str, tenant: &str, role: &str, ttl_secs: i64) -> anyhow::Result<String> {
    let key = secret().ok_or_else(|| anyhow::anyhow!("HIVE_JWT_SECRET not set"))?;
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: sub.into(),
        tenant: tenant.into(),
        role: role.into(),
        iat: now,
        exp: now + ttl_secs.max(1) as usize,
    };
    let token = encode(&Header::default(), &claims, &EncodingKey::from_secret(key.as_bytes()))?;
    Ok(token)
}

/// Verify a token and return its claims.
pub fn verify(token: &str) -> anyhow::Result<Claims> {
    let key = secret().ok_or_else(|| anyhow::anyhow!("HIVE_JWT_SECRET not set"))?;
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(key.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}

/// Middleware: when a JWT secret is configured, require a valid bearer token on
/// mutating requests (POST/PUT/DELETE). Reads are always allowed. With no secret
/// configured it is a pass-through (dev mode).
pub async fn require_auth(req: Request, next: Next) -> Response {
    if !enforced() {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let is_mutation = matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    // Allow the token-mint + health endpoints unauthenticated.
    let path = req.uri().path();
    let open = path == "/healthz" || path == "/v1/token";
    if !is_mutation || open {
        return next.run(req).await;
    }
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| verify(t).is_ok())
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}
