//! Hrana v2/v3 wire codec + execution against a real SQLite connection.
//!
//! This is the protocol half of the libsql lane (`crate::hrana` is the HTTP
//! half). It is a direct implementation of `libsql/docs/HRANA_3_SPEC.md`, and
//! the parts that look pedantic are the parts a real client breaks on:
//!
//! * **`integer` and `last_insert_rowid` are STRINGS on the wire.** JSON
//!   numbers are f64 in every JS client, so an i64 beyond 2^53 would silently
//!   lose its low bits. The spec makes them strings for exactly that reason and
//!   `@libsql/client` parses them back with `BigInt`.
//! * **`float` is a JSON number**, which cannot represent NaN/±Inf. SQLite can
//!   store them, so a column holding one is a LOUD error naming the column
//!   rather than a silent `null` (indistinguishable from a real NULL) or a
//!   silent type change to text.
//! * **`blob` is base64 WITH padding**: `atob` in a browser requires it, and
//!   every padded decoder accepts it. Decoding accepts unpadded too.
//! * **Every request in a pipeline executes, even after one errors** — the
//!   spec is explicit, and `batch`'s conditions are built on later steps
//!   observing an earlier step's failure.
//!
//! Everything here is synchronous and runs inside `spawn_blocking` (see
//! `sqlite_pool::PooledConn::call`): one blocking hop executes a whole
//! pipeline, so a 2-request `[execute, close]` POST — which is what
//! `@libsql/client`'s `execute()` sends — costs one hop, not two.

use base64::Engine;
use hive_crsql::rusqlite::types::{Value as SqlValue, ValueRef};
use hive_crsql::rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Rows buffered for one statement result. Hrana has a cursor endpoint for
/// unbounded reads; the pipeline endpoint buffers, so it needs a ceiling.
const MAX_ROWS: usize = 20_000;
/// Bytes buffered for one statement result.
const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;
/// Statements a stream may keep in its `store_sql` cache, and their total size.
const MAX_SQL_CACHE: usize = 256;
const MAX_SQL_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// Per-stream `store_sql` cache: `sql_id` -> SQL text.
pub type SqlCache = HashMap<i32, String>;

pub struct ExecOutput {
    pub results: Vec<Value>,
    /// A `close` request was seen — the caller must drop the stream and reply
    /// with a null baton.
    pub closed: bool,
    pub cache: SqlCache,
}

#[derive(Deserialize, Default)]
struct Stmt {
    #[serde(default)]
    sql: Option<String>,
    #[serde(default)]
    sql_id: Option<i32>,
    #[serde(default)]
    args: Vec<Value>,
    #[serde(default)]
    named_args: Vec<NamedArg>,
    #[serde(default)]
    want_rows: Option<bool>,
}

#[derive(Deserialize)]
struct NamedArg {
    name: String,
    value: Value,
}

#[derive(Deserialize)]
struct BatchStep {
    #[serde(default)]
    condition: Option<Value>,
    stmt: Stmt,
}

#[derive(Deserialize)]
struct Batch {
    #[serde(default)]
    steps: Vec<BatchStep>,
}

fn err_json(msg: impl AsRef<str>, code: Option<&str>) -> Value {
    match code {
        Some(c) => json!({ "message": msg.as_ref(), "code": c }),
        None => json!({ "message": msg.as_ref() }),
    }
}

fn result_err(msg: impl AsRef<str>, code: Option<&str>) -> Value {
    json!({ "type": "error", "error": err_json(msg, code) })
}

fn result_ok(response: Value) -> Value {
    json!({ "type": "ok", "response": response })
}

/// Execute a whole pipeline's `requests` array against one connection.
/// `version` gates the v3-only requests (`get_autocommit`, the `is_autocommit`
/// batch condition) so a v2 client can never observe a v3 response shape.
pub fn execute_pipeline(
    conn: &mut Connection,
    requests: Vec<Value>,
    mut cache: SqlCache,
    version: u8,
) -> ExecOutput {
    let mut results = Vec::with_capacity(requests.len());
    let mut closed = false;
    for req in requests {
        // A `close` short-circuits nothing: the spec says every request runs,
        // and a client legitimately pipelines [execute, close].
        let ty = req.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let out = match ty {
            "close" => {
                closed = true;
                Ok(json!({ "type": "close" }))
            }
            "execute" => match serde_json::from_value::<Stmt>(
                req.get("stmt").cloned().unwrap_or(Value::Null),
            ) {
                Ok(stmt) => resolve_sql(&stmt, &cache)
                    .and_then(|sql| execute_stmt(conn, &sql, &stmt))
                    .map(|r| json!({ "type": "execute", "result": r })),
                Err(e) => Err(format!("malformed stmt: {e}")),
            },
            "batch" => match serde_json::from_value::<Batch>(
                req.get("batch").cloned().unwrap_or(Value::Null),
            ) {
                Ok(batch) => execute_batch(conn, batch, &cache, version)
                    .map(|r| json!({ "type": "batch", "result": r })),
                Err(e) => Err(format!("malformed batch: {e}")),
            },
            "sequence" => sql_from_req(&req, &cache).and_then(|sql| {
                conn.execute_batch(&sql)
                    .map(|_| json!({ "type": "sequence" }))
                    .map_err(|e| e.to_string())
            }),
            "describe" => sql_from_req(&req, &cache)
                .and_then(|sql| describe(conn, &sql))
                .map(|r| json!({ "type": "describe", "result": r })),
            "store_sql" => store_sql(&req, &mut cache).map(|_| json!({ "type": "store_sql" })),
            "close_sql" => {
                if let Some(id) = req.get("sql_id").and_then(|v| v.as_i64()) {
                    cache.remove(&(id as i32));
                }
                Ok(json!({ "type": "close_sql" }))
            }
            "get_autocommit" => {
                if version < 3 {
                    Err("get_autocommit requires protocol version 3".to_string())
                } else {
                    Ok(json!({
                        "type": "get_autocommit",
                        "is_autocommit": conn.is_autocommit(),
                    }))
                }
            }
            other => Err(format!("unknown stream request type {other:?}")),
        };
        results.push(match out {
            Ok(v) => result_ok(v),
            Err(e) => result_err(e, Some("SQLITE_ERROR")),
        });
    }
    ExecOutput {
        results,
        closed,
        cache,
    }
}

fn store_sql(req: &Value, cache: &mut SqlCache) -> Result<(), String> {
    let id = req
        .get("sql_id")
        .and_then(|v| v.as_i64())
        .ok_or("store_sql requires sql_id")? as i32;
    let sql = req
        .get("sql")
        .and_then(|v| v.as_str())
        .ok_or("store_sql requires sql")?
        .to_string();
    if cache.contains_key(&id) {
        return Err(format!("sql_id {id} is already stored on this stream"));
    }
    if cache.len() >= MAX_SQL_CACHE {
        return Err(format!(
            "stream SQL cache is full ({MAX_SQL_CACHE} statements) — close_sql before storing more"
        ));
    }
    let total: usize = cache.values().map(|s| s.len()).sum::<usize>() + sql.len();
    if total > MAX_SQL_CACHE_BYTES {
        return Err(format!(
            "stream SQL cache exceeds {MAX_SQL_CACHE_BYTES} bytes"
        ));
    }
    cache.insert(id, sql);
    Ok(())
}

/// `sql` XOR `sql_id` — the spec requires exactly one, and accepting both (or
/// neither) is how a client silently runs the wrong statement.
fn resolve_sql(stmt: &Stmt, cache: &SqlCache) -> Result<String, String> {
    match (&stmt.sql, stmt.sql_id) {
        (Some(s), None) => Ok(s.clone()),
        (None, Some(id)) => cache
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("sql_id {id} is not stored on this stream")),
        (Some(_), Some(_)) => Err("stmt carries both sql and sql_id".into()),
        (None, None) => Err("stmt carries neither sql nor sql_id".into()),
    }
}

fn sql_from_req(req: &Value, cache: &SqlCache) -> Result<String, String> {
    let stmt = Stmt {
        sql: req
            .get("sql")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sql_id: req.get("sql_id").and_then(|v| v.as_i64()).map(|v| v as i32),
        ..Default::default()
    };
    resolve_sql(&stmt, cache)
}

// ---- Value codec ----------------------------------------------------------

/// Wire `Value` -> SQLite value. An unknown `type` is an error, never a
/// best-effort guess: binding the wrong type is a silently wrong query.
fn decode_value(v: &Value) -> Result<SqlValue, String> {
    let ty = v
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("argument is missing its `type`")?;
    match ty {
        "null" => Ok(SqlValue::Null),
        "integer" => {
            let raw = v
                .get("value")
                .and_then(|x| x.as_str())
                .ok_or("integer argument must carry a string `value`")?;
            raw.parse::<i64>()
                .map(SqlValue::Integer)
                .map_err(|_| format!("integer argument {raw:?} is not a 64-bit integer"))
        }
        "float" => v
            .get("value")
            .and_then(|x| x.as_f64())
            .map(SqlValue::Real)
            .ok_or_else(|| "float argument must carry a number `value`".to_string()),
        "text" => v
            .get("value")
            .and_then(|x| x.as_str())
            .map(|s| SqlValue::Text(s.to_string()))
            .ok_or_else(|| "text argument must carry a string `value`".to_string()),
        "blob" => {
            let b64 = v
                .get("base64")
                .and_then(|x| x.as_str())
                .ok_or("blob argument must carry a string `base64`")?;
            decode_b64(b64).map(SqlValue::Blob)
        }
        other => Err(format!("unknown argument type {other:?}")),
    }
}

/// Standard base64, padded or not — clients differ and both are unambiguous.
fn decode_b64(s: &str) -> Result<Vec<u8>, String> {
    let trimmed = s.trim();
    base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| {
            base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed.trim_end_matches('='))
        })
        .map_err(|e| format!("invalid base64 in blob argument: {e}"))
}

fn encode_value(v: ValueRef<'_>, col: &str) -> Result<Value, String> {
    Ok(match v {
        ValueRef::Null => json!({ "type": "null" }),
        ValueRef::Integer(i) => json!({ "type": "integer", "value": i.to_string() }),
        ValueRef::Real(f) => {
            let n = serde_json::Number::from_f64(f).ok_or_else(|| {
                format!(
                    "column {col:?} holds a non-finite float ({f}), which JSON cannot represent — \
                     cast it in the query (e.g. CAST(x AS TEXT))"
                )
            })?;
            json!({ "type": "float", "value": n })
        }
        ValueRef::Text(t) => {
            let s = std::str::from_utf8(t)
                .map_err(|_| format!("column {col:?} holds text that is not valid UTF-8"))?;
            json!({ "type": "text", "value": s })
        }
        ValueRef::Blob(b) => json!({
            "type": "blob",
            "base64": base64::engine::general_purpose::STANDARD.encode(b),
        }),
    })
}

// ---- Statement execution --------------------------------------------------

/// Bind every parameter of a prepared statement, or fail naming the one that
/// could not be bound. Unbound parameters default to NULL in SQLite, which
/// turns a client-side arity bug into a query that "works" and returns the
/// wrong rows — so this refuses instead.
fn bind(stmt: &mut hive_crsql::rusqlite::Statement<'_>, s: &Stmt) -> Result<(), String> {
    let expected = stmt.parameter_count();
    if s.args.len() > expected {
        return Err(format!(
            "statement takes {expected} parameters but {} positional arguments were supplied",
            s.args.len()
        ));
    }
    let mut bound = vec![false; expected];
    for (i, a) in s.args.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, decode_value(a)?)
            .map_err(|e| format!("cannot bind positional argument {}: {e}", i + 1))?;
        bound[i] = true;
    }
    for na in &s.named_args {
        // Clients send names with or without the SQLite sigil; try the raw
        // name first, then each sigil, so `:id` / `@id` / `$id` all resolve
        // from a bare `id`.
        let idx = ["", ":", "@", "$"]
            .iter()
            .find_map(|p| {
                let key = if na.name.starts_with([':', '@', '$']) {
                    na.name.clone()
                } else {
                    format!("{p}{}", na.name)
                };
                stmt.parameter_index(&key).ok().flatten()
            })
            .ok_or_else(|| format!("statement has no parameter named {:?}", na.name))?;
        stmt.raw_bind_parameter(idx, decode_value(&na.value)?)
            .map_err(|e| format!("cannot bind named argument {:?}: {e}", na.name))?;
        if idx >= 1 && idx <= expected {
            bound[idx - 1] = true;
        }
    }
    if let Some(missing) = bound.iter().position(|b| !b) {
        return Err(format!(
            "parameter {} of {expected} was not supplied",
            missing + 1
        ));
    }
    Ok(())
}

fn execute_stmt(conn: &Connection, sql: &str, s: &Stmt) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    bind(&mut stmt, s)?;

    // Column metadata is read from the statement itself (immutable borrow)
    // BEFORE stepping, because `raw_query` takes the statement mutably.
    let cols: Vec<Value> = stmt
        .columns()
        .iter()
        .map(|c| json!({ "name": c.name(), "decltype": c.decl_type() }))
        .collect();
    let col_names: Vec<String> = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let want_rows = s.want_rows.unwrap_or(true) && !col_names.is_empty();
    // `sqlite3_changes` is per-CONNECTION and is only updated by DML, so after
    // `INSERT` then `SELECT` it still reports the insert's count — which would
    // make every read claim it wrote a row (and carry the insert's
    // `last_insert_rowid`). `sqlite3_stmt_readonly` is the exact discriminator,
    // and it is also true for BEGIN/COMMIT/ROLLBACK, which change no rows.
    let readonly = stmt.readonly();
    // `readonly` alone is NOT enough. DDL (`CREATE`/`DROP`/`ALTER`) and most
    // `PRAGMA` writes are non-readonly yet never touch `sqlite3_changes`, so
    // they inherit whatever count the last DML on this CONNECTION left behind —
    // and connections are pooled and reused across requests, so a `CREATE TABLE`
    // could report another request's insert count and its `last_insert_rowid`.
    // `sqlite3_total_changes` moves for exactly the statements `sqlite3_changes`
    // is meaningful for, so its delta is the precise discriminator.
    let total_before = conn.total_changes();

    let mut rows_json: Vec<Value> = Vec::new();
    let mut rows_read: u64 = 0;
    let mut bytes: usize = 0;
    {
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            rows_read += 1;
            if !want_rows {
                continue;
            }
            let mut out = Vec::with_capacity(col_names.len());
            for (i, name) in col_names.iter().enumerate() {
                let v = row.get_ref(i).map_err(|e| e.to_string())?;
                let enc = encode_value(v, name)?;
                bytes += estimate_bytes(&enc);
                out.push(enc);
            }
            rows_json.push(Value::Array(out));
            if rows_json.len() > MAX_ROWS {
                return Err(format!(
                    "result set exceeds {MAX_ROWS} rows over the pipeline endpoint — paginate with LIMIT/OFFSET"
                ));
            }
            if bytes > MAX_RESULT_BYTES {
                return Err(format!(
                    "result set exceeds {}MiB over the pipeline endpoint — narrow the projection or paginate",
                    MAX_RESULT_BYTES / (1024 * 1024)
                ));
            }
        }
    }
    let wrote_rows = conn.total_changes() != total_before;
    let changes = if readonly || !wrote_rows {
        0
    } else {
        conn.changes()
    };
    // SQLite keeps the connection's last inserted rowid indefinitely, so it is
    // reported only for a statement that actually wrote — which now excludes
    // DDL/`PRAGMA`, since `changes` is gated on the `total_changes` delta above.
    // Deliberately NOT additionally gated on "the rowid moved": re-inserting a
    // rowid that was just deleted leaves it unchanged, and suppressing a real
    // insert's rowid is a worse failure than an UPDATE/DELETE echoing the
    // connection's last one.
    let last_insert_rowid = if changes > 0 {
        let id = conn.last_insert_rowid();
        (id != 0).then(|| id.to_string())
    } else {
        None
    };
    Ok(json!({
        "cols": cols,
        "rows": rows_json,
        "affected_row_count": changes,
        "last_insert_rowid": last_insert_rowid,
        // `rows_read` is the rows this statement STEPPED and `rows_written` the
        // rows it changed. libsql-server derives both from sqlite3_status
        // counters; these are the honest per-statement equivalents, not a
        // reimplementation of that accounting.
        "rows_read": rows_read,
        "rows_written": changes,
        "query_duration_ms": started.elapsed().as_secs_f64() * 1000.0,
    }))
}

/// Cheap size estimate — never serialises, just bounds the buffer.
fn estimate_bytes(v: &Value) -> usize {
    match v.get("value") {
        Some(Value::String(s)) => s.len() + 24,
        _ => match v.get("base64").and_then(|b| b.as_str()) {
            Some(b) => b.len() + 24,
            None => 32,
        },
    }
}

fn describe(conn: &Connection, sql: &str) -> Result<Value, String> {
    let stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let params: Vec<Value> = (1..=stmt.parameter_count())
        .map(|i| json!({ "name": stmt.parameter_name(i) }))
        .collect();
    let cols: Vec<Value> = stmt
        .columns()
        .iter()
        .map(|c| json!({ "name": c.name(), "decltype": c.decl_type() }))
        .collect();
    Ok(json!({
        "params": params,
        "cols": cols,
        "is_explain": stmt.is_explain() != 0,
        "is_readonly": stmt.readonly(),
    }))
}

// ---- Batch ----------------------------------------------------------------

/// One batch step's observed outcome, which later steps' conditions read.
#[derive(Clone, Copy, PartialEq)]
enum StepState {
    Skipped,
    Ok,
    Failed,
}

fn eval_cond(
    cond: &Value,
    states: &[StepState],
    conn: &Connection,
    version: u8,
) -> Result<bool, String> {
    let ty = cond
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("batch condition is missing its `type`")?;
    match ty {
        "ok" | "error" => {
            let step = cond
                .get("step")
                .and_then(|s| s.as_u64())
                .ok_or("batch condition is missing `step`")? as usize;
            // A condition referring to a step that has not run yet (or does not
            // exist) is FALSE, never an error — the spec allows forward
            // references and they simply cannot be satisfied.
            let state = states.get(step).copied().unwrap_or(StepState::Skipped);
            Ok(match ty {
                "ok" => state == StepState::Ok,
                _ => state == StepState::Failed,
            })
        }
        "not" => {
            let inner = cond.get("cond").ok_or("`not` condition is missing `cond`")?;
            Ok(!eval_cond(inner, states, conn, version)?)
        }
        "and" | "or" => {
            let conds = cond
                .get("conds")
                .and_then(|c| c.as_array())
                .ok_or("`and`/`or` condition is missing `conds`")?;
            let mut acc = ty == "and";
            for c in conds {
                let v = eval_cond(c, states, conn, version)?;
                acc = if ty == "and" { acc && v } else { acc || v };
            }
            Ok(acc)
        }
        "is_autocommit" => {
            if version < 3 {
                Err("is_autocommit condition requires protocol version 3".into())
            } else {
                Ok(conn.is_autocommit())
            }
        }
        other => Err(format!("unknown batch condition type {other:?}")),
    }
}

/// The `v3/cursor` endpoint's newline-delimited entry stream for one batch.
///
/// The entries are RENDERED from the same buffered batch execution the
/// pipeline endpoint uses — the wire shape is the spec's, the execution is not
/// a second implementation. That is a deliberate limit and it is the honest
/// one: `cursor` exists in Hrana so a huge result set need not be buffered, and
/// this server still buffers it under [`MAX_ROWS`]/[`MAX_RESULT_BYTES`]. A
/// client gets correct results and the same ceiling as the pipeline endpoint,
/// never a truncated stream.
pub fn execute_cursor(
    conn: &mut Connection,
    batch: Value,
    cache: &SqlCache,
    version: u8,
) -> Vec<Value> {
    let parsed = match serde_json::from_value::<Batch>(batch) {
        Ok(b) => b,
        Err(e) => {
            return vec![json!({
                "type": "error",
                "error": err_json(format!("malformed batch: {e}"), Some("SQLITE_ERROR")),
            })]
        }
    };
    let result = match execute_batch(conn, parsed, cache, version) {
        Ok(r) => r,
        Err(e) => {
            return vec![json!({
                "type": "error",
                "error": err_json(e, Some("SQLITE_ERROR")),
            })]
        }
    };
    let empty = Vec::new();
    let step_results = result["step_results"].as_array().unwrap_or(&empty);
    let step_errors = result["step_errors"].as_array().unwrap_or(&empty);
    let mut entries = Vec::new();
    for i in 0..step_results.len().max(step_errors.len()) {
        if let Some(e) = step_errors.get(i).filter(|v| !v.is_null()) {
            entries.push(json!({ "type": "step_error", "step": i, "error": e }));
            continue;
        }
        let Some(r) = step_results.get(i).filter(|v| !v.is_null()) else {
            // Skipped step: the spec emits no entries for it.
            continue;
        };
        entries.push(json!({ "type": "step_begin", "step": i, "cols": r["cols"] }));
        for row in r["rows"].as_array().unwrap_or(&empty) {
            entries.push(json!({ "type": "row", "row": row }));
        }
        entries.push(json!({
            "type": "step_end",
            "affected_row_count": r["affected_row_count"],
            "last_insert_rowid": r["last_insert_rowid"],
        }));
    }
    entries
}

fn execute_batch(
    conn: &Connection,
    batch: Batch,
    cache: &SqlCache,
    version: u8,
) -> Result<Value, String> {
    let n = batch.steps.len();
    let mut states: Vec<StepState> = vec![StepState::Skipped; n];
    let mut step_results: Vec<Value> = vec![Value::Null; n];
    let mut step_errors: Vec<Value> = vec![Value::Null; n];

    for (i, step) in batch.steps.iter().enumerate() {
        let run = match &step.condition {
            None => true,
            Some(c) => match eval_cond(c, &states, conn, version) {
                Ok(v) => v,
                Err(e) => {
                    // A malformed condition fails ITS step, not the batch —
                    // the remaining steps still get their conditions evaluated.
                    states[i] = StepState::Failed;
                    step_errors[i] = err_json(e, Some("SQLITE_ERROR"));
                    continue;
                }
            },
        };
        if !run {
            continue;
        }
        match resolve_sql(&step.stmt, cache).and_then(|sql| execute_stmt(conn, &sql, &step.stmt)) {
            Ok(r) => {
                states[i] = StepState::Ok;
                step_results[i] = r;
            }
            Err(e) => {
                states[i] = StepState::Failed;
                step_errors[i] = err_json(e, Some("SQLITE_ERROR"));
            }
        }
    }
    Ok(json!({ "step_results": step_results, "step_errors": step_errors }))
}
