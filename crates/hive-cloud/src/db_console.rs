//! A command console for ONE managed database INSTANCE, speaking that instance's own language.
//!
//! `db_metrics` answers "how big and how busy is this database"; this module answers "run this
//! against it". Every `DbKind` gets its own dialect, because one SQL-shaped box forced onto Redis or
//! a blob bucket would be a lie:
//!
//! * **Sqlite / Postgres / Supabase** — SQL text (Supabase IS Postgres underneath, plus Studio).
//! * **Redis** — a RESP command line (`GET foo`, `HGETALL h`, `SCAN 0`), never SQL.
//! * **Blob** — object operations (`LIST`, `HEAD k`, `GET k`, `PUT k v`, `DELETE k`).
//! * **Queue** — `DEPTH`, `PEEK n`, `PUSH msg`, `POP`.
//! * **Vector** — `COUNT`, `QUERY [0.1,0.2]`.
//! * **Pubsub** — `INFO`, `PUBLISH msg`.
//! * **Realtime** — `INFO` only: there is no REST publish surface, so it gets a read-only inspector
//!   rather than a fake write box.
//!
//! # The default is non-destructive
//!
//! Three tiers, decided SERVER-SIDE from the command text, never from a client label:
//!
//! * **read** — the default, always allowed for an authorized caller.
//! * **write** — needs `"write": true` on the request (INSERT/UPDATE/DELETE, `SET`, `PUT`, `PUSH`).
//! * **destructive** — needs `"destructive": true` AND a caller at team-admin or a platform operator
//!   (DROP/TRUNCATE/ALTER/GRANT, `FLUSHALL`/`FLUSHDB`).
//!
//! A statement in no known set is REFUSED, not executed: the console is an allowlist, so a command
//! invented after this file was written does not silently become reachable.
//!
//! # Security
//!
//! * Authorization is per-DATABASE and happens in the handler (`auth::require_auth` only proves
//!   "some valid token"). A caller authorized for database A cannot reach database B: a mismatch is
//!   a 404 that names nothing.
//! * No response or log line ever carries a credential. The record's `connection` map holds
//!   `DATABASE_URL`, passwords and tokens; they are read to dial the engine and never echoed, and
//!   engine error text is scrubbed (`scrub`) because a connection failure puts the whole DSN in its
//!   message.
//! * No host escape: `ATTACH`/`DETACH`, `COPY` and the Postgres server-file functions are refused
//!   outright, so the console cannot be steered at the node's filesystem or another database file.
//! * No shell. There is no path from a console command to a process.
//!
//! # Routing
//!
//! The engine kinds (Sqlite/Postgres/Supabase/Redis) are a file or a loopback port on
//! `Database::host_node`, so only that node can execute: every other node proxies there over HTTP
//! admin with a signed delegation token, falling back to the mesh (`/console-mesh`, the
//! `hrana-mesh` / `studio-mesh` shape), and the owner re-authorizes against its own record and never
//! re-proxies. The platform-native kinds are an in-process store with no owner, so they execute
//! locally — and say so in `scope`.

use crate::databases::{Database, DbKind};
use crate::state::CloudState;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Longest command text accepted. A console is one line of intent, not a dump file.
const INPUT_CAP: usize = 64 * 1024;
/// Per-command budget against the backing engine.
const ENGINE_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for the owner hop: the HTTP admin attempt and the mesh fallback.
const HOP_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HOP_MESH_TIMEOUT_SECS: u64 = 30;
/// Rows rendered in one reply (the engine may have produced more; `truncated` says so).
const ROW_CAP: usize = 200;
/// Bytes of one object value rendered into a reply.
const VALUE_CAP: usize = 4096;
/// Cap on an engine error message: enough to debug, short enough that no credential-shaped dump
/// survives it in full.
const ERR_CAP: usize = 300;

/// The request body. `write`/`destructive` are OPT-INS the server re-checks against its own
/// classification of `input`: a client cannot relabel a `DROP` as a read.
#[derive(serde::Deserialize)]
pub struct ConsoleReq {
    /// The command text, in the dialect of this database's kind.
    pub input: String,
    /// Permit a write-tier command (default false = read-only inspection).
    #[serde(default)]
    pub write: bool,
    /// Permit a destructive-tier command. Also requires team-admin or a platform operator.
    #[serde(default)]
    pub destructive: bool,
}

/// What a command DOES, decided from its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Read,
    Write,
    Destructive,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Read => "read",
            Tier::Write => "write",
            Tier::Destructive => "destructive",
        }
    }
}

/// One tabular result, whatever the dialect produced.
struct Out {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    engine: Option<String>,
    truncated: bool,
}

/// How a local execution failed. `Client` is an HTTP 4xx (the caller's text); `Engine` is reported
/// as `ok: false` with a scrubbed message, because a failing engine is not a caller error.
enum Fail {
    Client(String),
    Engine(String),
}

/// Who is asking: the tenant from the cryptographically-bound claim, never the `x-hive-team` header.
pub(crate) struct Caller {
    pub tenant: String,
    /// A verified `platform_admin` (or an unenforced dev node): may cross tenants.
    pub operator: bool,
    /// Team role at or above `admin`, the floor for a destructive command.
    pub admin: bool,
}

pub(crate) fn resolve_caller(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: Option<&crate::auth::Claims>,
) -> Caller {
    Caller {
        tenant: crate::admin::tenant(c, headers, claims),
        operator: crate::admin::require_operator(claims).is_ok(),
        admin: claims
            .map(|cl| {
                crate::roles::role_of(&cl.role).rank() >= crate::teams::Role::Admin.rank()
            })
            .unwrap_or(false),
    }
}

/// Whether this caller may touch this database at all: its own tenant's record, or an operator.
/// Deliberately a 404 at the call site, never a 403 — a caller must not be able to confirm that
/// another tenant's database id exists.
fn admits(caller: &Caller, db: &Database) -> bool {
    caller.operator
        || crate::admin::norm(crate::admin::record_tenant(&db.team))
            == crate::admin::norm(&caller.tenant)
}

/// The team to carry on an owner hop: an operator crosses tenants by presenting the DATABASE'S OWN
/// team, which the owner re-verifies, rather than a forgeable "I am an operator" flag.
fn carry_team(caller: &Caller, db: &Database) -> String {
    if caller.operator {
        crate::admin::record_tenant(&db.team).to_string()
    } else {
        caller.tenant.clone()
    }
}

// ---------------------------------------------------------------- classification

/// Strips leading comments and whitespace so `--drop` cannot hide a keyword from the leading-word
/// check (the same loop `hrana_proto::deny_if_forbidden_read_only_stmt` uses, and for the same
/// reason: a post-hoc check is not a boundary).
fn strip_noise(sql: &str) -> String {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
            continue;
        }
        if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("").trim_start();
            continue;
        }
        break;
    }
    s.trim().to_string()
}

fn first_word(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Substrings that reach OUT of the database: the host filesystem, another database file, or a
/// subprocess. Refused for every kind and every tier — there is no opt-in for a host escape.
const HOST_ESCAPE: &[&str] = &[
    "copy",
    "attach",
    "detach",
    // PRAGMA is engine and FILE configuration, not data: `writable_schema`, `journal_mode`,
    // `temp_store_directory` and friends all reach past the row set. Refused at every tier, which
    // is why it is NOT in `SQL_WRITE` — a write-tier PRAGMA would otherwise run as soon as a caller
    // switched the console to write mode.
    "pragma",
    "writable_schema",
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_stat_file",
    "pg_logdir_ls",
    "lo_import",
    "lo_export",
    "dblink",
    "file_fdw",
    "postgres_fdw",
];

const SQL_DESTRUCTIVE: &[&str] = &["drop", "truncate", "alter", "grant", "revoke"];

const SQL_WRITE: &[&str] = &[
    "insert", "update", "delete", "replace", "upsert", "merge", "create", "vacuum",
    "reindex", "analyze", "begin", "commit", "rollback", "savepoint", "release", "set", "reset",
    "comment", "refresh", "cluster", "lock", "listen", "unlisten", "notify", "call", "do",
];

const SQL_READ: &[&str] = &[
    "select", "with", "values", "table", "explain", "show", "describe", "desc", "fetch", "declare",
    "union", "intersect", "except",
];

fn sql_tier(sql: &str) -> Result<Tier, String> {
    let s = strip_noise(sql);
    if s.is_empty() {
        return Err("empty statement".to_string());
    }
    if s.contains(';') {
        // One statement per request: a second statement after a `;` is invisible to the
        // leading-keyword check and is the classic stacked-statement injection.
        return Err("one statement per request: a ';' lets another statement hide behind the first"
            .to_string());
    }
    let lower = s.to_ascii_lowercase();
    for bad in HOST_ESCAPE {
        if lower.contains(bad) {
            return Err(format!(
                "{bad} is not available from the console: it can reach the host filesystem, \
                 another database file or a subprocess"
            ));
        }
    }
    let head = first_word(&s);
    let head_lower = head.to_ascii_lowercase();
    if SQL_DESTRUCTIVE.contains(&head_lower.as_str()) {
        return Ok(Tier::Destructive);
    }
    if SQL_WRITE.contains(&head_lower.as_str()) {
        return Ok(Tier::Write);
    }
    if SQL_READ.contains(&head_lower.as_str()) {
        return Ok(Tier::Read);
    }
    Err(format!(
        "{head} is not in the console's statement set — allowed: {}",
        "SELECT/WITH/EXPLAIN/SHOW (read), INSERT/UPDATE/DELETE/CREATE (write), \
         DROP/TRUNCATE/ALTER (destructive)"
    ))
}

/// Redis commands the console speaks. Anything else is refused: the set is an allowlist, so a
/// command added to the engine later does not become reachable by accident. The engine-administration
/// class reuses the REST gateway's own deny list (one list, two surfaces).
const RESP_READ: &[&str] = &[
    "ping", "echo", "dbsize", "info", "time", "get", "mget", "strlen", "getrange", "exists",
    "type", "ttl", "pttl", "object", "memory", "llen", "lrange", "lindex", "scard", "smembers",
    "sismember", "srandmember", "zcard", "zrange", "zrevrange", "zscore", "zrank", "zcount",
    "hget", "hmget", "hgetall", "hkeys", "hvals", "hlen", "hexists", "hstrlen", "scan", "sscan",
    "hscan", "zscan", "xlen", "xrange", "geodist", "geopos", "bitcount", "getbit", "pfcount",
];

const RESP_WRITE: &[&str] = &[
    "set", "setnx", "setex", "psetex", "mset", "msetnx", "append", "setrange", "incr", "incrby",
    "incrbyfloat", "decr", "decrby", "del", "unlink", "expire", "pexpire", "expireat",
    "pexpireat", "persist", "rename", "renamenx", "getset", "getdel", "hset", "hsetnx", "hdel",
    "hincrby", "hincrbyfloat", "lpush", "rpush", "lpushx", "rpushx", "lpop", "rpop", "lset",
    "ltrim", "lrem", "rpoplpush", "sadd", "srem", "smove", "spop", "zadd", "zrem", "zincrby",
    "zremrangebyrank", "zremrangebyscore", "xadd", "xdel", "xtrim", "setbit", "pfadd",
];

/// Data-destroying WHOLE-DB commands: the same one-way door as `DROP TABLE`, so the same tier.
const RESP_DESTRUCTIVE: &[&str] = &["flushall", "flushdb"];

/// Console-only refusals on top of the gateway's list: `KEYS` walks the whole keyspace and blocks
/// the engine (use `SCAN`), and `CLIENT`/`COMMAND` are engine administration.
const RESP_EXTRA_DENY: &[&str] = &["keys", "client", "command", "lolwut", "hello", "auth"];

fn resp_tier(parts: &[String]) -> Result<Tier, String> {
    let cmd = parts[0].to_ascii_lowercase();
    let upper = parts[0].to_ascii_uppercase();
    if crate::db_rest::command_denied(&cmd).is_some() {
        return Err(format!("{upper} is not available from the console"));
    }
    if RESP_EXTRA_DENY.contains(&cmd.as_str()) {
        return Err(format!(
            "{upper} is not available from the console (use SCAN, not KEYS)"
        ));
    }
    if RESP_DESTRUCTIVE.contains(&cmd.as_str()) {
        return Ok(Tier::Destructive);
    }
    if RESP_WRITE.contains(&cmd.as_str()) {
        return Ok(Tier::Write);
    }
    if RESP_READ.contains(&cmd.as_str()) {
        return Ok(Tier::Read);
    }
    Err(format!(
        "{upper} is not in the console's command set — allowed: {}",
        "GET/HGETALL/LRANGE/SCAN/INFO/DBSIZE (read), SET/DEL/HSET/LPUSH (write), \
         FLUSHALL/FLUSHDB (destructive)"
    ))
}

/// The tier of a native (blob/queue/vector/pubsub/realtime) operation.
const NATIVE_READ: &[&str] = &["list", "head", "get", "stats", "depth", "peek", "count", "info", "query"];
const NATIVE_WRITE: &[&str] = &["put", "delete", "push", "pop", "publish"];

fn native_tier(kind: DbKind, op: &str) -> Result<Tier, String> {
    let o = op.to_ascii_lowercase();
    if kind == DbKind::Realtime && o != "info" {
        return Err(
            "realtime has no REST command surface; this console is a read-only inspector (INFO)"
                .to_string(),
        );
    }
    if kind == DbKind::Vector && !matches!(o.as_str(), "count" | "stats" | "query") {
        return Err(
            "vector has no delete surface; this console reads only (COUNT, QUERY)".to_string()
        );
    }
    if NATIVE_WRITE.contains(&o.as_str()) {
        return Ok(Tier::Write);
    }
    if NATIVE_READ.contains(&o.as_str()) {
        return Ok(Tier::Read);
    }
    Err(format!(
        "{} is not an operation this console offers for {}",
        op.to_uppercase(),
        kind.label()
    ))
}

/// Classifies `input` for one database's kind. `Deny` refusals are unconditional.
fn classify(kind: DbKind, input: &str) -> Result<Tier, String> {
    match kind {
        DbKind::Sqlite | DbKind::Postgres | DbKind::Supabase => sql_tier(input),
        DbKind::Redis => {
            let parts = split_cmd(input);
            if parts.is_empty() {
                return Err("empty command".to_string());
            }
            resp_tier(&parts)
        }
        _ => {
            let parts = split_cmd(input);
            let op = parts.first().cloned().unwrap_or_default();
            if op.is_empty() {
                return Err("empty command".to_string());
            }
            native_tier(kind, &op)
        }
    }
}

/// Shell-like tokenisation (single and double quotes group; no escapes, no globs, no `$`). The
/// console takes TEXT, never a shell: there is no path from here to a process.
fn split_cmd(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in s.chars() {
        match ch {
            '"' | '\'' => {
                if quote == Some(ch) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(ch);
                    started = true;
                } else {
                    cur.push(ch);
                }
            }
            c if c.is_whitespace() && quote.is_none() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------- execution

/// Which engine actually answers a SQLite database: `sqlite` opens the `.db`, `musql` the `.musq`
/// through the sidecar (which may be down — an honest 503, never a fall-through to the other file).
fn sqlite_engine_label(db: &Database) -> String {
    match crate::musql::engine_for_db(&db.connection) {
        crate::musql::Engine::Musql => "musql".to_string(),
        crate::musql::Engine::Rusqlite => "sqlite".to_string(),
    }
}

/// One Hrana pipeline: `execute` then `close`, so an execution never leaves a stream (and a pooled
/// connection) pinned for 30 s after the reply. Reuses `hrana::serve` wholesale — its credential
/// check, its owner routing, its rusqlite/musql choice and its quiescing guard — rather than a
/// second way to open the file.
async fn exec_sqlite(c: &Arc<CloudState>, db: &Database, sql: &str) -> Result<Out, Fail> {
    let token = db
        .connection
        .get("DB_REST_TOKEN")
        .cloned()
        .unwrap_or_default();
    if token.is_empty() {
        return Err(Fail::Engine(
            "this database has no Hrana token on record".to_string(),
        ));
    }
    let body = json!({
        "baton": Value::Null,
        "requests": [
            { "type": "execute", "stmt": { "sql": sql, "args": [] } },
            { "type": "close" }
        ],
    })
    .to_string();
    let resp = crate::hrana::serve(
        c.clone(),
        db.clone(),
        token,
        axum::http::Method::POST,
        "/v2/pipeline".to_string(),
        axum::body::Bytes::from(body),
    )
    .await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = parsed
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("the SQLite engine refused this statement")
            .to_string();
        return Err(if status.is_client_error() {
            Fail::Client(scrub(&msg))
        } else {
            Fail::Engine(scrub(&msg))
        });
    }
    let first = parsed
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(Value::Null);
    if first.get("type").and_then(|t| t.as_str()) == Some("error") {
        let msg = first
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("statement failed")
            .to_string();
        return Err(Fail::Engine(scrub(&msg)));
    }
    let result = first
        .get("response")
        .and_then(|r| r.get("result"))
        .cloned()
        .unwrap_or(Value::Null);
    let columns = result
        .get("cols")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let total = result.get("rows").and_then(|r| r.as_array()).map(|a| a.len());
    let rows: Vec<Vec<Value>> = result
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .take(ROW_CAP)
                .map(|r| {
                    r.as_array()
                        .map(|cells| cells.iter().map(decode_cell).collect())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = Out {
        columns,
        rows,
        engine: Some(sqlite_engine_label(db)),
        truncated: total.unwrap_or(0) > ROW_CAP,
    };
    if out.columns.is_empty() {
        // A statement that returns no column set (INSERT, PRAGMA that answers nothing) still has an
        // observable outcome, and an empty table reads as "the console did nothing".
        out.columns = vec!["affected_rows".to_string()];
        out.rows = vec![vec![json!(
            result.get("affected_row_count").and_then(|v| v.as_u64()).unwrap_or(0)
        )]];
    }
    Ok(out)
}

/// Hrana encodes a cell as `{"type":..,"value":..}` or `{"type":"base64","base64":..}`; the console
/// shows the value, and a non-UTF-8 blob is reported by size, never dumped.
fn decode_cell(v: &Value) -> Value {
    if let Some(inner) = v.get("value") {
        return inner.clone();
    }
    if let Some(b64) = v.get("base64").and_then(|b| b.as_str()) {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok();
        return match decoded {
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Value::String(s),
                Err(e) => json!(format!("<{} bytes binary>", e.as_bytes().len())),
            },
            None => Value::String("<undecodable>".to_string()),
        };
    }
    v.clone()
}

/// SQL over the engine's loopback port: the same `run_sql` the REST gateway uses, with REAL
/// parameter binding and no caller text interpolated.
async fn exec_sql(db: &Database, sql: &str) -> Result<Out, Fail> {
    let g = |k: &str| db.connection.get(k).cloned().unwrap_or_default();
    let port: u16 = match g("local_port").parse() {
        Ok(p) => p,
        Err(_) => {
            return Err(Fail::Engine(
                "this database has no live engine port on its host node".to_string(),
            ))
        }
    };
    let dbname = {
        let d = g("database");
        if d.is_empty() {
            "postgres".to_string()
        } else {
            d
        }
    };
    let req = crate::db_rest::SqlReq {
        query: sql.to_string(),
        params: Vec::new(),
    };
    let run = tokio::time::timeout(
        ENGINE_TIMEOUT,
        crate::db_rest::run_sql(port, &g("user"), &g("password"), &dbname, &req),
    )
    .await;
    let v = match run {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(Fail::Engine(scrub(&e.to_string()))),
        Err(_) => return Err(Fail::Engine("query timed out".to_string())),
    };
    let columns: Vec<String> = v
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let all = v.get("rows").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let rows: Vec<Vec<Value>> = all
        .iter()
        .take(ROW_CAP)
        .map(|row| {
            // Postgres replies as keyed objects; the console is one table, so the row is projected
            // in field order.
            columns
                .iter()
                .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    let mut out = Out {
        columns,
        rows,
        engine: Some("postgres".to_string()),
        truncated: all.len() > ROW_CAP,
    };
    if out.columns.is_empty() {
        out.columns = vec!["row_count".to_string()];
        out.rows = vec![vec![json!(
            v.get("rowCount").and_then(|n| n.as_u64()).unwrap_or(0)
        )]];
    }
    Ok(out)
}

/// RESP over the engine's loopback port. The reply is not a table, so it is rendered as one: a
/// scalar is one row, an array is one row per element.
async fn exec_redis(db: &Database, parts: &[String]) -> Result<Out, Fail> {
    let g = |k: &str| db.connection.get(k).cloned().unwrap_or_default();
    let port: u16 = match g("local_port").parse() {
        Ok(p) => p,
        Err(_) => {
            return Err(Fail::Engine(
                "this database has no live engine port on its host node".to_string(),
            ))
        }
    };
    let run = tokio::time::timeout(
        ENGINE_TIMEOUT,
        crate::resp::run_command(port, &g("password"), parts),
    )
    .await;
    let reply = match run {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(Fail::Engine(scrub(&e.to_string()))),
        Err(_) => return Err(Fail::Engine("command timed out".to_string())),
    };
    let v = match &reply {
        crate::resp::Reply::Error(e) => return Err(Fail::Engine(scrub(e))),
        other => other.to_json(),
    };
    let rows = match v {
        Value::Array(items) => items
            .into_iter()
            .take(ROW_CAP)
            .map(|i| vec![i])
            .collect::<Vec<Vec<Value>>>(),
        other => vec![vec![other]],
    };
    Ok(Out {
        columns: vec!["value".to_string()],
        rows,
        engine: Some("redis".to_string()),
        truncated: false,
    })
}

/// The tenant namespace a platform-native payload lives in: the `/v1/storage/*` lanes address by
/// NAME inside `<tenant>::<name>`, so the console addresses the same cells the tenant's app does.
fn native_ns(team: &str, name: &str) -> String {
    format!("{}::{}", crate::admin::norm(team), name)
}

/// Executes a platform-native operation against this node's in-process store. These kinds have no
/// owning node, so `scope` on the reply says the reading is this node's.
async fn exec_native(
    c: &Arc<CloudState>,
    db: &Database,
    input: &str,
    tier: Tier,
) -> Result<Out, Fail> {
    if tier != Tier::Read && !c.is_control_plane_leader() {
        // A write landed anywhere would diverge from the leader's copy with no replication path for
        // these stores; refuse rather than write a row only one node will ever see.
        return Err(Fail::Client(
            "this store's writes are served by the control-plane leader; retry".to_string(),
        ));
    }
    let g = |k: &str| db.connection.get(k).cloned().unwrap_or_default();
    let parts = split_cmd(input);
    let op = parts.first().cloned().unwrap_or_default().to_ascii_lowercase();
    let arg = |i: usize| parts.get(i).cloned().unwrap_or_default();
    let rest = |from: usize| parts.get(from..).unwrap_or_default().join(" ");
    let team = crate::admin::record_tenant(&db.team);

    match db.kind {
        DbKind::Blob => {
            let bucket = g("bucket");
            if bucket.is_empty() {
                return Err(Fail::Engine("this database records no bucket".to_string()));
            }
            let ns = native_ns(team, &bucket);
            match op.as_str() {
                "stats" => {
                    let (objects, bytes) = c.databases.blob_stats(&ns);
                    Ok(tab(
                        &["objects", "bytes"],
                        vec![vec![json!(objects), json!(bytes)]],
                        None,
                    ))
                }
                "list" => {
                    let keys = c.databases.blob_list(&ns);
                    let total = keys.len();
                    let rows = keys
                        .into_iter()
                        .take(ROW_CAP)
                        .map(|k| vec![Value::String(k)])
                        .collect();
                    Ok(trunc(tab(&["key"], rows, None), total > ROW_CAP))
                }
                "head" => {
                    let key = arg(1);
                    if key.is_empty() {
                        return Err(Fail::Client("HEAD needs a key".to_string()));
                    }
                    match c.databases.blob_object_size(&ns, &key) {
                        Some(n) => Ok(tab(&["key", "bytes"], vec![vec![json!(key), json!(n)]], None)),
                        None => Ok(tab(&["key", "bytes"], vec![], None)),
                    }
                }
                "get" => {
                    let key = arg(1);
                    if key.is_empty() {
                        return Err(Fail::Client("GET needs a key".to_string()));
                    }
                    match c.databases.blob_get(&ns, &key) {
                        Some(bytes) => {
                            let shown = &bytes[..bytes.len().min(VALUE_CAP)];
                            let value = match String::from_utf8(shown.to_vec()) {
                                Ok(s) => Value::String(s),
                                Err(_) => json!(format!("<{} bytes binary>", bytes.len())),
                            };
                            Ok(trunc(
                                tab(
                                    &["key", "bytes", "value"],
                                    vec![vec![json!(key), json!(bytes.len()), value]],
                                    None,
                                ),
                                bytes.len() > VALUE_CAP,
                            ))
                        }
                        None => Ok(tab(&["key", "bytes", "value"], vec![], None)),
                    }
                }
                "put" => {
                    let key = arg(1);
                    if key.is_empty() {
                        return Err(Fail::Client("PUT needs a key and a value".to_string()));
                    }
                    let value = rest(2);
                    c.databases.blob_put(&ns, &key, value.as_bytes().to_vec());
                    Ok(tab(
                        &["key", "bytes"],
                        vec![vec![json!(key), json!(value.len())]],
                        None,
                    ))
                }
                "delete" => {
                    let key = arg(1);
                    if key.is_empty() {
                        return Err(Fail::Client("DELETE needs a key".to_string()));
                    }
                    let existed = c.databases.blob_object_size(&ns, &key).is_some();
                    c.databases.blob_delete(&ns, &key);
                    Ok(tab(
                        &["key", "deleted"],
                        vec![vec![json!(key), json!(existed)]],
                        None,
                    ))
                }
                other => Err(Fail::Client(format!("unknown blob operation {other}"))),
            }
        }
        DbKind::Queue => {
            let queue = g("queue");
            if queue.is_empty() {
                return Err(Fail::Engine("this database records no queue".to_string()));
            }
            let ns = native_ns(team, &queue);
            match op.as_str() {
                "depth" | "stats" => Ok(tab(
                    &["depth"],
                    vec![vec![json!(c.databases.queue_depth(&ns))]],
                    None,
                )),
                "peek" => {
                    let n = arg(1).parse::<usize>().unwrap_or(10).clamp(1, ROW_CAP);
                    let rows = c
                        .databases
                        .queue_peek(&ns, n)
                        .into_iter()
                        .map(|m| vec![Value::String(m)])
                        .collect();
                    Ok(tab(&["message"], rows, None))
                }
                "push" => {
                    let msg = rest(1);
                    if msg.is_empty() {
                        return Err(Fail::Client("PUSH needs a message".to_string()));
                    }
                    let depth = c.databases.queue_push(&ns, msg.clone());
                    Ok(tab(
                        &["bytes", "depth"],
                        vec![vec![json!(msg.len()), json!(depth)]],
                        None,
                    ))
                }
                "pop" => match c.databases.queue_pop(&ns) {
                    Some(m) => Ok(tab(&["message"], vec![vec![Value::String(m)]], None)),
                    None => Ok(tab(&["message"], vec![], None)),
                },
                other => Err(Fail::Client(format!("unknown queue operation {other}"))),
            }
        }
        DbKind::Vector => {
            let index = g("index");
            if index.is_empty() {
                return Err(Fail::Engine("this database records no index".to_string()));
            }
            let ns = native_ns(team, &index);
            match op.as_str() {
                "count" | "stats" => {
                    let (count, dim) = c.databases.vector_stats(&ns);
                    Ok(tab(
                        &["vectors", "dimensions"],
                        vec![vec![json!(count), json!(dim.unwrap_or(0))]],
                        None,
                    ))
                }
                "query" => {
                    let v: Vec<f32> = match serde_json::from_str::<Vec<f32>>(&rest(1)) {
                        Ok(v) if !v.is_empty() => v,
                        _ => {
                            return Err(Fail::Client(
                                "QUERY needs a JSON vector, e.g. QUERY [0.1,0.2]".to_string(),
                            ))
                        }
                    };
                    let matches = c.databases.vector_query(&ns, &v, 10);
                    let rows = matches
                        .into_iter()
                        .map(|m| {
                            vec![
                                m.get("id").cloned().unwrap_or(Value::Null),
                                m.get("score").cloned().unwrap_or(Value::Null),
                                m.get("metadata").cloned().unwrap_or(Value::Null),
                            ]
                        })
                        .collect();
                    Ok(tab(&["id", "score", "metadata"], rows, None))
                }
                other => Err(Fail::Client(format!("unknown vector operation {other}"))),
            }
        }
        DbKind::Pubsub => {
            let topic = g("topic");
            if topic.is_empty() {
                return Err(Fail::Engine("this database records no topic".to_string()));
            }
            let ns = native_ns(team, &topic);
            match op.as_str() {
                "info" | "stats" => Ok(tab(
                    &["subscribers", "published"],
                    vec![vec![
                        json!(c.databases.subscriber_count(&ns)),
                        json!(c.databases.published_count(&ns)),
                    ]],
                    None,
                )),
                "publish" => {
                    let msg = rest(1);
                    if msg.is_empty() {
                        return Err(Fail::Client("PUBLISH needs a message".to_string()));
                    }
                    let n = c.databases.publish(&ns, msg.clone());
                    Ok(tab(
                        &["bytes", "delivered"],
                        vec![vec![json!(msg.len()), json!(n)]],
                        None,
                    ))
                }
                other => Err(Fail::Client(format!("unknown pub/sub operation {other}"))),
            }
        }
        DbKind::Realtime => {
            let room = g("room");
            if room.is_empty() {
                return Err(Fail::Engine("this database records no room".to_string()));
            }
            let ns = native_ns(team, &room);
            Ok(tab(
                &["subscribers", "published"],
                vec![vec![
                    json!(c.databases.subscriber_count(&ns)),
                    json!(c.databases.published_count(&ns)),
                ]],
                Some("realtime".to_string()),
            ))
        }
        other => Err(Fail::Client(format!(
            "{} has no console command surface",
            other.label()
        ))),
    }
}

fn tab(columns: &[&str], rows: Vec<Vec<Value>>, engine: Option<String>) -> Out {
    Out {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        rows,
        engine,
        truncated: false,
    }
}

fn trunc(mut out: Out, t: bool) -> Out {
    out.truncated = t;
    out
}

/// Executes one classified command on THIS node. The caller has already established that this node
/// hosts the engine (or that the kind is platform-native).
async fn exec_local(
    c: &Arc<CloudState>,
    db: &Database,
    req: &ConsoleReq,
    tier: Tier,
) -> Result<Out, Fail> {
    match db.kind {
        DbKind::Sqlite => exec_sqlite(c, db, req.input.trim()).await,
        DbKind::Postgres | DbKind::Supabase => exec_sql(db, req.input.trim()).await,
        DbKind::Redis => {
            let parts = split_cmd(&req.input);
            exec_redis(db, &parts).await
        }
        _ => exec_native(c, db, &req.input, tier).await,
    }
}

// ---------------------------------------------------------------- response

/// Removes credential-shaped text from an engine error: a Postgres connection failure puts the whole
/// DSN (`postgres://user:password@host/db`) in its message, and a console that echoed that would be
/// the easiest credential leak on the platform. Also bounds the length.
fn scrub(s: &str) -> String {
    let mut out = s.to_string();
    while let Some(start) = out.find("://") {
        let after = &out[start + 3..];
        let at = match after.find('@') {
            Some(i) => i,
            None => break,
        };
        let host_start = start + 3 + at;
        let end = out[host_start..]
            .find(|c: char| c == '/' || c == '?' || c == ' ' || c == ',' || c == '"')
            .map(|i| host_start + i)
            .unwrap_or(out.len());
        // Keep the scheme and the host, drop everything between them (userinfo).
        let replaced = format!("{}***:***@{}", &out[..start + 3], &out[host_start + 1..end]);
        out = format!("{replaced}{}", &out[end..]);
        if out.len() > ERR_CAP * 4 {
            break;
        }
    }
    if out.len() > ERR_CAP {
        out.truncate(ERR_CAP);
        out.push_str("…");
    }
    out
}

/// What this kind's console can do, for the UI (which renders the destructive tier differently).
fn capabilities(kind: DbKind) -> Value {
    let (write, destructive, note) = match kind {
        DbKind::Sqlite | DbKind::Postgres | DbKind::Supabase => (
            true,
            true,
            "DROP/TRUNCATE/ALTER need the destructive switch and an admin/operator caller",
        ),
        DbKind::Redis => (
            true,
            true,
            "FLUSHALL/FLUSHDB need the destructive switch and an admin/operator caller",
        ),
        DbKind::Blob => (
            true,
            false,
            "no bucket-delete surface: DELETE removes one object, never a whole bucket",
        ),
        DbKind::Queue => (true, false, "POP CONSUMES a message: it is a write, not a read"),
        DbKind::Vector => (false, false, "no delete surface: read-only inspector"),
        DbKind::Pubsub => (true, false, "PUBLISH fans out to live subscribers"),
        DbKind::Realtime => (false, false, "no REST surface: read-only inspector"),
    };
    json!({
        "read": true,
        "write": write,
        "destructive": destructive,
        "note": note,
    })
}

/// The placeholder and dialect hint the console's input box shows.
pub fn syntax_hint(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Sqlite | DbKind::Postgres | DbKind::Supabase => {
            "SELECT * FROM … — one SQL statement, no trailing semicolon"
        }
        DbKind::Redis => "GET mykey · HGETALL myhash · SCAN 0 · INFO — a RESP command, not SQL",
        DbKind::Blob => "LIST · HEAD mykey · GET mykey · PUT mykey value · DELETE mykey",
        DbKind::Queue => "DEPTH · PEEK 10 · PUSH message · POP",
        DbKind::Vector => "COUNT · QUERY [0.1,0.2,0.3]",
        DbKind::Pubsub => "INFO · PUBLISH message",
        DbKind::Realtime => "INFO",
    }
}

fn render(
    c: &Arc<CloudState>,
    db: &Database,
    req: &ConsoleReq,
    tier: Tier,
    out: Result<Out, Fail>,
    duration_ms: u64,
    proxied: bool,
) -> Value {
    let (ok, columns, rows, truncated, engine, message) = match out {
        Ok(o) => (
            true,
            o.columns,
            o.rows,
            o.truncated,
            o.engine,
            Value::Null,
        ),
        Err(Fail::Client(m)) => {
            return json!({
                "ok": false,
                "db_id": db.id,
                "kind": db.kind,
                "kind_label": db.kind.label(),
                "node": c.node_name,
                "tier": tier.label(),
                "op": req.input,
                "duration_ms": duration_ms,
                "error": m,
                "code": "REFUSED",
                "capabilities": capabilities(db.kind),
            })
        }
        Err(Fail::Engine(m)) => (false, Vec::new(), Vec::new(), false, None, json!(m)),
    };
    let row_count = rows.len();
    json!({
        "ok": ok,
        "db_id": db.id,
        "name": db.name,
        "kind": db.kind,
        "kind_label": db.kind.label(),
        "engine": engine,
        "host_node": db.host_node,
        "node": c.node_name,
        "proxied": proxied,
        "scope": if crate::db_metrics::owner_routed_kind(db.kind) { "instance" } else { "node-local" },
        "op": req.input,
        "tier": tier.label(),
        "columns": columns,
        "rows": rows,
        "row_count": row_count,
        "truncated": truncated,
        "duration_ms": duration_ms,
        "measured_at_ms": hive_core::now_ms(),
        "message": message,
        "capabilities": capabilities(db.kind),
        "hint": syntax_hint(db.kind),
    })
}

// ---------------------------------------------------------------- routing

/// The envelope carried to the owning node: the command plus the AUTHORITATIVE identity of the
/// caller that asked for it. Under enforcement the signed token wins over the raw `team` (the
/// `mesh_team_qs` shape), so a peer cannot widen the request; when auth is off there is nothing to
/// forge against and the raw fields are trusted (the documented dev posture of `write_scope`).
fn envelope(caller: &Caller, db: &Database, req: &ConsoleReq, tier: Tier) -> Value {
    let carry = carry_team(caller, db);
    let mut env = json!({
        "input": req.input,
        "write": req.write,
        "destructive": req.destructive,
        "tier": tier.label(),
        "team": carry,
        "role_rank": if caller.admin { "admin" } else { "member" },
        "operator": caller.operator,
    });
    if crate::auth::enforced() {
        if let Ok(tok) = crate::auth::issue(
            "mesh-internal",
            &carry,
            if caller.admin { "admin" } else { "member" },
            caller.operator,
            crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
        ) {
            env["tok"] = json!(tok);
        }
    }
    env
}

/// Sends one console envelope to the node hosting the engine: HTTP admin first, the mesh as
/// fallback (the `hrana::forward_to_owner` two-tier shape). `None` when neither answered, which the
/// caller turns into an honest 421 rather than a locally invented result.
async fn post_to_host(c: &Arc<CloudState>, node: &str, id: &str, env: &Value) -> Option<Value> {
    let route = format!("/v1/databases/{id}/console-mesh");
    // A `let`, never an `if let`: the read guard is not `Send`, and a scrutinee temporary lives to
    // the end of the block, which would make the whole future non-Send across the awaits below.
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        let mut rb = c
            .http
            .post(format!("{admin}{route}"))
            .json(env)
            .timeout(HOP_HTTP_TIMEOUT);
        // A mutation needs *some* valid bearer at `require_auth`; the hop's authority is the
        // envelope's own signed delegation, verified by `mesh_serve`.
        let hop_team = env.get("team").and_then(|t| t.as_str()).unwrap_or("mesh");
        if let Ok(tok) = crate::auth::issue(
            "mesh-internal",
            hop_team,
            "service",
            false,
            crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
        ) {
            rb = rb.header("authorization", format!("Bearer {tok}"));
        }
        if let Ok(r) = rb.send().await {
            if r.status().is_success() {
                if let Ok(v) = r.json::<Value>().await {
                    if answered(&v) {
                        return Some(v);
                    }
                    // An error envelope (unauthorized / not_found / not_hosted_here) is not an
                    // answer: fall through to the mesh rather than returning a refusal the caller
                    // would render as a failed query.
                    tracing::warn!(
                        db = %id,
                        node = %node,
                        error = %v.get("error").and_then(|e| e.as_str()).unwrap_or(""),
                        "console owner hop refused by the host node"
                    );
                }
            }
        }
    }
    let (peer, addr) = c
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id.clone()?, n.iroh_addr.clone()?)))?;
    let body = serde_json::to_vec(env).unwrap_or_default();
    let bytes = crate::gossip::request_to(
        c,
        &peer,
        &addr,
        hive_p2p::GOSSIP_POST,
        &route,
        &body,
        HOP_MESH_TIMEOUT_SECS,
    )
    .await?;
    let v = serde_json::from_slice::<Value>(&bytes).ok()?;
    answered(&v).then_some(v)
}

/// Whether a hop's payload is a real console result rather than a refusal envelope.
fn answered(v: &Value) -> bool {
    v.get("ok").is_some() && v.get("error").is_none()
}

/// The OWNER half of the console: re-resolves the record, re-derives the caller's identity from the
/// signed token (never the raw fields, under enforcement) and executes HERE. It never re-proxies,
/// so a stale router cannot build a forwarding loop.
pub async fn mesh_serve(cloud: &Arc<CloudState>, id: &str, env: &Value) -> Value {
    let raw_team = env.get("team").and_then(|t| t.as_str()).unwrap_or_default();
    let (tenant, admin, operator) = if crate::auth::enforced() {
        match env
            .get("tok")
            .and_then(|t| t.as_str())
            .and_then(|t| crate::auth::verify(t).ok())
        {
            Some(claims) => (
                claims.tenant,
                crate::roles::role_of(&claims.role).rank() >= crate::teams::Role::Admin.rank(),
                claims.platform_admin,
            ),
            // An unsigned or expired delegation resolves nothing: the same posture as every other
            // mesh arm, and the reason a peer cannot mint its own access here.
            None => return json!({ "error": "unauthorized" }),
        }
    } else {
        (
            raw_team.to_string(),
            env.get("role_rank").and_then(|r| r.as_str()) == Some("admin"),
            env.get("operator").and_then(|o| o.as_bool()).unwrap_or(false),
        )
    };
    if tenant.is_empty() {
        return json!({ "error": "unauthorized" });
    }
    let Some(db) = cloud.databases.get_raw(id) else {
        return json!({ "error": "not_found" });
    };
    let caller = Caller {
        tenant,
        operator,
        admin,
    };
    if !admits(&caller, &db) {
        return json!({ "error": "not_found" });
    }
    // The engine kinds are a FILE or a loopback port on `host_node`, so a hop addressed to a node
    // that does not host the instance must be REFUSED, not executed: opening the file here would
    // create a second, divergent copy, and dialing `local_port` here would reach whatever else
    // happens to listen on it. The platform-native kinds have no owner and are served anywhere.
    if crate::db_metrics::owner_routed_kind(db.kind) && db.host_node != cloud.node_name {
        return json!({ "error": "not_hosted_here", "code": "NOT_OWNER" });
    }
    let req = ConsoleReq {
        input: env.get("input").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        write: env.get("write").and_then(|v| v.as_bool()).unwrap_or(false),
        destructive: env.get("destructive").and_then(|v| v.as_bool()).unwrap_or(false),
    };
    if req.input.is_empty() || req.input.len() > INPUT_CAP {
        return json!({ "error": "empty or oversized command" });
    }
    // Classify HERE, on the owner, from the text: a tier carried across the wire is a label, not a
    // decision.
    let tier = match classify(db.kind, &req.input) {
        Ok(t) => t,
        Err(m) => return json!({ "error": m, "code": "REFUSED" }),
    };
    if let Err(m) = tier_gate(&caller, &req, tier) {
        return json!({ "error": m, "code": "REFUSED" });
    }
    let started = std::time::Instant::now();
    let out = exec_local(cloud, &db, &req, tier).await;
    let mut v = render(
        cloud,
        &db,
        &req,
        tier,
        out,
        started.elapsed().as_millis() as u64,
        true,
    );
    tally(cloud, &db, &v, req.input.len() as u64);
    if let Some(o) = v.as_object_mut() {
        o.insert("local".into(), json!(true));
    }
    v
}

/// Whether the caller asked for — and is allowed — this tier. Default posture: read-only.
fn tier_gate(caller: &Caller, req: &ConsoleReq, tier: Tier) -> Result<(), String> {
    match tier {
        Tier::Read => Ok(()),
        Tier::Write if !req.write => Err(
            "this command writes: send \"write\": true to run it (the console is read-only by \
             default)"
                .to_string(),
        ),
        Tier::Write => Ok(()),
        Tier::Destructive => {
            if !req.destructive {
                return Err(
                    "this command destroys data: send \"destructive\": true to run it".to_string(),
                );
            }
            if !caller.admin && !caller.operator {
                return Err(
                    "a destructive command needs a team admin or a platform operator".to_string(),
                );
            }
            Ok(())
        }
    }
}

/// Feeds the per-instance counters (`db_metrics`), so console traffic is visible on the same
/// per-instance metrics surface as the gateway's.
fn tally(c: &Arc<CloudState>, db: &Database, rendered: &Value, bytes_in: u64) {
    let ok = rendered.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let class = if ok {
        crate::db_metrics::ErrorClass::None
    } else if rendered.get("code").and_then(|v| v.as_str()) == Some("REFUSED") {
        crate::db_metrics::ErrorClass::Denied
    } else {
        crate::db_metrics::ErrorClass::Query
    };
    let bytes_out = serde_json::to_vec(rendered).map(|b| b.len() as u64).unwrap_or(0);
    let latency = rendered
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    crate::db_metrics::record(&db.id, ok, bytes_in, bytes_out, latency, class);
    let _ = c;
}

// ---------------------------------------------------------------- handler

/// `POST /v1/databases/:id/console` — run one command against ONE database instance.
///
/// Authorization is inside the handler, per DATABASE: `require_auth` proves only "some valid token",
/// and the tenant comes from the verified claim, never `x-hive-team`. A write needs `"write": true`
/// and a destructive command both `"destructive": true` and an admin/operator caller. The engine
/// kinds proxy to `host_node`; the platform-native kinds execute here and say so in `scope`.
pub(crate) async fn database_console(
    axum::extract::State(c): axum::extract::State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<ConsoleReq>,
) -> Result<axum::Json<Value>, (StatusCode, String)> {
    let caller = resolve_caller(&c, &headers, claims.as_ref().map(|e| &e.0));
    if caller.tenant == crate::admin::ANON_TENANT {
        return Err((
            StatusCode::UNAUTHORIZED,
            "authentication required to use the database console".to_string(),
        ));
    }
    if req.input.is_empty() || req.input.len() > INPUT_CAP {
        return Err((
            StatusCode::BAD_REQUEST,
            "empty or oversized command".to_string(),
        ));
    }

    let mut db = c.databases.get_raw(&id);
    if db.is_none() {
        // Records replicate with a lag, so a real database 404s here while the leader still has it.
        // The MASKED route is enough: this node only needs the routing facts, and the execution runs
        // against the owner's own unmasked record.
        if let Some(v) =
            crate::admin::databases_from_leader(&c, &format!("/v1/databases/{id}"), &caller.tenant)
                .await
        {
            if let Ok(d) = serde_json::from_value::<Database>(v) {
                db = Some(d);
            }
        }
    }
    let Some(db) = db else {
        return Err((StatusCode::NOT_FOUND, "no such database".to_string()));
    };
    if !admits(&caller, &db) {
        return Err((StatusCode::NOT_FOUND, "no such database".to_string()));
    }

    let tier = match classify(db.kind, &req.input) {
        Ok(t) => t,
        Err(m) => return Err((StatusCode::BAD_REQUEST, m)),
    };
    if let Err(m) = tier_gate(&caller, &req, tier) {
        return Err((StatusCode::FORBIDDEN, m));
    }

    if crate::db_metrics::owner_routed_kind(db.kind) {
        if db.host_node.is_empty() {
            return Err((
                StatusCode::MISDIRECTED_REQUEST,
                "this database has no host node (db:stranded-host); its engine cannot be reached \
                 from the console"
                    .to_string(),
            ));
        }
        if db.host_node != c.node_name {
            let env = envelope(&caller, &db, &req, tier);
            let Some(v) = post_to_host(&c, &db.host_node, &id, &env).await else {
                return Err((
                    StatusCode::MISDIRECTED_REQUEST,
                    format!(
                        "the node hosting this database ({}) could not be reached",
                        db.host_node
                    ),
                ));
            };
            return Ok(axum::Json(v));
        }
    }

    let started = std::time::Instant::now();
    let out = exec_local(&c, &db, &req, tier).await;
    let mut v = render(
        &c,
        &db,
        &req,
        tier,
        out,
        started.elapsed().as_millis() as u64,
        false,
    );
    tally(&c, &db, &v, req.input.len() as u64);
    if let Some(o) = v.as_object_mut() {
        o.insert("local".into(), json!(true));
    }
    // One refusal shape: a command the console refuses is a 400 with the reason, exactly like the
    // classification and tier refusals above, never a 200 that reads as a failed query.
    if v.get("code").and_then(|c| c.as_str()) == Some("REFUSED") {
        return Err((
            StatusCode::BAD_REQUEST,
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("command refused")
                .to_string(),
        ));
    }
    Ok(axum::Json(v))
}

/// `POST /v1/databases/:id/console-mesh` — the owner-side HTTP mount of [`mesh_serve`], exempt from
/// leader forwarding (it is addressed to a specific node, like `hrana-mesh`).
pub(crate) async fn database_console_mesh(
    axum::extract::State(c): axum::extract::State<Arc<CloudState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(env): axum::Json<Value>,
) -> axum::Json<Value> {
    axum::Json(mesh_serve(&c, &id, &env).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_tiers_are_decided_from_the_text() {
        assert_eq!(sql_tier("select 1").unwrap(), Tier::Read);
        assert_eq!(sql_tier("  -- hi\nselect 1").unwrap(), Tier::Read);
        assert_eq!(sql_tier("insert into t values (1)").unwrap(), Tier::Write);
        assert_eq!(sql_tier("delete from t").unwrap(), Tier::Write);
        assert_eq!(sql_tier("drop table t").unwrap(), Tier::Destructive);
        assert_eq!(sql_tier("truncate t").unwrap(), Tier::Destructive);
    }

    #[test]
    fn host_escapes_are_refused_at_every_tier() {
        for bad in [
            "attach database '/etc/passwd' as x",
            "copy t to program 'sh'",
            "select pg_read_file('/etc/passwd')",
            "select lo_export(1, '/tmp/x')",
            "pragma writable_schema = on",
        ] {
            assert!(sql_tier(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn stacked_statements_are_refused() {
        assert!(sql_tier("select 1; drop table t").is_err());
    }

    #[test]
    fn one_statement_per_request_allows_a_trailing_nothing_but_semicolon() {
        assert_eq!(sql_tier("select 1").unwrap(), Tier::Read);
        assert!(sql_tier("select 1;").is_err());
    }

    #[test]
    fn resp_tiers_and_denials() {
        assert_eq!(resp_tier(&["GET".into(), "k".into()]).unwrap(), Tier::Read);
        assert_eq!(resp_tier(&["SET".into(), "k".into(), "v".into()]).unwrap(), Tier::Write);
        assert_eq!(resp_tier(&["DEL".into(), "k".into()]).unwrap(), Tier::Write);
        assert_eq!(resp_tier(&["FLUSHALL".into()]).unwrap(), Tier::Destructive);
        // The gateway's own engine-administration list, reused.
        assert!(resp_tier(&["SHUTDOWN".into()]).is_err());
        assert!(resp_tier(&["EVAL".into()]).is_err());
        assert!(resp_tier(&["CONFIG".into(), "GET".into()]).is_err());
        // Console-only: KEYS walks the whole keyspace.
        assert!(resp_tier(&["KEYS".into(), "*".into()]).is_err());
        assert!(resp_tier(&["FAKECMD".into()]).is_err());
    }

    #[test]
    fn native_kinds_get_a_read_only_inspector_where_there_is_no_write_surface() {
        assert_eq!(native_tier(DbKind::Realtime, "INFO").unwrap(), Tier::Read);
        assert!(native_tier(DbKind::Realtime, "PUBLISH").is_err());
        assert!(native_tier(DbKind::Vector, "DELETE").is_err());
        assert_eq!(native_tier(DbKind::Queue, "POP").unwrap(), Tier::Write);
        assert_eq!(native_tier(DbKind::Blob, "LIST").unwrap(), Tier::Read);
    }

    #[test]
    fn destructive_needs_both_the_switch_and_a_privileged_caller() {
        let member = Caller {
            tenant: "acme".into(),
            operator: false,
            admin: false,
        };
        let admin = Caller {
            tenant: "acme".into(),
            operator: false,
            admin: true,
        };
        let req = ConsoleReq {
            input: "drop table t".into(),
            write: true,
            destructive: false,
        };
        assert!(tier_gate(&admin, &req, Tier::Destructive).is_err());
        let req = ConsoleReq {
            input: "drop table t".into(),
            write: true,
            destructive: true,
        };
        assert!(tier_gate(&member, &req, Tier::Destructive).is_err());
        assert!(tier_gate(&admin, &req, Tier::Destructive).is_ok());
    }

    #[test]
    fn writes_are_off_by_default() {
        let admin = Caller {
            tenant: "acme".into(),
            operator: true,
            admin: true,
        };
        let req = ConsoleReq {
            input: "delete from t".into(),
            write: false,
            destructive: false,
        };
        assert!(tier_gate(&admin, &req, Tier::Write).is_err());
    }

    #[test]
    fn scrub_removes_userinfo_and_bounds_length() {
        assert_eq!(
            scrub("engine unreachable: postgres://hive:s3cr3t@127.0.0.1:5432/hive"),
            "engine unreachable: postgres://***:***@127.0.0.1:5432/hive"
        );
        let long = "x".repeat(5000);
        assert!(scrub(&long).len() <= ERR_CAP + 1);
    }

    #[test]
    fn split_cmd_groups_quotes_and_breaks_on_whitespace() {
        assert_eq!(split_cmd("PUT k hello world"), vec!["PUT", "k", "hello", "world"]);
        assert_eq!(split_cmd("PUT k \"hello world\""), vec!["PUT", "k", "hello world"]);
        assert_eq!(split_cmd(""), Vec::<String>::new());
    }

    #[test]
    fn cross_tenant_access_is_refused() {
        let db = crate::databases::Database {
            id: "db_1".into(),
            name: "n".into(),
            project: "p".into(),
            team: "acme".into(),
            kind: DbKind::Redis,
            region: "r".into(),
            status: crate::databases::DbStatus::Ready,
            provider: String::new(),
            mode: "live".into(),
            created_ms: 0,
            connection: Default::default(),
            container: None,
            note: String::new(),
            replicas: Vec::new(),
            role: "primary".into(),
            primary_node: String::new(),
            db_host: String::new(),
            host_node: String::new(),
        };
        let other = Caller {
            tenant: "evil".into(),
            operator: false,
            admin: true,
        };
        assert!(!admits(&other, &db), "another tenant is refused even as an admin");
        let op = Caller {
            tenant: "evil".into(),
            operator: true,
            admin: false,
        };
        assert!(admits(&op, &db));
        assert_eq!(carry_team(&op, &db), "acme", "an operator carries the database's own team");
    }
}
