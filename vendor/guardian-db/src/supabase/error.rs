//! The typed error taxonomy for the Supabase-compatible gateway.
//!
//! Every gateway-level failure is a [`SupaError`] carrying an HTTP status and a
//! stable `SUPA_COMPAT_*` code, rendered as a JSON body. Engine (SQL) errors are
//! rendered separately in PostgREST shape by [`pgrst_error`], and GoTrue
//! endpoints use [`gotrue_error`]; both live here so the whole error surface is
//! in one place. **No secret is ever placed in an error body or log.**

use crate::sql::SqlError;
use crate::supabase::jwt::JwtError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// A gateway-level typed error. Each variant maps to an HTTP status and a
/// `SUPA_COMPAT_*` code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupaError {
    /// No `apikey` header (and no usable `Authorization`) was supplied.
    MissingApiKey,
    /// The `apikey` was present but is not a valid token for this project.
    InvalidApiKey,
    /// The `Authorization: Bearer` token failed verification.
    InvalidJwt(JwtError),
    /// A route requires the `service_role` and the caller does not have it.
    Forbidden(&'static str),
    /// A Kong service exists in the routing table but is not implemented in this
    /// slice. Carries the uppercase service token for the `SUPA_COMPAT_*` code.
    NotImplemented(&'static str),
    /// A PostgREST filter operator we do not support was requested.
    UnsupportedFilter(String),
    /// A malformed REST request (bad select list, body, range, ...).
    BadRequest(String),
    /// An OAuth/SSO auth provider was requested; unsupported in this slice.
    AuthProviderUnsupported(String),
    /// An engine error, rendered in PostgREST shape (`{code,message,details,hint}`)
    /// with the SQLSTATE as `code` and the SQLSTATE class mapped to the status.
    Sql(SqlError),
    /// An unexpected internal failure (never includes secrets).
    Internal(String),
}

impl SupaError {
    /// The HTTP status this error maps to.
    pub fn status(&self) -> StatusCode {
        match self {
            SupaError::MissingApiKey | SupaError::InvalidApiKey => StatusCode::UNAUTHORIZED,
            SupaError::InvalidJwt(_) => StatusCode::UNAUTHORIZED,
            SupaError::Forbidden(_) => StatusCode::FORBIDDEN,
            SupaError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            SupaError::UnsupportedFilter(_) | SupaError::BadRequest(_) => StatusCode::BAD_REQUEST,
            SupaError::AuthProviderUnsupported(_) => StatusCode::BAD_REQUEST,
            SupaError::Sql(e) => status_for_sqlstate(e.sqlstate()),
            SupaError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable `SUPA_COMPAT_*` (or Supabase) code string.
    pub fn code(&self) -> String {
        match self {
            SupaError::MissingApiKey => "SUPA_COMPAT_MISSING_API_KEY".to_string(),
            SupaError::InvalidApiKey => "SUPA_COMPAT_INVALID_API_KEY".to_string(),
            SupaError::InvalidJwt(_) => "SUPA_COMPAT_INVALID_JWT".to_string(),
            SupaError::Forbidden(_) => "SUPA_COMPAT_FORBIDDEN".to_string(),
            SupaError::NotImplemented(svc) => format!("SUPA_COMPAT_{svc}_NOT_IMPLEMENTED"),
            SupaError::UnsupportedFilter(_) => "SUPA_COMPAT_REST_UNSUPPORTED_FILTER".to_string(),
            SupaError::BadRequest(_) => "SUPA_COMPAT_REST_BAD_REQUEST".to_string(),
            SupaError::AuthProviderUnsupported(_) => {
                "SUPA_COMPAT_AUTH_PROVIDER_UNSUPPORTED".to_string()
            }
            SupaError::Sql(e) => e.sqlstate().to_string(),
            SupaError::Internal(_) => "SUPA_COMPAT_INTERNAL".to_string(),
        }
    }

    /// A human-readable message. Never contains a secret.
    pub fn message(&self) -> String {
        match self {
            SupaError::MissingApiKey => {
                "No API key found in request. Pass the project apikey header.".to_string()
            }
            SupaError::InvalidApiKey => "Invalid API key".to_string(),
            SupaError::InvalidJwt(e) => format!("Invalid authentication credentials: {e}"),
            SupaError::Forbidden(what) => {
                format!("{what} requires the service_role key")
            }
            SupaError::NotImplemented(svc) => format!(
                "the {} service is not implemented in this GuardianDB compatibility slice",
                svc.to_ascii_lowercase()
            ),
            SupaError::UnsupportedFilter(f) => {
                format!("unsupported PostgREST filter operator: {f}")
            }
            SupaError::BadRequest(m) => m.clone(),
            SupaError::AuthProviderUnsupported(p) => {
                format!("auth provider \"{p}\" is not supported")
            }
            SupaError::Sql(e) => e.to_string(),
            SupaError::Internal(m) => m.clone(),
        }
    }

    fn hint(&self) -> Option<String> {
        match self {
            SupaError::NotImplemented(_) => Some("tracked for a later slice".to_string()),
            SupaError::UnsupportedFilter(_) => Some(
                "supported operators: eq, neq, gt, gte, lt, lte, like, ilike, is, in".to_string(),
            ),
            SupaError::AuthProviderUnsupported(_) => {
                Some("only email/password auth is implemented in this slice".to_string())
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for SupaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for SupaError {}

impl IntoResponse for SupaError {
    fn into_response(self) -> Response {
        // Engine errors render in true PostgREST shape.
        if let SupaError::Sql(e) = &self {
            return pgrst_error(e);
        }
        let mut body = json!({
            "code": self.code(),
            "message": self.message(),
        });
        if let Some(hint) = self.hint() {
            body["hint"] = json!(hint);
        }
        (self.status(), Json(body)).into_response()
    }
}

/// Render an engine [`SqlError`] as a PostgREST-shaped error response:
/// `{"code","message","details","hint"}` where `code` is the SQLSTATE. The HTTP
/// status is derived from the SQLSTATE class (mirrors PostgREST's mapping).
pub fn pgrst_error(err: &SqlError) -> Response {
    let sqlstate = err.sqlstate();
    let status = status_for_sqlstate(sqlstate);
    let body = json!({
        "code": sqlstate,
        "message": err.to_string(),
        "details": Option::<String>::None,
        "hint": Option::<String>::None,
    });
    (status, Json(body)).into_response()
}

/// Map a SQLSTATE to the HTTP status PostgREST would return.
pub fn status_for_sqlstate(sqlstate: &str) -> StatusCode {
    match sqlstate {
        // Integrity constraint violations.
        "23505" => StatusCode::CONFLICT, // unique_violation
        "23502" | "23514" => StatusCode::BAD_REQUEST, // not_null / check
        "23503" => StatusCode::CONFLICT, // foreign_key_violation
        // Undefined objects.
        "42P01" | "42703" | "42883" | "42704" | "3F000" => StatusCode::NOT_FOUND,
        // Duplicate objects.
        "42P07" | "42P06" | "42710" => StatusCode::CONFLICT,
        // Insufficient privilege (row-level security) → 403, like PostgREST.
        "42501" => StatusCode::FORBIDDEN,
        // Syntax / typing / data errors → 400.
        s if s.starts_with("42") || s.starts_with("22") => StatusCode::BAD_REQUEST,
        // Feature not supported.
        "0A000" => StatusCode::NOT_IMPLEMENTED,
        // Lock / transaction issues.
        "55P03" | "40P01" | "25P02" => StatusCode::CONFLICT,
        // Storage / internal.
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Render a GoTrue-shaped error: `{"code": <http>, "error_code": <code>,
/// "msg": <message>}`. GoTrue's newer error body uses these fields; older
/// clients read `msg`. We include both `error_code` and `msg`.
pub fn gotrue_error(status: StatusCode, error_code: &str, msg: &str) -> Response {
    let body = json!({
        "code": status.as_u16(),
        "error_code": error_code,
        "msg": msg,
    });
    (status, Json(body)).into_response()
}

/// Render a GoTrue OAuth-style error: `{"error","error_description"}`. Used by
/// the token endpoint, which historically returns this shape.
pub fn gotrue_oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let body = json!({
        "error": error,
        "error_description": description,
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_json(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn invalid_api_key_shape() {
        let (status, body) = body_json(SupaError::InvalidApiKey.into_response()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "SUPA_COMPAT_INVALID_API_KEY");
        assert!(body["message"].is_string());
    }

    #[tokio::test]
    async fn not_implemented_shape() {
        let (status, body) = body_json(SupaError::NotImplemented("STORAGE").into_response()).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "SUPA_COMPAT_STORAGE_NOT_IMPLEMENTED");
        assert_eq!(body["hint"], "tracked for a later slice");
    }

    #[tokio::test]
    async fn unsupported_filter_is_400() {
        let (status, body) =
            body_json(SupaError::UnsupportedFilter("cs".into()).into_response()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "SUPA_COMPAT_REST_UNSUPPORTED_FILTER");
    }

    #[test]
    fn sqlstate_status_mapping() {
        assert_eq!(status_for_sqlstate("23505"), StatusCode::CONFLICT);
        assert_eq!(status_for_sqlstate("42P01"), StatusCode::NOT_FOUND);
        assert_eq!(status_for_sqlstate("42601"), StatusCode::BAD_REQUEST);
        assert_eq!(status_for_sqlstate("22P02"), StatusCode::BAD_REQUEST);
        assert_eq!(status_for_sqlstate("0A000"), StatusCode::NOT_IMPLEMENTED);
    }
}
