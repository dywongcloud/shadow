//! Project model, service configuration, and key generation.
//!
//! A [`SupabaseCompatProject`] is the single-project shell the gateway serves:
//! its identifiers, URLs, timestamps, and its [`ProjectKeys`] (the JWT secret
//! plus the signed `anon` / `service_role` API keys). Secrets never appear in
//! `Debug` output — they are wrapped in [`Secret`], and [`ProjectKeys`] redacts
//! the keys themselves.
//!
//! Key generation is **pure**: [`ProjectKeys::from_secret`] takes the secret and
//! an `iat` as parameters and never calls `SystemTime::now`/`rand`, so tests are
//! deterministic. The impure helpers [`generate_jwt_secret`] and
//! [`ProjectKeys::generate`] exist only for the binary's startup path.

use crate::supabase::jwt::{self, Claims, JwtError};
use chrono::{DateTime, Utc};

/// Ten years in seconds — Supabase's default lifetime for the `anon` /
/// `service_role` API keys.
pub const API_KEY_TTL_SECS: i64 = 10 * 365 * 24 * 60 * 60;

/// A secret string that is redacted in `Debug` / `Display` and never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The underlying secret. Call sites are responsible for never logging it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***redacted***)")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret(s)
    }
}

/// The signing secret and the two signed API keys for a project.
#[derive(Clone)]
pub struct ProjectKeys {
    /// HS256 signing secret (>= 40 chars). Redacted in `Debug`.
    pub jwt_secret: Secret,
    /// The signed `anon` API key (a real HS256 JWT, `role: "anon"`).
    pub anon_key: String,
    /// The signed `service_role` API key (`role: "service_role"`).
    pub service_role_key: String,
}

impl std::fmt::Debug for ProjectKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectKeys")
            .field("jwt_secret", &self.jwt_secret)
            .field("anon_key", &"***redacted***")
            .field("service_role_key", &"***redacted***")
            .finish()
    }
}

impl ProjectKeys {
    /// Derive the keys deterministically from a secret and an `iat` (unix
    /// seconds). Pure — no `now()`/`rand`. The `anon` and `service_role` keys are
    /// signed with the standard Supabase claim shape
    /// (`{role, iss:"supabase", iat, exp}`) and expire in [`API_KEY_TTL_SECS`].
    pub fn from_secret(secret: impl Into<String>, iat: i64) -> Result<Self, JwtError> {
        let secret = secret.into();
        let exp = iat + API_KEY_TTL_SECS;
        let anon_key = jwt::sign(&Claims::api_key("anon", iat, exp), &secret)?;
        let service_role_key = jwt::sign(&Claims::api_key("service_role", iat, exp), &secret)?;
        Ok(Self {
            jwt_secret: Secret::new(secret),
            anon_key,
            service_role_key,
        })
    }

    /// Generate a fresh random secret and derive the keys at `iat`. Impure only
    /// in its use of [`generate_jwt_secret`]; the `iat` is still injected.
    pub fn generate(iat: i64) -> Result<Self, JwtError> {
        Self::from_secret(generate_jwt_secret(), iat)
    }

    /// Verify an incoming `apikey` header value against this project's secret and
    /// return its claims. Any token signed by the project secret with a valid
    /// `exp` is accepted (this is exactly how Kong/PostgREST treat the apikey).
    pub fn verify_api_key(&self, token: &str, now: i64) -> Result<Claims, JwtError> {
        jwt::verify(token, self.jwt_secret.expose(), now)
    }
}

/// Generate a cryptographically-unpredictable 48-character alphanumeric JWT
/// secret (well over Supabase's 32/40-char minimum). Uses `fastrand`, seeded
/// from the OS on first use.
pub fn generate_jwt_secret() -> String {
    // Re-seed from entropy so process restarts do not reuse a secret.
    fastrand::seed(seed_from_entropy());
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..48)
        .map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char)
        .collect()
}

fn seed_from_entropy() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Mix in a fresh v4 uuid (OS CSPRNG) so the seed is not merely time-based.
    let uuid = uuid::Uuid::new_v4();
    let (hi, lo) = uuid.as_u64_pair();
    nanos ^ hi ^ lo.rotate_left(17)
}

/// A single Supabase-compatible project served by one gateway instance.
#[derive(Debug, Clone)]
pub struct SupabaseCompatProject {
    /// Internal project id (uuid).
    pub id: String,
    /// The 20-character project ref (the `<ref>` in `<ref>.supabase.co`).
    pub project_ref: String,
    /// Multi-tenant tenant id (defaults to the project ref for the single shell).
    pub tenant_id: String,
    /// Human-readable name.
    pub name: String,
    /// The relational database name backing this project.
    pub db_name: String,
    /// The public API URL (`SUPABASE_URL`), e.g. `http://127.0.0.1:54321`.
    pub api_url: String,
    /// The Postgres connection URL, when one is exposed (informational).
    pub db_url: Option<String>,
    /// The project's keys (secret + anon/service_role). Redacted in `Debug`.
    pub keys: ProjectKeys,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SupabaseCompatProject {
    /// Construct a project. All time-varying inputs (`keys`, timestamps) are
    /// injected, keeping this constructor pure and test-friendly.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        project_ref: impl Into<String>,
        name: impl Into<String>,
        db_name: impl Into<String>,
        api_url: impl Into<String>,
        db_url: Option<String>,
        keys: ProjectKeys,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        let project_ref = project_ref.into();
        Self {
            id: id.into(),
            tenant_id: project_ref.clone(),
            project_ref,
            name: name.into(),
            db_name: db_name.into(),
            api_url: api_url.into(),
            db_url,
            keys,
            created_at,
            updated_at,
        }
    }

    /// Build a default single-project shell from a database name, API URL, and
    /// pre-derived keys, stamping `created_at`/`updated_at` with `now`.
    pub fn shell(
        db_name: impl Into<String>,
        api_url: impl Into<String>,
        keys: ProjectKeys,
        now: DateTime<Utc>,
    ) -> Self {
        let db_name = db_name.into();
        let project_ref = default_project_ref();
        Self::new(
            uuid::Uuid::new_v4().to_string(),
            project_ref,
            format!("guardian-{db_name}"),
            db_name,
            api_url,
            None,
            keys,
            now,
            now,
        )
    }
}

/// A 20-character lowercase alphanumeric project ref, like Supabase assigns.
fn default_project_ref() -> String {
    fastrand::seed(seed_from_entropy());
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..20)
        .map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char)
        .collect()
}

/// Gateway-wide service configuration (GoTrue-shaped knobs).
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// The site URL used in auth redirects / emails (informational here).
    pub site_url: String,
    /// Access-token TTL in seconds (GoTrue default 3600).
    pub jwt_exp: i64,
    /// Refresh-token TTL in seconds (GoTrue default ~30 days).
    pub refresh_token_ttl: i64,
    /// The audience stamped into user access tokens.
    pub jwt_aud: String,
    /// Whether self-service signup is disabled.
    pub disable_signup: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            site_url: "http://localhost:3000".to_string(),
            jwt_exp: 3600,
            refresh_token_ttl: 30 * 24 * 60 * 60,
            jwt_aud: "authenticated".to_string(),
            disable_signup: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supabase::jwt;

    #[test]
    fn from_secret_is_deterministic() {
        let a = ProjectKeys::from_secret("test-secret-abcdefghijklmnop", 1_700_000_000).unwrap();
        let b = ProjectKeys::from_secret("test-secret-abcdefghijklmnop", 1_700_000_000).unwrap();
        assert_eq!(a.anon_key, b.anon_key);
        assert_eq!(a.service_role_key, b.service_role_key);
    }

    #[test]
    fn generated_keys_have_supabase_claim_shape() {
        let iat = 1_700_000_000;
        let keys = ProjectKeys::from_secret("test-secret-abcdefghijklmnop", iat).unwrap();

        let anon = jwt::verify(&keys.anon_key, keys.jwt_secret.expose(), iat).unwrap();
        assert_eq!(anon.role, "anon");
        assert_eq!(anon.iss.as_deref(), Some("supabase"));
        assert_eq!(anon.iat, iat);
        assert_eq!(anon.exp, iat + API_KEY_TTL_SECS);
        assert!(anon.sub.is_none() && anon.email.is_none());

        let service = jwt::verify(&keys.service_role_key, keys.jwt_secret.expose(), iat).unwrap();
        assert_eq!(service.role, "service_role");
        assert_eq!(service.pg_role(), "service_role");
    }

    #[test]
    fn generate_secret_is_long_and_varied() {
        let s1 = generate_jwt_secret();
        let s2 = generate_jwt_secret();
        assert!(s1.len() >= 40, "secret must be at least 40 chars");
        assert!(s1.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(s1, s2, "two generated secrets must differ");
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let s = Secret::new("hunter2-super-secret");
        let dumped = format!("{s:?}");
        assert!(
            !dumped.contains("hunter2"),
            "secret leaked in Debug: {dumped}"
        );
        let keys = ProjectKeys::from_secret("hunter2-super-secret-value-here", 1).unwrap();
        let dumped = format!("{keys:?}");
        assert!(!dumped.contains("hunter2"));
        assert!(!dumped.contains(&keys.anon_key));
    }

    #[test]
    fn verify_api_key_round_trips() {
        let iat = 1_700_000_000;
        let keys = ProjectKeys::from_secret("test-secret-abcdefghijklmnop", iat).unwrap();
        let claims = keys.verify_api_key(&keys.service_role_key, iat).unwrap();
        assert_eq!(claims.role, "service_role");
        assert!(keys.verify_api_key("not.a.jwt", iat).is_err());
    }
}
