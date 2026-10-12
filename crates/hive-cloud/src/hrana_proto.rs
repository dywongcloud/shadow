//! Hrana v2/v3 wire codec + execution against one [`SqlEngine`].
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
//!   silent type change to text. (A musql-backed database cannot hold one at
//!   all: `crate::musql::preflight` refuses to migrate a database that does.)
//! * **`blob` is base64 WITH padding**: `atob` in a browser requires it, and
//!   every padded decoder accepts it. Decoding accepts unpadded too.
//! * **Every request in a pipeline executes, even after one errors** — the
//!   spec is explicit, and `batch`'s conditions are built on later steps
//!   observing an earlier step's failure.
//!
//! Everything above the engine call is synchronous protocol work; the engine
//! call itself is awaited, so the rusqlite engine (blocking file IO) is driven
//! to completion inside `spawn_blocking` by `sqlite_pool::PooledConn` while the
//! musql engine (loopback HTTP) is awaited on the runtime. One pipeline
//! implementation serves both.
//!
//! Statement errors are tagged `SQLITE_ERROR` for EVERY engine. musqld emits
//! richer codes (`SQLITE_CONSTRAINT_UNIQUE`, `SQLITE_CONSTRAINT_FOREIGNKEY`,
//! `HRANA_PROTO_ERROR`); forwarding them would change the code every existing
//! tenant sees on day one, so `crate::musql` normalises them before they reach
//! this file and this file keeps tagging them the way it always has.

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sqlite_engine::{Budget, NamedArg, SqlEngine, SqlValue};

/// Rows buffered for one statement result. Hrana has a cursor endpoint for
/// unbounded reads; the pipeline endpoint buffers, so it needs a ceiling.
const MAX_ROWS: usize = 20_000;
/// Bytes buffered for one statement result.
const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;
/// Statements a stream may keep in its `store_sql` cache, and their total size.
const MAX_SQL_CACHE: usize = 256;
const MAX_SQL_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// Per-stream `store_sql` cache: `sql_id` -> SQL text.
pub type SqlCache = std::collections::HashMap<i32, String>;

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
    named_args: Vec<WireNamedArg>,
    #[serde(default)]
    want_rows: Option<bool>,
}

#[derive(Deserialize)]
struct WireNamedArg {
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

/// Execute a whole pipeline's `requests` array against one engine.
/// `version` gates the v3-only requests (`get_autocommit`, the `is_autocommit`
/// batch condition) so a v2 client can never observe a v3 response shape.
/// `read_only` is an ADDITIONAL, connection-independent enforcement layer —
/// see [`deny_if_forbidden_read_only_stmt`]'s doc for why a mutable
/// `PRAGMA query_only` alone is not a sufficient boundary.
pub async fn execute_pipeline<E: SqlEngine>(
    eng: &mut E,
    requests: Vec<Value>,
    mut cache: SqlCache,
    version: u8,
    read_only: bool,
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
                Ok(stmt) => match resolve_sql(&stmt, &cache) {
                    Ok(sql) => match deny_if_forbidden_read_only_stmt(&sql, read_only) {
                        Ok(()) => match wire_args(&stmt) {
                            Ok(a) => execute_stmt(eng, &sql, &stmt, a).await,
                            Err(e) => Err(e),
                        },
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                }
                .map(|r| json!({ "type": "execute", "result": r })),
                Err(e) => Err(format!("malformed stmt: {e}")),
            },
            "batch" => match serde_json::from_value::<Batch>(
                req.get("batch").cloned().unwrap_or(Value::Null),
            ) {
                Ok(batch) => execute_batch(eng, batch, &cache, version, read_only)
                    .await
                    .map(|r| json!({ "type": "batch", "result": r })),
                Err(e) => Err(format!("malformed batch: {e}")),
            },
            "sequence" if read_only => Err(
                "\"sequence\" (multi-statement execute_batch) is not permitted on a read-only \
                 browser_db session — a single string can hide an arbitrary later statement \
                 (e.g. a PRAGMA) past a leading-keyword check; use \"execute\" instead"
                    .to_string(),
            ),
            "sequence" => match sql_from_req(&req, &cache) {
                Ok(sql) => eng
                    .execute_batch(&sql)
                    .await
                    .map(|_| json!({ "type": "sequence" })),
                Err(e) => Err(e),
            },
            "describe" => match sql_from_req(&req, &cache) {
                Ok(sql) => describe(eng, &sql)
                    .await
                    .map(|r| json!({ "type": "describe", "result": r })),
                Err(e) => Err(e),
            },
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
                        "is_autocommit": eng.is_autocommit().await,
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

/// A read-only session's ONLY enforcement point that isn't a mutable
/// per-connection PRAGMA. `PRAGMA query_only=ON` is not a real boundary: it
/// is a session-scoped setting toggled by ordinary SQL text, and SQLite does
/// not gate PRAGMA execution behind the flag it itself controls, so
/// `PRAGMA query_only=OFF` sent over the same read-only-scoped connection
/// silently re-enables writes for the rest of the request. `ATTACH`/`DETACH`
/// are denied for the same reason (attaching a second database file sidesteps
/// this file's own `max_bytes` cap and any path-scoping assumption entirely).
/// Every raw SQL string reaching an engine's `run`/`execute_batch`/`describe`
/// from a read-only-scoped pipeline must pass through here first — never trust
/// a post-hoc readonly check, since PRAGMA/ATTACH survive that too.
fn deny_if_forbidden_read_only_stmt(sql: &str, read_only: bool) -> Result<(), String> {
    if !read_only {
        return Ok(());
    }
    let mut s = sql;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
            continue;
        }
        if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("");
            continue;
        }
        break;
    }
    let first_word: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    if matches!(first_word.as_str(), "PRAGMA" | "ATTACH" | "DETACH") {
        return Err(format!(
            "{first_word} is not permitted on a read-only browser_db session"
        ));
    }
    Ok(())
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

/// Wire `Value` -> engine value. An unknown `type` is an error, never a
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

fn encode_value(v: SqlValue, col: &str) -> Result<Value, String> {
    Ok(match v {
        SqlValue::Null => json!({ "type": "null" }),
        SqlValue::Integer(i) => json!({ "type": "integer", "value": i.to_string() }),
        SqlValue::Real(f) => {
            let n = serde_json::Number::from_f64(f).ok_or_else(|| {
                format!(
                    "column {col:?} holds a non-finite float ({f}), which JSON cannot represent — \
                     cast it in the query (e.g. CAST(x AS TEXT))"
                )
            })?;
            json!({ "type": "float", "value": n })
        }
        SqlValue::Text(t) => json!({ "type": "text", "value": t }),
        SqlValue::Blob(b) => json!({
            "type": "blob",
            "base64": base64::engine::general_purpose::STANDARD.encode(b),
        }),
    })
}

/// Decode one statement's arguments. Failing here fails the statement with a
/// named, typed error rather than binding a wrong-typed placeholder.
fn wire_args(s: &Stmt) -> Result<(Vec<SqlValue>, Vec<NamedArg>), String> {
    let mut args = Vec::with_capacity(s.args.len());
    for (i, a) in s.args.iter().enumerate() {
        args.push(
            decode_value(a).map_err(|e| format!("cannot decode positional argument {}: {e}", i + 1))?,
        );
    }
    let mut named = Vec::with_capacity(s.named_args.len());
    for na in &s.named_args {
        named.push(NamedArg {
            name: na.name.clone(),
            value: decode_value(&na.value)
                .map_err(|e| format!("cannot decode named argument {:?}: {e}", na.name))?,
        });
    }
    Ok((args, named))
}

// ---- Statement execution --------------------------------------------------

async fn execute_stmt<E: SqlEngine>(
    eng: &mut E,
    sql: &str,
    s: &Stmt,
    args: (Vec<SqlValue>, Vec<NamedArg>),
) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let (positional, named) = args;
    let out = eng
        .run(
            sql,
            s.want_rows.unwrap_or(true),
            &positional,
            &named,
            Budget {
                max_rows: MAX_ROWS,
                max_bytes: MAX_RESULT_BYTES,
            },
        )
        .await?;

    let cols: Vec<Value> = out
        .cols
        .iter()
        .map(|c| json!({ "name": c.name, "decltype": c.decltype }))
        .collect();
    let mut rows_json: Vec<Value> = Vec::with_capacity(out.rows.len());
    let mut bytes: usize = 0;
    for row in &out.rows {
        let mut out_row = Vec::with_capacity(row.len());
        for (i, v) in row.iter().enumerate() {
            let col = out.cols.get(i).map(|c| c.name.as_str()).unwrap_or("");
            let enc = encode_value(v.clone(), col)?;
            bytes += estimate_bytes(&enc);
            out_row.push(enc);
        }
        rows_json.push(Value::Array(out_row));
    }
    if bytes > MAX_RESULT_BYTES {
        return Err(format!(
            "result set exceeds {}MiB over the pipeline endpoint — narrow the projection or paginate",
            MAX_RESULT_BYTES / (1024 * 1024)
        ));
    }
    Ok(json!({
        "cols": cols,
        "rows": rows_json,
        "affected_row_count": out.rows_written,
        "last_insert_rowid": out.last_insert_rowid.map(|id| id.to_string()),
        // `rows_read` is the rows this statement STEPPED and `rows_written` the
        // rows it changed. libsql-server derives both from sqlite3_status
        // counters; these are the honest per-statement equivalents, not a
        // reimplementation of that accounting.
        "rows_read": out.rows_read,
        "rows_written": out.rows_written,
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

async fn describe<E: SqlEngine>(eng: &mut E, sql: &str) -> Result<Value, String> {
    let d = eng.describe(sql).await?;
    let params: Vec<Value> = d
        .params
        .iter()
        .map(|n| json!({ "name": n.clone() }))
        .collect();
    let cols: Vec<Value> = d
        .cols
        .iter()
        .map(|c| json!({ "name": c.name, "decltype": c.decltype }))
        .collect();
    Ok(json!({
        "params": params,
        "cols": cols,
        "is_explain": d.is_explain,
        "is_readonly": d.is_readonly,
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

async fn eval_cond<E: SqlEngine>(
    cond: &Value,
    states: &[StepState],
    eng: &mut E,
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
            let inner = cond
                .get("cond")
                .ok_or("`not` condition is missing `cond`")?;
            Ok(!Box::pin(eval_cond(inner, states, eng, version)).await?)
        }
        "and" | "or" => {
            let conds = cond
                .get("conds")
                .and_then(|c| c.as_array())
                .ok_or("`and`/`or` condition is missing `conds`")?;
            let mut acc = ty == "and";
            for c in conds {
                let v = Box::pin(eval_cond(c, states, eng, version)).await?;
                acc = if ty == "and" { acc && v } else { acc || v };
            }
            Ok(acc)
        }
        "is_autocommit" => {
            if version < 3 {
                Err("is_autocommit condition requires protocol version 3".into())
            } else {
                Ok(eng.is_autocommit().await)
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
pub async fn execute_cursor<E: SqlEngine>(
    eng: &mut E,
    batch: Value,
    cache: &SqlCache,
    version: u8,
    read_only: bool,
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
    let result = match execute_batch(eng, parsed, cache, version, read_only).await {
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

async fn execute_batch<E: SqlEngine>(
    eng: &mut E,
    batch: Batch,
    cache: &SqlCache,
    version: u8,
    read_only: bool,
) -> Result<Value, String> {
    let n = batch.steps.len();
    let mut states: Vec<StepState> = vec![StepState::Skipped; n];
    let mut step_results: Vec<Value> = vec![Value::Null; n];
    let mut step_errors: Vec<Value> = vec![Value::Null; n];

    for (i, step) in batch.steps.iter().enumerate() {
        let run = match &step.condition {
            None => true,
            Some(c) => match eval_cond(c, &states, eng, version).await {
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
        let res = match resolve_sql(&step.stmt, cache) {
            Ok(sql) => match deny_if_forbidden_read_only_stmt(&sql, read_only) {
                Ok(()) => match wire_args(&step.stmt) {
                    Ok(a) => execute_stmt(eng, &sql, &step.stmt, a).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        match res {
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
