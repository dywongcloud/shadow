//! postgres-meta-compatible API — the surface Supabase Studio talks to.
//!
//! Mounted at `/pg-meta` (and aliased at `/platform/pg-meta`), gated on the
//! **service_role** key (Studio always uses the service key). Every endpoint
//! mirrors the response keys of `github.com/supabase/postgres-meta`, derived
//! from the engine's own catalog: the structural endpoints (`/tables`,
//! `/columns`, `/policies`, ...) read the persisted [`Catalog`]; `/types`,
//! `/roles` and `/extensions` query the engine's `pg_catalog` views
//! (`pg_type`, `pg_roles`, `pg_available_extensions`) through a
//! `service_role` session — no new engine code.
//!
//! Honesty notes: the engine has no user-defined functions or triggers, so
//! `/functions` and `/triggers` return empty arrays (not errors, not fakes);
//! `bytes`/`live_rows_estimate` on `/tables` are reported as `0` (the engine
//! does not track relation statistics); `/roles` returns the engine owner from
//! `pg_roles` plus the three gateway roles (`anon`, `authenticated`,
//! `service_role`) that the JWT layer actually resolves.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json as AxumJson};
use serde_json::{Value as Json, json};

use crate::relational::Catalog;
use crate::relational::catalog::Table;
use crate::sql::engine::Session;
use crate::sql::{ExecResult, RelationalStorage};
use crate::supabase::error::SupaError;
use crate::supabase::gateway::{AppState, AuthContext, load_catalog, run_sql};
use crate::supabase::rest::parse_query_pairs;
use crate::supabase::storage::rows_to_objects;

/// The pg-meta subrouter (mounted at `/pg-meta` and `/platform/pg-meta`,
/// behind the apikey middleware; every handler additionally requires
/// `service_role`).
pub fn router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/schemas", get(schemas::<S>))
        .route("/tables", get(tables::<S>))
        .route("/columns", get(columns::<S>))
        .route("/indexes", get(indexes::<S>))
        .route("/constraints", get(constraints::<S>))
        .route("/functions", get(empty_list::<S>))
        .route("/triggers", get(empty_list::<S>))
        .route("/extensions", get(extensions::<S>))
        .route("/roles", get(roles::<S>))
        .route("/policies", get(policies::<S>))
        .route("/types", get(types::<S>))
        .route("/views", get(views::<S>))
        .route("/query", post(query::<S>))
        .fallback(unsupported_route)
}

/// Typed catch-all for pg-meta paths this slice does not implement.
async fn unsupported_route() -> Response {
    let body = json!({
        "code": "SUPA_COMPAT_PG_META_UNSUPPORTED_ROUTE",
        "message": "this postgres-meta route is not implemented in the GuardianDB compatibility slice",
    });
    (StatusCode::NOT_FOUND, AxumJson(body)).into_response()
}

/// Every pg-meta endpoint requires the service_role key (Studio's key).
fn require_service(auth: &AuthContext) -> Result<(), SupaError> {
    if auth.is_service_role() {
        Ok(())
    } else {
        Err(SupaError::Forbidden("the pg-meta API"))
    }
}

async fn run<F>(fut: F) -> Response
where
    F: Future<Output = Result<Response, SupaError>>,
{
    fut.await.unwrap_or_else(|e| e.into_response())
}

/// Schema filters postgres-meta accepts (`included_schemas` /
/// `excluded_schemas`, comma-separated).
struct SchemaFilter {
    included: Option<Vec<String>>,
    excluded: Vec<String>,
}

impl SchemaFilter {
    fn parse(query: Option<&str>) -> Self {
        let mut included = None;
        let mut excluded = Vec::new();
        for (k, v) in parse_query_pairs(query.unwrap_or("")) {
            let list = || {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            };
            match k.as_str() {
                "included_schemas" => included = Some(list()),
                "excluded_schemas" => excluded = list(),
                _ => {}
            }
        }
        Self { included, excluded }
    }

    fn allows(&self, schema: &str) -> bool {
        if self.excluded.iter().any(|s| s == schema) {
            return false;
        }
        match &self.included {
            Some(list) => list.iter().any(|s| s == schema),
            None => true,
        }
    }
}

async fn catalog_or_default<S: RelationalStorage + 'static>(
    state: &AppState<S>,
) -> Result<Catalog, SupaError> {
    Ok(load_catalog(&state.db)
        .await?
        .unwrap_or_else(|| Catalog::new(&state.db.name)))
}

// ---------------------------------------------------------------------------
// Structural endpoints (from the persisted catalog)
// ---------------------------------------------------------------------------

async fn schemas<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let out: Vec<Json> = catalog
            .schemas()
            .filter(|s| filter.allows(&s.name))
            .map(|s| json!({ "id": s.oid, "name": s.name, "owner": s.owner }))
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn tables<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let out: Vec<Json> = catalog
            .tables()
            .filter(|t| filter.allows(&t.schema))
            .map(|t| table_json(&catalog, t))
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

fn table_json(catalog: &Catalog, t: &Table) -> Json {
    let columns: Vec<Json> = t
        .columns
        .iter()
        .map(|c| column_json(t, c.ordinal, c))
        .collect();
    let primary_keys: Vec<Json> = t
        .pk_columns()
        .into_iter()
        .map(|name| {
            json!({
                "schema": t.schema,
                "table_name": t.name,
                "name": name,
                "table_id": t.oid,
            })
        })
        .collect();
    let mut relationships = Vec::new();
    for fk in &t.foreign_keys {
        for (i, col) in fk.columns.iter().enumerate() {
            relationships.push(json!({
                "id": t.oid,
                "constraint_name": fk.name,
                "source_schema": t.schema,
                "source_table_name": t.name,
                "source_column_name": col,
                "target_table_schema": fk.ref_schema,
                "target_table_name": fk.ref_table,
                "target_column_name": fk.ref_columns.get(i).cloned().unwrap_or_default(),
            }));
        }
    }
    // FKs from other tables pointing at this one are also part of the
    // postgres-meta relationships array.
    for other in catalog.tables() {
        if other.oid == t.oid {
            continue;
        }
        for fk in &other.foreign_keys {
            if fk.ref_schema == t.schema && fk.ref_table == t.name {
                for (i, col) in fk.columns.iter().enumerate() {
                    relationships.push(json!({
                        "id": other.oid,
                        "constraint_name": fk.name,
                        "source_schema": other.schema,
                        "source_table_name": other.name,
                        "source_column_name": col,
                        "target_table_schema": t.schema,
                        "target_table_name": t.name,
                        "target_column_name": fk.ref_columns.get(i).cloned().unwrap_or_default(),
                    }));
                }
            }
        }
    }
    json!({
        "id": t.oid,
        "schema": t.schema,
        "name": t.name,
        "rls_enabled": t.rls_enabled,
        "rls_forced": t.rls_forced,
        "replica_identity": "DEFAULT",
        "bytes": 0,
        "size": "0 bytes",
        "live_rows_estimate": 0,
        "dead_rows_estimate": 0,
        "comment": Json::Null,
        "columns": columns,
        "primary_keys": primary_keys,
        "relationships": relationships,
    })
}

fn column_json(t: &Table, ordinal: usize, c: &crate::relational::catalog::Column) -> Json {
    let is_unique = t
        .uniques
        .iter()
        .any(|u| u.columns.len() == 1 && u.columns[0] == c.name);
    json!({
        "id": format!("{}.{}", t.oid, ordinal + 1),
        "table_id": t.oid,
        "schema": t.schema,
        "table": t.name,
        "name": c.name,
        "ordinal_position": ordinal + 1,
        "data_type": c.ty.name(),
        "format": c.ty.udt_name(),
        "default_value": c.default.clone().map(Json::String).unwrap_or(Json::Null),
        "is_identity": c.identity_sequence.is_some(),
        "identity_generation": Json::Null,
        "is_generated": false,
        "is_nullable": c.nullable,
        "is_updatable": true,
        "is_unique": is_unique,
        "enums": Json::Array(vec![]),
        "check": Json::Null,
        "comment": Json::Null,
    })
}

async fn columns<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let out: Vec<Json> = catalog
            .tables()
            .filter(|t| filter.allows(&t.schema))
            .flat_map(|t| t.columns.iter().map(move |c| column_json(t, c.ordinal, c)))
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn indexes<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let out: Vec<Json> = catalog
            .indexes()
            .filter(|i| filter.allows(&i.schema))
            .map(|i| {
                let cols = i
                    .columns
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let unique = if i.unique { "UNIQUE " } else { "" };
                json!({
                    "id": i.oid,
                    "schema": i.schema,
                    "table": i.table,
                    "name": i.name,
                    "columns": i.columns,
                    "is_unique": i.unique,
                    "is_primary": i.primary,
                    "index_definition": format!(
                        "CREATE {unique}INDEX \"{}\" ON \"{}\".\"{}\" USING {} ({cols})",
                        i.name, i.schema, i.table, i.method
                    ),
                    "access_method": i.method,
                })
            })
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn constraints<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let mut out = Vec::new();
        let mut id = 30_000u32;
        let mut push = |t: &Table, name: &str, ctype: &str, definition: String| {
            out.push(json!({
                "id": id,
                "name": name,
                "schema": t.schema,
                "table": t.name,
                "table_id": t.oid,
                "type": ctype,
                "definition": definition,
            }));
            id += 1;
        };
        for t in catalog.tables().filter(|t| filter.allows(&t.schema)) {
            if let Some(pk) = &t.primary_key {
                push(
                    t,
                    &pk.name,
                    "p",
                    format!("PRIMARY KEY ({})", pk.columns.join(", ")),
                );
            }
            for u in &t.uniques {
                push(
                    t,
                    &u.name,
                    "u",
                    format!("UNIQUE ({})", u.columns.join(", ")),
                );
            }
            for fk in &t.foreign_keys {
                push(
                    t,
                    &fk.name,
                    "f",
                    format!(
                        "FOREIGN KEY ({}) REFERENCES {}.{} ({}) ON UPDATE {} ON DELETE {}",
                        fk.columns.join(", "),
                        fk.ref_schema,
                        fk.ref_table,
                        fk.ref_columns.join(", "),
                        fk.on_update.as_sql(),
                        fk.on_delete.as_sql(),
                    ),
                );
            }
            for c in &t.checks {
                push(t, &c.name, "c", format!("CHECK ({})", c.expr));
            }
        }
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn policies<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let mut out = Vec::new();
        let mut id = 40_000u32;
        for t in catalog.tables().filter(|t| filter.allows(&t.schema)) {
            for p in &t.policies {
                let roles: Vec<String> = if p.roles.is_empty() {
                    vec!["public".to_string()]
                } else {
                    p.roles.clone()
                };
                out.push(json!({
                    "id": id,
                    "schema": t.schema,
                    "table": t.name,
                    "table_id": t.oid,
                    "name": p.name,
                    "action": if p.permissive { "PERMISSIVE" } else { "RESTRICTIVE" },
                    "roles": roles,
                    "command": p.cmd.as_sql(),
                    "definition": p.using_expr.clone().map(Json::String).unwrap_or(Json::Null),
                    "check": p.check_expr.clone().map(Json::String).unwrap_or(Json::Null),
                }));
                id += 1;
            }
        }
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn views<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        require_service(&auth)?;
        let catalog = catalog_or_default(&state).await?;
        let filter = SchemaFilter::parse(query.as_deref());
        let out: Vec<Json> = catalog
            .views()
            .filter(|v| filter.allows(&v.schema))
            .map(|v| {
                json!({
                    "id": v.oid,
                    "schema": v.schema,
                    "name": v.name,
                    "definition": v.query,
                    "is_updatable": false,
                    "comment": Json::Null,
                })
            })
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn empty_list<S: RelationalStorage + 'static>(
    State(_state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        require_service(&auth)?;
        // The engine has no user-defined functions or triggers; an empty array
        // is the honest postgres-meta answer.
        Ok(AxumJson(Json::Array(vec![])).into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// Catalog-view-backed endpoints (through a service_role session)
// ---------------------------------------------------------------------------

async fn extensions<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        require_service(&auth)?;
        let result = run_sql(
            &state.db,
            "service_role",
            "SELECT name, default_version, installed_version, runtime, comment \
             FROM pg_available_extensions ORDER BY name",
            vec![],
        )
        .await
        .map_err(SupaError::Sql)?;
        let rows = expect_objects(result)?;
        let out: Vec<Json> = rows
            .into_iter()
            .map(|r| {
                let installed = !r
                    .get("installed_version")
                    .cloned()
                    .unwrap_or(Json::Null)
                    .is_null();
                json!({
                    "name": r.get("name").cloned().unwrap_or(Json::Null),
                    "schema": if installed { json!("pg_catalog") } else { Json::Null },
                    "default_version": r.get("default_version").cloned().unwrap_or(Json::Null),
                    "installed_version": r.get("installed_version").cloned().unwrap_or(Json::Null),
                    // GuardianDB extra: where the extension executes.
                    "runtime": r.get("runtime").cloned().unwrap_or(Json::Null),
                    "comment": r.get("comment").cloned().unwrap_or(Json::Null),
                })
            })
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

async fn roles<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        require_service(&auth)?;
        let result = run_sql(
            &state.db,
            "service_role",
            "SELECT oid, rolname, rolsuper, rolcanlogin FROM pg_roles ORDER BY oid",
            vec![],
        )
        .await
        .map_err(SupaError::Sql)?;
        let mut out: Vec<Json> = expect_objects(result)?
            .into_iter()
            .map(|r| {
                role_json(
                    r.get("oid").and_then(Json::as_i64).unwrap_or(0),
                    r.get("rolname").and_then(Json::as_str).unwrap_or(""),
                    r.get("rolsuper").and_then(Json::as_bool).unwrap_or(false),
                    r.get("rolcanlogin")
                        .and_then(Json::as_bool)
                        .unwrap_or(false),
                    r.get("rolsuper").and_then(Json::as_bool).unwrap_or(false),
                )
            })
            .collect();
        // The gateway's JWT layer resolves these three roles; they are as real
        // as roles get in this engine, so Studio should see them.
        out.push(role_json(16379, "anon", false, false, false));
        out.push(role_json(16380, "authenticated", false, false, false));
        out.push(role_json(16381, "service_role", false, false, true));
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

fn role_json(id: i64, name: &str, superuser: bool, can_login: bool, bypass_rls: bool) -> Json {
    json!({
        "id": id,
        "name": name,
        "is_superuser": superuser,
        "can_create_db": superuser,
        "can_create_role": superuser,
        "inherit_role": true,
        "can_login": can_login,
        "is_replication_role": false,
        "can_bypass_rls": bypass_rls,
        "active_connections": 0,
        "connection_limit": -1,
        "password": "********",
        "valid_until": Json::Null,
        "config": Json::Null,
    })
}

async fn types<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    run(async {
        require_service(&auth)?;
        let result = run_sql(
            &state.db,
            "service_role",
            "SELECT oid, typname FROM pg_type ORDER BY oid",
            vec![],
        )
        .await
        .map_err(SupaError::Sql)?;
        let out: Vec<Json> = expect_objects(result)?
            .into_iter()
            .map(|r| {
                json!({
                    "id": r.get("oid").cloned().unwrap_or(Json::Null),
                    "name": r.get("typname").cloned().unwrap_or(Json::Null),
                    "schema": "pg_catalog",
                    "format": r.get("typname").cloned().unwrap_or(Json::Null),
                    "enums": Json::Array(vec![]),
                    "attributes": Json::Array(vec![]),
                    "comment": Json::Null,
                })
            })
            .collect();
        Ok(AxumJson(Json::Array(out)).into_response())
    })
    .await
}

// ---------------------------------------------------------------------------
// POST /query — Studio's SQL editor path
// ---------------------------------------------------------------------------

async fn query<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    run(async {
        require_service(&auth)?;
        let parsed: Json = serde_json::from_slice(&body)
            .map_err(|e| SupaError::BadRequest(format!("invalid JSON body: {e}")))?;
        let Some(sql) = parsed.get("query").and_then(Json::as_str) else {
            return Err(SupaError::BadRequest(
                "body must be a JSON object with a \"query\" string".into(),
            ));
        };
        // Studio runs as the service key; the session matches (RLS bypass).
        let mut session = Session::new(state.db.clone(), "service_role".to_string());
        match session.execute(sql).await {
            Ok(results) => {
                // postgres-meta returns the rows of the (last) statement; DDL
                // and DML without RETURNING produce an empty array.
                let rows = match results.into_iter().last() {
                    Some(ExecResult::Rows { fields, rows }) => rows_to_objects(&fields, &rows),
                    _ => Vec::new(),
                };
                Ok(AxumJson(Json::Array(rows)).into_response())
            }
            Err(e) => {
                let body = json!({
                    "error": {
                        "message": e.to_string(),
                        "code": e.sqlstate(),
                    }
                });
                Ok((StatusCode::BAD_REQUEST, AxumJson(body)).into_response())
            }
        }
    })
    .await
}

fn expect_objects(result: ExecResult) -> Result<Vec<Json>, SupaError> {
    match result {
        ExecResult::Rows { fields, rows } => Ok(rows_to_objects(&fields, &rows)),
        ExecResult::Command { tag } => Err(SupaError::Internal(format!(
            "expected rows but got a command tag: {tag}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_filter_parsing() {
        let f = SchemaFilter::parse(Some("included_schemas=public,auth"));
        assert!(f.allows("public"));
        assert!(f.allows("auth"));
        assert!(!f.allows("storage"));

        let f = SchemaFilter::parse(Some("excluded_schemas=storage"));
        assert!(f.allows("public"));
        assert!(!f.allows("storage"));

        let f = SchemaFilter::parse(None);
        assert!(f.allows("anything"));
    }
}
