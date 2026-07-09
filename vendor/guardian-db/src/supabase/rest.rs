//! PostgREST-compatible REST over the SQL engine.
//!
//! Translates PostgREST query syntax (`select=`, `col=op.value` filters,
//! `order=`, `limit`/`offset`, the `Range` header, `Prefer:` directives) into
//! parameterised SQL run through a per-request [`Session`](crate::sql::engine::Session)
//! bound to the caller's role. All literal values are bound as `$n` parameters —
//! identifiers are validated against `[A-Za-z_][A-Za-z0-9_]*` — so no request
//! input is ever interpolated into SQL text.
//!
//! Results render as PostgREST JSON: an array of objects, or a single object
//! with `Accept: application/vnd.pgrst.object+json`. Errors render in PostgREST
//! shape (`{code,message,details,hint}`) with the SQLSTATE as `code`.

use std::collections::BTreeSet;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json as AxumJson};
use serde_json::{Map, Value as Json};

use crate::relational::catalog::Table;
use crate::sql::{ExecResult, OutField, RelationalStorage, SqlType, SqlValue};
use crate::supabase::error::SupaError;
use crate::supabase::gateway::{AppState, AuthContext, header_str, load_catalog, run_sql_as};

/// The REST subrouter mounted at `/rest/v1`.
pub fn router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route(
            "/{table}",
            get(rest_get::<S>)
                .post(rest_post::<S>)
                .patch(rest_patch::<S>)
                .delete(rest_delete::<S>),
        )
        .route("/rpc/{name}", get(rest_rpc::<S>).post(rest_rpc::<S>))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn rest_get<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(table): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    do_select(
        &state,
        &auth,
        &table,
        query.as_deref().unwrap_or(""),
        &headers,
    )
    .await
    .unwrap_or_else(|e| e.into_response())
}

async fn rest_post<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(table): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    do_insert(
        &state,
        &auth,
        &table,
        query.as_deref().unwrap_or(""),
        &headers,
        &body,
    )
    .await
    .unwrap_or_else(|e| e.into_response())
}

async fn rest_patch<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(table): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    do_update(
        &state,
        &auth,
        &table,
        query.as_deref().unwrap_or(""),
        &headers,
        &body,
    )
    .await
    .unwrap_or_else(|e| e.into_response())
}

async fn rest_delete<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(table): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    do_delete(
        &state,
        &auth,
        &table,
        query.as_deref().unwrap_or(""),
        &headers,
    )
    .await
    .unwrap_or_else(|e| e.into_response())
}

async fn rest_rpc<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    do_rpc(&state, &auth, &name, &headers, &body)
        .await
        .unwrap_or_else(|e| e.into_response())
}

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

async fn do_select<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    table: &str,
    query: &str,
    headers: &HeaderMap,
) -> Result<Response, SupaError> {
    let schema = request_schema(headers, false);
    validate_ident(table, "table")?;
    validate_ident(&schema, "schema")?;

    let rq = RestQuery::parse(query)?;
    let prefer = Prefer::parse(headers);
    let catalog = load_catalog(&state.db).await?;
    let table_def = catalog
        .as_ref()
        .and_then(|c| c.resolve_table_name(Some(&schema), table).map(|q| (c, q)))
        .and_then(|(c, q)| c.get_table(&q).cloned());

    let cols = build_select_list(&rq.select)?;
    let mut buf = SqlBuf::new();
    let where_sql = build_where(&mut buf, &rq.filters, table_def.as_ref())?;
    let order_sql = build_order(&rq.order)?;

    // Range header widens into limit/offset when the explicit params are absent.
    let (limit, offset) = resolve_window(&rq, headers);

    let mut sql = format!("SELECT {cols} FROM \"{schema}\".\"{table}\"{where_sql}{order_sql}");
    if let Some(l) = limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    if let Some(o) = offset.filter(|o| *o > 0) {
        sql.push_str(&format!(" OFFSET {o}"));
    }

    let result = run_sql_as(&state.db, auth, &sql, buf.params)
        .await
        .map_err(SupaError::Sql)?;
    let (fields, rows) = expect_rows(result)?;
    let objects = rows_to_json(&fields, &rows);

    // Content-Range: `start-end/total` (or `/*` without an exact count).
    let start = offset.unwrap_or(0);
    let end = if rows.is_empty() {
        start.saturating_sub(1)
    } else {
        start + rows.len() - 1
    };
    let total = if prefer.count_exact {
        Some(count_exact(state, auth, &schema, table, &rq, table_def.as_ref()).await?)
    } else {
        None
    };
    let content_range = match total {
        Some(t) if rows.is_empty() => format!("*/{t}"),
        Some(t) => format!("{start}-{end}/{t}"),
        None if rows.is_empty() => "*/*".to_string(),
        None => format!("{start}-{end}/*"),
    };

    let status = if prefer.count_exact {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    render_rows(objects, wants_single(headers), status, Some(content_range))
}

async fn count_exact<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    schema: &str,
    table: &str,
    rq: &RestQuery,
    table_def: Option<&Table>,
) -> Result<i64, SupaError> {
    let mut buf = SqlBuf::new();
    let where_sql = build_where(&mut buf, &rq.filters, table_def)?;
    let sql = format!("SELECT count(*) AS c FROM \"{schema}\".\"{table}\"{where_sql}");
    let result = run_sql_as(&state.db, auth, &sql, buf.params)
        .await
        .map_err(SupaError::Sql)?;
    let (_, rows) = expect_rows(result)?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| v.as_i64())
        .unwrap_or(0))
}

// ---------------------------------------------------------------------------
// INSERT / UPSERT
// ---------------------------------------------------------------------------

async fn do_insert<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    table: &str,
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, SupaError> {
    let schema = request_schema(headers, true);
    validate_ident(table, "table")?;
    validate_ident(&schema, "schema")?;
    let rq = RestQuery::parse(query)?;
    let prefer = Prefer::parse(headers);

    let value: Json = serde_json::from_slice(body)
        .map_err(|e| SupaError::BadRequest(format!("invalid JSON body: {e}")))?;
    let rows: Vec<Map<String, Json>> = match value {
        Json::Array(arr) => arr.into_iter().map(as_object).collect::<Result<_, _>>()?,
        Json::Object(o) => vec![o],
        _ => {
            return Err(SupaError::BadRequest(
                "request body must be a JSON object or array of objects".into(),
            ));
        }
    };
    if rows.is_empty() {
        // Nothing to insert; PostgREST returns an empty representation.
        return render_rows(Vec::new(), false, StatusCode::CREATED, None);
    }

    let catalog = load_catalog(&state.db).await?;
    let table_def = catalog
        .as_ref()
        .and_then(|c| c.resolve_table_name(Some(&schema), table).map(|q| (c, q)))
        .and_then(|(c, q)| c.get_table(&q).cloned());

    // Column set = union of all object keys, in a stable order.
    let mut column_set: BTreeSet<String> = BTreeSet::new();
    for row in &rows {
        column_set.extend(row.keys().cloned());
    }
    let columns: Vec<String> = column_set.into_iter().collect();
    if columns.is_empty() {
        return Err(SupaError::BadRequest("no columns to insert".into()));
    }
    for c in &columns {
        validate_ident(c, "column")?;
    }

    let mut buf = SqlBuf::new();
    let mut tuples = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut cells = Vec::with_capacity(columns.len());
        for col in &columns {
            match row.get(col) {
                Some(v) => {
                    let sv = json_to_sqlvalue(v, col_type(table_def.as_ref(), col));
                    cells.push(buf.bind(sv));
                }
                None => cells.push("DEFAULT".to_string()),
            }
        }
        tuples.push(format!("({})", cells.join(", ")));
    }

    let column_list = columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let on_conflict = build_on_conflict(&prefer, &rq.on_conflict, &columns, table_def.as_ref())?;
    let returning = if prefer.return_repr {
        " RETURNING *"
    } else {
        ""
    };

    let sql = format!(
        "INSERT INTO \"{schema}\".\"{table}\" ({column_list}) VALUES {}{on_conflict}{returning}",
        tuples.join(", ")
    );
    let result = run_sql_as(&state.db, auth, &sql, buf.params)
        .await
        .map_err(SupaError::Sql)?;

    if prefer.return_repr {
        let (fields, rows) = expect_rows(result)?;
        let objects = rows_to_json(&fields, &rows);
        render_rows(objects, wants_single(headers), StatusCode::CREATED, None)
    } else {
        Ok(empty(StatusCode::CREATED))
    }
}

fn build_on_conflict(
    prefer: &Prefer,
    on_conflict: &[String],
    columns: &[String],
    table_def: Option<&Table>,
) -> Result<String, SupaError> {
    match prefer.resolution {
        None => Ok(String::new()),
        Some(Resolution::IgnoreDuplicates) => Ok(" ON CONFLICT DO NOTHING".to_string()),
        Some(Resolution::MergeDuplicates) => {
            let targets: Vec<String> = if !on_conflict.is_empty() {
                on_conflict.to_vec()
            } else {
                table_def.map(|t| t.pk_columns()).unwrap_or_default()
            };
            if targets.is_empty() {
                return Err(SupaError::BadRequest(
                    "upsert (resolution=merge-duplicates) requires on_conflict= or a primary key"
                        .into(),
                ));
            }
            for c in &targets {
                validate_ident(c, "on_conflict column")?;
            }
            let target_list = targets
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let sets: Vec<String> = columns
                .iter()
                .filter(|c| !targets.contains(c))
                .map(|c| format!("\"{c}\" = EXCLUDED.\"{c}\""))
                .collect();
            if sets.is_empty() {
                Ok(format!(" ON CONFLICT ({target_list}) DO NOTHING"))
            } else {
                Ok(format!(
                    " ON CONFLICT ({target_list}) DO UPDATE SET {}",
                    sets.join(", ")
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UPDATE
// ---------------------------------------------------------------------------

async fn do_update<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    table: &str,
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, SupaError> {
    let schema = request_schema(headers, true);
    validate_ident(table, "table")?;
    validate_ident(&schema, "schema")?;
    let rq = RestQuery::parse(query)?;
    let prefer = Prefer::parse(headers);

    let value: Json = serde_json::from_slice(body)
        .map_err(|e| SupaError::BadRequest(format!("invalid JSON body: {e}")))?;
    let obj = match value {
        Json::Object(o) => o,
        _ => {
            return Err(SupaError::BadRequest(
                "PATCH body must be a JSON object".into(),
            ));
        }
    };
    if obj.is_empty() {
        return Err(SupaError::BadRequest(
            "PATCH body must set at least one column".into(),
        ));
    }

    let catalog = load_catalog(&state.db).await?;
    let table_def = catalog
        .as_ref()
        .and_then(|c| c.resolve_table_name(Some(&schema), table).map(|q| (c, q)))
        .and_then(|(c, q)| c.get_table(&q).cloned());

    let mut buf = SqlBuf::new();
    let mut sets = Vec::with_capacity(obj.len());
    for (col, v) in &obj {
        validate_ident(col, "column")?;
        let sv = json_to_sqlvalue(v, col_type(table_def.as_ref(), col));
        sets.push(format!("\"{col}\" = {}", buf.bind(sv)));
    }
    let where_sql = build_where(&mut buf, &rq.filters, table_def.as_ref())?;
    let returning = if prefer.return_repr {
        " RETURNING *"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE \"{schema}\".\"{table}\" SET {}{where_sql}{returning}",
        sets.join(", ")
    );
    let result = run_sql_as(&state.db, auth, &sql, buf.params)
        .await
        .map_err(SupaError::Sql)?;

    if prefer.return_repr {
        let (fields, rows) = expect_rows(result)?;
        let objects = rows_to_json(&fields, &rows);
        render_rows(objects, wants_single(headers), StatusCode::OK, None)
    } else {
        Ok(empty(StatusCode::NO_CONTENT))
    }
}

// ---------------------------------------------------------------------------
// DELETE
// ---------------------------------------------------------------------------

async fn do_delete<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    table: &str,
    query: &str,
    headers: &HeaderMap,
) -> Result<Response, SupaError> {
    let schema = request_schema(headers, true);
    validate_ident(table, "table")?;
    validate_ident(&schema, "schema")?;
    let rq = RestQuery::parse(query)?;
    let prefer = Prefer::parse(headers);

    let catalog = load_catalog(&state.db).await?;
    let table_def = catalog
        .as_ref()
        .and_then(|c| c.resolve_table_name(Some(&schema), table).map(|q| (c, q)))
        .and_then(|(c, q)| c.get_table(&q).cloned());

    let mut buf = SqlBuf::new();
    let where_sql = build_where(&mut buf, &rq.filters, table_def.as_ref())?;
    let returning = if prefer.return_repr {
        " RETURNING *"
    } else {
        ""
    };
    let sql = format!("DELETE FROM \"{schema}\".\"{table}\"{where_sql}{returning}");
    let result = run_sql_as(&state.db, auth, &sql, buf.params)
        .await
        .map_err(SupaError::Sql)?;

    if prefer.return_repr {
        let (fields, rows) = expect_rows(result)?;
        let objects = rows_to_json(&fields, &rows);
        render_rows(objects, wants_single(headers), StatusCode::OK, None)
    } else {
        Ok(empty(StatusCode::NO_CONTENT))
    }
}

// ---------------------------------------------------------------------------
// RPC
// ---------------------------------------------------------------------------

async fn do_rpc<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    name: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, SupaError> {
    validate_ident(name, "function")?;
    // Named args from the JSON body. Positional order is by sorted key name
    // (JSON object keys are unordered); this is the one documented divergence
    // from PostgREST's true named-argument mapping.
    let args: Map<String, Json> = if body.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Json>(body) {
            Ok(Json::Object(o)) => o,
            Ok(Json::Null) => Map::new(),
            Ok(_) => {
                return Err(SupaError::BadRequest(
                    "rpc body must be a JSON object of named arguments".into(),
                ));
            }
            Err(e) => return Err(SupaError::BadRequest(format!("invalid JSON body: {e}"))),
        }
    };

    let mut buf = SqlBuf::new();
    let placeholders: Vec<String> = args
        .values()
        .map(|v| buf.bind(json_to_sqlvalue(v, None)))
        .collect();
    let sql = format!("SELECT {name}({})", placeholders.join(", "));
    let result = run_sql_as(&state.db, auth, &sql, buf.params)
        .await
        .map_err(SupaError::Sql)?;
    let (fields, rows) = expect_rows(result)?;
    let objects = rows_to_json(&fields, &rows);
    render_rows(objects, wants_single(headers), StatusCode::OK, None)
}

// ---------------------------------------------------------------------------
// Query parsing
// ---------------------------------------------------------------------------

/// A parsed PostgREST query string.
#[derive(Debug, Default, Clone)]
pub struct RestQuery {
    pub select: Vec<SelectItem>,
    pub filters: Vec<Filter>,
    pub order: Vec<OrderItem>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub on_conflict: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    pub column: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub negate: bool,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    Like,
    Ilike,
    Is,
    In,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderItem {
    pub column: String,
    pub ascending: bool,
    pub nulls_first: Option<bool>,
}

impl RestQuery {
    pub fn parse(query: &str) -> Result<Self, SupaError> {
        let mut rq = RestQuery::default();
        for (key, value) in parse_query_pairs(query) {
            match key.as_str() {
                "select" => rq.select = parse_select(&value)?,
                "order" => rq.order = parse_order(&value)?,
                "limit" => {
                    rq.limit =
                        Some(value.parse().map_err(|_| {
                            SupaError::BadRequest(format!("invalid limit: {value}"))
                        })?)
                }
                "offset" => {
                    rq.offset =
                        Some(value.parse().map_err(|_| {
                            SupaError::BadRequest(format!("invalid offset: {value}"))
                        })?)
                }
                "on_conflict" => {
                    rq.on_conflict = value.split(',').map(|s| s.trim().to_string()).collect()
                }
                // Logical operator trees are not supported in this slice.
                "and" | "or" | "not" => {
                    return Err(SupaError::UnsupportedFilter(key));
                }
                // Reserved PostgREST keys we accept but do not act on here.
                "columns" => {}
                // Anything else is a column filter `col=op.value`.
                _ => rq.filters.push(parse_filter(key, &value)?),
            }
        }
        Ok(rq)
    }
}

fn parse_select(value: &str) -> Result<Vec<SelectItem>, SupaError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    for raw in value.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.contains('(') || raw.contains(')') {
            return Err(SupaError::BadRequest(
                "embedded resource selects are not supported in this slice".into(),
            ));
        }
        if raw == "*" {
            items.push(SelectItem {
                column: "*".into(),
                alias: None,
            });
            continue;
        }
        // `alias:column` (PostgREST rename syntax).
        let item = match raw.split_once(':') {
            Some((alias, column)) => {
                validate_ident(alias.trim(), "select alias")?;
                validate_ident(column.trim(), "select column")?;
                SelectItem {
                    column: column.trim().to_string(),
                    alias: Some(alias.trim().to_string()),
                }
            }
            None => {
                validate_ident(raw, "select column")?;
                SelectItem {
                    column: raw.to_string(),
                    alias: None,
                }
            }
        };
        items.push(item);
    }
    Ok(items)
}

fn parse_order(value: &str) -> Result<Vec<OrderItem>, SupaError> {
    let mut items = Vec::new();
    for raw in value.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut parts = raw.split('.');
        let column = parts.next().unwrap_or("").trim();
        validate_ident(column, "order column")?;
        let mut ascending = true;
        let mut nulls_first = None;
        for tok in parts {
            match tok.trim() {
                "asc" => ascending = true,
                "desc" => ascending = false,
                "nullsfirst" => nulls_first = Some(true),
                "nullslast" => nulls_first = Some(false),
                other => {
                    return Err(SupaError::BadRequest(format!(
                        "invalid order modifier: {other}"
                    )));
                }
            }
        }
        items.push(OrderItem {
            column: column.to_string(),
            ascending,
            nulls_first,
        });
    }
    Ok(items)
}

fn parse_filter(column: String, value: &str) -> Result<Filter, SupaError> {
    let (negate, rest) = match value.strip_prefix("not.") {
        Some(r) => (true, r),
        None => (false, value),
    };
    let (op_str, operand) = rest
        .split_once('.')
        .ok_or_else(|| SupaError::UnsupportedFilter(rest.to_string()))?;
    let op = match op_str {
        "eq" => FilterOp::Eq,
        "neq" => FilterOp::Neq,
        "gt" => FilterOp::Gt,
        "gte" => FilterOp::Gte,
        "lt" => FilterOp::Lt,
        "lte" => FilterOp::Lte,
        "like" => FilterOp::Like,
        "ilike" => FilterOp::Ilike,
        "is" => FilterOp::Is,
        "in" => FilterOp::In,
        other => return Err(SupaError::UnsupportedFilter(other.to_string())),
    };
    Ok(Filter {
        column,
        op,
        negate,
        value: operand.to_string(),
    })
}

/// Split a query string into ordered `(key, value)` pairs, percent-decoding both.
pub fn parse_query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Prefer header
// ---------------------------------------------------------------------------

/// Parsed `Prefer:` directives that shape the response.
#[derive(Debug, Clone, Copy, Default)]
struct Prefer {
    /// `return=representation` — echo the affected rows back.
    return_repr: bool,
    /// `count=exact` — include a total in `Content-Range`.
    count_exact: bool,
    /// `resolution=merge-duplicates` / `ignore-duplicates` — upsert behaviour.
    resolution: Option<Resolution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    MergeDuplicates,
    IgnoreDuplicates,
}

impl Prefer {
    fn parse(headers: &HeaderMap) -> Self {
        let mut p = Prefer::default();
        for hv in headers.get_all("prefer") {
            let Ok(s) = hv.to_str() else { continue };
            for part in s.split(',') {
                match part.trim() {
                    "return=representation" => p.return_repr = true,
                    "return=minimal" | "return=headers-only" => p.return_repr = false,
                    "count=exact" => p.count_exact = true,
                    "resolution=merge-duplicates" => {
                        p.resolution = Some(Resolution::MergeDuplicates)
                    }
                    "resolution=ignore-duplicates" => {
                        p.resolution = Some(Resolution::IgnoreDuplicates)
                    }
                    _ => {}
                }
            }
        }
        p
    }
}

// ---------------------------------------------------------------------------
// SQL construction
// ---------------------------------------------------------------------------

/// Accumulates bound parameters and hands out `$n` placeholders.
struct SqlBuf {
    params: Vec<SqlValue>,
}

impl SqlBuf {
    fn new() -> Self {
        Self { params: Vec::new() }
    }

    fn bind(&mut self, value: SqlValue) -> String {
        self.params.push(value);
        format!("${}", self.params.len())
    }
}

fn build_select_list(items: &[SelectItem]) -> Result<String, SupaError> {
    if items.is_empty() {
        return Ok("*".to_string());
    }
    let mut cols = Vec::with_capacity(items.len());
    for it in items {
        if it.column == "*" {
            cols.push("*".to_string());
            continue;
        }
        match &it.alias {
            Some(alias) => cols.push(format!("\"{}\" AS \"{}\"", it.column, alias)),
            None => cols.push(format!("\"{}\"", it.column)),
        }
    }
    Ok(cols.join(", "))
}

fn build_where(
    buf: &mut SqlBuf,
    filters: &[Filter],
    table_def: Option<&Table>,
) -> Result<String, SupaError> {
    if filters.is_empty() {
        return Ok(String::new());
    }
    let mut clauses = Vec::with_capacity(filters.len());
    for f in filters {
        validate_ident(&f.column, "filter column")?;
        let col = format!("\"{}\"", f.column);
        let ty = col_type(table_def, &f.column);
        let clause = match f.op {
            FilterOp::Eq => format!("{col} = {}", buf.bind(coerce(&f.value, ty))),
            FilterOp::Neq => format!("{col} <> {}", buf.bind(coerce(&f.value, ty))),
            FilterOp::Gt => format!("{col} > {}", buf.bind(coerce(&f.value, ty))),
            FilterOp::Gte => format!("{col} >= {}", buf.bind(coerce(&f.value, ty))),
            FilterOp::Lt => format!("{col} < {}", buf.bind(coerce(&f.value, ty))),
            FilterOp::Lte => format!("{col} <= {}", buf.bind(coerce(&f.value, ty))),
            FilterOp::Like => {
                let pat = f.value.replace('*', "%");
                format!("{col} LIKE {}", buf.bind(SqlValue::Text(pat)))
            }
            FilterOp::Ilike => {
                let pat = f.value.replace('*', "%");
                format!("{col} ILIKE {}", buf.bind(SqlValue::Text(pat)))
            }
            FilterOp::Is => match f.value.to_ascii_lowercase().as_str() {
                "null" | "unknown" => format!("{col} IS NULL"),
                "true" => format!("{col} IS TRUE"),
                "false" => format!("{col} IS FALSE"),
                other => {
                    return Err(SupaError::BadRequest(format!(
                        "is.{other}: expected null, true, false, or unknown"
                    )));
                }
            },
            FilterOp::In => {
                let items = parse_in_list(&f.value);
                if items.is_empty() {
                    // `col IN ()` is an empty set → always false.
                    "FALSE".to_string()
                } else {
                    let binds: Vec<String> =
                        items.iter().map(|v| buf.bind(coerce(v, ty))).collect();
                    format!("{col} IN ({})", binds.join(", "))
                }
            }
        };
        clauses.push(if f.negate {
            format!("NOT ({clause})")
        } else {
            clause
        });
    }
    Ok(format!(" WHERE {}", clauses.join(" AND ")))
}

fn build_order(items: &[OrderItem]) -> Result<String, SupaError> {
    if items.is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::with_capacity(items.len());
    for it in items {
        let dir = if it.ascending { "ASC" } else { "DESC" };
        let nulls = match it.nulls_first {
            Some(true) => " NULLS FIRST",
            Some(false) => " NULLS LAST",
            None => "",
        };
        parts.push(format!("\"{}\" {dir}{nulls}", it.column));
    }
    Ok(format!(" ORDER BY {}", parts.join(", ")))
}

/// Parse a PostgREST `in.(a,b,"c,d")` operand into its elements.
fn parse_in_list(operand: &str) -> Vec<String> {
    let inner = operand
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(operand.trim());
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() || !out.is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

/// Coerce a raw string operand to the column's declared type, falling back to
/// value inference when the type is unknown or the text does not parse.
fn coerce(raw: &str, ty: Option<&SqlType>) -> SqlValue {
    match ty {
        Some(t) => SqlValue::from_text(raw, t).unwrap_or_else(|_| infer_value(raw)),
        None => infer_value(raw),
    }
}

fn infer_value(raw: &str) -> SqlValue {
    if let Ok(i) = raw.parse::<i64>() {
        SqlValue::Int8(i)
    } else if let Ok(f) = raw.parse::<f64>() {
        SqlValue::Float8(f)
    } else if raw.eq_ignore_ascii_case("true") {
        SqlValue::Bool(true)
    } else if raw.eq_ignore_ascii_case("false") {
        SqlValue::Bool(false)
    } else {
        SqlValue::Text(raw.to_string())
    }
}

/// Convert a JSON body value to a typed [`SqlValue`] for the target column.
fn json_to_sqlvalue(v: &Json, ty: Option<&SqlType>) -> SqlValue {
    if v.is_null() {
        return SqlValue::Null;
    }
    match ty {
        Some(t) => SqlValue::decode_json(v, t).unwrap_or_else(|_| json_default(v)),
        None => json_default(v),
    }
}

fn json_default(v: &Json) -> SqlValue {
    match v {
        Json::Null => SqlValue::Null,
        Json::Bool(b) => SqlValue::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Int8(i)
            } else {
                SqlValue::Float8(n.as_f64().unwrap_or(0.0))
            }
        }
        Json::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Json(other.clone()),
    }
}

fn col_type<'a>(table_def: Option<&'a Table>, column: &str) -> Option<&'a SqlType> {
    table_def.and_then(|t| t.column(column)).map(|c| &c.ty)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render a single [`SqlValue`] as natural PostgREST JSON.
pub fn value_to_json(v: &SqlValue) -> Json {
    match v {
        SqlValue::Null => Json::Null,
        SqlValue::Bool(b) => Json::Bool(*b),
        SqlValue::Int2(n) => Json::from(*n),
        SqlValue::Int4(n) => Json::from(*n),
        SqlValue::Int8(n) => Json::from(*n),
        SqlValue::Float4(n) => serde_json::Number::from_f64(*n as f64)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        SqlValue::Float8(n) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        SqlValue::Numeric(d) => {
            // PostgREST renders numeric as a JSON number when possible.
            serde_json::from_str::<Json>(&d.normalize().to_string())
                .unwrap_or_else(|_| Json::String(d.normalize().to_string()))
        }
        SqlValue::Text(s) | SqlValue::Citext(s) => Json::String(s.clone()),
        SqlValue::Bytea(_) => Json::String(v.to_text().unwrap_or_default()),
        SqlValue::Uuid(u) => Json::String(u.to_string()),
        // ISO-8601 (with a `T` separator) is what PostgREST/GoTrue clients parse.
        SqlValue::Timestamptz(ts) => Json::String(ts.to_rfc3339()),
        SqlValue::Timestamp(ts) => Json::String(ts.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        SqlValue::Date(d) => Json::String(d.format("%Y-%m-%d").to_string()),
        SqlValue::Time(t) => Json::String(t.format("%H:%M:%S%.f").to_string()),
        SqlValue::Json(j) => j.clone(),
        SqlValue::Array(items) => Json::Array(items.iter().map(value_to_json).collect()),
        SqlValue::Vector(vs) => Json::Array(
            vs.iter()
                .map(|f| {
                    serde_json::Number::from_f64(*f as f64)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                })
                .collect(),
        ),
        // Extension types (hstore, ltree, cube, ...) render as their
        // PostgreSQL text form, like PostgREST does for unknown types.
        other => Json::String(other.to_text().unwrap_or_default()),
    }
}

fn rows_to_json(fields: &[OutField], rows: &[Vec<SqlValue>]) -> Vec<Json> {
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

fn render_rows(
    objects: Vec<Json>,
    single: bool,
    status: StatusCode,
    content_range: Option<String>,
) -> Result<Response, SupaError> {
    let body = if single {
        match objects.len() {
            1 => objects.into_iter().next().unwrap(),
            0 => {
                return Err(SupaError::BadRequest(
                    "JSON object requested, but no rows were returned".into(),
                ));
            }
            _ => {
                return Err(SupaError::BadRequest(
                    "JSON object requested, but multiple rows were returned".into(),
                ));
            }
        }
    } else {
        Json::Array(objects)
    };
    let mut resp = (status, AxumJson(body)).into_response();
    if let Some(cr) = content_range
        && let Ok(v) = cr.parse()
    {
        resp.headers_mut().insert("content-range", v);
    }
    Ok(resp)
}

fn empty(status: StatusCode) -> Response {
    status.into_response()
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Extract `Rows` from an [`ExecResult`], erroring if a command tag came back.
fn expect_rows(result: ExecResult) -> Result<(Vec<OutField>, Vec<Vec<SqlValue>>), SupaError> {
    match result {
        ExecResult::Rows { fields, rows } => Ok((fields, rows)),
        ExecResult::Command { tag } => Err(SupaError::Internal(format!(
            "expected rows but the statement returned a command tag: {tag}"
        ))),
    }
}

fn wants_single(headers: &HeaderMap) -> bool {
    header_str(headers, "accept")
        .map(|a| a.contains("application/vnd.pgrst.object+json"))
        .unwrap_or(false)
}

/// The schema a request targets: the `Accept-Profile` (reads) / `Content-Profile`
/// (writes) header, defaulting to `public`.
fn request_schema(headers: &HeaderMap, write: bool) -> String {
    let header = if write {
        "content-profile"
    } else {
        "accept-profile"
    };
    header_str(headers, header)
        .map(str::to_string)
        .unwrap_or_else(|| "public".to_string())
}

fn resolve_window(rq: &RestQuery, headers: &HeaderMap) -> (Option<usize>, Option<usize>) {
    if rq.limit.is_some() || rq.offset.is_some() {
        return (rq.limit, rq.offset);
    }
    if let Some((start, end)) = parse_range(headers) {
        let limit = end.checked_sub(start).map(|d| d + 1);
        return (limit, Some(start));
    }
    (None, None)
}

/// Parse a `Range: 0-9` (optionally `items=0-9`) header into `(start, end)`.
fn parse_range(headers: &HeaderMap) -> Option<(usize, usize)> {
    let raw = header_str(headers, "range")?;
    let spec = raw.rsplit('=').next().unwrap_or(raw).trim();
    let (s, e) = spec.split_once('-')?;
    let start = s.trim().parse().ok()?;
    let end = e.trim().parse().ok()?;
    Some((start, end))
}

/// Validate a SQL identifier: `[A-Za-z_][A-Za-z0-9_]*`. This is the sole defense
/// for identifiers (which cannot be bound as parameters).
pub fn validate_ident(s: &str, what: &str) -> Result<(), SupaError> {
    let ok = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(SupaError::BadRequest(format!("invalid {what} name: {s:?}")))
    }
}

fn as_object(v: Json) -> Result<Map<String, Json>, SupaError> {
    match v {
        Json::Object(o) => Ok(o),
        _ => Err(SupaError::BadRequest(
            "each element of the array must be a JSON object".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_pairs_decodes() {
        let pairs = parse_query_pairs("select=id%2Cname&name=eq.a%20b");
        assert_eq!(pairs[0], ("select".into(), "id,name".into()));
        assert_eq!(pairs[1], ("name".into(), "eq.a b".into()));
    }

    #[test]
    fn select_list_translation() {
        let items = parse_select("id,full_name:name,*").unwrap();
        assert_eq!(
            build_select_list(&items).unwrap(),
            "\"id\", \"name\" AS \"full_name\", *"
        );
        assert_eq!(build_select_list(&[]).unwrap(), "*");
    }

    #[test]
    fn eq_filter_translates_to_parameterised_sql() {
        let rq = RestQuery::parse("id=eq.1&name=eq.alice").unwrap();
        let mut buf = SqlBuf::new();
        let sql = build_where(&mut buf, &rq.filters, None).unwrap();
        assert_eq!(sql, " WHERE \"id\" = $1 AND \"name\" = $2");
        assert!(matches!(buf.params[0], SqlValue::Int8(1)));
        assert!(matches!(&buf.params[1], SqlValue::Text(s) if s == "alice"));
    }

    #[test]
    fn like_maps_star_to_percent() {
        let rq = RestQuery::parse("name=ilike.*ali*").unwrap();
        let mut buf = SqlBuf::new();
        let sql = build_where(&mut buf, &rq.filters, None).unwrap();
        assert_eq!(sql, " WHERE \"name\" ILIKE $1");
        assert!(matches!(&buf.params[0], SqlValue::Text(s) if s == "%ali%"));
    }

    #[test]
    fn in_filter_translation() {
        let rq = RestQuery::parse("id=in.(1,2,3)").unwrap();
        let mut buf = SqlBuf::new();
        let sql = build_where(&mut buf, &rq.filters, None).unwrap();
        assert_eq!(sql, " WHERE \"id\" IN ($1, $2, $3)");
        assert_eq!(buf.params.len(), 3);
    }

    #[test]
    fn is_null_and_negation() {
        let rq = RestQuery::parse("deleted_at=is.null&active=not.is.false").unwrap();
        let mut buf = SqlBuf::new();
        let sql = build_where(&mut buf, &rq.filters, None).unwrap();
        assert_eq!(
            sql,
            " WHERE \"deleted_at\" IS NULL AND NOT (\"active\" IS FALSE)"
        );
        assert!(buf.params.is_empty());
    }

    #[test]
    fn order_translation() {
        let items = parse_order("created_at.desc.nullslast,name.asc").unwrap();
        assert_eq!(
            build_order(&items).unwrap(),
            " ORDER BY \"created_at\" DESC NULLS LAST, \"name\" ASC"
        );
    }

    #[test]
    fn unsupported_filter_is_rejected() {
        let err = RestQuery::parse("tags=cs.{a,b}").unwrap_err();
        assert!(matches!(err, SupaError::UnsupportedFilter(op) if op == "cs"));
        let err = RestQuery::parse("or=(a.eq.1,b.eq.2)").unwrap_err();
        assert!(matches!(err, SupaError::UnsupportedFilter(_)));
    }

    #[test]
    fn invalid_identifier_is_rejected() {
        assert!(validate_ident("users", "table").is_ok());
        assert!(validate_ident("user_id", "column").is_ok());
        assert!(validate_ident("drop table x", "table").is_err());
        assert!(validate_ident("a\"b", "table").is_err());
        assert!(validate_ident("1col", "column").is_err());
    }

    #[test]
    fn value_to_json_shapes() {
        assert_eq!(value_to_json(&SqlValue::Int4(5)), Json::from(5));
        assert_eq!(value_to_json(&SqlValue::Bool(true)), Json::Bool(true));
        assert_eq!(value_to_json(&SqlValue::Null), Json::Null);
        assert_eq!(
            value_to_json(&SqlValue::Text("hi".into())),
            Json::String("hi".into())
        );
        assert_eq!(
            value_to_json(&SqlValue::Json(serde_json::json!({"a":1}))),
            serde_json::json!({"a":1})
        );
    }

    #[test]
    fn range_header_parsing() {
        let mut h = HeaderMap::new();
        h.insert("range", "0-9".parse().unwrap());
        assert_eq!(parse_range(&h), Some((0, 9)));
        h.insert("range", "items=5-14".parse().unwrap());
        assert_eq!(parse_range(&h), Some((5, 14)));
    }
}
