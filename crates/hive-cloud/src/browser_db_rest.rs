//! libsql/Hrana + Upstash-style REST surface for `browser_db` replicas
//! (bn-browser-db-rest) — the fleet-replicated CRR database
//! (`crate::browser_db`) has, until now, been reachable ONLY through the
//! browser↔fleet `Op::CrrSync` protocol; this module gives it a plain HTTP
//! query interface too, so a server, a CI job, or `curl` can read/write the
//! same replica a tenant's browsers are syncing, without embedding a QUIC
//! client.
//!
//! # The "no guaranteed local replica" problem, and its fix
//!
//! Unlike managed SQLite (`DbKind::Sqlite`), a `browser_db` project has no
//! `Database` record and no `host_node` field — its module doc is explicit
//! that "there is deliberately NO owner-proxy HTTP arm: a file copy is not a
//! merge", so a replica FILE only exists on nodes that have actually served
//! a CRR round for that project. A REST request landing on a random node
//! (round-robin DNS) could hit one with no local copy at all.
//!
//! The fix is the SAME deterministic rendezvous-hash election this codebase
//! already uses for the managed-inference GPU coordinator
//! (`inference::coordinator_for`) and for `browser_db`'s own
//! `auto_db_deployment_for_tenant`/shard placement: every node computes the
//! identical answer from the gossiped healthy-node roster
//! ([`crate::browser_db::rest_owner_for_project`]), so there is no election
//! round trip and no coordinator state. The elected owner lazily
//! opens-or-creates its local replica (schema applied from the resolved
//! spec) on its FIRST request — the same self-heal `reconcile_replicas`
//! already performs on a timer, just triggered synchronously instead of
//! waiting up to `HIVE_BROWSER_DB_RECONCILE_SECS`. Every other node PROXIES
//! to the elected owner — HTTP admin first, the iroh gossip transport as the
//! NAT'd-peer fallback — mirroring `hrana.rs`'s owner-routing shape exactly,
//! with one difference named honestly: `hrana` proxies to a STORED
//! `Database::host_node` field, this proxies to a COMPUTED election result,
//! because there is no database record to store one in.
//!
//! # Credential
//!
//! `browser_db`'s existing access model is a QUIC-endpoint-identity
//! admission lease — structurally unlike a static bearer token, and not
//! reusable here. This module adds a NEW, independently-revocable credential
//! (`POST /v1/projects/:project/browser-db/rest-token`), minted the way
//! `drive_api::webdav_token_mint` mints the WebDAV credential: a random
//! token is generated, shown to the caller exactly ONCE in the mint
//! response, and only its SHA-256 hash is stored
//! (`project_settings::BrowserDbRestToken`, replicated fleet-wide via
//! `store_sync::REGISTRY`'s existing "projects" entry — the same store
//! `browser_db`'s own opt-in policy already lives in). Two independent
//! slots — team (read+write) and public (read-only, mintable only while
//! `browser_db.public_read` is enabled and re-checked live on every
//! request, not just at mint time) — mirror the Team/Public scope duality
//! `browser_admission` already encodes for the CRR sync protocol.
//! [`credential_scope`] checks a presented bearer constant-time against both
//! hashes, the exact shape `db_rest::credential_matches` and
//! `relational::drive_token_verify` already use.
//!
//! # Two dialects, one connection lifecycle
//!
//! Both dialects open the local replica via `hive_crsql::open` (loads the
//! real cr-sqlite extension — `sqlite_pool`'s bare `rusqlite::Connection`
//! must NEVER be pointed at a `browser-dbs` file, exactly the hazard
//! AGENTS.md names) and wrap the whole request in one explicit
//! transaction, calling `hive_crsql::set_ts` once right after `BEGIN` for
//! any non-read-only request — before the writes, as `hive_crsql::set_ts`'s
//! own doc requires, and covering every statement in the pipeline/batch
//! because the transaction stays open (autocommit off) until this module's
//! own `COMMIT`, so the extension's per-commit `ts` reset never fires
//! mid-pipeline. (Known v1 scope limit: a client-supplied pipeline may not
//! itself issue literal `BEGIN`/`COMMIT` SQL text, since one is already
//! open; use Hrana's own `"batch"` request type for atomic multi-statement
//! groups, which does not emit SQL `BEGIN` text.)
//!
//! * **libsql/Hrana** (`POST v2/pipeline`, `POST v3/pipeline`, `GET v2`,
//!   `GET v3` capability probes) reuses `hrana_proto::execute_pipeline`
//!   VERBATIM against the cr-sqlite-loaded connection — the wire codec is
//!   generic over any `rusqlite::Connection`, so this gets the full,
//!   already-hardened Hrana value/error/type handling for free. Interactive
//!   multi-request streams (a baton continuing across HTTP requests) are
//!   NOT supported in v1 — every pipeline is self-contained and closes
//!   automatically (`baton` in the reply is always `null`); a client
//!   presenting a non-empty `baton` gets an honest `STREAM_EXPIRED`. `v3
//!   cursor` (unbounded streaming reads) is out of scope for the same
//!   reason and simply 404s with a message naming the gap.
//! * **Upstash-style JSON-over-HTTP** (`POST sql`, `{query, params} ->
//!   {fields, rows, rowCount}`) is the SQL-over-HTTP shape `db_rest`'s
//!   `postgres_rest` already established for Postgres, applied to SQLite:
//!   real parameter binding (`raw_bind_parameter`, never string
//!   interpolation), `rowCount` gated on the prepared statement's own
//!   `readonly()` flag (the same `sqlite3_stmt_readonly`/`sqlite3_changes`
//!   discipline `hrana`'s doc names — an unguarded read after a write must
//!   never claim it wrote a row), and the same integer-precision discipline
//!   Hrana enforces: an i64 that does not fit exactly in an f64 is encoded
//!   as a JSON STRING rather than silently losing bits.
//!
//! # Caps
//!
//! `max_value_bytes` is enforced ONLY at the CRR sync boundary by design —
//! `ResolvedBrowserDbPolicy`'s own doc says so explicitly ("not on local SQL
//! execution: an oversized value persists in its origin replica but never
//! replicates") — so a REST write that creates an oversized value is
//! already handled correctly with NO extra code here: `sync_round`'s export
//! half will simply never ship it onward. `max_bytes` (the per-replica total
//! size cap) is different: it binds every write path, REST included. This
//! module enforces it the same way `apply_push_batches` does — checked
//! against the file's size on disk BEFORE the write, and on a real detected
//! write (`Connection::total_changes` advancing) the whole transaction is
//! ROLLED BACK with a typed quota message, never partially applied. Reads
//! are never refused for being over cap (mirrors `sync_round`'s "the export
//! half is still served" rule for a read-only/refused grant).

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use fluid_core::ResolvedBrowserDbPolicy;
use fluid_gateway::BrowserScope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::project_settings::BrowserDbRestToken;
use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

/// Max request body for a pipeline/sql POST — the `hrana`/`db_rest` cap.
const BODY_CAP: usize = 4 * 1024 * 1024;
/// Rows buffered for one `sql` result — the `postgres_rest` cap, applied to
/// SQLite (a single connection per request means no server-side cursor to
/// page through here either).
const MAX_ROWS: usize = 10_000;

// ---------------------------------------------------------------------------
// Credential mint
// ---------------------------------------------------------------------------

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn rand_token(prefix: &str) -> String {
    format!(
        "{prefix}_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(serde::Deserialize, Default)]
struct MintReq {
    #[serde(default)]
    scope: String,
}

/// `POST /v1/projects/:project/browser-db/rest-token` — mint (rotating) one
/// of this project's two REST credentials. JWT/API-key authed via
/// `require_project` (only a tenant member may mint or rotate); the
/// resulting token is shown exactly once.
async fn rest_token_mint(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    AxPath(project): AxPath<String>,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::admin::require_project(&cloud, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let scope_str = if body.is_empty() {
        String::new()
    } else {
        serde_json::from_slice::<MintReq>(&body)
            .map(|r| r.scope)
            .unwrap_or_default()
    };
    let public = match scope_str.as_str() {
        "" | "team" => false,
        "public" => true,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("scope must be \"team\" or \"public\", got {other:?}"),
            ))
        }
    };
    let Some(policy) = crate::browser_db::policy_for_project(&cloud, &project) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "project has no live browser_db opt-in — deploy a fluid.json browser_db block, \
             or opt in from the Storages page, first"
                .into(),
        ));
    };
    let resolved = policy.resolve();
    if public && !resolved.public_read {
        return Err((
            StatusCode::BAD_REQUEST,
            "browser_db.public_read is not enabled for this project — enable it before \
             minting a public (read-only) REST token"
                .into(),
        ));
    }
    let token = rand_token(if public { "bdbpub" } else { "bdbrest" });
    let hash = sha256_hex(&token);
    cloud.projects.set_browser_db_rest_token(
        &project,
        public,
        Some(BrowserDbRestToken {
            hash,
            created_ms: hive_core::now_ms(),
        }),
    );
    let (libsql_url, sql_url) = rest_urls(&cloud, &project);
    Ok(Json(json!({
        "project": project,
        "scope": if public { "public" } else { "team" },
        "token": token,
        "libsql_url": libsql_url,
        "sql_url": sql_url,
    })))
}

/// The two public URLs of a project's REST/Hrana surface, derived from the
/// same `api_base()` everywhere they are published (mint response, status
/// endpoint) — one construction, never two format strings to keep in sync.
/// The libsql base's TRAILING SLASH is load-bearing: libsql clients resolve
/// `v2/pipeline` RELATIVELY against it (the `hrana.rs` precedent).
pub(crate) fn rest_urls(cloud: &Arc<CloudState>, project: &str) -> (String, String) {
    let base = cloud.api_base();
    let base_trimmed = base.trim_end_matches('/');
    let scheme_swapped = base_trimmed
        .strip_prefix("https://")
        .map(|rest| format!("libsql://{rest}"))
        .unwrap_or_else(|| base_trimmed.to_string());
    (
        format!("{scheme_swapped}/v1/browser-db/{project}/"),
        format!("{base_trimmed}/v1/browser-db/{project}/sql"),
    )
}

/// Which scope (if any) `bearer` matches for `project`'s minted REST
/// credentials — constant-time on the stored HASH, the
/// `relational::drive_token_verify` / `db_rest::credential_matches` shape.
/// `None` covers both "no token was ever minted" and "wrong token": a probe
/// must not be able to distinguish them.
fn credential_scope(cloud: &Arc<CloudState>, project: &str, bearer: &str) -> Option<BrowserScope> {
    if bearer.is_empty() {
        return None;
    }
    let settings = cloud.projects.get(project);
    let hash = sha256_hex(bearer);
    if let Some(t) = &settings.browser_db_rest_team {
        if ct_eq(t.hash.as_bytes(), hash.as_bytes()) {
            return Some(BrowserScope::Team);
        }
    }
    if let Some(t) = &settings.browser_db_rest_public {
        if ct_eq(t.hash.as_bytes(), hash.as_bytes()) {
            return Some(BrowserScope::Public);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Per-project write concurrency admission (the `db_rest::rest_conn_permit`
// shape) — bounds how many REST requests may hold an open connection to one
// project's replica file at once, so a burst of callers queues via a typed
// 503 rather than piling up unbounded blocking-pool threads each holding
// their own sqlite3 handle.
// ---------------------------------------------------------------------------

const MAX_REST_CONNS_PER_PROJECT: usize = 8;

static REST_CONNS: OnceLock<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>> =
    OnceLock::new();

fn rest_conn_permit(project: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let map = REST_CONNS.get_or_init(Default::default);
    let sem = {
        let mut m = map.lock();
        m.entry(project.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MAX_REST_CONNS_PER_PROJECT)))
            .clone()
    };
    sem.try_acquire_owned().ok()
}

// ---------------------------------------------------------------------------
// Local execution
// ---------------------------------------------------------------------------

/// Open-or-create the local replica, apply schema, run `f` inside one
/// explicit transaction with `set_ts` called once (for a non-read-only
/// request) before any write, enforce the `max_bytes` cap on a real detected
/// write, and commit — or roll back on any error, including a cap refusal.
async fn with_replica<F, T>(
    project: String,
    resolved: ResolvedBrowserDbPolicy,
    read_only: bool,
    f: F,
) -> Result<T, String>
where
    F: FnOnce(&mut hive_crsql::rusqlite::Connection) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let Some(_permit) = rest_conn_permit(&project) else {
        return Err(
            "browser_db REST concurrency limit reached for this project; retry".to_string(),
        );
    };
    tokio::task::spawn_blocking(move || -> Result<T, String> {
        let path =
            crate::browser_db::store_dir().join(crate::browser_db::replica_file_name(&project));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut conn = hive_crsql::open(&path).map_err(|e| {
            format!("replica open failed (cr-sqlite extension present on this node?): {e}")
        })?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(|e| e.to_string())?;
        crate::browser_db::ensure_schema(&conn, &resolved).map_err(|e| e.to_string())?;
        if read_only {
            conn.pragma_update(None, "query_only", true)
                .map_err(|e| e.to_string())?;
        }
        let changes_before = conn.total_changes();
        // `BEGIN IMMEDIATE` acquires a RESERVED (write) lock up front — the
        // right choice for a write session (fails fast on contention rather
        // than deadlocking against another writer's later upgrade attempt),
        // but `query_only=ON` refuses to take a write lock AT ALL, so issuing
        // it for a read-only (Public-scope) session would refuse every
        // request, including a plain SELECT, with "attempt to write a
        // readonly database" — caught live via the Public-scope read test.
        // A read-only session uses a plain (deferred) `BEGIN` instead.
        let begin = if read_only {
            "BEGIN"
        } else {
            "BEGIN IMMEDIATE"
        };
        conn.execute_batch(begin).map_err(|e| e.to_string())?;
        // Measured AFTER the RESERVED lock is held, not before: SQLite
        // serializes writers on that lock (a concurrent `BEGIN IMMEDIATE`
        // blocks/fails until this transaction ends), so `bytes_before` here
        // is the true just-committed size with no other writer able to have
        // landed a change in between. Reading it before BEGIN let up to
        // `MAX_REST_CONNS_PER_PROJECT` concurrent writers all observe the
        // same stale under-cap size and all pass the cap check below.
        let bytes_before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if !read_only {
            if let Err(e) = hive_crsql::set_ts(&conn, hive_core::now_ms()) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.to_string());
            }
        }
        let result = f(&mut conn);
        match result {
            Ok(value) => {
                let wrote = conn.total_changes() != changes_before;
                if !read_only && wrote && bytes_before >= resolved.max_bytes {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!(
                        "replica is {bytes_before} bytes, at or over max_bytes {} — writes are \
                         refused until it shrinks or the cap is raised (reads still served)",
                        resolved.max_bytes
                    ));
                }
                if let Err(e) = conn.execute_batch("COMMIT") {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("commit failed: {e}"));
                }
                Ok(value)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| format!("worker join failed: {e}"))?
}

async fn pipeline(
    project: &str,
    resolved: &ResolvedBrowserDbPolicy,
    read_only: bool,
    version: u8,
    body: &[u8],
) -> Response {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON body: {e}"),
                None,
            )
        }
    };
    if parsed
        .get("baton")
        .and_then(|b| b.as_str())
        .filter(|b| !b.is_empty())
        .is_some()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "interactive baton continuation is not supported by this REST surface — each \
             pipeline POST is self-contained; use Hrana's \"batch\" request type inside one \
             pipeline for atomic multi-statement groups",
            Some("STREAM_EXPIRED"),
        );
    }
    let requests: Vec<Value> = parsed
        .get("requests")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let out = with_replica(
        project.to_string(),
        resolved.clone(),
        read_only,
        move |conn| {
            let o = crate::hrana_proto::execute_pipeline(
                conn,
                requests,
                crate::hrana_proto::SqlCache::new(),
                version,
                read_only,
            );
            Ok(o.results)
        },
    )
    .await;
    match out {
        Ok(results) => {
            ok_json(json!({ "baton": Value::Null, "base_url": Value::Null, "results": results }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e, Some("SQLITE_ERROR")),
    }
}

#[derive(serde::Deserialize)]
struct SqlReq {
    query: String,
    #[serde(default)]
    params: Vec<Value>,
}

async fn sql(
    project: &str,
    resolved: &ResolvedBrowserDbPolicy,
    read_only: bool,
    body: &[u8],
) -> Response {
    let req: SqlReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("bad body (expected {{query, params?}}): {e}"),
                None,
            )
        }
    };
    let out = with_replica(
        project.to_string(),
        resolved.clone(),
        read_only,
        move |conn| run_upstash_sql(conn, &req.query, &req.params),
    )
    .await;
    match out {
        Ok(v) => ok_json(v),
        Err(e) => err(StatusCode::BAD_REQUEST, &e, None),
    }
}

fn json_to_sql(v: &Value) -> hive_crsql::rusqlite::types::Value {
    use hive_crsql::rusqlite::types::Value as SqlValue;
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// Real parameter binding (never string interpolation), a real prepared
/// statement, real rows — the `db_rest::run_sql` shape applied to SQLite.
fn run_upstash_sql(
    conn: &hive_crsql::rusqlite::Connection,
    query: &str,
    params: &[Value],
) -> Result<Value, String> {
    let mut stmt = conn.prepare(query).map_err(|e| e.to_string())?;
    let expected = stmt.parameter_count();
    if params.len() != expected {
        return Err(format!(
            "statement takes {expected} parameters but {} were supplied",
            params.len()
        ));
    }
    for (i, p) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, json_to_sql(p))
            .map_err(|e| format!("cannot bind param {}: {e}", i + 1))?;
    }
    // Gate `rowCount` on the statement's own readonly-ness — `sqlite3_changes`
    // is per-CONNECTION and only DML updates it, so an unguarded read after a
    // write would claim it wrote a row (the `hrana.rs` doc's own warning).
    let is_readonly = stmt.readonly();
    let col_names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows_json = Vec::new();
    {
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut obj = serde_json::Map::with_capacity(col_names.len());
            for (i, name) in col_names.iter().enumerate() {
                let vref = row.get_ref(i).map_err(|e| e.to_string())?;
                obj.insert(name.clone(), sql_value_to_json(vref, name)?);
            }
            rows_json.push(Value::Object(obj));
            if rows_json.len() >= MAX_ROWS {
                return Err(format!(
                    "result too large (max {MAX_ROWS} rows over browser_db REST — narrow the \
                     projection or paginate)"
                ));
            }
        }
    }
    let fields: Vec<Value> = col_names.iter().map(|n| json!({ "name": n })).collect();
    let affected = if is_readonly {
        rows_json.len() as u64
    } else {
        conn.changes()
    };
    Ok(json!({ "fields": fields, "rows": rows_json, "rowCount": affected }))
}

/// Precision discipline: `integer` beyond what an f64 can hold exactly
/// becomes a JSON STRING rather than silently losing bits (the `hrana.rs`
/// wire-codec rule, applied to the plain-JSON dialect). Blob is base64;
/// non-finite float is a loud, named error (JSON cannot represent it and
/// `null` would be indistinguishable from a real NULL) — both the
/// `db_rest::col_to_json` precedent.
fn sql_value_to_json(
    v: hive_crsql::rusqlite::types::ValueRef<'_>,
    col: &str,
) -> Result<Value, String> {
    use hive_crsql::rusqlite::types::ValueRef;
    Ok(match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => {
            if i.unsigned_abs() <= (1u64 << 53) {
                json!(i)
            } else {
                Value::String(i.to_string())
            }
        }
        ValueRef::Real(f) => {
            let n = serde_json::Number::from_f64(f).ok_or_else(|| {
                format!(
                    "column \"{col}\" holds a non-finite float ({f}), which JSON cannot \
                     represent — cast it in the query (e.g. CAST(x AS TEXT))"
                )
            })?;
            Value::Number(n)
        }
        ValueRef::Text(t) => Value::String(
            std::str::from_utf8(t)
                .map_err(|_| format!("column \"{col}\" holds text that is not valid UTF-8"))?
                .to_string(),
        ),
        ValueRef::Blob(b) => Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
    })
}

// ---------------------------------------------------------------------------
// Dispatch + owner routing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Endpoint {
    Pipeline(u8),
    Probe(u8),
    Sql,
}

fn route(method: &Method, path: &str) -> Option<Endpoint> {
    let p = path.trim_end_matches('/');
    match (method, p) {
        (&Method::POST, "/v2/pipeline") => Some(Endpoint::Pipeline(2)),
        (&Method::POST, "/v3/pipeline") => Some(Endpoint::Pipeline(3)),
        (&Method::GET, "/v2") => Some(Endpoint::Probe(2)),
        (&Method::GET, "/v3") => Some(Endpoint::Probe(3)),
        (&Method::POST, "/sql") => Some(Endpoint::Sql),
        _ => None,
    }
}

async fn serve_local(
    project: &str,
    resolved: &ResolvedBrowserDbPolicy,
    read_only: bool,
    endpoint: Endpoint,
    body: &[u8],
) -> Response {
    match endpoint {
        Endpoint::Probe(v) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({ "version": v, "encoding": "json" }).to_string(),
        )
            .into_response(),
        Endpoint::Pipeline(v) => pipeline(project, resolved, read_only, v, body).await,
        Endpoint::Sql => sql(project, resolved, read_only, body).await,
    }
}

/// Entry point every dialect HTTP handler funnels through: credential check,
/// live opt-in + scope re-check, owner election, then either serve locally
/// (this node IS the elected owner) or proxy (mirrors `hrana::serve`).
async fn serve(
    cloud: Arc<CloudState>,
    project: String,
    bearer: String,
    method: Method,
    path: String,
    body: Bytes,
) -> Response {
    let Some(scope) = credential_scope(&cloud, &project, &bearer) else {
        let mut r = err(
            StatusCode::UNAUTHORIZED,
            "missing or invalid auth token for this project's browser_db REST surface",
            Some("UNAUTHORIZED"),
        );
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return r;
    };
    let Some(policy) = crate::browser_db::policy_for_project(&cloud, &project) else {
        return err(
            StatusCode::NOT_FOUND,
            "project has no live browser_db opt-in",
            None,
        );
    };
    let resolved = policy.resolve();
    if scope == BrowserScope::Public && !resolved.public_read {
        return err(
            StatusCode::FORBIDDEN,
            "browser_db.public_read has been disabled for this project since this token was minted",
            None,
        );
    }
    let Some(endpoint) = route(&method, &path) else {
        return err(
            StatusCode::NOT_FOUND,
            "unsupported browser_db REST endpoint — this server speaks Hrana v2/v3 over JSON \
             (POST v2/pipeline, POST v3/pipeline) plus POST sql ({query, params})",
            None,
        );
    };
    if body.len() > BODY_CAP {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
            None,
        );
    }
    let read_only = scope == BrowserScope::Public;

    let Some(owner) = crate::browser_db::rest_owner_for_project(&cloud, &project) else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no healthy node available to serve this project's browser_db REST surface",
            None,
        );
    };
    if owner != cloud.node_name {
        return match forward_to_owner(&cloud, &owner, &project, &bearer, &path, &body).await {
            Some(r) => r,
            None => err(
                StatusCode::MISDIRECTED_REQUEST,
                "this project's browser_db REST owner could not be reached",
                Some("HOST_UNREACHABLE"),
            ),
        };
    }
    cors(serve_local(&project, &resolved, read_only, endpoint, &body).await)
}

fn envelope(bearer: &str, path: &str, body: &[u8]) -> Value {
    json!({
        "path": path,
        "auth": bearer,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
    })
}

async fn forward_to_owner(
    cloud: &Arc<CloudState>,
    owner: &str,
    project: &str,
    bearer: &str,
    path: &str,
    body: &[u8],
) -> Option<Response> {
    let env = envelope(bearer, path, body);
    let route_path = format!("/v1/projects/{project}/browser-db/rest-mesh");
    let mut why = String::new();

    let admin = cloud.node_admins.read().get(owner).cloned();
    if admin.is_none() {
        why.push_str("no HTTP admin URL known for the REST owner; ");
    }
    if let Some(admin) = admin {
        let mut rb = cloud
            .http
            .post(format!("{admin}{route_path}"))
            .json(&env)
            .timeout(std::time::Duration::from_secs(30));
        if crate::auth::enforced() {
            if let Ok(tok) = crate::auth::issue("mesh-internal", "", "service", false, crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS) {
                rb = rb.bearer_auth(tok);
            }
        }
        match rb.send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(v) => match decode_envelope(&v) {
                    Some(resp) => return Some(resp),
                    None => why.push_str("owner returned an undecodable envelope; "),
                },
                Err(e) => why.push_str(&format!("owner reply was not JSON ({e}); ")),
            },
            Ok(r) => why.push_str(&format!("owner HTTP admin answered {}; ", r.status())),
            Err(e) => why.push_str(&format!("owner HTTP admin unreachable ({e}); ")),
        }
    }

    let target = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == owner)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    let Some(target) = target else {
        tracing::warn!(
            project = %project, owner = %owner, reason = %why,
            "browser_db REST owner proxy failed: owner has no gossip address in this node's registry"
        );
        return None;
    };
    let payload = serde_json::to_vec(&env).ok()?;
    let bytes = match crate::gossip::request_to(
        cloud,
        &target.0,
        &target.1,
        hive_p2p::GOSSIP_POST,
        &route_path,
        &payload,
        30,
    )
    .await
    {
        Some(b) => b,
        None => {
            tracing::warn!(
                project = %project, owner = %owner, peer_id = %target.0, reason = %why,
                "browser_db REST owner proxy failed: mesh gossip request timed out or errored"
            );
            return None;
        }
    };
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    decode_envelope(&v)
}

fn decode_envelope(v: &Value) -> Option<Response> {
    let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
    let status = StatusCode::from_u16(status).ok()?;
    let ct = v
        .get("content_type")
        .and_then(|c| c.as_str())
        .unwrap_or("application/json")
        .to_string();
    let body = base64::engine::general_purpose::STANDARD
        .decode(v.get("body_b64").and_then(|b| b.as_str()).unwrap_or(""))
        .ok()?;
    let ct = header::HeaderValue::from_str(&ct).ok()?;
    Some(cors(
        (status, [(header::CONTENT_TYPE, ct)], body).into_response(),
    ))
}

/// Owner-side re-check: this is `hrana::mesh_serve`'s shape, re-deriving
/// authorization from THIS node's own replicated state rather than trusting
/// anything the envelope claims — the `proxy_to_owner` precedent. Refuses to
/// re-proxy: if this node is not the elected owner either, the caller's
/// routing information is stale, and answering honestly beats a proxy loop.
pub async fn mesh_serve(cloud: &Arc<CloudState>, project: &str, env: &Value) -> Value {
    let refuse = |status: StatusCode, msg: &str| {
        json!({
            "status": status.as_u16(),
            "content_type": "application/json",
            "body_b64": base64::engine::general_purpose::STANDARD
                .encode(json!({ "message": msg }).to_string()),
        })
    };
    let bearer = env.get("auth").and_then(|a| a.as_str()).unwrap_or_default();
    let Some(scope) = credential_scope(cloud, project, bearer) else {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "invalid auth token for this project's browser_db REST surface",
        );
    };
    let Some(policy) = crate::browser_db::policy_for_project(cloud, project) else {
        return refuse(
            StatusCode::NOT_FOUND,
            "project has no live browser_db opt-in",
        );
    };
    let resolved = policy.resolve();
    if scope == BrowserScope::Public && !resolved.public_read {
        return refuse(
            StatusCode::FORBIDDEN,
            "browser_db.public_read has been disabled for this project since this token was minted",
        );
    }
    if crate::browser_db::rest_owner_for_project(cloud, project).as_deref()
        != Some(cloud.node_name.as_str())
    {
        return refuse(
            StatusCode::MISDIRECTED_REQUEST,
            "this node is not the current browser_db REST owner for this project",
        );
    }
    let path = env.get("path").and_then(|p| p.as_str()).unwrap_or_default();
    let Some(endpoint) = route(&Method::POST, path).or_else(|| route(&Method::GET, path)) else {
        return refuse(
            StatusCode::NOT_FOUND,
            "unsupported browser_db REST endpoint",
        );
    };
    let body = base64::engine::general_purpose::STANDARD
        .decode(env.get("body_b64").and_then(|b| b.as_str()).unwrap_or(""))
        .unwrap_or_default();
    if body.len() > BODY_CAP {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let read_only = scope == BrowserScope::Public;
    let resp = serve_local(project, &resolved, read_only, endpoint, &body).await;
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), BODY_CAP * 4)
        .await
        .unwrap_or_default();
    json!({
        "status": status,
        "content_type": ct,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

// ---------------------------------------------------------------------------
// Route wiring
// ---------------------------------------------------------------------------

pub fn routes() -> axum::Router<Arc<CloudState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/v1/projects/:project/browser-db/rest-token",
            post(rest_token_mint),
        )
        .route(
            "/v1/projects/:project/browser-db/rest-mesh",
            post(mesh_route),
        )
        .route(
            "/v1/browser-db/:project/v2/pipeline",
            post(api_pipeline_v2).options(preflight),
        )
        .route(
            "/v1/browser-db/:project/v3/pipeline",
            post(api_pipeline_v3).options(preflight),
        )
        .route(
            "/v1/browser-db/:project/v2",
            get(api_probe_v2).options(preflight),
        )
        .route(
            "/v1/browser-db/:project/v3",
            get(api_probe_v3).options(preflight),
        )
        .route(
            "/v1/browser-db/:project/sql",
            post(api_sql).options(preflight),
        )
}

async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn api_serve(
    cloud: Arc<CloudState>,
    project: String,
    headers: HeaderMap,
    method: Method,
    path: &str,
    body: Bytes,
) -> Response {
    let bearer = crate::db_rest::bearer_token(&headers).unwrap_or_default();
    serve(cloud, project, bearer, method, path.to_string(), body).await
}

async fn api_pipeline_v2(
    State(c): State<Arc<CloudState>>,
    AxPath(project): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    api_serve(c, project, headers, Method::POST, "/v2/pipeline", body).await
}
async fn api_pipeline_v3(
    State(c): State<Arc<CloudState>>,
    AxPath(project): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    api_serve(c, project, headers, Method::POST, "/v3/pipeline", body).await
}
async fn api_probe_v2(
    State(c): State<Arc<CloudState>>,
    AxPath(project): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    api_serve(c, project, headers, Method::GET, "/v2", Bytes::new()).await
}
async fn api_probe_v3(
    State(c): State<Arc<CloudState>>,
    AxPath(project): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    api_serve(c, project, headers, Method::GET, "/v3", Bytes::new()).await
}
async fn api_sql(
    State(c): State<Arc<CloudState>>,
    AxPath(project): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    api_serve(c, project, headers, Method::POST, "/sql", body).await
}

async fn mesh_route(
    State(c): State<Arc<CloudState>>,
    AxPath(project): AxPath<String>,
    Json(env): Json<Value>,
) -> Json<Value> {
    Json(mesh_serve(&c, &project, &env).await)
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

fn ok_json(v: Value) -> Response {
    cors(
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            v.to_string(),
        )
            .into_response(),
    )
}

fn err(code: StatusCode, msg: &str, hrana_code: Option<&str>) -> Response {
    let body = match hrana_code {
        Some(c) => json!({ "message": msg, "code": c, "error": msg }),
        None => json!({ "message": msg, "error": msg }),
    };
    cors(
        (
            code,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response(),
    )
}

fn cors(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        "access-control-allow-origin",
        header::HeaderValue::from_static("*"),
    );
    h.insert(
        "access-control-allow-methods",
        header::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        "access-control-allow-headers",
        header::HeaderValue::from_static("authorization, content-type"),
    );
    resp
}
