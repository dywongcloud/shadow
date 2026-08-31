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

/// TTL (seconds) for a `mesh-internal` delegation token minted to authenticate
/// one node-to-node forwarded call (`gossip::team_claims`'s `?tok=`,
/// `fetch_from_host`/`proxy_get_json`'s `Authorization: Bearer`). Must clear
/// the iroh mesh's own dial/connect budget with real headroom: `fetch_from_host`
/// gives `gossip::request_to` up to 20s specifically because `PeerPool::acquire`
/// can fall back to a fresh-discovery dial when its cached hint is stale — a
/// token minted just before that fallback fires, plus any receiver-side queuing
/// under load, was landing on the verifying node already expired. Witnessed
/// live 2026-08-28: a stuck build-status poller retrying a forwarded
/// `/v1/builds/:id` read logged "mesh delegation token present but INVALID"
/// (jsonwebtoken's `ExpiredSignature`) 500+ times/hour against two real build
/// ids, every retry re-minting a fresh-at-mint-time 60s token that still
/// expired in transit. 90s clears the 20s dial budget with a wide margin
/// without meaningfully widening the token's replay window (it is still
/// single-purpose, mesh-peer-trust-gated, and self-invalidates on use by the
/// receiver's own expiry check).
pub const MESH_DELEGATION_TOKEN_TTL_SECS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,    // user id
    pub tenant: String, // org/team id (multi-tenancy)
    pub role: String, // TENANT-scoped role: owner | member | service (never a platform-operator signal)
    pub exp: usize,   // expiry (unix seconds)
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
    // Parse the `hive_jwt` cookie out of the Cookie header.
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

/// Resolve a bearer that is a dashboard-issued API key (`hive_…`, see
/// `apikeys.rs`) into synthesized tenant-scoped [`Claims`]. API keys are the
/// documented CLI/SDK credential, so they must clear the same mutation gates a
/// platform JWT does — before this, the JWT-only check silently made every API
/// key READ-ONLY on an enforced platform (mutations 401'd at both gates).
/// `platform_admin` is always false: a key scopes to its team, never to
/// platform-operator authority.
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

/// Middleware: when a JWT secret is configured, require a valid bearer token on
/// mutating requests (POST/PUT/DELETE). Reads are always allowed. With no secret
/// configured it is a pass-through (dev mode).
///
/// Tenancy: whenever a valid platform JWT — or a valid dashboard API key — is
/// presented (read OR write), the verified [`Claims`] are inserted into the
/// request's extensions so downstream handlers can resolve the tenant from the
/// cryptographically-bound `tenant` claim — never from a client-supplied
/// `x-hive-team` header, which is spoofable. Extensions are in-process only and
/// cannot be set by the client.
pub async fn require_auth(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::state::CloudState>>,
    mut req: Request,
    next: Next,
) -> Response {
    // Verify the bearer token (if any) up front and bind its claims to the
    // request. This runs for reads too, so read handlers also get the
    // authoritative tenant. A JWT wins; a dashboard API key is an equal
    // second-class citizen (tenant-scoped claims, never platform_admin).
    //
    // API-key claims are bound even in DEV (no `HIVE_JWT_SECRET`): key
    // verification is self-contained (hash lookup), needs no signing secret, and
    // an identity endpoint must not report a valid key as unauthenticated just
    // because the platform isn't enforcing JWTs. Only the JWT half depends on
    // the secret.
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
    // Allow the token-mint + health endpoints unauthenticated.
    let path = req.uri().path();
    // GitHub/Stripe webhooks can't present a platform JWT — they are
    // authenticated by their own HMAC signature inside the handler
    // (GITHUB_WEBHOOK_SECRET / STRIPE_WEBHOOK_SECRET respectively). The two
    // zkauth endpoints are called server-to-server by the dashboard's
    // preview-unlock route with ONLY a shared x-hive-internal secret (see
    // zkauth::internal_ok) — never a platform JWT, since the whole point is
    // authenticating a browser tab that has no session on this node at all.
    // Without this exemption every preview-unlock attempt 401'd here before
    // ever reaching zkauth's own (correct) internal-token check — live-
    // witnessed as "missing or invalid bearer token" on both routes.
    let open = path == "/healthz"
        || path == "/v1/token"
        || path == "/v1/git/webhook"
        || path == "/v1/billing/webhook"
        || path == "/v1/zkauth/register"
        || path == "/v1/zkauth/preview-proof"
        || path == "/v1/zkauth/project-team"
        // This exact collection POST still fails closed in `admit()` via
        // `fresh_user_claims`. Letting it reach that gate is what gives a
        // missing/expired browser session the same structured
        // `{error:{code,retryable}}` contract as every other admission denial;
        // the middleware already inserted verified claims above when present.
        // Item DELETE/accept routes remain protected here.
        || path == "/v1/browser/admissions"
        // libsql/Hrana pipeline POSTs carry the DATABASE's own bearer
        // (`DB_REST_TOKEN`), not a platform JWT — exactly the posture the
        // `<slug>.{db_domain}` REST surface already has, which never passes
        // through this middleware at all because it is served off the edge
        // pipeline. `hrana::serve` fails closed on a wrong/absent token via
        // `db_rest::credential_matches`, so letting the request reach it is
        // what makes the two mount points behave identically instead of the
        // platform-API one 401ing every legitimate libsql client.
        || path.starts_with("/v1/sqlite/")
        // browser_db's libsql/Hrana + Upstash REST surface carries the
        // project's own minted `browser_db_rest` bearer, not a platform
        // JWT — exactly the `/v1/sqlite/` posture above.
        // `browser_db_rest::serve` fails closed on a wrong/absent token via
        // `browser_db_rest::credential_scope`.
        || path.starts_with("/v1/browser-db/")
        // WebDAV clients (Finder/Explorer/davfs2) only speak HTTP
        // Basic/Digest, never a bearer JWT — `drive_webdav.rs`'s own
        // Basic-auth check (the project's hashed drive access token) fails
        // closed with 401 before any filesystem call reaches `dav_server`,
        // the same shape as `/v1/sqlite/`'s per-database bearer above.
        || path.starts_with("/v1/drive/webdav/");
    if !is_mutation || open {
        let mut resp = next.run(req).await;
        // A GET/HEAD is never rejected for missing auth (reads stay reachable
        // even mid-mint-race) — but an unauthenticated one silently resolves
        // to `admin::ANON_TENANT` (`resolve_tenant`'s own fail-closed design:
        // scope to a namespace that owns nothing, never leak the real
        // tenant's data to a tokenless caller). That produces a real 200 with
        // a wrongly-scoped empty/anonymous body, which is indistinguishable
        // from "you really have zero X" to the caller and — critically —
        // never a 401/403, the ONLY status `ui/lib/api.ts`'s `fetchWithTimeout`
        // treats as "re-mint the session and retry." A cookie that expires
        // mid-session (the 1h TTL, `SessionToken`'s 50-min proactive re-mint
        // notwithstanding — a tab backgrounded past that cadence, a failed
        // re-mint) or a genuine mint-race on a fresh load therefore renders a
        // permanently-empty list with no error and nothing that ever
        // corrects it (live-reported: API keys page). Stamping this header
        // lets the client apply the exact same re-mint-and-retry it already
        // has for 401/403, without weakening the read path's own
        // stay-reachable posture (still a real 200; still whatever an
        // exempted caller with no session is supposed to see).
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
