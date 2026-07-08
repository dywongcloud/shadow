//! pg_graphql-compatible GraphQL endpoint (`/graphql/v1`).
//!
//! Reflects the `public` schema of the engine catalog into a GraphQL schema
//! shaped like the [`pg_graphql`](https://github.com/supabase/pg_graphql)
//! extension with **inflection off** (pg_graphql's default): a table
//! `blog_posts` becomes type `blog_posts`, query field `blog_postsCollection`,
//! mutations `insertIntoblog_postsCollection` / `updateblog_postsCollection` /
//! `deleteFromblog_postsCollection`, and so on. Only tables **with a primary
//! key** are reflected (pg_graphql's own rule — cursors and `nodeId` need one).
//!
//! Every top-level field compiles to parameterised SQL executed through the
//! same per-request session machinery REST uses (role + `request.jwt.claims`
//! bound), so row-level security governs GraphQL exactly like it governs
//! `/rest/v1`.
//!
//! Truthfulness contract: anything outside the implemented subset returns a
//! GraphQL error (`{"errors":[{"message": ...}]}`, HTTP 200) — never a silent
//! wrong answer. Documented divergences from pg_graphql live in
//! `docs/supabase-compat.md` (§GraphQL).

use std::collections::{BTreeMap, HashMap};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json as AxumJson};
use base64::Engine as _;
use futures::future::BoxFuture;
use graphql_parser::query as q;
use serde_json::{Map, Value as Json, json};

use crate::relational::Catalog;
use crate::relational::catalog::Table;
use crate::sql::engine::Session;
use crate::sql::{ExecResult, RelationalStorage, SqlType, SqlValue};
use crate::supabase::error::SupaError;
use crate::supabase::gateway::{AppState, AuthContext, load_catalog, run_sql_as};
use crate::supabase::rest::parse_query_pairs;

type GqlField = q::Field<'static, String>;
type GqlSelSet = q::SelectionSet<'static, String>;
type GqlValue = q::Value<'static, String>;
type GqlDirective = q::Directive<'static, String>;
type Frag = q::FragmentDefinition<'static, String>;

/// Maximum relationship-traversal depth for a single query.
const MAX_DEPTH: usize = 8;
/// Maximum nested fragment expansion (guards fragment cycles).
const MAX_FRAGMENT_DEPTH: usize = 32;
/// Default page size when neither `first` nor `last` is given (pg_graphql's
/// default `max_rows` is 30; we use it as the default page size and do not
/// clamp explicit `first`/`last` — see docs).
const DEFAULT_PAGE_SIZE: i64 = 30;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The GraphQL subrouter, mounted at `/graphql/v1` **under the apikey layer**.
pub fn router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/", get(handle_get::<S>).post(handle_post::<S>))
        .fallback(unknown_path)
}

/// pg_graphql serves exactly `/graphql/v1`; subpaths are a typed error, never
/// a bare 404.
async fn unknown_path() -> Response {
    (
        StatusCode::NOT_FOUND,
        AxumJson(json!({
            "errors": [{"message": "GraphQL requests must target /graphql/v1 exactly (no subpath)"}]
        })),
    )
        .into_response()
}

async fn handle_post<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    body: Bytes,
) -> Response {
    let parsed: Json = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return SupaError::BadRequest(format!("invalid JSON body: {e}")).into_response();
        }
    };
    let Some(query) = parsed.get("query").and_then(Json::as_str) else {
        return SupaError::BadRequest("body must contain a \"query\" string".into())
            .into_response();
    };
    let variables = match parsed.get("variables") {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(o)) => o.clone(),
        Some(_) => {
            return SupaError::BadRequest("\"variables\" must be a JSON object".into())
                .into_response();
        }
    };
    let operation_name = parsed
        .get("operationName")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let resp = run_request(&state, &auth, query, variables, operation_name).await;
    AxumJson(resp).into_response()
}

/// `GET /graphql/v1?query=...&variables=...&operationName=...` (GraphiQL
/// convenience). Mutations over GET are rejected with a GraphQL error, per
/// the GraphQL-over-HTTP spec.
async fn handle_get<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    Extension(auth): Extension<AuthContext>,
    RawQuery(raw): RawQuery,
) -> Response {
    let pairs = parse_query_pairs(raw.as_deref().unwrap_or(""));
    let mut query = None;
    let mut variables = Map::new();
    let mut operation_name = None;
    for (k, v) in pairs {
        match k.as_str() {
            "query" => query = Some(v),
            "variables" if !v.is_empty() => match serde_json::from_str::<Json>(&v) {
                Ok(Json::Object(o)) => variables = o,
                Ok(Json::Null) => {}
                _ => {
                    return SupaError::BadRequest("\"variables\" must be a JSON object".into())
                        .into_response();
                }
            },
            "operationName" if !v.is_empty() => operation_name = Some(v),
            _ => {}
        }
    }
    let Some(query) = query else {
        return SupaError::BadRequest("missing \"query\" parameter".into()).into_response();
    };
    let resp = run_request_readonly(&state, &auth, &query, variables, operation_name).await;
    AxumJson(resp).into_response()
}

// ---------------------------------------------------------------------------
// Errors and response assembly
// ---------------------------------------------------------------------------

/// An in-band GraphQL error (rendered as `{"errors":[{"message": ...}]}` with
/// HTTP 200, per GraphQL-over-HTTP convention).
#[derive(Debug)]
struct GqlError(String);

impl GqlError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

type GResult<T> = Result<T, GqlError>;

fn errors_response(message: &str) -> Json {
    json!({ "errors": [{ "message": message }] })
}

fn errors_with_null_data(message: &str) -> Json {
    json!({ "data": Json::Null, "errors": [{ "message": message }] })
}

// ---------------------------------------------------------------------------
// Request execution
// ---------------------------------------------------------------------------

async fn run_request<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    query: &str,
    variables: Map<String, Json>,
    operation_name: Option<String>,
) -> Json {
    run_request_inner(state, auth, query, variables, operation_name, true).await
}

async fn run_request_readonly<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    query: &str,
    variables: Map<String, Json>,
    operation_name: Option<String>,
) -> Json {
    run_request_inner(state, auth, query, variables, operation_name, false).await
}

async fn run_request_inner<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &AuthContext,
    query: &str,
    variables: Map<String, Json>,
    operation_name: Option<String>,
    allow_mutations: bool,
) -> Json {
    // 1. Parse the document.
    let doc = match q::parse_query::<String>(query) {
        Ok(d) => d.into_static(),
        Err(e) => return errors_response(&format!("syntax error: {e}")),
    };

    // 2. Partition operations and fragments.
    let mut operations = Vec::new();
    let mut fragments: HashMap<String, Frag> = HashMap::new();
    for def in doc.definitions {
        match def {
            q::Definition::Operation(op) => operations.push(op),
            q::Definition::Fragment(f) => {
                fragments.insert(f.name.clone(), f);
            }
        }
    }

    // 3. Select the operation.
    let op = match select_operation(&operations, operation_name.as_deref()) {
        Ok(op) => op,
        Err(e) => return errors_response(&e.0),
    };
    let (kind, var_defs, sel) = match op {
        q::OperationDefinition::SelectionSet(s) => (OpKind::Query, &[][..], s),
        q::OperationDefinition::Query(qq) => (
            OpKind::Query,
            &qq.variable_definitions[..],
            &qq.selection_set,
        ),
        q::OperationDefinition::Mutation(m) => (
            OpKind::Mutation,
            &m.variable_definitions[..],
            &m.selection_set,
        ),
        q::OperationDefinition::Subscription(_) => {
            return errors_response(
                "subscriptions are not supported by this pg_graphql-compatible endpoint",
            );
        }
    };
    if kind == OpKind::Mutation && !allow_mutations {
        return errors_response("mutations are not allowed over GET; use POST");
    }

    // 4. Coerce variables (defaults applied; required variables enforced).
    let vars = match coerce_variables(var_defs, variables) {
        Ok(v) => v,
        Err(e) => return errors_response(&e.0),
    };

    // 5. Reflect the schema from the current catalog snapshot.
    let catalog = match load_catalog(&state.db).await {
        Ok(Some(c)) => c,
        Ok(None) => Catalog::new(&state.db.name),
        Err(e) => return errors_response(&format!("catalog load failed: {e}")),
    };
    let schema = reflect(&catalog);

    let exec = Exec {
        state,
        auth,
        schema,
        fragments,
        vars,
    };

    // 6. Execute. Any field error aborts the whole operation (like pg_graphql,
    //    which runs the request in one transaction) — no partial data.
    let result = match kind {
        OpKind::Query => exec.query_root(sel).await,
        OpKind::Mutation => exec.mutation_root(sel).await,
    };
    match result {
        Ok(data) => json!({ "data": data }),
        Err(e) => errors_with_null_data(&e.0),
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum OpKind {
    Query,
    Mutation,
}

fn select_operation<'d>(
    operations: &'d [q::OperationDefinition<'static, String>],
    name: Option<&str>,
) -> GResult<&'d q::OperationDefinition<'static, String>> {
    fn op_name<'d>(op: &'d q::OperationDefinition<'static, String>) -> Option<&'d str> {
        match op {
            q::OperationDefinition::SelectionSet(_) => None,
            q::OperationDefinition::Query(o) => o.name.as_deref(),
            q::OperationDefinition::Mutation(o) => o.name.as_deref(),
            q::OperationDefinition::Subscription(o) => o.name.as_deref(),
        }
    }
    match name {
        Some(n) => operations
            .iter()
            .find(|op| op_name(op) == Some(n))
            .ok_or_else(|| GqlError::new(format!("unknown operation \"{n}\""))),
        None => match operations.len() {
            0 => Err(GqlError::new("the document contains no operations")),
            1 => Ok(&operations[0]),
            _ => Err(GqlError::new(
                "operationName is required when the document contains multiple operations",
            )),
        },
    }
}

fn coerce_variables(
    defs: &[q::VariableDefinition<'static, String>],
    provided: Map<String, Json>,
) -> GResult<Map<String, Json>> {
    let mut out = provided;
    for def in defs {
        let present = out.get(&def.name).map(|v| !v.is_null()).unwrap_or(false);
        if present {
            continue;
        }
        if let Some(default) = &def.default_value {
            let v = literal_to_json(default)?;
            out.insert(def.name.clone(), v);
        } else if matches!(def.var_type, q::Type::NonNullType(_)) {
            return Err(GqlError::new(format!(
                "variable ${} of a non-null type was not provided",
                def.name
            )));
        }
    }
    Ok(out)
}

/// Convert a *literal* (variable-free) GraphQL value to JSON.
fn literal_to_json(v: &GqlValue) -> GResult<Json> {
    match v {
        q::Value::Variable(name) => Err(GqlError::new(format!(
            "variable ${name} cannot be used inside a default value"
        ))),
        other => value_to_json_with(other, &Map::new()),
    }
}

fn value_to_json_with(v: &GqlValue, vars: &Map<String, Json>) -> GResult<Json> {
    Ok(match v {
        q::Value::Variable(name) => vars.get(name.as_str()).cloned().unwrap_or(Json::Null),
        q::Value::Int(n) => n
            .as_i64()
            .map(Json::from)
            .ok_or_else(|| GqlError::new("integer literal out of range"))?,
        q::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        q::Value::String(s) => Json::String(s.clone()),
        q::Value::Boolean(b) => Json::Bool(*b),
        q::Value::Null => Json::Null,
        q::Value::Enum(e) => Json::String(e.clone()),
        q::Value::List(items) => Json::Array(
            items
                .iter()
                .map(|i| value_to_json_with(i, vars))
                .collect::<GResult<Vec<_>>>()?,
        ),
        q::Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                out.insert(k.clone(), value_to_json_with(val, vars)?);
            }
            Json::Object(out)
        }
    })
}

// ---------------------------------------------------------------------------
// Scalar mapping
// ---------------------------------------------------------------------------

/// The pg_graphql scalar a column maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scalar {
    Int,
    Float,
    String,
    Boolean,
    BigInt,
    BigFloat,
    Uuid,
    Date,
    Time,
    Datetime,
    Json,
    Opaque,
}

impl Scalar {
    fn name(self) -> &'static str {
        match self {
            Scalar::Int => "Int",
            Scalar::Float => "Float",
            Scalar::String => "String",
            Scalar::Boolean => "Boolean",
            Scalar::BigInt => "BigInt",
            Scalar::BigFloat => "BigFloat",
            Scalar::Uuid => "UUID",
            Scalar::Date => "Date",
            Scalar::Time => "Time",
            Scalar::Datetime => "Datetime",
            Scalar::Json => "JSON",
            Scalar::Opaque => "Opaque",
        }
    }

    /// Comparators beyond `eq/neq/in/is` this scalar's filter carries.
    fn is_ordered(self) -> bool {
        matches!(
            self,
            Scalar::Int
                | Scalar::Float
                | Scalar::String
                | Scalar::BigInt
                | Scalar::BigFloat
                | Scalar::Date
                | Scalar::Time
                | Scalar::Datetime
        )
    }

    fn is_filterable(self) -> bool {
        !matches!(self, Scalar::Json | Scalar::Opaque)
    }
}

/// Map a SQL column type to `(scalar, is_list)`. Unknown/exotic types map to
/// `String` (their PostgreSQL text form) — reflection never fails on a type.
fn scalar_of(ty: &SqlType) -> (Scalar, bool) {
    if let SqlType::Array(inner) = ty {
        // Nested arrays render as the element's text form (String).
        let elem = match inner.as_ref() {
            SqlType::Array(_) => Scalar::String,
            other => scalar_of(other).0,
        };
        return (elem, true);
    }
    let s = match ty {
        SqlType::Boolean => Scalar::Boolean,
        SqlType::SmallInt | SqlType::Integer => Scalar::Int,
        SqlType::BigInt => Scalar::BigInt,
        SqlType::Real | SqlType::DoublePrecision => Scalar::Float,
        SqlType::Numeric { .. } => Scalar::BigFloat,
        SqlType::Text | SqlType::Varchar(_) | SqlType::Char(_) | SqlType::Citext => Scalar::String,
        SqlType::Bytea => Scalar::Opaque,
        SqlType::Uuid => Scalar::Uuid,
        SqlType::Date => Scalar::Date,
        SqlType::Time => Scalar::Time,
        SqlType::Timestamp | SqlType::Timestamptz => Scalar::Datetime,
        SqlType::Json | SqlType::Jsonb => Scalar::Json,
        // Extension/exotic types (vector, hstore, ltree, cube, unknown):
        // reflected as String (PostgreSQL text form), never a crash.
        _ => Scalar::String,
    };
    (s, false)
}

/// Render a SQL value as its GraphQL scalar JSON form (pg_graphql shapes:
/// BigInt/BigFloat as strings, JSON as a serialized string, bytea as base64).
fn render_value(v: &SqlValue) -> Json {
    match v {
        SqlValue::Null => Json::Null,
        SqlValue::Bool(b) => Json::Bool(*b),
        SqlValue::Int2(n) => Json::from(*n),
        SqlValue::Int4(n) => Json::from(*n),
        SqlValue::Int8(n) => Json::String(n.to_string()),
        SqlValue::Float4(n) => serde_json::Number::from_f64(*n as f64)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        SqlValue::Float8(n) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        SqlValue::Numeric(d) => Json::String(d.normalize().to_string()),
        SqlValue::Text(s) | SqlValue::Citext(s) => Json::String(s.clone()),
        SqlValue::Bytea(b) => Json::String(base64::engine::general_purpose::STANDARD.encode(b)),
        SqlValue::Uuid(u) => Json::String(u.to_string()),
        SqlValue::Timestamptz(ts) => Json::String(ts.to_rfc3339()),
        SqlValue::Timestamp(ts) => Json::String(ts.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        SqlValue::Date(d) => Json::String(d.format("%Y-%m-%d").to_string()),
        SqlValue::Time(t) => Json::String(t.format("%H:%M:%S%.f").to_string()),
        SqlValue::Json(j) => Json::String(j.to_string()),
        SqlValue::Array(items) => Json::Array(items.iter().map(render_value).collect()),
        other => Json::String(other.to_text().unwrap_or_default()),
    }
}

/// Coerce a GraphQL input value (as JSON) to a typed [`SqlValue`] for `ty`.
fn coerce_input(v: &Json, ty: &SqlType, what: &str) -> GResult<SqlValue> {
    if v.is_null() {
        return Ok(SqlValue::Null);
    }
    // pg_graphql's JSON scalar is a *string* of serialized JSON on input.
    if matches!(ty, SqlType::Json | SqlType::Jsonb) {
        let Json::String(s) = v else {
            return Err(GqlError::new(format!(
                "{what}: JSON input must be a String containing serialized JSON"
            )));
        };
        let parsed: Json = serde_json::from_str(s)
            .map_err(|e| GqlError::new(format!("{what}: invalid JSON string: {e}")))?;
        return Ok(SqlValue::Json(parsed));
    }
    SqlValue::decode_json(v, ty)
        .map_err(|e| GqlError::new(format!("{what}: invalid value for {}: {e}", ty.name())))
}

// ---------------------------------------------------------------------------
// Reflection
// ---------------------------------------------------------------------------

/// A reflected column.
#[derive(Debug, Clone)]
struct GqlColumn {
    name: String,
    ty: SqlType,
    scalar: Scalar,
    is_list: bool,
    nullable: bool,
}

impl GqlColumn {
    fn filterable(&self) -> bool {
        !self.is_list && self.scalar.is_filterable()
    }

    fn orderable(&self) -> bool {
        self.filterable()
    }
}

/// A reflected relationship field.
#[derive(Debug, Clone)]
enum Rel {
    /// Child → parent object field: `WHERE parent.ref_cols = row.fk_cols`.
    Parent {
        ref_type: String,
        fk_cols: Vec<String>,
        ref_cols: Vec<String>,
    },
    /// Parent → child collection field: `WHERE child.fk_cols = row.ref_cols`.
    Child {
        child_type: String,
        fk_cols: Vec<String>,
        ref_cols: Vec<String>,
    },
}

/// A reflected table (GraphQL type name == table name; inflection off).
struct TableInfo {
    table: Table,
    columns: Vec<GqlColumn>,
    col_index: HashMap<String, usize>,
    pk_cols: Vec<(String, SqlType)>,
    rels: BTreeMap<String, Rel>,
}

impl TableInfo {
    fn column(&self, name: &str) -> Option<&GqlColumn> {
        self.col_index.get(name).map(|i| &self.columns[*i])
    }
}

/// The reflected GraphQL schema for one request.
struct RelSchema {
    tables: BTreeMap<String, TableInfo>,
    /// query collection field name → table type name
    collections: HashMap<String, String>,
    /// mutation field name → (kind, table type name)
    mutations: HashMap<String, (MutKind, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MutKind {
    Insert,
    Update,
    Delete,
}

/// Names a user table may not use (they collide with built-in schema types).
const RESERVED_TYPE_NAMES: &[&str] = &[
    "Query",
    "Mutation",
    "Subscription",
    "Node",
    "PageInfo",
    "OrderByDirection",
    "FilterIs",
    "Int",
    "Float",
    "String",
    "Boolean",
    "ID",
    "BigInt",
    "BigFloat",
    "UUID",
    "Date",
    "Time",
    "Datetime",
    "JSON",
    "Opaque",
    "Cursor",
];

/// A valid GraphQL name: `/[_A-Za-z][_A-Za-z0-9]*/`, not starting with `__`.
fn is_gql_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with("__")
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reflect the `public`-schema user tables into a [`RelSchema`].
///
/// Skipped (honestly absent from the schema, per the documented rules):
/// tables without a primary key, tables/columns whose names are not valid
/// GraphQL names, tables with a column named `nodeId`, tables whose name
/// collides with a built-in type name, and views (not reflected in this
/// slice).
fn reflect(catalog: &Catalog) -> RelSchema {
    let mut tables: BTreeMap<String, TableInfo> = BTreeMap::new();

    for t in catalog.tables() {
        if t.schema != "public" {
            continue;
        }
        if t.primary_key.is_none() {
            continue;
        }
        if !is_gql_name(&t.name) || RESERVED_TYPE_NAMES.contains(&t.name.as_str()) {
            continue;
        }
        if !t.columns.iter().all(|c| is_gql_name(&c.name)) {
            continue;
        }
        if t.columns.iter().any(|c| c.name == "nodeId") {
            continue;
        }
        let columns: Vec<GqlColumn> = t
            .columns
            .iter()
            .map(|c| {
                let (scalar, is_list) = scalar_of(&c.ty);
                GqlColumn {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    scalar,
                    is_list,
                    nullable: c.nullable,
                }
            })
            .collect();
        let col_index = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        let pk_cols = t
            .pk_columns()
            .into_iter()
            .filter_map(|name| t.column(&name).map(|c| (name.clone(), c.ty.clone())))
            .collect();
        tables.insert(
            t.name.clone(),
            TableInfo {
                table: t.clone(),
                columns,
                col_index,
                pk_cols,
                rels: BTreeMap::new(),
            },
        );
    }

    // Relationship pass: for every FK between two reflected tables, add a
    // parent object field on the child and a child collection field on the
    // parent. Naming convention (constraint-name independent, documented):
    //   - parent field:  "<ref_table>", or "<ref_table>_by_<fkcols>" when the
    //     plain name collides (multiple FKs to the same table or a column of
    //     the same name);
    //   - child field:   "<child_table>Collection", or
    //     "<child_table>_by_<fkcols>Collection" on collision.
    // A field whose disambiguated name still collides is omitted.
    struct RelCand {
        host: String,
        base: String,
        fallback: String,
        rel: Rel,
    }
    let mut cands: Vec<RelCand> = Vec::new();
    for ti in tables.values() {
        for fk in &ti.table.foreign_keys {
            if fk.ref_schema != "public" || !tables.contains_key(&fk.ref_table) {
                continue;
            }
            if fk.columns.len() != fk.ref_columns.len() {
                continue;
            }
            let by = fk.columns.join("_");
            cands.push(RelCand {
                host: ti.table.name.clone(),
                base: fk.ref_table.clone(),
                fallback: format!("{}_by_{by}", fk.ref_table),
                rel: Rel::Parent {
                    ref_type: fk.ref_table.clone(),
                    fk_cols: fk.columns.clone(),
                    ref_cols: fk.ref_columns.clone(),
                },
            });
            cands.push(RelCand {
                host: fk.ref_table.clone(),
                base: format!("{}Collection", ti.table.name),
                fallback: format!("{}_by_{by}Collection", ti.table.name),
                rel: Rel::Child {
                    child_type: ti.table.name.clone(),
                    fk_cols: fk.columns.clone(),
                    ref_cols: fk.ref_columns.clone(),
                },
            });
        }
    }
    // Count base-name usage per host to decide when to disambiguate.
    let mut base_counts: HashMap<(String, String), usize> = HashMap::new();
    for c in &cands {
        *base_counts
            .entry((c.host.clone(), c.base.clone()))
            .or_insert(0) += 1;
    }
    for c in cands {
        let host = tables.get_mut(&c.host).expect("host table reflected");
        let ambiguous = base_counts[&(c.host.clone(), c.base.clone())] > 1;
        let name = if !ambiguous && host.column(&c.base).is_none() && is_gql_name(&c.base) {
            c.base
        } else if host.column(&c.fallback).is_none() && is_gql_name(&c.fallback) {
            c.fallback
        } else {
            continue; // still colliding: omit the field (documented)
        };
        host.rels.entry(name).or_insert(c.rel);
    }

    let mut collections = HashMap::new();
    let mut mutations = HashMap::new();
    for name in tables.keys() {
        collections.insert(format!("{name}Collection"), name.clone());
        mutations.insert(
            format!("insertInto{name}Collection"),
            (MutKind::Insert, name.clone()),
        );
        mutations.insert(
            format!("update{name}Collection"),
            (MutKind::Update, name.clone()),
        );
        mutations.insert(
            format!("deleteFrom{name}Collection"),
            (MutKind::Delete, name.clone()),
        );
    }

    RelSchema {
        tables,
        collections,
        mutations,
    }
}

// ---------------------------------------------------------------------------
// Cursors and nodeId
// ---------------------------------------------------------------------------

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn un_b64(s: &str) -> GResult<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| GqlError::new("malformed cursor / nodeId (invalid base64)"))?;
    String::from_utf8(bytes).map_err(|_| GqlError::new("malformed cursor / nodeId (not UTF-8)"))
}

/// Cursor = base64 of the JSON array of the row's primary-key values.
fn encode_cursor(pk_values: &[Json]) -> String {
    b64(&Json::Array(pk_values.to_vec()).to_string())
}

fn decode_cursor(s: &str) -> GResult<Vec<Json>> {
    let raw = un_b64(s)?;
    match serde_json::from_str::<Json>(&raw) {
        Ok(Json::Array(vals)) => Ok(vals),
        _ => Err(GqlError::new("malformed cursor (expected a JSON array)")),
    }
}

/// nodeId = base64 of `["<schema>", "<table>", <pk values...>]`.
fn encode_node_id(schema: &str, table: &str, pk_values: &[Json]) -> String {
    let mut arr = vec![Json::String(schema.into()), Json::String(table.into())];
    arr.extend(pk_values.iter().cloned());
    b64(&Json::Array(arr).to_string())
}

fn decode_node_id(s: &str) -> GResult<(String, String, Vec<Json>)> {
    let raw = un_b64(s)?;
    let Ok(Json::Array(vals)) = serde_json::from_str::<Json>(&raw) else {
        return Err(GqlError::new("malformed nodeId (expected a JSON array)"));
    };
    if vals.len() < 3 {
        return Err(GqlError::new(
            "malformed nodeId (expected [schema, table, pk...])",
        ));
    }
    let schema = vals[0]
        .as_str()
        .ok_or_else(|| GqlError::new("malformed nodeId (schema must be a string)"))?
        .to_string();
    let table = vals[1]
        .as_str()
        .ok_or_else(|| GqlError::new("malformed nodeId (table must be a string)"))?
        .to_string();
    Ok((schema, table, vals[2..].to_vec()))
}

// ---------------------------------------------------------------------------
// SQL construction helpers
// ---------------------------------------------------------------------------

/// Accumulates bound parameters and hands out `$n` placeholders.
struct Params {
    values: Vec<SqlValue>,
}

impl Params {
    fn new() -> Self {
        Self { values: Vec::new() }
    }

    fn bind(&mut self, v: SqlValue) -> String {
        self.values.push(v);
        format!("${}", self.values.len())
    }
}

/// A result set with by-name column access.
struct RowSet {
    idx: HashMap<String, usize>,
    rows: Vec<Vec<SqlValue>>,
}

impl RowSet {
    fn from_exec(result: ExecResult) -> GResult<RowSet> {
        match result {
            ExecResult::Rows { fields, rows } => Ok(RowSet {
                idx: fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (f.name.clone(), i))
                    .collect(),
                rows,
            }),
            ExecResult::Command { tag } => Err(GqlError::new(format!(
                "internal: expected rows, got command tag {tag}"
            ))),
        }
    }

    fn get<'r>(&self, row: &'r [SqlValue], col: &str) -> Option<&'r SqlValue> {
        self.idx.get(col).and_then(|i| row.get(*i))
    }
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

struct Exec<'a, S: RelationalStorage + 'static> {
    state: &'a AppState<S>,
    auth: &'a AuthContext,
    schema: RelSchema,
    fragments: HashMap<String, Frag>,
    vars: Map<String, Json>,
}

impl<'a, S: RelationalStorage + 'static> Exec<'a, S> {
    // -- selection-set machinery -------------------------------------------

    fn value_to_json(&self, v: &GqlValue) -> GResult<Json> {
        value_to_json_with(v, &self.vars)
    }

    /// Evaluate `@skip` / `@include`; unknown directives are an error.
    fn directives_pass(&self, dirs: &[GqlDirective]) -> GResult<bool> {
        for d in dirs {
            match d.name.as_str() {
                "skip" | "include" => {
                    let (_, val) =
                        d.arguments.iter().find(|(n, _)| n == "if").ok_or_else(|| {
                            GqlError::new(format!("@{} requires an \"if\" argument", d.name))
                        })?;
                    let b = self.value_to_json(val)?.as_bool().ok_or_else(|| {
                        GqlError::new(format!("@{}(if:) must be a Boolean", d.name))
                    })?;
                    if (d.name == "skip" && b) || (d.name == "include" && !b) {
                        return Ok(false);
                    }
                }
                other => {
                    return Err(GqlError::new(format!(
                        "unknown directive \"@{other}\" (only @skip and @include are supported)"
                    )));
                }
            }
        }
        Ok(true)
    }

    /// Does a fragment type condition apply to the concrete type `type_name`?
    fn condition_applies(&self, cond: &str, type_name: &str) -> bool {
        cond == type_name || (cond == "Node" && self.schema.tables.contains_key(type_name))
    }

    /// Flatten a selection set against concrete type `type_name`: expand
    /// fragments (named + inline), apply @skip/@include, and merge fields
    /// sharing a response key (spec CollectFields).
    fn flatten(&self, sel: &GqlSelSet, type_name: &str) -> GResult<Vec<GqlField>> {
        let mut out: Vec<GqlField> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        self.flatten_into(sel, type_name, &mut out, &mut index, 0)?;
        Ok(out)
    }

    fn flatten_into(
        &self,
        sel: &GqlSelSet,
        type_name: &str,
        out: &mut Vec<GqlField>,
        index: &mut HashMap<String, usize>,
        depth: usize,
    ) -> GResult<()> {
        if depth > MAX_FRAGMENT_DEPTH {
            return Err(GqlError::new("fragment nesting too deep (cycle?)"));
        }
        for item in &sel.items {
            match item {
                q::Selection::Field(f) => {
                    if !self.directives_pass(&f.directives)? {
                        continue;
                    }
                    let key = f.alias.clone().unwrap_or_else(|| f.name.clone());
                    match index.get(&key) {
                        Some(i) => {
                            // Merge sub-selections of fields sharing a key.
                            let items = f.selection_set.items.clone();
                            out[*i].selection_set.items.extend(items);
                        }
                        None => {
                            index.insert(key, out.len());
                            out.push(f.clone());
                        }
                    }
                }
                q::Selection::FragmentSpread(spread) => {
                    if !self.directives_pass(&spread.directives)? {
                        continue;
                    }
                    let frag = self.fragments.get(&spread.fragment_name).ok_or_else(|| {
                        GqlError::new(format!("unknown fragment \"{}\"", spread.fragment_name))
                    })?;
                    let q::TypeCondition::On(cond) = &frag.type_condition;
                    if self.condition_applies(cond, type_name) {
                        self.flatten_into(&frag.selection_set, type_name, out, index, depth + 1)?;
                    }
                }
                q::Selection::InlineFragment(inline) => {
                    if !self.directives_pass(&inline.directives)? {
                        continue;
                    }
                    let applies = match &inline.type_condition {
                        Some(q::TypeCondition::On(cond)) => self.condition_applies(cond, type_name),
                        None => true,
                    };
                    if applies {
                        self.flatten_into(&inline.selection_set, type_name, out, index, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Collect a field's arguments into a JSON map, rejecting unknown names.
    fn args_map(&self, field: &GqlField, allowed: &[&str]) -> GResult<Map<String, Json>> {
        let mut out = Map::new();
        for (name, value) in &field.arguments {
            if !allowed.contains(&name.as_str()) {
                return Err(GqlError::new(format!(
                    "unknown argument \"{name}\" on field \"{}\"",
                    field.name
                )));
            }
            out.insert(name.clone(), self.value_to_json(value)?);
        }
        Ok(out)
    }

    fn response_key(field: &GqlField) -> String {
        field.alias.clone().unwrap_or_else(|| field.name.clone())
    }

    // -- SQL execution -------------------------------------------------------

    async fn sql(&self, sql: &str, params: Vec<SqlValue>) -> GResult<RowSet> {
        let result = run_sql_as(&self.state.db, self.auth, sql, params)
            .await
            .map_err(|e| GqlError::new(e.to_string()))?;
        RowSet::from_exec(result)
    }

    // -- roots ---------------------------------------------------------------

    async fn query_root(&self, sel: &GqlSelSet) -> GResult<Json> {
        let fields = self.flatten(sel, "Query")?;
        let intro = build_introspection(&self.schema);
        let mut data = Map::new();
        for field in &fields {
            let key = Self::response_key(field);
            let value = match field.name.as_str() {
                "__typename" => Json::String("Query".into()),
                "__schema" => {
                    self.args_map(field, &[])?;
                    self.project(&intro.schema_json, &field.selection_set, &intro)?
                }
                "__type" => {
                    let args = self.args_map(field, &["name"])?;
                    let name = args
                        .get("name")
                        .and_then(Json::as_str)
                        .ok_or_else(|| GqlError::new("__type requires a String \"name\""))?;
                    match intro.types_by_name.get(name) {
                        Some(t) => self.project(t, &field.selection_set, &intro)?,
                        None => Json::Null,
                    }
                }
                "node" => self.resolve_node(field).await?,
                name => match self.schema.collections.get(name) {
                    Some(type_name) => {
                        self.resolve_collection(type_name.clone(), field, Vec::new(), 0)
                            .await?
                    }
                    None => {
                        return Err(GqlError::new(format!(
                            "Unknown field \"{name}\" on type \"Query\""
                        )));
                    }
                },
            };
            merge_into(&mut data, key, value);
        }
        Ok(Json::Object(data))
    }

    /// Mutations run sequentially in document order; each mutation field runs
    /// in its own transaction, and the first error aborts the remainder (a
    /// documented divergence from pg_graphql's single whole-request
    /// transaction).
    async fn mutation_root(&self, sel: &GqlSelSet) -> GResult<Json> {
        let fields = self.flatten(sel, "Mutation")?;
        let mut data = Map::new();
        for field in &fields {
            let key = Self::response_key(field);
            let value = match field.name.as_str() {
                "__typename" => Json::String("Mutation".into()),
                name => match self.schema.mutations.get(name).cloned() {
                    Some((kind, type_name)) => match kind {
                        MutKind::Insert => self.resolve_insert(&type_name, field).await?,
                        MutKind::Update => {
                            self.resolve_update_delete(&type_name, field, true).await?
                        }
                        MutKind::Delete => {
                            self.resolve_update_delete(&type_name, field, false).await?
                        }
                    },
                    None => {
                        return Err(GqlError::new(format!(
                            "Unknown field \"{name}\" on type \"Mutation\""
                        )));
                    }
                },
            };
            merge_into(&mut data, key, value);
        }
        Ok(Json::Object(data))
    }

    // -- node lookup ----------------------------------------------------------

    async fn resolve_node(&self, field: &GqlField) -> GResult<Json> {
        let args = self.args_map(field, &["nodeId"])?;
        let node_id = args
            .get("nodeId")
            .and_then(Json::as_str)
            .ok_or_else(|| GqlError::new("node requires a nodeId: ID! argument"))?;
        let (schema, table, pk_json) = decode_node_id(node_id)?;
        if schema != "public" {
            return Err(GqlError::new(format!(
                "nodeId references schema \"{schema}\"; only \"public\" is reflected"
            )));
        }
        let Some(ti) = self.schema.tables.get(&table) else {
            return Err(GqlError::new(format!(
                "nodeId references table \"{table}\", which is not reflected in the GraphQL schema"
            )));
        };
        if pk_json.len() != ti.pk_cols.len() {
            return Err(GqlError::new("nodeId has the wrong number of key values"));
        }
        let mut p = Params::new();
        let mut conds = Vec::new();
        for ((col, ty), v) in ti.pk_cols.iter().zip(pk_json.iter()) {
            let sv = coerce_input(v, ty, "nodeId")?;
            conds.push(format!("\"{col}\" = {}", p.bind(sv)));
        }
        let sql = format!(
            "SELECT * FROM \"public\".\"{}\" WHERE {} LIMIT 1",
            ti.table.name,
            conds.join(" AND ")
        );
        let rs = self.sql(&sql, p.values).await?;
        if rs.rows.is_empty() {
            return Ok(Json::Null);
        }
        let row = rs.rows[0].clone();
        self.resolve_row(ti, &rs, &row, &field.selection_set, 1)
            .await
    }

    // -- collections ------------------------------------------------------------

    /// Resolve a `<t>Collection` field (top-level or nested one-to-many).
    /// `extra` carries fixed equality conditions from a parent row.
    fn resolve_collection<'x>(
        &'x self,
        type_name: String,
        field: &'x GqlField,
        extra: Vec<(String, SqlValue)>,
        depth: usize,
    ) -> BoxFuture<'x, GResult<Json>> {
        Box::pin(async move {
            if depth > MAX_DEPTH {
                return Err(GqlError::new(format!(
                    "query exceeds the maximum relationship depth of {MAX_DEPTH}"
                )));
            }
            let ti = self
                .schema
                .tables
                .get(&type_name)
                .ok_or_else(|| GqlError::new(format!("unknown type \"{type_name}\"")))?;
            let args = self.args_map(
                field,
                &[
                    "first", "last", "before", "after", "offset", "filter", "orderBy",
                ],
            )?;

            let int_arg = |name: &str| -> GResult<Option<i64>> {
                match args.get(name) {
                    None | Some(Json::Null) => Ok(None),
                    Some(v) => v
                        .as_i64()
                        .map(Some)
                        .ok_or_else(|| GqlError::new(format!("\"{name}\" must be an Int"))),
                }
            };
            let str_arg = |name: &str| -> GResult<Option<String>> {
                match args.get(name) {
                    None | Some(Json::Null) => Ok(None),
                    Some(Json::String(s)) => Ok(Some(s.clone())),
                    Some(_) => Err(GqlError::new(format!("\"{name}\" must be a Cursor string"))),
                }
            };

            let first = int_arg("first")?;
            let last = int_arg("last")?;
            let offset = int_arg("offset")?;
            let before = str_arg("before")?;
            let after = str_arg("after")?;
            let filter = match args.get("filter") {
                None | Some(Json::Null) => None,
                Some(Json::Object(o)) => Some(o.clone()),
                Some(_) => return Err(GqlError::new("\"filter\" must be an input object")),
            };
            let order_by = self.parse_order_by(ti, args.get("orderBy"))?;

            // Combination rules (truthful errors where correct keyset
            // pagination is not implemented).
            if first.is_some() && last.is_some() {
                return Err(GqlError::new(
                    "\"first\" and \"last\" cannot both be provided",
                ));
            }
            if before.is_some() && after.is_some() {
                return Err(GqlError::new(
                    "\"before\" and \"after\" cannot both be provided",
                ));
            }
            if (before.is_some() || after.is_some()) && !order_by.is_empty() {
                return Err(GqlError::new(
                    "cursor pagination (before/after) combined with orderBy is not supported; \
                     cursors are keyed on the primary key ordering only",
                ));
            }
            if first.map(|v| v < 0).unwrap_or(false) || last.map(|v| v < 0).unwrap_or(false) {
                return Err(GqlError::new("\"first\"/\"last\" must be non-negative"));
            }
            if last.is_some() && after.is_some() {
                return Err(GqlError::new("\"last\" cannot be combined with \"after\""));
            }
            if first.is_some() && before.is_some() {
                return Err(GqlError::new(
                    "\"first\" cannot be combined with \"before\"",
                ));
            }
            if last.is_some() && offset.is_some() {
                return Err(GqlError::new("\"offset\" cannot be combined with \"last\""));
            }
            if offset.map(|v| v < 0).unwrap_or(false) {
                return Err(GqlError::new("\"offset\" must be non-negative"));
            }

            let backward = last.is_some() || before.is_some();
            let page = if backward {
                last.unwrap_or(DEFAULT_PAGE_SIZE)
            } else {
                first.unwrap_or(DEFAULT_PAGE_SIZE)
            };

            // WHERE: fixed conds + filter (+ cursor predicate for the page query).
            let mut p = Params::new();
            let mut base = Vec::new();
            for (col, v) in &extra {
                base.push(format!("\"{col}\" = {}", p.bind(v.clone())));
            }
            if let Some(f) = &filter {
                let clause = self.compile_filter(ti, f, &mut p)?;
                base.push(clause);
            }
            let mut page_conds = base.clone();
            if let Some(cur) = &after {
                page_conds.push(self.cursor_predicate(ti, cur, ">", &mut p)?);
            }
            if let Some(cur) = &before {
                page_conds.push(self.cursor_predicate(ti, cur, "<", &mut p)?);
            }

            // ORDER BY: user order + PK tiebreak, or PK order; flipped when
            // paginating backward.
            let mut order_parts: Vec<(String, &'static str)> = Vec::new();
            for (col, dir) in &order_by {
                order_parts.push((col.clone(), dir.sql()));
            }
            for (col, _) in &ti.pk_cols {
                if !order_by.iter().any(|(c, _)| c == col) {
                    order_parts.push((col.clone(), OrderDir::AscNullsLast.sql()));
                }
            }
            let order_sql = order_parts
                .iter()
                .map(|(col, dir)| {
                    let d = if backward { flip_dir(dir) } else { dir };
                    format!("\"{col}\" {d}")
                })
                .collect::<Vec<_>>()
                .join(", ");

            let where_sql = if page_conds.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", page_conds.join(" AND "))
            };
            let mut sql = format!(
                "SELECT * FROM \"public\".\"{}\"{} ORDER BY {} LIMIT {}",
                ti.table.name,
                where_sql,
                order_sql,
                page + 1
            );
            if let Some(o) = offset.filter(|o| *o > 0) {
                sql.push_str(&format!(" OFFSET {o}"));
            }
            let mut rs = self.sql(&sql, p.values).await?;

            let has_more = rs.rows.len() as i64 > page;
            rs.rows.truncate(page as usize);
            if backward {
                rs.rows.reverse();
            }
            let (has_next, has_prev) = if backward {
                (before.is_some(), has_more)
            } else {
                (
                    has_more,
                    after.is_some() || offset.map(|o| o > 0).unwrap_or(false),
                )
            };

            // Resolve the connection selection.
            let conn_type = format!("{type_name}Connection");
            let conn_fields = self.flatten(&field.selection_set, &conn_type)?;
            let needs_total = conn_fields.iter().any(|f| f.name == "totalCount");
            let total = if needs_total {
                let mut cp = Params::new();
                let mut conds = Vec::new();
                for (col, v) in &extra {
                    conds.push(format!("\"{col}\" = {}", cp.bind(v.clone())));
                }
                if let Some(f) = &filter {
                    conds.push(self.compile_filter(ti, f, &mut cp)?);
                }
                let where_sql = if conds.is_empty() {
                    String::new()
                } else {
                    format!(" WHERE {}", conds.join(" AND "))
                };
                let sql = format!(
                    "SELECT count(*) AS c FROM \"public\".\"{}\"{}",
                    ti.table.name, where_sql
                );
                let crs = self.sql(&sql, cp.values).await?;
                crs.rows
                    .first()
                    .and_then(|r| r.first())
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            } else {
                0
            };

            let mut out = Map::new();
            for cf in &conn_fields {
                let key = Self::response_key(cf);
                let value = match cf.name.as_str() {
                    "__typename" => Json::String(conn_type.clone()),
                    "totalCount" => Json::from(total),
                    "pageInfo" => self.resolve_page_info(ti, &rs, cf, has_next, has_prev)?,
                    "edges" => {
                        let edge_type = format!("{type_name}Edge");
                        let edge_fields = self.flatten(&cf.selection_set, &edge_type)?;
                        let mut edges = Vec::with_capacity(rs.rows.len());
                        for row in &rs.rows {
                            let mut edge = Map::new();
                            for ef in &edge_fields {
                                let ekey = Self::response_key(ef);
                                let evalue = match ef.name.as_str() {
                                    "__typename" => Json::String(edge_type.clone()),
                                    "cursor" => Json::String(self.row_cursor(ti, &rs, row)?),
                                    "node" => {
                                        self.resolve_row(ti, &rs, row, &ef.selection_set, depth + 1)
                                            .await?
                                    }
                                    other => {
                                        return Err(GqlError::new(format!(
                                            "Unknown field \"{other}\" on type \"{edge_type}\""
                                        )));
                                    }
                                };
                                merge_into(&mut edge, ekey, evalue);
                            }
                            edges.push(Json::Object(edge));
                        }
                        Json::Array(edges)
                    }
                    other => {
                        return Err(GqlError::new(format!(
                            "Unknown field \"{other}\" on type \"{conn_type}\""
                        )));
                    }
                };
                merge_into(&mut out, key, value);
            }
            Ok(Json::Object(out))
        })
    }

    fn resolve_page_info(
        &self,
        ti: &TableInfo,
        rs: &RowSet,
        field: &GqlField,
        has_next: bool,
        has_prev: bool,
    ) -> GResult<Json> {
        let fields = self.flatten(&field.selection_set, "PageInfo")?;
        let mut out = Map::new();
        for f in &fields {
            let key = Self::response_key(f);
            let value = match f.name.as_str() {
                "__typename" => Json::String("PageInfo".into()),
                "hasNextPage" => Json::Bool(has_next),
                "hasPreviousPage" => Json::Bool(has_prev),
                "startCursor" => match rs.rows.first() {
                    Some(row) => Json::String(self.row_cursor(ti, rs, row)?),
                    None => Json::Null,
                },
                "endCursor" => match rs.rows.last() {
                    Some(row) => Json::String(self.row_cursor(ti, rs, row)?),
                    None => Json::Null,
                },
                other => {
                    return Err(GqlError::new(format!(
                        "Unknown field \"{other}\" on type \"PageInfo\""
                    )));
                }
            };
            merge_into(&mut out, key, value);
        }
        Ok(Json::Object(out))
    }

    fn row_pk_json(&self, ti: &TableInfo, rs: &RowSet, row: &[SqlValue]) -> GResult<Vec<Json>> {
        ti.pk_cols
            .iter()
            .map(|(col, _)| {
                rs.get(row, col)
                    .map(SqlValue::encode_json)
                    .ok_or_else(|| GqlError::new(format!("internal: missing pk column {col}")))
            })
            .collect()
    }

    fn row_cursor(&self, ti: &TableInfo, rs: &RowSet, row: &[SqlValue]) -> GResult<String> {
        Ok(encode_cursor(&self.row_pk_json(ti, rs, row)?))
    }

    /// Lexicographic keyset predicate on the primary key.
    fn cursor_predicate(
        &self,
        ti: &TableInfo,
        cursor: &str,
        op: &str,
        p: &mut Params,
    ) -> GResult<String> {
        let values = decode_cursor(cursor)?;
        if values.len() != ti.pk_cols.len() {
            return Err(GqlError::new(
                "cursor does not match the table's primary key",
            ));
        }
        let coerced: Vec<SqlValue> = ti
            .pk_cols
            .iter()
            .zip(values.iter())
            .map(|((_, ty), v)| coerce_input(v, ty, "cursor"))
            .collect::<GResult<Vec<_>>>()?;
        let mut alts = Vec::new();
        for i in 0..ti.pk_cols.len() {
            let mut parts = Vec::new();
            for ((col, _), val) in ti.pk_cols.iter().zip(coerced.iter()).take(i) {
                parts.push(format!("\"{col}\" = {}", p.bind(val.clone())));
            }
            let (col, _) = &ti.pk_cols[i];
            parts.push(format!("\"{col}\" {op} {}", p.bind(coerced[i].clone())));
            alts.push(format!("({})", parts.join(" AND ")));
        }
        Ok(format!("({})", alts.join(" OR ")))
    }

    fn parse_order_by(
        &self,
        ti: &TableInfo,
        value: Option<&Json>,
    ) -> GResult<Vec<(String, OrderDir)>> {
        let Some(v) = value else {
            return Ok(Vec::new());
        };
        if v.is_null() {
            return Ok(Vec::new());
        }
        let Json::Array(items) = v else {
            return Err(GqlError::new("\"orderBy\" must be a list of input objects"));
        };
        let mut out = Vec::new();
        for item in items {
            let Json::Object(obj) = item else {
                return Err(GqlError::new("each orderBy entry must be an input object"));
            };
            if obj.len() != 1 {
                return Err(GqlError::new(
                    "each orderBy entry must set exactly one column (input object field order \
                     is not preserved; use one list element per column)",
                ));
            }
            let (col, dir) = obj.iter().next().expect("len checked");
            let column = ti.column(col).ok_or_else(|| {
                GqlError::new(format!(
                    "unknown or non-orderable column \"{col}\" in orderBy for \"{}\"",
                    ti.table.name
                ))
            })?;
            if !column.orderable() {
                return Err(GqlError::new(format!(
                    "column \"{col}\" ({}) cannot be ordered",
                    column.scalar.name()
                )));
            }
            let dir = dir.as_str().and_then(OrderDir::parse).ok_or_else(|| {
                GqlError::new(
                    "orderBy direction must be one of AscNullsFirst, AscNullsLast, \
                         DescNullsFirst, DescNullsLast",
                )
            })?;
            out.push((col.clone(), dir));
        }
        Ok(out)
    }

    // -- filters -----------------------------------------------------------------

    /// Compile a `<t>Filter` input object to a single parenthesized SQL clause.
    fn compile_filter(
        &self,
        ti: &TableInfo,
        obj: &Map<String, Json>,
        p: &mut Params,
    ) -> GResult<String> {
        let mut clauses = Vec::new();
        for (key, value) in obj {
            match key.as_str() {
                "and" | "or" => {
                    let Json::Array(items) = value else {
                        return Err(GqlError::new(format!(
                            "\"{key}\" must be a list of filter objects"
                        )));
                    };
                    let mut subs = Vec::new();
                    for item in items {
                        let Json::Object(sub) = item else {
                            return Err(GqlError::new(format!(
                                "\"{key}\" entries must be filter objects"
                            )));
                        };
                        subs.push(self.compile_filter(ti, sub, p)?);
                    }
                    let joined = if subs.is_empty() {
                        if key == "and" {
                            "TRUE".to_string()
                        } else {
                            "FALSE".to_string()
                        }
                    } else {
                        subs.join(if key == "and" { " AND " } else { " OR " })
                    };
                    clauses.push(format!("({joined})"));
                }
                "not" => {
                    let Json::Object(sub) = value else {
                        return Err(GqlError::new("\"not\" must be a filter object"));
                    };
                    clauses.push(format!("(NOT {})", self.compile_filter(ti, sub, p)?));
                }
                col => {
                    let column = ti.column(col).ok_or_else(|| {
                        GqlError::new(format!(
                            "unknown or non-filterable column \"{col}\" in filter for \"{}\"",
                            ti.table.name
                        ))
                    })?;
                    if !column.filterable() {
                        return Err(GqlError::new(format!(
                            "column \"{col}\" ({}) cannot be filtered",
                            column.scalar.name()
                        )));
                    }
                    let Json::Object(comps) = value else {
                        return Err(GqlError::new(format!(
                            "filter for column \"{col}\" must be an input object of comparators"
                        )));
                    };
                    for (comp, cv) in comps {
                        clauses.push(self.compile_comparator(column, comp, cv, p)?);
                    }
                }
            }
        }
        if clauses.is_empty() {
            return Ok("(TRUE)".to_string());
        }
        Ok(format!("({})", clauses.join(" AND ")))
    }

    fn compile_comparator(
        &self,
        column: &GqlColumn,
        comp: &str,
        value: &Json,
        p: &mut Params,
    ) -> GResult<String> {
        let col = format!("\"{}\"", column.name);
        let what = format!("filter on \"{}\"", column.name);
        let scalar = column.scalar;
        let ordered = scalar.is_ordered();
        let stringy = scalar == Scalar::String;
        let bind = |p: &mut Params, v: &Json| -> GResult<String> {
            Ok(p.bind(coerce_input(v, &column.ty, &what)?))
        };
        Ok(match comp {
            "eq" => format!("{col} = {}", bind(p, value)?),
            "neq" => format!("{col} <> {}", bind(p, value)?),
            "gt" if ordered => format!("{col} > {}", bind(p, value)?),
            "gte" if ordered => format!("{col} >= {}", bind(p, value)?),
            "lt" if ordered => format!("{col} < {}", bind(p, value)?),
            "lte" if ordered => format!("{col} <= {}", bind(p, value)?),
            "like" if stringy => {
                let s = value
                    .as_str()
                    .ok_or_else(|| GqlError::new(format!("{what}: like takes a String")))?;
                format!("{col} LIKE {}", p.bind(SqlValue::Text(s.to_string())))
            }
            "ilike" if stringy => {
                let s = value
                    .as_str()
                    .ok_or_else(|| GqlError::new(format!("{what}: ilike takes a String")))?;
                format!("{col} ILIKE {}", p.bind(SqlValue::Text(s.to_string())))
            }
            "startsWith" if stringy => {
                let s = value
                    .as_str()
                    .ok_or_else(|| GqlError::new(format!("{what}: startsWith takes a String")))?;
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                format!(
                    "{col} LIKE {} ESCAPE '\\'",
                    p.bind(SqlValue::Text(format!("{escaped}%")))
                )
            }
            "is" => match value.as_str() {
                Some("NULL") => format!("{col} IS NULL"),
                Some("NOT_NULL") => format!("{col} IS NOT NULL"),
                _ => {
                    return Err(GqlError::new(format!(
                        "{what}: \"is\" takes NULL or NOT_NULL"
                    )));
                }
            },
            "in" => {
                let Json::Array(items) = value else {
                    return Err(GqlError::new(format!("{what}: \"in\" takes a list")));
                };
                if items.is_empty() {
                    "FALSE".to_string()
                } else {
                    let binds = items
                        .iter()
                        .map(|v| bind(p, v))
                        .collect::<GResult<Vec<_>>>()?;
                    format!("{col} IN ({})", binds.join(", "))
                }
            }
            other => {
                return Err(GqlError::new(format!(
                    "unsupported comparator \"{other}\" for column \"{}\" ({})",
                    column.name,
                    scalar.name()
                )));
            }
        })
    }

    // -- row resolution -----------------------------------------------------------

    fn resolve_row<'x>(
        &'x self,
        ti: &'x TableInfo,
        rs: &'x RowSet,
        row: &'x [SqlValue],
        sel: &'x GqlSelSet,
        depth: usize,
    ) -> BoxFuture<'x, GResult<Json>> {
        Box::pin(async move {
            if depth > MAX_DEPTH {
                return Err(GqlError::new(format!(
                    "query exceeds the maximum relationship depth of {MAX_DEPTH}"
                )));
            }
            let type_name = &ti.table.name;
            let fields = self.flatten(sel, type_name)?;
            let mut out = Map::new();
            for f in &fields {
                let key = Self::response_key(f);
                let value = match f.name.as_str() {
                    "__typename" => Json::String(type_name.clone()),
                    "nodeId" => Json::String(encode_node_id(
                        "public",
                        type_name,
                        &self.row_pk_json(ti, rs, row)?,
                    )),
                    name => {
                        if let Some(_col) = ti.column(name) {
                            match rs.get(row, name) {
                                Some(v) => render_value(v),
                                None => Json::Null,
                            }
                        } else if let Some(rel) = ti.rels.get(name) {
                            self.resolve_rel(rs, row, rel, f, depth).await?
                        } else {
                            return Err(GqlError::new(format!(
                                "Unknown field \"{name}\" on type \"{type_name}\""
                            )));
                        }
                    }
                };
                merge_into(&mut out, key, value);
            }
            Ok(Json::Object(out))
        })
    }

    async fn resolve_rel(
        &self,
        rs: &RowSet,
        row: &[SqlValue],
        rel: &Rel,
        field: &GqlField,
        depth: usize,
    ) -> GResult<Json> {
        match rel {
            Rel::Parent {
                ref_type,
                fk_cols,
                ref_cols,
            } => {
                self.args_map(field, &[])?; // parent object fields take no args
                let parent = self
                    .schema
                    .tables
                    .get(ref_type)
                    .ok_or_else(|| GqlError::new(format!("unknown type \"{ref_type}\"")))?;
                let mut values = Vec::with_capacity(fk_cols.len());
                for col in fk_cols {
                    match rs.get(row, col) {
                        Some(SqlValue::Null) | None => return Ok(Json::Null),
                        Some(v) => values.push(v.clone()),
                    }
                }
                let mut p = Params::new();
                let conds: Vec<String> = ref_cols
                    .iter()
                    .zip(values)
                    .map(|(rc, v)| format!("\"{rc}\" = {}", p.bind(v)))
                    .collect();
                let sql = format!(
                    "SELECT * FROM \"public\".\"{}\" WHERE {} LIMIT 1",
                    parent.table.name,
                    conds.join(" AND ")
                );
                let prs = self.sql(&sql, p.values).await?;
                match prs.rows.first().cloned() {
                    // RLS may hide the parent row: null, truthfully.
                    None => Ok(Json::Null),
                    Some(prow) => {
                        self.resolve_row(parent, &prs, &prow, &field.selection_set, depth + 1)
                            .await
                    }
                }
            }
            Rel::Child {
                child_type,
                fk_cols,
                ref_cols,
            } => {
                let mut extra = Vec::with_capacity(fk_cols.len());
                for (fk, rc) in fk_cols.iter().zip(ref_cols.iter()) {
                    let v = rs.get(row, rc).cloned().unwrap_or(SqlValue::Null);
                    extra.push((fk.clone(), v));
                }
                self.resolve_collection(child_type.clone(), field, extra, depth + 1)
                    .await
            }
        }
    }

    // -- mutations ----------------------------------------------------------------

    async fn resolve_insert(&self, type_name: &str, field: &GqlField) -> GResult<Json> {
        let ti = self
            .schema
            .tables
            .get(type_name)
            .ok_or_else(|| GqlError::new(format!("unknown type \"{type_name}\"")))?;
        let args = self.args_map(field, &["objects"])?;
        let Some(Json::Array(objects)) = args.get("objects") else {
            return Err(GqlError::new(
                "insert requires an \"objects\" list of input objects",
            ));
        };
        let mut rows: Vec<&Map<String, Json>> = Vec::with_capacity(objects.len());
        for o in objects {
            match o {
                Json::Object(m) => rows.push(m),
                _ => {
                    return Err(GqlError::new(
                        "each element of \"objects\" must be an input object",
                    ));
                }
            }
        }
        if rows.is_empty() {
            let resp_type = format!("{type_name}InsertResponse");
            return self
                .resolve_mutation_response(
                    ti,
                    &resp_type,
                    RowSet {
                        idx: HashMap::new(),
                        rows: Vec::new(),
                    },
                    field,
                )
                .await;
        }

        // Column set = union of all object keys, in a stable order.
        let mut column_set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for r in &rows {
            for k in r.keys() {
                column_set.insert(k.as_str());
            }
        }
        let mut columns = Vec::with_capacity(column_set.len());
        for c in column_set {
            let col = ti.column(c).ok_or_else(|| {
                GqlError::new(format!(
                    "Unknown field \"{c}\" on input type \"{type_name}InsertInput\""
                ))
            })?;
            columns.push(col);
        }
        let mut p = Params::new();
        let mut tuples = Vec::with_capacity(rows.len());
        for r in &rows {
            let mut cells = Vec::with_capacity(columns.len());
            for col in &columns {
                match r.get(&col.name) {
                    Some(v) => {
                        let what = format!("insert value for \"{}\"", col.name);
                        cells.push(p.bind(coerce_input(v, &col.ty, &what)?));
                    }
                    None => cells.push("DEFAULT".to_string()),
                }
            }
            tuples.push(format!("({})", cells.join(", ")));
        }
        let column_list = columns
            .iter()
            .map(|c| format!("\"{}\"", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO \"public\".\"{}\" ({column_list}) VALUES {} RETURNING *",
            ti.table.name,
            tuples.join(", ")
        );
        let rs = self.sql(&sql, p.values).await?;
        let resp_type = format!("{type_name}InsertResponse");
        self.resolve_mutation_response(ti, &resp_type, rs, field)
            .await
    }

    /// UPDATE / DELETE with pg_graphql `atMost` semantics: run inside a
    /// transaction, count the affected rows, and roll back with a GraphQL
    /// error when more than `atMost` rows would be touched.
    async fn resolve_update_delete(
        &self,
        type_name: &str,
        field: &GqlField,
        is_update: bool,
    ) -> GResult<Json> {
        let ti = self
            .schema
            .tables
            .get(type_name)
            .ok_or_else(|| GqlError::new(format!("unknown type \"{type_name}\"")))?;
        let allowed: &[&str] = if is_update {
            &["set", "filter", "atMost"]
        } else {
            &["filter", "atMost"]
        };
        let args = self.args_map(field, allowed)?;
        let at_most = match args.get("atMost") {
            None | Some(Json::Null) => 1,
            Some(v) => v
                .as_i64()
                .filter(|n| *n >= 0)
                .ok_or_else(|| GqlError::new("\"atMost\" must be a non-negative Int"))?,
        };
        let filter = match args.get("filter") {
            None | Some(Json::Null) => None,
            Some(Json::Object(o)) => Some(o.clone()),
            Some(_) => return Err(GqlError::new("\"filter\" must be an input object")),
        };

        let mut p = Params::new();
        let mut sql = if is_update {
            let Some(Json::Object(set)) = args.get("set") else {
                return Err(GqlError::new("update requires a \"set\" input object"));
            };
            if set.is_empty() {
                return Err(GqlError::new("\"set\" must assign at least one column"));
            }
            let mut sets = Vec::with_capacity(set.len());
            for (colname, v) in set {
                let col = ti.column(colname).ok_or_else(|| {
                    GqlError::new(format!(
                        "Unknown field \"{colname}\" on input type \"{type_name}UpdateInput\""
                    ))
                })?;
                let what = format!("update value for \"{colname}\"");
                sets.push(format!(
                    "\"{colname}\" = {}",
                    p.bind(coerce_input(v, &col.ty, &what)?)
                ));
            }
            format!(
                "UPDATE \"public\".\"{}\" SET {}",
                ti.table.name,
                sets.join(", ")
            )
        } else {
            format!("DELETE FROM \"public\".\"{}\"", ti.table.name)
        };
        if let Some(f) = &filter {
            let clause = self.compile_filter(ti, f, &mut p)?;
            sql.push_str(&format!(" WHERE {clause}"));
        }
        sql.push_str(" RETURNING *");

        // One transaction per mutation field, in the caller's session (role +
        // claims bound) so RLS governs the statement.
        let mut session = Session::new(self.state.db.clone(), self.auth.role.clone());
        session.set_var("request.jwt.claims", &self.auth.claims_json());
        session
            .execute("BEGIN")
            .await
            .map_err(|e| GqlError::new(e.to_string()))?;
        let outcome = async {
            let prepared = session
                .prepare(&sql)
                .map_err(|e| GqlError::new(e.to_string()))?;
            session
                .execute_one(&prepared.statement, &p.values)
                .await
                .map_err(|e| GqlError::new(e.to_string()))
        }
        .await;
        let result = match outcome {
            Ok(r) => r,
            Err(e) => {
                let _ = session.execute("ROLLBACK").await;
                return Err(e);
            }
        };
        let rs = match RowSet::from_exec(result) {
            Ok(rs) => rs,
            Err(e) => {
                let _ = session.execute("ROLLBACK").await;
                return Err(e);
            }
        };
        if rs.rows.len() as i64 > at_most {
            let _ = session.execute("ROLLBACK").await;
            return Err(GqlError::new(if is_update {
                "update impacts too many records"
            } else {
                "delete impacts too many records"
            }));
        }
        session
            .execute("COMMIT")
            .await
            .map_err(|e| GqlError::new(e.to_string()))?;

        let resp_type = format!(
            "{type_name}{}Response",
            if is_update { "Update" } else { "Delete" }
        );
        self.resolve_mutation_response(ti, &resp_type, rs, field)
            .await
    }

    async fn resolve_mutation_response(
        &self,
        ti: &TableInfo,
        resp_type: &str,
        rs: RowSet,
        field: &GqlField,
    ) -> GResult<Json> {
        let fields = self.flatten(&field.selection_set, resp_type)?;
        let mut out = Map::new();
        for f in &fields {
            let key = Self::response_key(f);
            let value = match f.name.as_str() {
                "__typename" => Json::String(resp_type.to_string()),
                "affectedCount" => Json::from(rs.rows.len() as i64),
                "records" => {
                    let mut records = Vec::with_capacity(rs.rows.len());
                    for row in &rs.rows {
                        records.push(self.resolve_row(ti, &rs, row, &f.selection_set, 1).await?);
                    }
                    Json::Array(records)
                }
                other => {
                    return Err(GqlError::new(format!(
                        "Unknown field \"{other}\" on type \"{resp_type}\""
                    )));
                }
            };
            merge_into(&mut out, key, value);
        }
        Ok(Json::Object(out))
    }

    // -- introspection projection ---------------------------------------------

    /// Project a selection set over pre-built introspection data. Objects
    /// carry `__typename`; named-type references carry a `__deref` marker so
    /// full type details can be fetched from the type map when requested.
    fn project(&self, value: &Json, sel: &GqlSelSet, intro: &Intro) -> GResult<Json> {
        match value {
            Json::Null => Ok(Json::Null),
            Json::Array(items) => Ok(Json::Array(
                items
                    .iter()
                    .map(|i| self.project(i, sel, intro))
                    .collect::<GResult<Vec<_>>>()?,
            )),
            Json::Object(obj) => {
                let tn = obj
                    .get("__typename")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let fields = self.flatten(sel, &tn)?;
                let mut out = Map::new();
                for f in &fields {
                    let key = Self::response_key(f);
                    let resolved = if f.name == "__typename" {
                        Json::String(tn.clone())
                    } else {
                        let looked_up = match obj.get(f.name.as_str()) {
                            Some(v) => Some(v.clone()),
                            None => obj
                                .get("__deref")
                                .and_then(Json::as_str)
                                .and_then(|n| intro.types_by_name.get(n))
                                .and_then(|full| full.get(f.name.as_str()))
                                .cloned(),
                        };
                        match looked_up {
                            Some(v) => {
                                if f.selection_set.items.is_empty() {
                                    v
                                } else {
                                    self.project(&v, &f.selection_set, intro)?
                                }
                            }
                            None => {
                                if intro_field_valid(&tn, &f.name) {
                                    Json::Null
                                } else {
                                    return Err(GqlError::new(format!(
                                        "Unknown field \"{}\" on type \"{tn}\"",
                                        f.name
                                    )));
                                }
                            }
                        }
                    };
                    merge_into(&mut out, key, resolved);
                }
                Ok(Json::Object(out))
            }
            leaf => Ok(leaf.clone()),
        }
    }
}

/// Merge a resolved value into the response map (fields merged by key; objects
/// deep-merge, everything else keeps the first resolution).
fn merge_into(map: &mut Map<String, Json>, key: String, value: Json) {
    match map.get_mut(&key) {
        None => {
            map.insert(key, value);
        }
        Some(existing) => merge_json(existing, value),
    }
}

fn merge_json(a: &mut Json, b: Json) {
    match (a, b) {
        (Json::Object(ao), Json::Object(bo)) => {
            for (k, v) in bo {
                match ao.get_mut(&k) {
                    Some(av) => merge_json(av, v),
                    None => {
                        ao.insert(k, v);
                    }
                }
            }
        }
        (Json::Array(aa), Json::Array(ba)) if aa.len() == ba.len() => {
            for (av, bv) in aa.iter_mut().zip(ba) {
                merge_json(av, bv);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Introspection data
// ---------------------------------------------------------------------------

struct Intro {
    schema_json: Json,
    types_by_name: Map<String, Json>,
}

/// Fields legal on each introspection type (missing ones resolve to null —
/// e.g. `fields` on a SCALAR, `specifiedByURL` everywhere).
fn intro_field_valid(type_name: &str, field: &str) -> bool {
    match type_name {
        "__Schema" => matches!(
            field,
            "description"
                | "types"
                | "queryType"
                | "mutationType"
                | "subscriptionType"
                | "directives"
        ),
        "__Type" => matches!(
            field,
            "kind"
                | "name"
                | "description"
                | "fields"
                | "interfaces"
                | "possibleTypes"
                | "enumValues"
                | "inputFields"
                | "ofType"
                | "specifiedByURL"
                | "isOneOf"
        ),
        "__Field" => matches!(
            field,
            "name" | "description" | "args" | "type" | "isDeprecated" | "deprecationReason"
        ),
        "__InputValue" => matches!(
            field,
            "name" | "description" | "type" | "defaultValue" | "isDeprecated" | "deprecationReason"
        ),
        "__EnumValue" => matches!(
            field,
            "name" | "description" | "isDeprecated" | "deprecationReason"
        ),
        "__Directive" => matches!(
            field,
            "name" | "description" | "locations" | "args" | "isRepeatable"
        ),
        _ => false,
    }
}

// Type-reference JSON builders. Named references carry `__deref` so a client
// selecting more than kind/name/ofType gets the full type.
fn t_named(kind: &str, name: &str) -> Json {
    json!({
        "__typename": "__Type",
        "kind": kind,
        "name": name,
        "ofType": Json::Null,
        "__deref": name,
    })
}

fn t_scalar(name: &str) -> Json {
    t_named("SCALAR", name)
}

fn t_object(name: &str) -> Json {
    t_named("OBJECT", name)
}

fn t_input(name: &str) -> Json {
    t_named("INPUT_OBJECT", name)
}

fn t_nn(of: Json) -> Json {
    json!({"__typename": "__Type", "kind": "NON_NULL", "name": Json::Null, "ofType": of})
}

fn t_list(of: Json) -> Json {
    json!({"__typename": "__Type", "kind": "LIST", "name": Json::Null, "ofType": of})
}

fn field_json(name: &str, args: Vec<Json>, ty: Json, description: Option<&str>) -> Json {
    json!({
        "__typename": "__Field",
        "name": name,
        "description": description,
        "args": args,
        "type": ty,
        "isDeprecated": false,
        "deprecationReason": Json::Null,
    })
}

fn input_value(name: &str, ty: Json, default_value: Option<&str>) -> Json {
    json!({
        "__typename": "__InputValue",
        "name": name,
        "description": Json::Null,
        "type": ty,
        "defaultValue": default_value,
        "isDeprecated": false,
        "deprecationReason": Json::Null,
    })
}

fn enum_value(name: &str) -> Json {
    json!({
        "__typename": "__EnumValue",
        "name": name,
        "description": Json::Null,
        "isDeprecated": false,
        "deprecationReason": Json::Null,
    })
}

/// A full type entry. Field-kind slots not applicable to `kind` stay null,
/// matching the introspection spec.
fn full_type(kind: &str, name: &str, description: Option<&str>) -> Map<String, Json> {
    let mut m = Map::new();
    m.insert("__typename".into(), json!("__Type"));
    m.insert("kind".into(), json!(kind));
    m.insert("name".into(), json!(name));
    m.insert("description".into(), json!(description));
    m.insert("fields".into(), Json::Null);
    m.insert("interfaces".into(), Json::Null);
    m.insert("possibleTypes".into(), Json::Null);
    m.insert("enumValues".into(), Json::Null);
    m.insert("inputFields".into(), Json::Null);
    m.insert("ofType".into(), Json::Null);
    m.insert("specifiedByURL".into(), Json::Null);
    m.insert("isOneOf".into(), Json::Null);
    if matches!(kind, "OBJECT" | "INTERFACE") {
        m.insert("interfaces".into(), json!([]));
    }
    m
}

/// The column's output type reference.
fn column_typeref(col: &GqlColumn) -> Json {
    let base = t_scalar(col.scalar.name());
    let inner = if col.is_list { t_list(base) } else { base };
    if col.nullable { inner } else { t_nn(inner) }
}

/// The column's input type reference (always nullable — inserts may omit).
fn column_input_typeref(col: &GqlColumn) -> Json {
    let base = t_scalar(col.scalar.name());
    if col.is_list { t_list(base) } else { base }
}

fn collection_args(type_name: &str) -> Vec<Json> {
    vec![
        input_value("first", t_scalar("Int"), None),
        input_value("last", t_scalar("Int"), None),
        input_value("before", t_scalar("Cursor"), None),
        input_value("after", t_scalar("Cursor"), None),
        input_value("offset", t_scalar("Int"), None),
        input_value("filter", t_input(&format!("{type_name}Filter")), None),
        input_value(
            "orderBy",
            t_list(t_nn(t_input(&format!("{type_name}OrderBy")))),
            None,
        ),
    ]
}

/// Build the full introspection data for a reflected schema.
fn build_introspection(schema: &RelSchema) -> Intro {
    let mut types: Vec<Json> = Vec::new();

    // Built-in and pg_graphql scalars.
    let scalars: &[(&str, &str)] = &[
        ("Int", "A signed 32-bit integer"),
        ("Float", "A signed double-precision floating-point value"),
        ("String", "A UTF-8 character sequence"),
        ("Boolean", "true or false"),
        ("ID", "A globally unique identifier"),
        (
            "BigInt",
            "An arbitrary-precision integer, serialized as a string",
        ),
        (
            "BigFloat",
            "An arbitrary-precision decimal, serialized as a string",
        ),
        ("UUID", "A universally unique identifier"),
        ("Date", "A date (ISO 8601), e.g. 2021-06-11"),
        ("Time", "A time without time zone (ISO 8601)"),
        ("Datetime", "A date and time (ISO 8601)"),
        ("JSON", "A JSON value, serialized as a string"),
        (
            "Opaque",
            "A value not directly representable; bytea is base64",
        ),
        ("Cursor", "An opaque pagination cursor"),
    ];
    for (name, desc) in scalars {
        types.push(Json::Object(full_type("SCALAR", name, Some(desc))));
    }

    // Enums.
    {
        let mut t = full_type("ENUM", "OrderByDirection", Some("Column order direction"));
        t.insert(
            "enumValues".into(),
            json!([
                enum_value("AscNullsFirst"),
                enum_value("AscNullsLast"),
                enum_value("DescNullsFirst"),
                enum_value("DescNullsLast"),
            ]),
        );
        types.push(Json::Object(t));

        let mut t = full_type("ENUM", "FilterIs", None);
        t.insert(
            "enumValues".into(),
            json!([enum_value("NULL"), enum_value("NOT_NULL")]),
        );
        types.push(Json::Object(t));
    }

    // Node interface.
    {
        let mut t = full_type("INTERFACE", "Node", None);
        t.insert(
            "fields".into(),
            json!([field_json(
                "nodeId",
                vec![],
                t_nn(t_scalar("ID")),
                Some("Globally unique identifier")
            )]),
        );
        t.insert(
            "possibleTypes".into(),
            Json::Array(schema.tables.keys().map(|n| t_object(n)).collect()),
        );
        types.push(Json::Object(t));
    }

    // PageInfo.
    {
        let mut t = full_type("OBJECT", "PageInfo", None);
        t.insert(
            "fields".into(),
            json!([
                field_json("endCursor", vec![], t_scalar("String"), None),
                field_json("hasNextPage", vec![], t_nn(t_scalar("Boolean")), None),
                field_json("hasPreviousPage", vec![], t_nn(t_scalar("Boolean")), None),
                field_json("startCursor", vec![], t_scalar("String"), None),
            ]),
        );
        types.push(Json::Object(t));
    }

    // Per-scalar filter input types.
    let filter_scalars = [
        Scalar::Int,
        Scalar::Float,
        Scalar::String,
        Scalar::Boolean,
        Scalar::BigInt,
        Scalar::BigFloat,
        Scalar::Uuid,
        Scalar::Date,
        Scalar::Time,
        Scalar::Datetime,
    ];
    for s in filter_scalars {
        let sname = s.name();
        let mut fields = vec![
            input_value("eq", t_scalar(sname), None),
            input_value("neq", t_scalar(sname), None),
            input_value("in", t_list(t_nn(t_scalar(sname))), None),
            input_value("is", t_named("ENUM", "FilterIs"), None),
        ];
        if s.is_ordered() {
            for op in ["gt", "gte", "lt", "lte"] {
                fields.push(input_value(op, t_scalar(sname), None));
            }
        }
        if s == Scalar::String {
            for op in ["like", "ilike", "startsWith"] {
                fields.push(input_value(op, t_scalar("String"), None));
            }
        }
        let mut t = full_type("INPUT_OBJECT", &format!("{sname}Filter"), None);
        t.insert("inputFields".into(), Json::Array(fields));
        types.push(Json::Object(t));
    }

    // Per-table types.
    let mut query_fields = vec![field_json(
        "node",
        vec![input_value("nodeId", t_nn(t_scalar("ID")), None)],
        t_named("INTERFACE", "Node"),
        Some("Retrieve a record by its globally unique ID"),
    )];
    let mut mutation_fields = Vec::new();

    for (name, ti) in &schema.tables {
        // Object type.
        let mut fields = vec![field_json(
            "nodeId",
            vec![],
            t_nn(t_scalar("ID")),
            Some("Globally unique identifier"),
        )];
        for col in &ti.columns {
            fields.push(field_json(&col.name, vec![], column_typeref(col), None));
        }
        for (rel_name, rel) in &ti.rels {
            match rel {
                Rel::Parent { ref_type, .. } => {
                    fields.push(field_json(rel_name, vec![], t_object(ref_type), None));
                }
                Rel::Child { child_type, .. } => {
                    fields.push(field_json(
                        rel_name,
                        collection_args(child_type),
                        t_object(&format!("{child_type}Connection")),
                        None,
                    ));
                }
            }
        }
        let mut t = full_type("OBJECT", name, None);
        t.insert("fields".into(), Json::Array(fields));
        t.insert("interfaces".into(), json!([t_named("INTERFACE", "Node")]));
        types.push(Json::Object(t));

        // Edge.
        let mut t = full_type("OBJECT", &format!("{name}Edge"), None);
        t.insert(
            "fields".into(),
            json!([
                field_json("cursor", vec![], t_nn(t_scalar("String")), None),
                field_json("node", vec![], t_nn(t_object(name)), None),
            ]),
        );
        types.push(Json::Object(t));

        // Connection.
        let mut t = full_type("OBJECT", &format!("{name}Connection"), None);
        t.insert(
            "fields".into(),
            json!([
                field_json(
                    "edges",
                    vec![],
                    t_nn(t_list(t_nn(t_object(&format!("{name}Edge"))))),
                    None
                ),
                field_json("pageInfo", vec![], t_nn(t_object("PageInfo")), None),
                field_json(
                    "totalCount",
                    vec![],
                    t_nn(t_scalar("Int")),
                    Some("The total number of records matching the filter")
                ),
            ]),
        );
        types.push(Json::Object(t));

        // Filter.
        let mut ffields = Vec::new();
        for col in ti.columns.iter().filter(|c| c.filterable()) {
            ffields.push(input_value(
                &col.name,
                t_input(&format!("{}Filter", col.scalar.name())),
                None,
            ));
        }
        ffields.push(input_value(
            "and",
            t_list(t_nn(t_input(&format!("{name}Filter")))),
            None,
        ));
        ffields.push(input_value(
            "or",
            t_list(t_nn(t_input(&format!("{name}Filter")))),
            None,
        ));
        ffields.push(input_value("not", t_input(&format!("{name}Filter")), None));
        let mut t = full_type("INPUT_OBJECT", &format!("{name}Filter"), None);
        t.insert("inputFields".into(), Json::Array(ffields));
        types.push(Json::Object(t));

        // OrderBy.
        let ofields: Vec<Json> = ti
            .columns
            .iter()
            .filter(|c| c.orderable())
            .map(|c| input_value(&c.name, t_named("ENUM", "OrderByDirection"), None))
            .collect();
        let mut t = full_type("INPUT_OBJECT", &format!("{name}OrderBy"), None);
        t.insert("inputFields".into(), Json::Array(ofields));
        types.push(Json::Object(t));

        // InsertInput / UpdateInput.
        for suffix in ["InsertInput", "UpdateInput"] {
            let ifields: Vec<Json> = ti
                .columns
                .iter()
                .map(|c| input_value(&c.name, column_input_typeref(c), None))
                .collect();
            let mut t = full_type("INPUT_OBJECT", &format!("{name}{suffix}"), None);
            t.insert("inputFields".into(), Json::Array(ifields));
            types.push(Json::Object(t));
        }

        // Mutation responses.
        for suffix in ["InsertResponse", "UpdateResponse", "DeleteResponse"] {
            let mut t = full_type("OBJECT", &format!("{name}{suffix}"), None);
            t.insert(
                "fields".into(),
                json!([
                    field_json(
                        "affectedCount",
                        vec![],
                        t_nn(t_scalar("Int")),
                        Some("Count of the records impacted by the mutation")
                    ),
                    field_json(
                        "records",
                        vec![],
                        t_nn(t_list(t_nn(t_object(name)))),
                        Some("Array of records impacted by the mutation")
                    ),
                ]),
            );
            types.push(Json::Object(t));
        }

        // Query / Mutation fields.
        query_fields.push(field_json(
            &format!("{name}Collection"),
            collection_args(name),
            t_object(&format!("{name}Connection")),
            Some("A pagable collection of type `{name}`"),
        ));
        mutation_fields.push(field_json(
            &format!("insertInto{name}Collection"),
            vec![input_value(
                "objects",
                t_nn(t_list(t_nn(t_input(&format!("{name}InsertInput"))))),
                None,
            )],
            t_object(&format!("{name}InsertResponse")),
            None,
        ));
        mutation_fields.push(field_json(
            &format!("update{name}Collection"),
            vec![
                input_value("set", t_nn(t_input(&format!("{name}UpdateInput"))), None),
                input_value("filter", t_input(&format!("{name}Filter")), None),
                input_value("atMost", t_nn(t_scalar("Int")), Some("1")),
            ],
            t_object(&format!("{name}UpdateResponse")),
            None,
        ));
        mutation_fields.push(field_json(
            &format!("deleteFrom{name}Collection"),
            vec![
                input_value("filter", t_input(&format!("{name}Filter")), None),
                input_value("atMost", t_nn(t_scalar("Int")), Some("1")),
            ],
            t_object(&format!("{name}DeleteResponse")),
            None,
        ));
    }

    // Query and Mutation roots.
    {
        let mut t = full_type("OBJECT", "Query", Some("The root type for querying data"));
        t.insert("fields".into(), Json::Array(query_fields));
        types.push(Json::Object(t));
    }
    let has_mutations = !schema.tables.is_empty();
    if has_mutations {
        let mut t = full_type(
            "OBJECT",
            "Mutation",
            Some("The root type for creating and mutating data"),
        );
        t.insert("fields".into(), Json::Array(mutation_fields));
        types.push(Json::Object(t));
    }

    let mut types_by_name = Map::new();
    for t in &types {
        if let Some(name) = t.get("name").and_then(Json::as_str) {
            types_by_name.insert(name.to_string(), t.clone());
        }
    }

    let directives = json!([
        {
            "__typename": "__Directive",
            "name": "include",
            "description": "Include this field only when the `if` argument is true",
            "locations": ["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"],
            "args": [input_value("if", t_nn(t_scalar("Boolean")), None)],
            "isRepeatable": false,
        },
        {
            "__typename": "__Directive",
            "name": "skip",
            "description": "Skip this field when the `if` argument is true",
            "locations": ["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"],
            "args": [input_value("if", t_nn(t_scalar("Boolean")), None)],
            "isRepeatable": false,
        },
    ]);

    let schema_json = json!({
        "__typename": "__Schema",
        "description": Json::Null,
        "queryType": t_object("Query"),
        // No subscriptions in this slice — truthfully null.
        "subscriptionType": Json::Null,
        "mutationType": if has_mutations { t_object("Mutation") } else { Json::Null },
        "types": types,
        "directives": directives,
    });

    Intro {
        schema_json,
        types_by_name,
    }
}

// ---------------------------------------------------------------------------
// Order directions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderDir {
    AscNullsFirst,
    AscNullsLast,
    DescNullsFirst,
    DescNullsLast,
}

impl OrderDir {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "AscNullsFirst" => Some(OrderDir::AscNullsFirst),
            "AscNullsLast" => Some(OrderDir::AscNullsLast),
            "DescNullsFirst" => Some(OrderDir::DescNullsFirst),
            "DescNullsLast" => Some(OrderDir::DescNullsLast),
            _ => None,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            OrderDir::AscNullsFirst => "ASC NULLS FIRST",
            OrderDir::AscNullsLast => "ASC NULLS LAST",
            OrderDir::DescNullsFirst => "DESC NULLS FIRST",
            OrderDir::DescNullsLast => "DESC NULLS LAST",
        }
    }
}

/// Flip a SQL order direction for backward (`last`/`before`) pagination.
fn flip_dir(dir: &str) -> &'static str {
    match dir {
        "ASC NULLS FIRST" => "DESC NULLS LAST",
        "ASC NULLS LAST" => "DESC NULLS FIRST",
        "DESC NULLS FIRST" => "ASC NULLS LAST",
        "DESC NULLS LAST" => "ASC NULLS FIRST",
        _ => "ASC NULLS LAST",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        let vals = vec![json!(42), json!("abc")];
        let c = encode_cursor(&vals);
        assert_eq!(decode_cursor(&c).unwrap(), vals);
    }

    #[test]
    fn node_id_round_trips() {
        let id = encode_node_id("public", "todos", &[json!(7)]);
        let (schema, table, pks) = decode_node_id(&id).unwrap();
        assert_eq!(schema, "public");
        assert_eq!(table, "todos");
        assert_eq!(pks, vec![json!(7)]);
        assert!(decode_node_id("!!!").is_err());
    }

    #[test]
    fn gql_name_validation() {
        assert!(is_gql_name("blog_posts"));
        assert!(is_gql_name("_x1"));
        assert!(!is_gql_name("__meta"));
        assert!(!is_gql_name("1abc"));
        assert!(!is_gql_name("a-b"));
        assert!(!is_gql_name(""));
    }

    #[test]
    fn scalar_mapping() {
        assert_eq!(scalar_of(&SqlType::Integer), (Scalar::Int, false));
        assert_eq!(scalar_of(&SqlType::BigInt), (Scalar::BigInt, false));
        assert_eq!(
            scalar_of(&SqlType::Numeric {
                precision: None,
                scale: None
            }),
            (Scalar::BigFloat, false)
        );
        assert_eq!(scalar_of(&SqlType::Uuid), (Scalar::Uuid, false));
        assert_eq!(scalar_of(&SqlType::Jsonb), (Scalar::Json, false));
        assert_eq!(scalar_of(&SqlType::Bytea), (Scalar::Opaque, false));
        assert_eq!(
            scalar_of(&SqlType::Array(Box::new(SqlType::Text))),
            (Scalar::String, true)
        );
        // Exotic types degrade to String, never a crash.
        assert_eq!(scalar_of(&SqlType::Ltree), (Scalar::String, false));
        assert_eq!(scalar_of(&SqlType::HStore), (Scalar::String, false));
    }

    #[test]
    fn bigint_and_json_render_as_strings() {
        assert_eq!(render_value(&SqlValue::Int8(9)), json!("9"));
        assert_eq!(
            render_value(&SqlValue::Json(json!({"a": 1}))),
            json!("{\"a\":1}")
        );
        assert_eq!(render_value(&SqlValue::Int4(9)), json!(9));
    }

    #[test]
    fn order_direction_flip() {
        assert_eq!(flip_dir(OrderDir::AscNullsLast.sql()), "DESC NULLS FIRST");
        assert_eq!(flip_dir(OrderDir::DescNullsFirst.sql()), "ASC NULLS LAST");
    }
}
