//! HS256 JSON Web Tokens, implemented from scratch on `hmac` + `sha2` +
//! `base64` (all already in-tree for the `sql` feature) — no `jsonwebtoken`
//! dependency.
//!
//! This is the exact JWT shape Supabase uses: a compact `header.payload.sig`
//! token, HMAC-SHA256 over `header.payload`, base64url (no padding) throughout.
//! [`Claims`] carries the Supabase/GoTrue claim set; unknown claims round-trip
//! through [`Claims::extra`]. [`verify`] checks the signature in constant time
//! and enforces `exp`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// A JWT verification / signing error. Never carries the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    /// The token is not three base64url segments separated by dots.
    Malformed,
    /// A segment was not valid base64url or the payload was not JSON.
    Decode,
    /// The signature did not match (tampered token or wrong secret).
    BadSignature,
    /// The token's `exp` is in the past.
    Expired,
    /// The token was not yet valid (`nbf`/`iat` in the future beyond leeway).
    NotYetValid,
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JwtError::Malformed => "malformed JWT",
            JwtError::Decode => "could not decode JWT",
            JwtError::BadSignature => "JWT signature is invalid",
            JwtError::Expired => "JWT has expired",
            JwtError::NotYetValid => "JWT is not yet valid",
        };
        f.write_str(s)
    }
}

impl std::error::Error for JwtError {}

/// The Supabase/GoTrue claim set. Fields absent from a given token are omitted
/// on serialize and default to `None`/empty on deserialize, so both the tiny
/// `{role,iss,iat,exp}` API keys and full user access tokens round-trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    /// Issuer (Supabase API keys use `"supabase"`; user tokens use the API URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// The role claim: `anon`, `authenticated`, `service_role`, or custom.
    pub role: String,
    /// Subject — the user id (uuid) for user tokens; absent for API keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// User email, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Audience (GoTrue uses `"authenticated"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// Issued-at (unix seconds).
    pub iat: i64,
    /// Expiry (unix seconds).
    pub exp: i64,
    /// GoTrue session id, when the token belongs to a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Any other claims (e.g. `app_metadata`, `user_metadata`, `amr`).
    #[serde(flatten)]
    pub extra: Map<String, Json>,
}

impl Claims {
    /// A minimal API-key claim set (`{role, iss:"supabase", iat, exp}`), matching
    /// the shape Supabase signs its `anon` / `service_role` keys with.
    pub fn api_key(role: impl Into<String>, iat: i64, exp: i64) -> Self {
        Self {
            iss: Some("supabase".to_string()),
            role: role.into(),
            sub: None,
            email: None,
            aud: None,
            iat,
            exp,
            session_id: None,
            extra: Map::new(),
        }
    }

    /// The Postgres role this token maps onto. Supabase maps the JWT `role`
    /// claim directly to a Postgres role name; unknown roles map to `anon`
    /// defensively (an unrecognised role never silently gains privileges).
    pub fn pg_role(&self) -> &str {
        match self.role.as_str() {
            "service_role" => "service_role",
            "authenticated" => "authenticated",
            "anon" => "anon",
            // A custom role claim is passed through verbatim (PostgREST behaviour),
            // but only if it is a plausible identifier; otherwise fall back to anon.
            other if is_ident(other) => other,
            _ => "anon",
        }
    }

    /// Is this a real end-user token (has a `sub`) rather than an API key?
    pub fn is_user(&self) -> bool {
        self.sub.as_deref().is_some_and(|s| !s.is_empty())
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

/// Sign `claims` into a compact HS256 JWT with `secret`.
pub fn sign(claims: &Claims, secret: &str) -> Result<String, JwtError> {
    let header = Json::Object({
        let mut m = Map::new();
        m.insert("alg".to_string(), Json::String("HS256".to_string()));
        m.insert("typ".to_string(), Json::String("JWT".to_string()));
        m
    });
    let header_b64 = B64.encode(serde_json::to_vec(&header).map_err(|_| JwtError::Decode)?);
    let payload_b64 = B64.encode(serde_json::to_vec(claims).map_err(|_| JwtError::Decode)?);
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig = sign_hs256(signing_input.as_bytes(), secret);
    let sig_b64 = B64.encode(sig);
    Ok(format!("{signing_input}.{sig_b64}"))
}

/// Verify a compact HS256 JWT with `secret`, enforcing `exp` against `now`
/// (unix seconds). Returns the decoded [`Claims`] on success.
pub fn verify(token: &str, secret: &str, now: i64) -> Result<Claims, JwtError> {
    let mut parts = token.split('.');
    let header_b64 = parts.next().ok_or(JwtError::Malformed)?;
    let payload_b64 = parts.next().ok_or(JwtError::Malformed)?;
    let sig_b64 = parts.next().ok_or(JwtError::Malformed)?;
    if parts.next().is_some() {
        return Err(JwtError::Malformed);
    }
    if header_b64.is_empty() || payload_b64.is_empty() || sig_b64.is_empty() {
        return Err(JwtError::Malformed);
    }

    // Recompute and constant-time-compare the signature.
    let signing_input = format!("{header_b64}.{payload_b64}");
    let expected = sign_hs256(signing_input.as_bytes(), secret);
    let provided = B64.decode(sig_b64).map_err(|_| JwtError::Decode)?;
    if !constant_time_eq(&expected, &provided) {
        return Err(JwtError::BadSignature);
    }

    let payload = B64.decode(payload_b64).map_err(|_| JwtError::Decode)?;
    let claims: Claims = serde_json::from_slice(&payload).map_err(|_| JwtError::Decode)?;

    // A small leeway absorbs clock skew, matching GoTrue's default (0 here, we
    // keep it strict but allow exp == now to still be valid).
    if now > claims.exp {
        return Err(JwtError::Expired);
    }
    Ok(claims)
}

fn sign_hs256(data: &[u8], secret: &str) -> Vec<u8> {
    let mut mac =
        <HmacSha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Length-independent, branch-free byte comparison for the signature check.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "super-secret-jwt-token-with-at-least-32-characters-long";

    fn now() -> i64 {
        1_700_000_000
    }

    fn user_claims() -> Claims {
        Claims {
            iss: Some("http://localhost:54321/auth/v1".to_string()),
            role: "authenticated".to_string(),
            sub: Some("11111111-1111-1111-1111-111111111111".to_string()),
            email: Some("alice@example.com".to_string()),
            aud: Some("authenticated".to_string()),
            iat: now(),
            exp: now() + 3600,
            session_id: Some("22222222-2222-2222-2222-222222222222".to_string()),
            extra: Map::new(),
        }
    }

    #[test]
    fn round_trip_authenticated() {
        let claims = user_claims();
        let token = sign(&claims, SECRET).unwrap();
        let back = verify(&token, SECRET, now()).unwrap();
        assert_eq!(back, claims);
        assert_eq!(back.pg_role(), "authenticated");
        assert!(back.is_user());
    }

    #[test]
    fn round_trip_anon_and_service_role() {
        for role in ["anon", "service_role"] {
            let claims = Claims::api_key(role, now(), now() + 10_000);
            let token = sign(&claims, SECRET).unwrap();
            let back = verify(&token, SECRET, now()).unwrap();
            assert_eq!(back.role, role);
            assert_eq!(back.pg_role(), role);
            assert_eq!(back.iss.as_deref(), Some("supabase"));
            assert!(!back.is_user());
        }
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let token = sign(&user_claims(), SECRET).unwrap();
        // Flip the FIRST signature character to a different base64url char: it
        // decodes cleanly to different bytes (unlike the trailing char, whose
        // low bits must be canonical), so verification reaches — and fails at —
        // the signature comparison rather than at decoding.
        let (rest, sig) = token.rsplit_once('.').unwrap();
        let mut sig_chars: Vec<char> = sig.chars().collect();
        sig_chars[0] = if sig_chars[0] == 'A' { 'B' } else { 'A' };
        let tampered = format!("{rest}.{}", sig_chars.into_iter().collect::<String>());
        assert_eq!(
            verify(&tampered, SECRET, now()),
            Err(JwtError::BadSignature)
        );
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let token = sign(&user_claims(), SECRET).unwrap();
        let mut parts: Vec<&str> = token.split('.').collect();
        // Re-encode a payload claiming service_role, keep the original signature.
        let forged = Claims::api_key("service_role", now(), now() + 3600);
        let forged_b64 = B64.encode(serde_json::to_vec(&forged).unwrap());
        parts[1] = &forged_b64;
        let forged_token = parts.join(".");
        assert_eq!(
            verify(&forged_token, SECRET, now()),
            Err(JwtError::BadSignature)
        );
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = sign(&user_claims(), SECRET).unwrap();
        assert_eq!(
            verify(&token, "a-different-secret-value-entirely", now()),
            Err(JwtError::BadSignature)
        );
    }

    #[test]
    fn expired_token_is_rejected() {
        let mut claims = user_claims();
        claims.exp = now() - 1;
        let token = sign(&claims, SECRET).unwrap();
        assert_eq!(verify(&token, SECRET, now()), Err(JwtError::Expired));
    }

    #[test]
    fn not_expired_at_exact_boundary() {
        let mut claims = user_claims();
        claims.exp = now();
        let token = sign(&claims, SECRET).unwrap();
        assert!(verify(&token, SECRET, now()).is_ok());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        for bad in ["", "abc", "a.b", "a.b.c.d", ".b.c", "a..c"] {
            assert!(
                verify(bad, SECRET, now()).is_err(),
                "expected error for {bad:?}"
            );
        }
    }

    #[test]
    fn extra_claims_round_trip() {
        let mut claims = user_claims();
        claims.extra.insert(
            "app_metadata".to_string(),
            serde_json::json!({"provider": "email"}),
        );
        let token = sign(&claims, SECRET).unwrap();
        let back = verify(&token, SECRET, now()).unwrap();
        assert_eq!(
            back.extra.get("app_metadata"),
            Some(&serde_json::json!({"provider": "email"}))
        );
    }

    #[test]
    fn unknown_role_falls_back_to_anon() {
        let claims = Claims::api_key("'; DROP TABLE users; --", now(), now() + 100);
        assert_eq!(claims.pg_role(), "anon");
    }
}
