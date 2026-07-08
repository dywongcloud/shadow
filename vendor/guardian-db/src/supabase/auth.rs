//! GoTrue-compatible authentication over the SQL engine.
//!
//! On first use the `auth` schema is bootstrapped by running DDL through a
//! [`Session`](crate::sql::engine::Session) (see [`BOOTSTRAP_SQL`]). Passwords
//! are hashed with `bcrypt` (the same crate the pgcrypto extension uses); access
//! tokens are HS256 JWTs signed with the project secret; refresh tokens are
//! opaque, rotated on use, and stored in `auth.refresh_tokens`.
//!
//! Responses match GoTrue's JSON: the token endpoints return an
//! `AccessTokenResponse` (`{access_token, token_type:"bearer", expires_in,
//! expires_at, refresh_token, user}`), and errors use GoTrue's
//! `{code,error_code,msg}` / `{error,error_description}` shapes. OAuth/SSO
//! providers return a typed [`SupaError::AuthProviderUnsupported`], never fake
//! success.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json as AxumJson};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value as Json, json};

use crate::sql::{ExecResult, OutField, RelationalStorage, SqlValue};
use crate::supabase::error::{SupaError, gotrue_error, gotrue_oauth_error};
use crate::supabase::gateway::{AppState, AuthContext, run_batch, run_sql};
use crate::supabase::jwt::{self, Claims};
use crate::supabase::rest::{parse_query_pairs, value_to_json};

/// bcrypt work factor for stored passwords (kept moderate so tests stay fast
/// while remaining well above brute-force feasibility).
const AUTH_BCRYPT_COST: u32 = 10;

/// The columns selected whenever a user is returned to a client.
const USER_COLUMNS: &str = "id, aud, role, email, email_confirmed_at, last_sign_in_at, \
     raw_app_meta_data, raw_user_meta_data, created_at, updated_at, phone, is_anonymous";

/// The `auth` schema bootstrap. Column set is the Supabase/GoTrue subset that
/// GuardianDB's engine supports (uuid / text / timestamptz / jsonb / boolean /
/// bigint). All statements are `IF NOT EXISTS`, so bootstrap is idempotent.
///
/// Notes on divergence from stock Supabase (verified against GuardianDB's
/// engine): GoTrue's generated/identity columns, partial indexes and
/// `CHECK`-heavy columns are omitted; `auth.refresh_tokens` is keyed by its
/// opaque `token` (stock uses a `bigserial id`), which the engine supports as a
/// text primary key.
pub const BOOTSTRAP_SQL: &str = "
CREATE SCHEMA IF NOT EXISTS auth;

CREATE TABLE IF NOT EXISTS auth.users (
    id uuid PRIMARY KEY,
    aud text,
    role text,
    email text,
    encrypted_password text,
    email_confirmed_at timestamptz,
    invited_at timestamptz,
    confirmation_token text,
    confirmation_sent_at timestamptz,
    recovery_token text,
    recovery_sent_at timestamptz,
    email_change_token_new text,
    email_change text,
    email_change_sent_at timestamptz,
    last_sign_in_at timestamptz,
    raw_app_meta_data jsonb,
    raw_user_meta_data jsonb,
    is_super_admin boolean,
    created_at timestamptz,
    updated_at timestamptz,
    phone text,
    phone_confirmed_at timestamptz,
    banned_until timestamptz,
    deleted_at timestamptz,
    is_anonymous boolean
);

CREATE TABLE IF NOT EXISTS auth.refresh_tokens (
    token text PRIMARY KEY,
    user_id uuid,
    session_id uuid,
    parent text,
    revoked boolean,
    created_at timestamptz,
    updated_at timestamptz
);

CREATE TABLE IF NOT EXISTS auth.sessions (
    id uuid PRIMARY KEY,
    user_id uuid,
    created_at timestamptz,
    updated_at timestamptz,
    not_after timestamptz,
    aal text
);

CREATE TABLE IF NOT EXISTS auth.identities (
    id uuid PRIMARY KEY,
    user_id uuid,
    identity_data jsonb,
    provider text,
    provider_id text,
    email text,
    created_at timestamptz,
    updated_at timestamptz
);

CREATE TABLE IF NOT EXISTS auth.audit_log_entries (
    id uuid PRIMARY KEY,
    payload jsonb,
    ip_address text,
    created_at timestamptz
);

CREATE TABLE IF NOT EXISTS auth.instances (
    id uuid PRIMARY KEY,
    raw_base_config text,
    created_at timestamptz,
    updated_at timestamptz
);

CREATE TABLE IF NOT EXISTS auth.schema_migrations (
    version text PRIMARY KEY
);
";

/// The Auth subrouter mounted at `/auth/v1`.
pub fn router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/signup", post(signup::<S>))
        .route("/token", post(token::<S>))
        .route("/logout", post(logout::<S>))
        .route("/user", get(get_user::<S>).put(put_user::<S>))
        .route(
            "/admin/users",
            get(admin_list_users::<S>).post(admin_create_user::<S>),
        )
        .route(
            "/admin/users/{id}",
            get(admin_get_user::<S>)
                .put(admin_update_user::<S>)
                .delete(admin_delete_user::<S>),
        )
}

// ---------------------------------------------------------------------------
// Schema bootstrap
// ---------------------------------------------------------------------------

/// Bootstrap the `auth` schema exactly once for this gateway instance.
pub async fn ensure_schema<S: RelationalStorage + 'static>(
    state: &AppState<S>,
) -> Result<(), SupaError> {
    state
        .schema_ready
        .get_or_try_init(|| async {
            run_batch(&state.db, "service_role", BOOTSTRAP_SQL)
                .await
                .map_err(SupaError::Sql)?;
            Ok::<(), SupaError>(())
        })
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// signup
// ---------------------------------------------------------------------------

async fn signup<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        if state.config.disable_signup {
            return Ok(gotrue_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "signup_disabled",
                "Signups not allowed for this instance",
            ));
        }
        let obj = json_object(&body)?;
        let email = require_email(&obj)?;
        let password = require_password(&obj)?;
        let metadata = obj.get("data").cloned().unwrap_or_else(|| json!({}));

        if find_user_by_email(&state, &email).await?.is_some() {
            return Ok(gotrue_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "user_already_exists",
                "User already registered",
            ));
        }

        let user_id = uuid::Uuid::new_v4();
        create_user_row(&state, user_id, &email, &password, metadata, true).await?;
        let issued = issue_session(&state, user_id, &email).await?;
        let user = fetch_user_json(&state, "id", &SqlValue::Uuid(user_id))
            .await?
            .ok_or_else(|| SupaError::Internal("user vanished after insert".into()))?;
        Ok((
            StatusCode::OK,
            AxumJson(access_token_response(&issued, user)),
        )
            .into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// token (grant_type = password | refresh_token)
// ---------------------------------------------------------------------------

async fn token<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let grant = parse_query_pairs(query.as_deref().unwrap_or(""))
            .into_iter()
            .find(|(k, _)| k == "grant_type")
            .map(|(_, v)| v)
            .unwrap_or_default();
        match grant.as_str() {
            "password" => token_password(&state, &body).await,
            "refresh_token" => token_refresh(&state, &body).await,
            "" => Ok(gotrue_oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "grant_type is required",
            )),
            other if is_oauth_grant(other) => {
                Err(SupaError::AuthProviderUnsupported(other.to_string()))
            }
            other => Ok(gotrue_oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                &format!("grant_type \"{other}\" is not supported"),
            )),
        }
    })
    .await
}

async fn token_password<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    body: &[u8],
) -> Result<Response, SupaError> {
    let obj = json_object(body)?;
    let email = require_email(&obj)?;
    let password = require_password(&obj)?;

    let Some((user_id, hash)) = find_user_credentials(state, &email).await? else {
        return Ok(invalid_credentials());
    };
    if !verify_password(&password, &hash) {
        return Ok(invalid_credentials());
    }
    mark_signed_in(state, user_id).await?;
    let issued = issue_session(state, user_id, &email).await?;
    let user = fetch_user_json(state, "id", &SqlValue::Uuid(user_id))
        .await?
        .ok_or_else(|| SupaError::Internal("user missing".into()))?;
    Ok((
        StatusCode::OK,
        AxumJson(access_token_response(&issued, user)),
    )
        .into_response())
}

async fn token_refresh<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    body: &[u8],
) -> Result<Response, SupaError> {
    let obj = json_object(body)?;
    let token = obj
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| SupaError::BadRequest("refresh_token is required".into()))?;

    // Look up a live (non-revoked) refresh token.
    let result = run_sql(
        &state.db,
        "service_role",
        "SELECT user_id, session_id FROM auth.refresh_tokens WHERE token = $1 AND revoked = FALSE",
        vec![SqlValue::Text(token.clone())],
    )
    .await
    .map_err(SupaError::Sql)?;
    let (_, rows) = rows_of(result)?;
    let Some(row) = rows.first() else {
        return Ok(gotrue_oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "Invalid Refresh Token: Already Used",
        ));
    };
    let user_id = match &row[0] {
        SqlValue::Uuid(u) => *u,
        other => uuid::Uuid::parse_str(&other.to_text().unwrap_or_default())
            .map_err(|_| SupaError::Internal("bad user_id on refresh token".into()))?,
    };
    let session_id = match &row[1] {
        SqlValue::Uuid(u) => Some(*u),
        SqlValue::Null => None,
        other => uuid::Uuid::parse_str(&other.to_text().unwrap_or_default()).ok(),
    };

    // Rotate: revoke the presented token, mint a new one on the same session.
    run_sql(
        &state.db,
        "service_role",
        "UPDATE auth.refresh_tokens SET revoked = TRUE, updated_at = $2 WHERE token = $1",
        vec![
            SqlValue::Text(token.clone()),
            SqlValue::Timestamptz(Utc::now()),
        ],
    )
    .await
    .map_err(SupaError::Sql)?;

    let email = fetch_user_email(state, user_id).await?.unwrap_or_default();
    let issued = issue_session_with(state, user_id, &email, session_id, Some(token)).await?;
    let user = fetch_user_json(state, "id", &SqlValue::Uuid(user_id))
        .await?
        .ok_or_else(|| SupaError::Internal("user missing".into()))?;
    Ok((
        StatusCode::OK,
        AxumJson(access_token_response(&issued, user)),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

async fn logout<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let Some(uid) = auth.user_id() else {
            return Err(SupaError::InvalidJwt(jwt::JwtError::Malformed));
        };
        let user_id = uuid::Uuid::parse_str(uid)
            .map_err(|_| SupaError::BadRequest("invalid user id in token".into()))?;
        // Revoke this user's refresh tokens (all sessions in this slice).
        run_sql(
            &state.db,
            "service_role",
            "UPDATE auth.refresh_tokens SET revoked = TRUE, updated_at = $2 WHERE user_id = $1",
            vec![SqlValue::Uuid(user_id), SqlValue::Timestamptz(Utc::now())],
        )
        .await
        .map_err(SupaError::Sql)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// GET / PUT /user
// ---------------------------------------------------------------------------

async fn get_user<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let user_id = require_user(&auth)?;
        match fetch_user_json(&state, "id", &SqlValue::Uuid(user_id)).await? {
            Some(user) => Ok((StatusCode::OK, AxumJson(user)).into_response()),
            None => Ok(gotrue_error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                "User not found",
            )),
        }
    })
    .await
}

async fn put_user<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let user_id = require_user(&auth)?;
        let obj = json_object(&body)?;
        apply_user_update(&state, user_id, &obj).await?;
        match fetch_user_json(&state, "id", &SqlValue::Uuid(user_id)).await? {
            Some(user) => Ok((StatusCode::OK, AxumJson(user)).into_response()),
            None => Ok(gotrue_error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                "User not found",
            )),
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// Admin (service_role only)
// ---------------------------------------------------------------------------

async fn admin_list_users<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        require_service_role(&auth)?;
        ensure_schema(&state).await?;
        let sql = format!("SELECT {USER_COLUMNS} FROM auth.users ORDER BY created_at");
        let result = run_sql(&state.db, "service_role", &sql, Vec::new())
            .await
            .map_err(SupaError::Sql)?;
        let (fields, rows) = rows_of(result)?;
        let users: Vec<Json> = rows.iter().map(|r| row_to_user(&fields, r)).collect();
        Ok((
            StatusCode::OK,
            AxumJson(json!({"users": users, "aud": "authenticated"})),
        )
            .into_response())
    })
    .await
}

async fn admin_create_user<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    run(async {
        require_service_role(&auth)?;
        ensure_schema(&state).await?;
        let obj = json_object(&body)?;
        let email = require_email(&obj)?;
        let password = obj
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let metadata = obj
            .get("user_metadata")
            .or_else(|| obj.get("data"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        if find_user_by_email(&state, &email).await?.is_some() {
            return Ok(gotrue_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "user_already_exists",
                "A user with this email address has already been registered",
            ));
        }
        let confirm = obj
            .get("email_confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let user_id = uuid::Uuid::new_v4();
        create_user_row(&state, user_id, &email, &password, metadata, confirm).await?;
        let user = fetch_user_json(&state, "id", &SqlValue::Uuid(user_id))
            .await?
            .ok_or_else(|| SupaError::Internal("user missing after insert".into()))?;
        Ok((StatusCode::OK, AxumJson(user)).into_response())
    })
    .await
}

async fn admin_get_user<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Response {
    run(async {
        require_service_role(&auth)?;
        ensure_schema(&state).await?;
        let user_id = parse_uuid(&id)?;
        match fetch_user_json(&state, "id", &SqlValue::Uuid(user_id)).await? {
            Some(user) => Ok((StatusCode::OK, AxumJson(user)).into_response()),
            None => Ok(gotrue_error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                "User not found",
            )),
        }
    })
    .await
}

async fn admin_update_user<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    run(async {
        require_service_role(&auth)?;
        ensure_schema(&state).await?;
        let user_id = parse_uuid(&id)?;
        let obj = json_object(&body)?;
        apply_user_update(&state, user_id, &obj).await?;
        match fetch_user_json(&state, "id", &SqlValue::Uuid(user_id)).await? {
            Some(user) => Ok((StatusCode::OK, AxumJson(user)).into_response()),
            None => Ok(gotrue_error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                "User not found",
            )),
        }
    })
    .await
}

async fn admin_delete_user<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Response {
    run(async {
        require_service_role(&auth)?;
        ensure_schema(&state).await?;
        let user_id = parse_uuid(&id)?;
        let existing = fetch_user_json(&state, "id", &SqlValue::Uuid(user_id)).await?;
        if existing.is_none() {
            return Ok(gotrue_error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                "User not found",
            ));
        }
        run_sql(
            &state.db,
            "service_role",
            "DELETE FROM auth.refresh_tokens WHERE user_id = $1",
            vec![SqlValue::Uuid(user_id)],
        )
        .await
        .map_err(SupaError::Sql)?;
        run_sql(
            &state.db,
            "service_role",
            "DELETE FROM auth.users WHERE id = $1",
            vec![SqlValue::Uuid(user_id)],
        )
        .await
        .map_err(SupaError::Sql)?;
        Ok((StatusCode::OK, AxumJson(existing.unwrap())).into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// Data helpers
// ---------------------------------------------------------------------------

async fn create_user_row<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    user_id: uuid::Uuid,
    email: &str,
    password: &str,
    metadata: Json,
    confirm: bool,
) -> Result<(), SupaError> {
    let now = Utc::now();
    let hash = hash_password(password)?;
    let app_meta = json!({"provider": "email", "providers": ["email"]});
    let confirmed_at = if confirm {
        SqlValue::Timestamptz(now)
    } else {
        SqlValue::Null
    };
    run_sql(
        &state.db,
        "service_role",
        "INSERT INTO auth.users \
         (id, aud, role, email, encrypted_password, email_confirmed_at, \
          raw_app_meta_data, raw_user_meta_data, created_at, updated_at, is_anonymous) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        vec![
            SqlValue::Uuid(user_id),
            SqlValue::Text("authenticated".into()),
            SqlValue::Text("authenticated".into()),
            SqlValue::Text(email.to_string()),
            SqlValue::Text(hash),
            confirmed_at,
            SqlValue::Json(app_meta),
            SqlValue::Json(metadata),
            SqlValue::Timestamptz(now),
            SqlValue::Timestamptz(now),
            SqlValue::Bool(false),
        ],
    )
    .await
    .map_err(SupaError::Sql)?;
    Ok(())
}

async fn apply_user_update<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    user_id: uuid::Uuid,
    obj: &Map<String, Json>,
) -> Result<(), SupaError> {
    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(email) = obj.get("email").and_then(|v| v.as_str()) {
        params.push(SqlValue::Text(email.to_lowercase()));
        sets.push(format!("email = ${}", params.len()));
    }
    if let Some(password) = obj.get("password").and_then(|v| v.as_str()) {
        params.push(SqlValue::Text(hash_password(password)?));
        sets.push(format!("encrypted_password = ${}", params.len()));
    }
    if let Some(data) = obj.get("data").or_else(|| obj.get("user_metadata")) {
        params.push(SqlValue::Json(data.clone()));
        sets.push(format!("raw_user_meta_data = ${}", params.len()));
    }
    if sets.is_empty() {
        return Ok(());
    }
    params.push(SqlValue::Timestamptz(Utc::now()));
    sets.push(format!("updated_at = ${}", params.len()));
    params.push(SqlValue::Uuid(user_id));
    let sql = format!(
        "UPDATE auth.users SET {} WHERE id = ${}",
        sets.join(", "),
        params.len()
    );
    run_sql(&state.db, "service_role", &sql, params)
        .await
        .map_err(SupaError::Sql)?;
    Ok(())
}

async fn mark_signed_in<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    user_id: uuid::Uuid,
) -> Result<(), SupaError> {
    let now = Utc::now();
    run_sql(
        &state.db,
        "service_role",
        "UPDATE auth.users SET last_sign_in_at = $1, updated_at = $1 WHERE id = $2",
        vec![SqlValue::Timestamptz(now), SqlValue::Uuid(user_id)],
    )
    .await
    .map_err(SupaError::Sql)?;
    Ok(())
}

async fn find_user_by_email<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    email: &str,
) -> Result<Option<uuid::Uuid>, SupaError> {
    Ok(find_user_credentials(state, email).await?.map(|(id, _)| id))
}

async fn find_user_credentials<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    email: &str,
) -> Result<Option<(uuid::Uuid, String)>, SupaError> {
    let result = run_sql(
        &state.db,
        "service_role",
        "SELECT id, encrypted_password FROM auth.users WHERE email = $1",
        vec![SqlValue::Text(email.to_lowercase())],
    )
    .await
    .map_err(SupaError::Sql)?;
    let (_, rows) = rows_of(result)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let id = match &row[0] {
        SqlValue::Uuid(u) => *u,
        other => uuid::Uuid::parse_str(&other.to_text().unwrap_or_default())
            .map_err(|_| SupaError::Internal("bad user id".into()))?,
    };
    let hash = row[1].to_text().unwrap_or_default();
    Ok(Some((id, hash)))
}

async fn fetch_user_email<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    user_id: uuid::Uuid,
) -> Result<Option<String>, SupaError> {
    let result = run_sql(
        &state.db,
        "service_role",
        "SELECT email FROM auth.users WHERE id = $1",
        vec![SqlValue::Uuid(user_id)],
    )
    .await
    .map_err(SupaError::Sql)?;
    let (_, rows) = rows_of(result)?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| v.to_text()))
}

async fn fetch_user_json<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    column: &str,
    value: &SqlValue,
) -> Result<Option<Json>, SupaError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM auth.users WHERE {column} = $1");
    let result = run_sql(&state.db, "service_role", &sql, vec![value.clone()])
        .await
        .map_err(SupaError::Sql)?;
    let (fields, rows) = rows_of(result)?;
    Ok(rows.first().map(|r| row_to_user(&fields, r)))
}

// ---------------------------------------------------------------------------
// Token issuance
// ---------------------------------------------------------------------------

struct Issued {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    expires_at: i64,
}

async fn issue_session<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    user_id: uuid::Uuid,
    email: &str,
) -> Result<Issued, SupaError> {
    issue_session_with(state, user_id, email, None, None).await
}

/// Issue an access + refresh token, creating a session when one is not supplied
/// (fresh sign-in) or reusing `session_id` (refresh rotation).
async fn issue_session_with<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    user_id: uuid::Uuid,
    email: &str,
    session_id: Option<uuid::Uuid>,
    parent: Option<String>,
) -> Result<Issued, SupaError> {
    let now = Utc::now();
    let session_id = match session_id {
        Some(s) => s,
        None => {
            let s = uuid::Uuid::new_v4();
            run_sql(
                &state.db,
                "service_role",
                "INSERT INTO auth.sessions (id, user_id, created_at, updated_at, aal) \
                 VALUES ($1, $2, $3, $3, $4)",
                vec![
                    SqlValue::Uuid(s),
                    SqlValue::Uuid(user_id),
                    SqlValue::Timestamptz(now),
                    SqlValue::Text("aal1".into()),
                ],
            )
            .await
            .map_err(SupaError::Sql)?;
            s
        }
    };

    let refresh = random_token();
    run_sql(
        &state.db,
        "service_role",
        "INSERT INTO auth.refresh_tokens \
         (token, user_id, session_id, parent, revoked, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, FALSE, $5, $5)",
        vec![
            SqlValue::Text(refresh.clone()),
            SqlValue::Uuid(user_id),
            SqlValue::Uuid(session_id),
            parent.map(SqlValue::Text).unwrap_or(SqlValue::Null),
            SqlValue::Timestamptz(now),
        ],
    )
    .await
    .map_err(SupaError::Sql)?;

    let iat = now.timestamp();
    let exp = iat + state.config.jwt_exp;
    let claims = Claims {
        iss: Some(format!("{}/auth/v1", state.project.api_url)),
        role: "authenticated".to_string(),
        sub: Some(user_id.to_string()),
        email: Some(email.to_string()),
        aud: Some(state.config.jwt_aud.clone()),
        iat,
        exp,
        session_id: Some(session_id.to_string()),
        extra: Map::new(),
    };
    let access_token = jwt::sign(&claims, state.project.keys.jwt_secret.expose())
        .map_err(|e| SupaError::Internal(format!("token signing failed: {e}")))?;

    Ok(Issued {
        access_token,
        refresh_token: refresh,
        expires_in: state.config.jwt_exp,
        expires_at: exp,
    })
}

fn access_token_response(issued: &Issued, user: Json) -> Json {
    json!({
        "access_token": issued.access_token,
        "token_type": "bearer",
        "expires_in": issued.expires_in,
        "expires_at": issued.expires_at,
        "refresh_token": issued.refresh_token,
        "user": user,
    })
}

// ---------------------------------------------------------------------------
// User JSON shaping
// ---------------------------------------------------------------------------

fn row_to_user(fields: &[OutField], row: &[SqlValue]) -> Json {
    let mut m = Map::new();
    for (f, v) in fields.iter().zip(row.iter()) {
        m.insert(f.name.clone(), value_to_json(v));
    }
    let app_metadata = m
        .remove("raw_app_meta_data")
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let user_metadata = m
        .remove("raw_user_meta_data")
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let take = |m: &mut Map<String, Json>, k: &str| m.remove(k).unwrap_or(Json::Null);
    let email_confirmed_at = m.get("email_confirmed_at").cloned().unwrap_or(Json::Null);
    json!({
        "id": take(&mut m, "id"),
        "aud": take(&mut m, "aud"),
        "role": take(&mut m, "role"),
        "email": take(&mut m, "email"),
        "email_confirmed_at": email_confirmed_at,
        "confirmed_at": take(&mut m, "email_confirmed_at"),
        "phone": m.remove("phone").filter(|v| !v.is_null()).unwrap_or_else(|| json!("")),
        "last_sign_in_at": take(&mut m, "last_sign_in_at"),
        "app_metadata": app_metadata,
        "user_metadata": user_metadata,
        "identities": json!([]),
        "created_at": take(&mut m, "created_at"),
        "updated_at": take(&mut m, "updated_at"),
        "is_anonymous": m.remove("is_anonymous").filter(|v| !v.is_null()).unwrap_or(Json::Bool(false)),
    })
}

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

fn hash_password(password: &str) -> Result<String, SupaError> {
    bcrypt::hash(password, AUTH_BCRYPT_COST)
        .map_err(|e| SupaError::Internal(format!("password hashing failed: {e}")))
}

fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Run a fallible response-producing future, mapping any [`SupaError`] to its
/// response so handlers stay flat.
async fn run<F>(fut: F) -> Response
where
    F: std::future::Future<Output = Result<Response, SupaError>>,
{
    match fut.await {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

fn json_object(body: &[u8]) -> Result<Map<String, Json>, SupaError> {
    if body.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(o)) => Ok(o),
        Ok(Json::Null) => Ok(Map::new()),
        Ok(_) => Err(SupaError::BadRequest(
            "request body must be a JSON object".into(),
        )),
        Err(e) => Err(SupaError::BadRequest(format!("invalid JSON body: {e}"))),
    }
}

fn require_email(obj: &Map<String, Json>) -> Result<String, SupaError> {
    let email = obj
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SupaError::BadRequest("email is required".into()))?;
    Ok(email)
}

fn require_password(obj: &Map<String, Json>) -> Result<String, SupaError> {
    obj.get("password")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| SupaError::BadRequest("password is required".into()))
}

fn require_user(auth: &AuthContext) -> Result<uuid::Uuid, SupaError> {
    let uid = auth
        .user_id()
        .ok_or(SupaError::InvalidJwt(jwt::JwtError::Malformed))?;
    parse_uuid(uid)
}

fn require_service_role(auth: &AuthContext) -> Result<(), SupaError> {
    if auth.is_service_role() {
        Ok(())
    } else {
        Err(SupaError::Forbidden("the admin API"))
    }
}

fn parse_uuid(s: &str) -> Result<uuid::Uuid, SupaError> {
    uuid::Uuid::parse_str(s).map_err(|_| SupaError::BadRequest(format!("invalid user id: {s}")))
}

fn invalid_credentials() -> Response {
    gotrue_oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_grant",
        "Invalid login credentials",
    )
}

fn is_oauth_grant(grant: &str) -> bool {
    matches!(
        grant,
        "authorization_code" | "pkce" | "id_token" | "web3" | "implicit"
    )
}

fn random_token() -> String {
    // Two v4 UUIDs (OS CSPRNG) → 64 hex chars of unpredictable entropy.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn rows_of(result: ExecResult) -> Result<(Vec<OutField>, Vec<Vec<SqlValue>>), SupaError> {
    match result {
        ExecResult::Rows { fields, rows } => Ok((fields, rows)),
        ExecResult::Command { tag } => Err(SupaError::Internal(format!(
            "expected rows, got command tag: {tag}"
        ))),
    }
}

/// Unix time helper kept for symmetry with GoTrue's `expires_at` semantics.
#[allow(dead_code)]
fn unix(dt: DateTime<Utc>) -> i64 {
    dt.timestamp()
}
