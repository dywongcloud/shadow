//! Supabase Storage-compatible API over the SQL engine.
//!
//! Buckets and object metadata live in the `storage` schema (bootstrapped on
//! first use, see [`BOOTSTRAP_SQL`]); object **bytes** live in a dedicated
//! `storage._blobs` table (`object_id uuid PRIMARY KEY, content bytea`) written
//! through parameterised SQL. Because the engine's `bytea` values persist in
//! the replicated document store, uploads replicate like any other row — the
//! honest trade-off is that object bytes travel through the SQL layer (JSON
//! documents, base64-encoded), which is fine for this slice; an iroh-blobs
//! content-addressed path is a later optimisation.
//!
//! Authorization rides Row-Level Security: `storage.buckets` and
//! `storage.objects` are created with RLS **enabled and no policies**, so
//! `service_role` (an RLS-bypass role) has full access and every other role is
//! default-denied until the operator adds `CREATE POLICY` rules (e.g.
//! owner-scoped policies comparing `owner = auth.uid()`). Object queries run
//! as the request's resolved role with its JWT claims injected, exactly like
//! `/rest/v1`. Blob bytes are only ever fetched *after* the caller's
//! role-bound query proved the object row visible.
//!
//! Errors use the storage-api shape `{"statusCode","error","message"}` (with
//! `statusCode` as a string, like Supabase's storage service).

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json as AxumJson};
use chrono::Utc;
use serde_json::{Map, Value as Json, json};

use crate::sql::{ExecResult, OutField, RelationalStorage, SqlValue};
use crate::supabase::error::{SupaError, status_for_sqlstate};
use crate::supabase::gateway::{AppState, AuthContext, header_str, run_batch, run_sql, run_sql_as};
use crate::supabase::jwt::{self, Claims};
use crate::supabase::rest::{parse_query_pairs, value_to_json};

/// Maximum accepted upload size (bytes). Objects pass through the SQL layer,
/// so this is deliberately conservative; buckets can lower it further with
/// `file_size_limit`.
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// The `storage` schema bootstrap: the storage-api column subset GuardianDB's
/// engine supports. `allowed_mime_types` is `jsonb` (stock Supabase uses
/// `text[]`); `storage._blobs` holds the object bytes. All three tables have
/// row security enabled with **no default policies**: `service_role` bypasses
/// RLS, every other role is default-denied until the operator adds policies.
pub const BOOTSTRAP_SQL: &str = "
CREATE SCHEMA IF NOT EXISTS storage;

CREATE TABLE IF NOT EXISTS storage.buckets (
    id text PRIMARY KEY,
    name text,
    owner uuid,
    public boolean,
    file_size_limit bigint,
    allowed_mime_types jsonb,
    created_at timestamptz,
    updated_at timestamptz
);

CREATE TABLE IF NOT EXISTS storage.objects (
    id uuid PRIMARY KEY,
    bucket_id text,
    name text,
    owner uuid,
    metadata jsonb,
    created_at timestamptz,
    updated_at timestamptz,
    last_accessed_at timestamptz,
    UNIQUE (bucket_id, name)
);

CREATE TABLE IF NOT EXISTS storage._blobs (
    object_id uuid PRIMARY KEY,
    content bytea
);

ALTER TABLE storage.buckets ENABLE ROW LEVEL SECURITY;
ALTER TABLE storage.objects ENABLE ROW LEVEL SECURITY;
ALTER TABLE storage._blobs ENABLE ROW LEVEL SECURITY;
";

/// The columns returned whenever a bucket row is rendered.
const BUCKET_COLUMNS: &str =
    "id, name, owner, public, file_size_limit, allowed_mime_types, created_at, updated_at";

/// The columns returned whenever an object row is rendered.
const OBJECT_COLUMNS: &str =
    "id, name, bucket_id, owner, metadata, created_at, updated_at, last_accessed_at";

// ---------------------------------------------------------------------------
// Routers
// ---------------------------------------------------------------------------

/// The authenticated storage routes, mounted at `/storage/v1` **behind** the
/// apikey middleware.
pub fn protected_router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/bucket", post(create_bucket::<S>).get(list_buckets::<S>))
        .route(
            "/bucket/{id}",
            get(get_bucket::<S>)
                .put(update_bucket::<S>)
                .delete(delete_bucket::<S>),
        )
        .route("/bucket/{id}/empty", post(empty_bucket::<S>))
        .route(
            "/object/{bucket}/{*path}",
            post(upload::<S>)
                .put(upload::<S>)
                .get(download_authed::<S>)
                .delete(delete_object::<S>),
        )
        .route("/object/{bucket}", axum::routing::delete(bulk_delete::<S>))
        .route("/object/move", post(move_object::<S>))
        .route("/object/copy", post(copy_object::<S>))
        .route("/object/list/{bucket}", post(list_objects::<S>))
        .route(
            "/object/sign/{bucket}/{*path}",
            post(create_signed_url::<S>),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
}

/// The unauthenticated storage routes (public buckets and signed URLs),
/// mounted at `/storage/v1` **outside** the apikey middleware.
pub fn public_router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/object/public/{bucket}/{*path}", get(download_public::<S>))
        .route("/object/sign/{bucket}/{*path}", get(download_signed::<S>))
}

/// Typed catch-all for `/storage/v1` paths this slice does not implement —
/// never a bare 404.
pub async fn unsupported_route() -> Response {
    storage_error(
        StatusCode::NOT_FOUND,
        "SUPA_COMPAT_STORAGE_UNSUPPORTED_ROUTE",
        "this storage route is not implemented in the GuardianDB compatibility slice",
    )
}

// ---------------------------------------------------------------------------
// Error rendering (storage-api shape)
// ---------------------------------------------------------------------------

/// Render a storage-api-shaped error: `{"statusCode","error","message"}` with
/// `statusCode` as a string, matching Supabase's storage service.
pub fn storage_error(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        AxumJson(json!({
            "statusCode": status.as_u16().to_string(),
            "error": error,
            "message": message,
        })),
    )
        .into_response()
}

/// Convert a gateway error into the storage-api error shape. SQL errors keep
/// their SQLSTATE as the `error` field (RLS denials surface as `42501` → 403).
fn storage_error_from(e: SupaError) -> Response {
    if let SupaError::Sql(err) = &e {
        let state = err.sqlstate();
        return storage_error(status_for_sqlstate(state), state, &err.to_string());
    }
    storage_error(e.status(), &e.code(), &e.message())
}

/// Run a storage handler body, converting infrastructure errors to the
/// storage-api shape.
async fn run<F>(fut: F) -> Response
where
    F: Future<Output = Result<Response, SupaError>>,
{
    fut.await.unwrap_or_else(storage_error_from)
}

fn not_found(what: &str) -> Response {
    storage_error(StatusCode::NOT_FOUND, "not_found", what)
}

// ---------------------------------------------------------------------------
// Schema bootstrap
// ---------------------------------------------------------------------------

/// Bootstrap the `storage` schema exactly once per gateway instance.
pub async fn ensure_schema<S: RelationalStorage + 'static>(
    state: &AppState<S>,
) -> Result<(), SupaError> {
    state
        .storage_ready
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
// Buckets
// ---------------------------------------------------------------------------

async fn create_bucket<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let obj = json_object(&body)?;
        let name = obj
            .get("name")
            .and_then(Json::as_str)
            .or_else(|| obj.get("id").and_then(Json::as_str))
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            return Ok(storage_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "bucket name is required",
            ));
        }
        let id = obj
            .get("id")
            .and_then(Json::as_str)
            .unwrap_or(&name)
            .to_string();
        let public = obj.get("public").and_then(Json::as_bool).unwrap_or(false);
        let file_size_limit = obj.get("file_size_limit").and_then(Json::as_i64);
        let allowed = obj.get("allowed_mime_types").cloned();
        let now = Utc::now();

        let result = run_sql_as(
            &state.db,
            &auth,
            "INSERT INTO storage.buckets \
             (id, name, owner, public, file_size_limit, allowed_mime_types, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            vec![
                SqlValue::Text(id.clone()),
                SqlValue::Text(name),
                owner_value(&auth),
                SqlValue::Bool(public),
                file_size_limit.map(SqlValue::Int8).unwrap_or(SqlValue::Null),
                allowed.map(SqlValue::Json).unwrap_or(SqlValue::Null),
                SqlValue::Timestamptz(now),
                SqlValue::Timestamptz(now),
            ],
        )
        .await;
        if let Err(e) = result {
            if e.sqlstate() == "23505" {
                return Ok(storage_error(
                    StatusCode::CONFLICT,
                    "Duplicate",
                    "The resource already exists",
                ));
            }
            return Err(SupaError::Sql(e));
        }
        Ok((StatusCode::OK, AxumJson(json!({ "name": id }))).into_response())
    })
    .await
}

async fn list_buckets<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let result = run_sql_as(
            &state.db,
            &auth,
            &format!("SELECT {BUCKET_COLUMNS} FROM storage.buckets ORDER BY name"),
            vec![],
        )
        .await
        .map_err(SupaError::Sql)?;
        Ok(AxumJson(Json::Array(result_objects(result)?)).into_response())
    })
    .await
}

async fn get_bucket<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let result = run_sql_as(
            &state.db,
            &auth,
            &format!("SELECT {BUCKET_COLUMNS} FROM storage.buckets WHERE id = $1"),
            vec![SqlValue::Text(id)],
        )
        .await
        .map_err(SupaError::Sql)?;
        match result_objects(result)?.into_iter().next() {
            Some(bucket) => Ok(AxumJson(bucket).into_response()),
            None => Ok(not_found("Bucket not found")),
        }
    })
    .await
}

async fn update_bucket<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let obj = json_object(&body)?;
        let mut sets = vec!["updated_at = $1".to_string()];
        let mut params = vec![SqlValue::Timestamptz(Utc::now())];
        if let Some(public) = obj.get("public").and_then(Json::as_bool) {
            params.push(SqlValue::Bool(public));
            sets.push(format!("public = ${}", params.len()));
        }
        if obj.contains_key("file_size_limit") {
            params.push(
                obj.get("file_size_limit")
                    .and_then(Json::as_i64)
                    .map(SqlValue::Int8)
                    .unwrap_or(SqlValue::Null),
            );
            sets.push(format!("file_size_limit = ${}", params.len()));
        }
        if obj.contains_key("allowed_mime_types") {
            let v = obj.get("allowed_mime_types").cloned().unwrap_or(Json::Null);
            params.push(if v.is_null() {
                SqlValue::Null
            } else {
                SqlValue::Json(v)
            });
            sets.push(format!("allowed_mime_types = ${}", params.len()));
        }
        params.push(SqlValue::Text(id));
        let sql = format!(
            "UPDATE storage.buckets SET {} WHERE id = ${} RETURNING id",
            sets.join(", "),
            params.len()
        );
        let result = run_sql_as(&state.db, &auth, &sql, params)
            .await
            .map_err(SupaError::Sql)?;
        if result_objects(result)?.is_empty() {
            return Ok(not_found("Bucket not found"));
        }
        Ok(AxumJson(json!({ "message": "Successfully updated" })).into_response())
    })
    .await
}

async fn delete_bucket<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        // A bucket must be empty before it can be deleted (storage-api rule).
        let count = run_sql(
            &state.db,
            "service_role",
            "SELECT count(*) AS c FROM storage.objects WHERE bucket_id = $1",
            vec![SqlValue::Text(id.clone())],
        )
        .await
        .map_err(SupaError::Sql)?;
        if scalar_i64(&count) > 0 {
            return Ok(storage_error(
                StatusCode::CONFLICT,
                "invalid_request",
                "The bucket you tried to delete is not empty",
            ));
        }
        let result = run_sql_as(
            &state.db,
            &auth,
            "DELETE FROM storage.buckets WHERE id = $1 RETURNING id",
            vec![SqlValue::Text(id)],
        )
        .await
        .map_err(SupaError::Sql)?;
        if result_objects(result)?.is_empty() {
            return Ok(not_found("Bucket not found"));
        }
        Ok(AxumJson(json!({ "message": "Successfully deleted" })).into_response())
    })
    .await
}

async fn empty_bucket<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let result = run_sql_as(
            &state.db,
            &auth,
            "DELETE FROM storage.objects WHERE bucket_id = $1 RETURNING id",
            vec![SqlValue::Text(id)],
        )
        .await
        .map_err(SupaError::Sql)?;
        delete_blobs_for(&state, result_objects(result)?).await?;
        Ok(AxumJson(json!({ "message": "Successfully emptied" })).into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// Objects: upload / download
// ---------------------------------------------------------------------------

async fn upload<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let name = normalize_object_name(&path)?;
        let content_type = header_str(&headers, "content-type")
            .unwrap_or("application/octet-stream")
            .to_string();
        if content_type.starts_with("multipart/form-data") {
            return Ok(storage_error(
                StatusCode::NOT_IMPLEMENTED,
                "SUPA_COMPAT_STORAGE_MULTIPART_UNSUPPORTED",
                "multipart/form-data uploads are not implemented in this slice; \
                 send the file as the raw request body with its content-type header \
                 (what supabase-js does in browsers)",
            ));
        }

        // Bucket existence + limits are checked internally (service_role), like
        // the storage service does; the object write itself is role-bound.
        let Some(bucket_row) = fetch_bucket(&state, &bucket).await? else {
            return Ok(not_found("Bucket not found"));
        };
        if let Some(limit) = bucket_row.get("file_size_limit").and_then(Json::as_i64)
            && (body.len() as i64) > limit
        {
            return Ok(storage_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Payload too large",
                "The object exceeded the maximum allowed size",
            ));
        }
        if let Some(allowed) = bucket_row
            .get("allowed_mime_types")
            .and_then(Json::as_array)
            && !mime_allowed(&content_type, allowed)
        {
            return Ok(storage_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_mime_type",
                &format!("mime type {content_type} is not supported"),
            ));
        }

        let upsert = header_str(&headers, "x-upsert")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let id = uuid::Uuid::new_v4();
        let now = Utc::now();
        let metadata = json!({
            "mimetype": content_type,
            "size": body.len(),
            "cacheControl": header_str(&headers, "cache-control").unwrap_or("no-cache"),
            "lastModified": now.to_rfc3339(),
            "contentLength": body.len(),
        });

        let conflict = if upsert {
            " ON CONFLICT (bucket_id, name) DO UPDATE SET owner = EXCLUDED.owner, \
             metadata = EXCLUDED.metadata, updated_at = EXCLUDED.updated_at"
        } else {
            ""
        };
        let sql = format!(
            "INSERT INTO storage.objects \
             (id, bucket_id, name, owner, metadata, created_at, updated_at, last_accessed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8){conflict} RETURNING id"
        );
        let result = run_sql_as(
            &state.db,
            &auth,
            &sql,
            vec![
                SqlValue::Uuid(id),
                SqlValue::Text(bucket.clone()),
                SqlValue::Text(name.clone()),
                owner_value(&auth),
                SqlValue::Json(metadata),
                SqlValue::Timestamptz(now),
                SqlValue::Timestamptz(now),
                SqlValue::Timestamptz(now),
            ],
        )
        .await;
        let rows = match result {
            Ok(r) => result_objects(r)?,
            Err(e) if e.sqlstate() == "23505" => {
                return Ok(storage_error(
                    StatusCode::CONFLICT,
                    "Duplicate",
                    "The resource already exists",
                ));
            }
            Err(e) => return Err(SupaError::Sql(e)),
        };
        let object_id = rows
            .first()
            .and_then(|o| o.get("id"))
            .and_then(Json::as_str)
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .ok_or_else(|| SupaError::Internal("object insert returned no id".into()))?;

        // Bytes are written after the role-bound insert proved the caller may
        // create the object; the blob table itself is internal (service_role).
        run_sql(
            &state.db,
            "service_role",
            "INSERT INTO storage._blobs (object_id, content) VALUES ($1, $2) \
             ON CONFLICT (object_id) DO UPDATE SET content = EXCLUDED.content",
            vec![SqlValue::Uuid(object_id), SqlValue::Bytea(body.to_vec())],
        )
        .await
        .map_err(SupaError::Sql)?;

        Ok((
            StatusCode::OK,
            AxumJson(json!({
                "Id": object_id.to_string(),
                "Key": format!("{bucket}/{name}"),
            })),
        )
            .into_response())
    })
    .await
}

async fn download_authed<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path((bucket, path)): Path<(String, String)>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let name = normalize_object_name(&path)?;
        // Role-bound visibility check: RLS policies on storage.objects govern
        // whether this row exists for the caller.
        let result = run_sql_as(
            &state.db,
            &auth,
            "SELECT id, metadata FROM storage.objects WHERE bucket_id = $1 AND name = $2",
            vec![SqlValue::Text(bucket), SqlValue::Text(name)],
        )
        .await
        .map_err(SupaError::Sql)?;
        match result_objects(result)?.into_iter().next() {
            Some(row) => serve_object_row(&state, &row).await,
            None => Ok(not_found("Object not found")),
        }
    })
    .await
}

async fn download_public<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Path((bucket, path)): Path<(String, String)>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let name = normalize_object_name(&path)?;
        // Only objects in a public bucket are served without credentials.
        let Some(bucket_row) = fetch_bucket(&state, &bucket).await? else {
            return Ok(not_found("Object not found"));
        };
        if !bucket_row
            .get("public")
            .and_then(Json::as_bool)
            .unwrap_or(false)
        {
            return Ok(not_found("Object not found"));
        }
        serve_object(&state, &bucket, &name).await
    })
    .await
}

async fn create_signed_url<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path((bucket, path)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let name = normalize_object_name(&path)?;
        let obj = json_object(&body)?;
        let expires_in = obj.get("expiresIn").and_then(Json::as_i64).unwrap_or(0);
        if expires_in <= 0 {
            return Ok(storage_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "expiresIn must be a positive number of seconds",
            ));
        }
        // The signer must be able to see the object under their own role.
        let visible = run_sql_as(
            &state.db,
            &auth,
            "SELECT id FROM storage.objects WHERE bucket_id = $1 AND name = $2",
            vec![SqlValue::Text(bucket.clone()), SqlValue::Text(name.clone())],
        )
        .await
        .map_err(SupaError::Sql)?;
        if result_objects(visible)?.is_empty() {
            return Ok(not_found("Object not found"));
        }
        let now = Utc::now().timestamp();
        let mut claims = Claims::api_key("anon", now, now + expires_in);
        claims.iss = None;
        claims
            .extra
            .insert("url".to_string(), json!(format!("{bucket}/{name}")));
        let token = jwt::sign(&claims, state.project.keys.jwt_secret.expose())
            .map_err(|e| SupaError::Internal(format!("could not sign url token: {e}")))?;
        let signed = format!(
            "/object/sign/{bucket}/{}?token={token}",
            percent_encode_path(&name)
        );
        Ok(AxumJson(json!({ "signedURL": signed })).into_response())
    })
    .await
}

async fn download_signed<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Path((bucket, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let name = normalize_object_name(&path)?;
        let token = parse_query_pairs(query.as_deref().unwrap_or(""))
            .into_iter()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v)
            .unwrap_or_default();
        if token.is_empty() {
            return Ok(storage_error(
                StatusCode::BAD_REQUEST,
                "invalid_signature",
                "missing token query parameter",
            ));
        }
        let claims = match jwt::verify(
            &token,
            state.project.keys.jwt_secret.expose(),
            Utc::now().timestamp(),
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(storage_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_signature",
                    &format!("invalid signed url token: {e}"),
                ));
            }
        };
        let signed_for = claims.extra.get("url").and_then(Json::as_str).unwrap_or("");
        if signed_for != format!("{bucket}/{name}") {
            return Ok(storage_error(
                StatusCode::BAD_REQUEST,
                "invalid_signature",
                "token does not match the requested object",
            ));
        }
        serve_object(&state, &bucket, &name).await
    })
    .await
}

// ---------------------------------------------------------------------------
// Objects: delete / move / copy / list
// ---------------------------------------------------------------------------

async fn delete_object<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path((bucket, path)): Path<(String, String)>,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let name = normalize_object_name(&path)?;
        let result = run_sql_as(
            &state.db,
            &auth,
            "DELETE FROM storage.objects WHERE bucket_id = $1 AND name = $2 RETURNING id",
            vec![SqlValue::Text(bucket), SqlValue::Text(name)],
        )
        .await
        .map_err(SupaError::Sql)?;
        let deleted = result_objects(result)?;
        if deleted.is_empty() {
            return Ok(not_found("Object not found"));
        }
        delete_blobs_for(&state, deleted).await?;
        Ok(AxumJson(json!({ "message": "Successfully deleted" })).into_response())
    })
    .await
}

/// `DELETE /object/{bucket}` with `{"prefixes": ["a.txt", ...]}` — the bulk
/// endpoint storage-js `remove()` calls. Returns the deleted rows.
async fn bulk_delete<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(bucket): Path<String>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let obj = json_object(&body)?;
        let prefixes: Vec<String> = obj
            .get("prefixes")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Json::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if prefixes.is_empty() {
            return Ok(storage_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "prefixes is required and must be a non-empty array of object names",
            ));
        }
        let mut all = Vec::new();
        for name in prefixes {
            let result = run_sql_as(
                &state.db,
                &auth,
                &format!(
                    "DELETE FROM storage.objects WHERE bucket_id = $1 AND name = $2 \
                     RETURNING {OBJECT_COLUMNS}"
                ),
                vec![SqlValue::Text(bucket.clone()), SqlValue::Text(name)],
            )
            .await
            .map_err(SupaError::Sql)?;
            all.extend(result_objects(result)?);
        }
        delete_blobs_for(&state, all.clone()).await?;
        Ok(AxumJson(Json::Array(all)).into_response())
    })
    .await
}

async fn move_object<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let (bucket, source, dest) = move_copy_args(&body)?;
        let result = run_sql_as(
            &state.db,
            &auth,
            "UPDATE storage.objects SET name = $1, updated_at = $2 \
             WHERE bucket_id = $3 AND name = $4 RETURNING id",
            vec![
                SqlValue::Text(dest),
                SqlValue::Timestamptz(Utc::now()),
                SqlValue::Text(bucket),
                SqlValue::Text(source),
            ],
        )
        .await;
        match result {
            Ok(r) => {
                if result_objects(r)?.is_empty() {
                    Ok(not_found("Object not found"))
                } else {
                    Ok(AxumJson(json!({ "message": "Successfully moved" })).into_response())
                }
            }
            Err(e) if e.sqlstate() == "23505" => Ok(storage_error(
                StatusCode::CONFLICT,
                "Duplicate",
                "The destination object already exists",
            )),
            Err(e) => Err(SupaError::Sql(e)),
        }
    })
    .await
}

async fn copy_object<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let (bucket, source, dest) = move_copy_args(&body)?;
        // Role-bound read of the source row.
        let src = run_sql_as(
            &state.db,
            &auth,
            "SELECT id, metadata FROM storage.objects WHERE bucket_id = $1 AND name = $2",
            vec![SqlValue::Text(bucket.clone()), SqlValue::Text(source)],
        )
        .await
        .map_err(SupaError::Sql)?;
        let Some(src_row) = result_objects(src)?.into_iter().next() else {
            return Ok(not_found("Object not found"));
        };
        let src_id = row_uuid(&src_row, "id")
            .ok_or_else(|| SupaError::Internal("source object has no id".into()))?;
        let new_id = uuid::Uuid::new_v4();
        let now = Utc::now();
        // Role-bound insert of the destination row (WITH CHECK applies).
        let inserted = run_sql_as(
            &state.db,
            &auth,
            "INSERT INTO storage.objects \
             (id, bucket_id, name, owner, metadata, created_at, updated_at, last_accessed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
            vec![
                SqlValue::Uuid(new_id),
                SqlValue::Text(bucket.clone()),
                SqlValue::Text(dest.clone()),
                owner_value(&auth),
                src_row
                    .get("metadata")
                    .cloned()
                    .map(SqlValue::Json)
                    .unwrap_or(SqlValue::Null),
                SqlValue::Timestamptz(now),
                SqlValue::Timestamptz(now),
                SqlValue::Timestamptz(now),
            ],
        )
        .await;
        if let Err(e) = inserted {
            if e.sqlstate() == "23505" {
                return Ok(storage_error(
                    StatusCode::CONFLICT,
                    "Duplicate",
                    "The destination object already exists",
                ));
            }
            return Err(SupaError::Sql(e));
        }
        // Copy the bytes (internal).
        let blob = fetch_blob(&state, src_id).await?;
        run_sql(
            &state.db,
            "service_role",
            "INSERT INTO storage._blobs (object_id, content) VALUES ($1, $2) \
             ON CONFLICT (object_id) DO UPDATE SET content = EXCLUDED.content",
            vec![SqlValue::Uuid(new_id), SqlValue::Bytea(blob)],
        )
        .await
        .map_err(SupaError::Sql)?;
        Ok(AxumJson(json!({
            "Id": new_id.to_string(),
            "Key": format!("{bucket}/{dest}"),
        }))
        .into_response())
    })
    .await
}

async fn list_objects<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(bucket): Path<String>,
    body: Bytes,
) -> Response {
    run(async {
        ensure_schema(&state).await?;
        let obj = if body.is_empty() {
            Map::new()
        } else {
            json_object(&body)?
        };
        let prefix = obj
            .get("prefix")
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        let limit = obj
            .get("limit")
            .and_then(Json::as_u64)
            .map(|l| l as usize)
            .unwrap_or(100);
        let offset = obj
            .get("offset")
            .and_then(Json::as_u64)
            .map(|o| o as usize)
            .unwrap_or(0);
        let search = obj
            .get("search")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        let (sort_col, sort_desc) = match obj.get("sortBy") {
            Some(Json::Object(s)) => {
                let col = s.get("column").and_then(Json::as_str).unwrap_or("name");
                let order = s.get("order").and_then(Json::as_str).unwrap_or("asc");
                (col.to_string(), order.eq_ignore_ascii_case("desc"))
            }
            _ => ("name".to_string(), false),
        };
        if !matches!(
            sort_col.as_str(),
            "name" | "created_at" | "updated_at" | "last_accessed_at"
        ) {
            return Ok(storage_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("cannot sort by column {sort_col}"),
            ));
        }

        // Role-bound scan: RLS decides which rows exist for the caller.
        // Prefix/search/window are applied here (names are opaque bytes to the
        // engine's LIKE, so filtering in the gateway avoids escape pitfalls).
        let result = run_sql_as(
            &state.db,
            &auth,
            &format!("SELECT {OBJECT_COLUMNS} FROM storage.objects WHERE bucket_id = $1"),
            vec![SqlValue::Text(bucket)],
        )
        .await
        .map_err(SupaError::Sql)?;
        let mut rows: Vec<Json> = result_objects(result)?
            .into_iter()
            .filter(|o| {
                let name = o.get("name").and_then(Json::as_str).unwrap_or("");
                name.starts_with(&prefix) && (search.is_empty() || name.contains(&search))
            })
            .collect();
        rows.sort_by(|a, b| {
            let ka = a.get(&sort_col).and_then(Json::as_str).unwrap_or("");
            let kb = b.get(&sort_col).and_then(Json::as_str).unwrap_or("");
            if sort_desc { kb.cmp(ka) } else { ka.cmp(kb) }
        });
        let page: Vec<Json> = rows.into_iter().skip(offset).take(limit).collect();
        Ok(AxumJson(Json::Array(page)).into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Fetch a bucket row (internal, service_role — existence checks and limits).
async fn fetch_bucket<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    id: &str,
) -> Result<Option<Json>, SupaError> {
    let result = run_sql(
        &state.db,
        "service_role",
        &format!("SELECT {BUCKET_COLUMNS} FROM storage.buckets WHERE id = $1"),
        vec![SqlValue::Text(id.to_string())],
    )
    .await
    .map_err(SupaError::Sql)?;
    Ok(result_objects(result)?.into_iter().next())
}

/// Serve an object's bytes after the caller proved they may see the row
/// (role-bound query, public bucket, or verified signed URL).
async fn serve_object<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    bucket: &str,
    name: &str,
) -> Result<Response, SupaError> {
    let result = run_sql(
        &state.db,
        "service_role",
        "SELECT id, metadata FROM storage.objects WHERE bucket_id = $1 AND name = $2",
        vec![
            SqlValue::Text(bucket.to_string()),
            SqlValue::Text(name.to_string()),
        ],
    )
    .await
    .map_err(SupaError::Sql)?;
    match result_objects(result)?.into_iter().next() {
        Some(row) => serve_object_row(state, &row).await,
        None => Ok(not_found("Object not found")),
    }
}

async fn serve_object_row<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    row: &Json,
) -> Result<Response, SupaError> {
    let id = row_uuid(row, "id").ok_or_else(|| SupaError::Internal("object has no id".into()))?;
    let bytes = fetch_blob(state, id).await?;
    let meta = row.get("metadata").cloned().unwrap_or(Json::Null);
    let content_type = meta
        .get("mimetype")
        .and_then(Json::as_str)
        .unwrap_or("application/octet-stream")
        .to_string();
    let cache_control = meta
        .get("cacheControl")
        .and_then(Json::as_str)
        .unwrap_or("no-cache")
        .to_string();
    let mut resp = (StatusCode::OK, bytes).into_response();
    if let Ok(v) = content_type.parse() {
        resp.headers_mut().insert("content-type", v);
    }
    if let Ok(v) = cache_control.parse() {
        resp.headers_mut().insert("cache-control", v);
    }
    Ok(resp)
}

/// Fetch an object's bytes from `storage._blobs` (internal, service_role).
async fn fetch_blob<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    object_id: uuid::Uuid,
) -> Result<Vec<u8>, SupaError> {
    let result = run_sql(
        &state.db,
        "service_role",
        "SELECT content FROM storage._blobs WHERE object_id = $1",
        vec![SqlValue::Uuid(object_id)],
    )
    .await
    .map_err(SupaError::Sql)?;
    match result {
        ExecResult::Rows { rows, .. } => match rows.into_iter().next().and_then(|mut r| {
            if r.is_empty() {
                None
            } else {
                Some(r.remove(0))
            }
        }) {
            Some(SqlValue::Bytea(bytes)) => Ok(bytes),
            Some(SqlValue::Null) | None => Err(SupaError::Internal(
                "object bytes are missing from storage._blobs".into(),
            )),
            Some(other) => Ok(other.to_text().unwrap_or_default().into_bytes()),
        },
        ExecResult::Command { .. } => Err(SupaError::Internal(
            "blob query returned a command tag".into(),
        )),
    }
}

/// Delete the blob rows for a set of deleted object rows (internal).
async fn delete_blobs_for<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    deleted: Vec<Json>,
) -> Result<(), SupaError> {
    for row in deleted {
        if let Some(id) = row_uuid(&row, "id") {
            run_sql(
                &state.db,
                "service_role",
                "DELETE FROM storage._blobs WHERE object_id = $1",
                vec![SqlValue::Uuid(id)],
            )
            .await
            .map_err(SupaError::Sql)?;
        }
    }
    Ok(())
}

/// The `owner` value for a write: the authenticated user's id (`sub` claim)
/// when it is a uuid, else NULL (API-key and service writes have no owner).
fn owner_value(auth: &AuthContext) -> SqlValue {
    auth.user_id()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(SqlValue::Uuid)
        .unwrap_or(SqlValue::Null)
}

fn row_uuid(row: &Json, key: &str) -> Option<uuid::Uuid> {
    row.get(key)
        .and_then(Json::as_str)
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
}

fn move_copy_args(body: &Bytes) -> Result<(String, String, String), SupaError> {
    let obj = json_object(body)?;
    let get = |k: &str| {
        obj.get(k)
            .and_then(Json::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    let bucket =
        get("bucketId").ok_or_else(|| SupaError::BadRequest("bucketId is required".into()))?;
    let source =
        get("sourceKey").ok_or_else(|| SupaError::BadRequest("sourceKey is required".into()))?;
    let dest = get("destinationKey")
        .ok_or_else(|| SupaError::BadRequest("destinationKey is required".into()))?;
    Ok((
        bucket,
        normalize_object_name(&source)?,
        normalize_object_name(&dest)?,
    ))
}

fn json_object(body: &Bytes) -> Result<Map<String, Json>, SupaError> {
    if body.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(o)) => Ok(o),
        Ok(_) => Err(SupaError::BadRequest("body must be a JSON object".into())),
        Err(e) => Err(SupaError::BadRequest(format!("invalid JSON body: {e}"))),
    }
}

/// Normalize an object key: strip leading slashes, reject empty / traversal
/// segments. Keys are only ever bound as SQL parameters, so this is a shape
/// check, not an injection defense.
fn normalize_object_name(path: &str) -> Result<String, SupaError> {
    let name = path.trim_start_matches('/');
    if name.is_empty() {
        return Err(SupaError::BadRequest("object key must not be empty".into()));
    }
    if name
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(SupaError::BadRequest(format!(
            "invalid object key: {name:?}"
        )));
    }
    Ok(name.to_string())
}

/// Does `content_type` match the bucket's allowed list (exact, `type/*`, or
/// `*/*`)?
fn mime_allowed(content_type: &str, allowed: &[Json]) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    allowed.iter().filter_map(Json::as_str).any(|a| {
        a == "*/*"
            || a.eq_ignore_ascii_case(ct)
            || a.strip_suffix("/*").is_some_and(|prefix| {
                ct.to_ascii_lowercase()
                    .starts_with(&format!("{}/", prefix.to_ascii_lowercase()))
            })
    })
}

/// Minimal percent-encoding for path segments inside a generated signed URL.
fn percent_encode_path(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn scalar_i64(result: &ExecResult) -> i64 {
    match result {
        ExecResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        ExecResult::Command { .. } => 0,
    }
}

/// Render an [`ExecResult`]'s rows as JSON objects (empty for command tags).
fn result_objects(result: ExecResult) -> Result<Vec<Json>, SupaError> {
    match result {
        ExecResult::Rows { fields, rows } => Ok(rows_to_objects(&fields, &rows)),
        ExecResult::Command { .. } => Ok(Vec::new()),
    }
}

pub(crate) fn rows_to_objects(fields: &[OutField], rows: &[Vec<SqlValue>]) -> Vec<Json> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (f, val) in fields.iter().zip(row.iter()) {
                obj.insert(f.name.clone(), value_to_json(val));
            }
            Json::Object(obj)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_name_normalization() {
        assert_eq!(normalize_object_name("a/b.txt").unwrap(), "a/b.txt");
        assert_eq!(normalize_object_name("/a.txt").unwrap(), "a.txt");
        assert!(normalize_object_name("").is_err());
        assert!(normalize_object_name("a//b").is_err());
        assert!(normalize_object_name("a/../b").is_err());
        assert!(normalize_object_name("./a").is_err());
    }

    #[test]
    fn mime_matching() {
        let allowed = vec![json!("image/png"), json!("text/*")];
        assert!(mime_allowed("image/png", &allowed));
        assert!(mime_allowed("text/plain", &allowed));
        assert!(mime_allowed("text/plain; charset=utf-8", &allowed));
        assert!(!mime_allowed("application/json", &allowed));
        assert!(mime_allowed("anything/at-all", &[json!("*/*")]));
    }

    #[test]
    fn signed_path_encoding() {
        assert_eq!(percent_encode_path("a/b c.txt"), "a/b%20c.txt");
        assert_eq!(percent_encode_path("plain.txt"), "plain.txt");
    }
}
