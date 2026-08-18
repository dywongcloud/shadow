//! HTTP REST surface of the per-tenant DB gateway — the third protocol next to
//! the Postgres/Redis wire proxies in [`crate::db_gateway`].
//!
//! Served on the SAME public HTTPS listener as tenant apps: the edge pipeline
//! branches here for any `Host: <slug>.{db_domain}` request (the `*.{db_domain}`
//! wildcard cert is already in the shared SNI resolver). Two dialects:
//!   * Redis kinds — Upstash-compatible: `POST /` with `["SET","k","v"]`,
//!     `POST /pipeline` with `[[...],[...]]`, or `GET /<CMD>/<arg>/<arg>`;
//!     replies `{"result": ...}` / `[{"result": ...}, ...]`.
//!   * Postgres kinds — SQL-over-HTTP (Neon-style): `POST /sql` with
//!     `{"query": "select $1::int + 1", "params": ["41"]}`; replies
//!     `{"fields": [...], "rows": [...], "rowCount": n}`.
//!
//! SECURITY (ZeroTrust): every request must present `Authorization: Bearer`
//! matching THIS database's own credential (Redis: its REST token or password;
//! Postgres: its password) — compared constant-time. The bearer scopes to exactly
//! one DB: there is no cross-DB or team-wide token here. Queries use REAL
//! parameter binding (extended protocol, params typed `unknown` so Postgres
//! infers like literals) — never string interpolation. The engine itself is only
//! reachable on the host node's loopback; this handler runs on that node because
//! per-DB DNS (`vercel_dns`) points `<slug>` at it.

use crate::databases::{Database, DbKind};
use crate::state::CloudState;
use axum::extract::Request;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::sync::Arc;

/// Max request-body bytes (SQL text / redis command JSON).
const BODY_CAP: usize = 4 * 1024 * 1024;
/// Max cumulative RESPONSE bytes buffered for one request (redis pipeline reply
/// set, or SQL result rows) — bounds a bounded-request-to-unbounded-response
/// memory amplification (e.g. 1000x GET on large stored values) regardless of
/// per-item/per-row caps.
const REPLY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
/// Whole-request budget against the backing engine.
const ENGINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Entry point from the edge pipeline. `host` is the port-stripped lowercased
/// Host header, already verified to end with `.{db_domain}`.
pub async fn handle(cloud: Arc<CloudState>, host: String, req: Request) -> Response {
    // CORS preflight: the REST surface is meant to be callable from browsers
    // (Upstash parity). Auth still applies to the actual request.
    if req.method() == Method::OPTIONS {
        return with_cors(StatusCode::NO_CONTENT.into_response());
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();

    // Resolve the DB FIRST so auth compares against per-DB credentials. Unknown
    // slug = 404 (leaks nothing: the wildcard DNS makes every label resolvable).
    let Some(db) = cloud.databases.by_db_host(&host) else {
        return with_cors(err(StatusCode::NOT_FOUND, "no database at this hostname"));
    };

    // Supabase Studio: this database's whole hostname is the Studio dashboard,
    // gated by HTTP BASIC auth (the DASHBOARD_USERNAME/DASHBOARD_PASSWORD
    // mechanism the upstream stack's Kong enforces on its `/` catch-all —
    // here enforced by this arm instead of a Kong container) and reverse-
    // proxied to the studio container's loopback port. Branch BEFORE the
    // bearer check: Studio speaks basic-auth, not the DB REST bearer.
    if db.kind == DbKind::Supabase {
        return supabase_studio_proxy(&cloud, &db, req).await;
    }

    // The engine lives on the host node's loopback. If per-DB DNS routed the
    // client elsewhere (stale record), say so honestly instead of a generic 500.
    let local = db
        .connection
        .get("local_port")
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<u16>().ok());

    // ---- AuthZ: bearer must match THIS DB's credential (constant-time) --------
    let bearer = bearer_token(&headers).unwrap_or_default();
    if bearer.is_empty() || !credential_matches(&db, &bearer) {
        let mut resp = err(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token for this database",
        );
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return with_cors(resp);
    }

    // SQLite speaks libsql/Hrana, not a wire protocol on a published port: its
    // engine is a FILE on `Database::host_node`, so it has no `local_port` and
    // its handler does its own owner routing (proxying to that node rather than
    // opening a second, empty file here). Branch BEFORE the `local_port`
    // requirement below, which is a Postgres/Redis concept.
    if db.kind == DbKind::Sqlite {
        let (_parts, body) = req.into_parts();
        let bytes = match axum::body::to_bytes(body, BODY_CAP).await {
            Ok(b) => b,
            Err(_) => {
                return with_cors(err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"))
            }
        };
        return with_cors(crate::hrana::serve(cloud, db, bearer, method, path, bytes).await);
    }

    let Some(local_port) = local else {
        return with_cors(err(
            StatusCode::MISDIRECTED_REQUEST,
            "database is not hosted on this node (stale DNS?) or has no live engine",
        ));
    };

    // Cap concurrent REST-originated engine connections PER DB. Every call opens
    // a fresh connection (no pool — see resp.rs/run_sql doc comments); without a
    // cap, a burst of REST callers could exhaust the container's own
    // max_connections (Postgres default 100) and starve the app's OWN
    // DATABASE_URL clients on the same engine. Reserves headroom rather than
    // eliminating the risk entirely (still correct-first, not a real pool).
    let Some(_permit) = rest_conn_permit(&db.id) else {
        return with_cors(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "database REST concurrency limit reached; retry",
        ));
    };

    // ---- Dispatch by engine kind ----------------------------------------------
    let out = match db.kind {
        DbKind::Redis => redis_rest(&db, local_port, &method, &path, req).await,
        DbKind::Postgres => postgres_rest(&db, local_port, &method, &path, req).await,
        // Native HTTP kinds already have their REST surface on the platform API
        // (team-scoped, JWT/API-key auth). Point there rather than duplicating a
        // second, differently-authenticated copy of those routes here.
        _ => err(
            StatusCode::NOT_IMPLEMENTED,
            &format!(
                "{} REST is served by the platform API (see this database's connection env, e.g. https://api.{}/v1/storage/...)",
                db.kind.label(),
                cloud.platform_domain
            ),
        ),
    };
    with_cors(out)
}

/// Max concurrent REST-originated engine connections for ONE database — well
/// under Postgres's default `max_connections=100`, leaving headroom for the
/// app's own `DATABASE_URL` pool on the same container.
const MAX_REST_CONNS_PER_DB: usize = 16;

static REST_CONNS: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
> = std::sync::OnceLock::new();

/// Acquire one of this DB's REST connection slots; `None` if all are in use
/// (caller should respond 503, not queue indefinitely).
fn rest_conn_permit(db_id: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let map = REST_CONNS.get_or_init(Default::default);
    let sem = {
        let mut m = map.lock();
        m.entry(db_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MAX_REST_CONNS_PER_DB)))
            .clone()
    };
    sem.try_acquire_owned().ok()
}

// ---- Redis: Upstash-compatible ------------------------------------------------

async fn redis_rest(
    db: &Database,
    port: u16,
    method: &Method,
    path: &str,
    req: Request,
) -> Response {
    let password = db.connection.get("password").cloned().unwrap_or_default();
    // `Upstash-Encoding: base64` (sent BY DEFAULT by @upstash/redis >=1.x):
    // the client base64-DECODES every string result (except the literal "OK")
    // after parsing the JSON. Ignoring the header corrupts every read for
    // those clients — a stored value that happens to be base64-alphabet text
    // decodes into garbage ("flow" → "~Z0" was the live fingerprint: a
    // workflow app's queue route double-decoded into a 404 URL and every CBOR
    // blob into "Unexpected end of CBOR data", stalling all its runs). Honor
    // it: base64-encode string results server-side so the client's decode is
    // a clean round-trip.
    let b64 = req
        .headers()
        .get("upstash-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("base64"))
        .unwrap_or(false);

    // GET /<CMD>/<arg>/<arg>: path segments are the command, URL-decoded.
    if method == Method::GET && path != "/" && !path.is_empty() {
        let parts: Vec<String> = path
            .trim_matches('/')
            .split('/')
            .map(|s| {
                percent_encoding::percent_decode_str(s)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return err(StatusCode::BAD_REQUEST, "empty command path");
        }
        if let Some(deny) = command_denied(&parts[0]) {
            return deny;
        }
        return run_redis(port, &password, &parts, b64).await;
    }

    if method != Method::POST {
        return err(
            StatusCode::METHOD_NOT_ALLOWED,
            "use POST / with a command array, POST /pipeline, or GET /<CMD>/<args>",
        );
    }
    let Some(body) = read_body(req).await else {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };

    // POST /pipeline: [["SET","a","1"],["GET","a"]] → one reply object per command.
    if path == "/pipeline" {
        let cmds: Vec<Vec<String>> = match serde_json::from_slice::<Vec<Vec<Value>>>(&body) {
            Ok(c) if !c.is_empty() => c
                .into_iter()
                .map(|c| c.into_iter().map(json_arg).collect())
                .collect(),
            Ok(_) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "pipeline body must be a non-empty array of command arrays",
                )
            }
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad pipeline body: {e}")),
        };
        if cmds.len() > 1000 {
            return err(
                StatusCode::BAD_REQUEST,
                "pipeline too long (max 1000 commands)",
            );
        }
        for c in &cmds {
            if c.is_empty() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "pipeline contains an empty command",
                );
            }
            if let Some(deny) = command_denied(&c[0]) {
                return deny;
            }
        }
        return match tokio::time::timeout(
            ENGINE_TIMEOUT,
            crate::resp::run_pipeline(port, &password, &cmds),
        )
        .await
        {
            Ok(Ok(replies)) => {
                let body: Vec<Value> = replies
                    .iter()
                    .map(|r| match r {
                        crate::resp::Reply::Error(e) => json!({ "error": e }),
                        ok => json!({ "result": maybe_b64(ok.to_json(), b64) }),
                    })
                    .collect();
                ok_json(json!(body))
            }
            Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, &format!("redis engine error: {e}")),
            Err(_) => err(StatusCode::GATEWAY_TIMEOUT, "redis engine timed out"),
        };
    }

    // POST /: single command array ["SET","k","v"].
    let parts: Vec<String> = match serde_json::from_slice::<Vec<Value>>(&body) {
        Ok(p) if !p.is_empty() => p.into_iter().map(json_arg).collect(),
        Ok(_) => return err(StatusCode::BAD_REQUEST, "command array must be non-empty"),
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("bad command body (expected JSON array): {e}"),
            )
        }
    };
    if let Some(deny) = command_denied(&parts[0]) {
        return deny;
    }
    run_redis(port, &password, &parts, b64).await
}

/// Mirror of the @upstash/redis client's response decode, applied in the
/// ENCODE direction (its `decode()` is: number/undefined unchanged; array →
/// per-element string-decode with recursion into nested arrays; string → "OK"
/// passthrough else base64decode). Encoding every non-"OK" string — including
/// numeric-looking ones — is the correct round-trip: the client decodes them
/// back verbatim.
fn maybe_b64(v: Value, on: bool) -> Value {
    use base64::Engine as _;
    if !on {
        return v;
    }
    fn enc(v: Value) -> Value {
        match v {
            Value::String(s) if s == "OK" => Value::String(s),
            Value::String(s) => {
                Value::String(base64::engine::general_purpose::STANDARD.encode(s.as_bytes()))
            }
            Value::Array(a) => Value::Array(a.into_iter().map(enc).collect()),
            other => other,
        }
    }
    enc(v)
}

async fn run_redis(port: u16, password: &str, parts: &[String], b64: bool) -> Response {
    match tokio::time::timeout(
        ENGINE_TIMEOUT,
        crate::resp::run_command(port, password, parts),
    )
    .await
    {
        Ok(Ok(crate::resp::Reply::Error(e))) => err(StatusCode::BAD_REQUEST, &e),
        Ok(Ok(reply)) => ok_json(json!({ "result": maybe_b64(reply.to_json(), b64) })),
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, &format!("redis engine error: {e}")),
        Err(_) => err(StatusCode::GATEWAY_TIMEOUT, "redis engine timed out"),
    }
}

/// Commands that reconfigure/kill the engine or leak host state are refused —
/// the REST bearer proves access to the DATA, not to engine administration.
/// (Upstash blocks the same class.) ALSO denied: server-side scripting/functions
/// (EVAL/FUNCTION LOAD can persist and run arbitrary Lua, and EVAL can reach
/// non-noscript commands like MIGRATE even though MIGRATE itself is denied —
/// scripting is a real bypass of this list, not just an admin convenience) and
/// the SUBSCRIBE family (RESP2 emits >1 reply frame per command / unsolicited
/// pushes once subscribed, which desyncs `run_pipeline`'s one-reply-per-command
/// assumption and silently misattributes later replies to the wrong command).
fn command_denied(cmd: &str) -> Option<Response> {
    const DENY: &[&str] = &[
        "shutdown",
        "config",
        "debug",
        "module",
        "acl",
        "cluster",
        "replicaof",
        "slaveof",
        "migrate",
        "save",
        "bgsave",
        "bgrewriteaof",
        "failover",
        "latency",
        "monitor",
        "psync",
        "sync",
        "slowlog",
        "eval",
        "evalsha",
        "eval_ro",
        "evalsha_ro",
        "fcall",
        "fcall_ro",
        "function",
        "script",
        "reset",
        "swapdb",
        "subscribe",
        "psubscribe",
        "ssubscribe",
        "unsubscribe",
        "punsubscribe",
        "sunsubscribe",
    ];
    if DENY.contains(&cmd.to_ascii_lowercase().as_str()) {
        return Some(err(
            StatusCode::FORBIDDEN,
            &format!(
                "command {} is not available over the REST gateway",
                cmd.to_uppercase()
            ),
        ));
    }
    None
}

/// Upstash sends command args as JSON strings, but numbers/bools also appear;
/// coerce scalars to their redis (string) form, structured values to JSON text.
fn json_arg(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---- Postgres: SQL over HTTP ----------------------------------------------------

#[derive(serde::Deserialize)]
struct SqlReq {
    query: String,
    #[serde(default)]
    params: Vec<Value>,
}

async fn postgres_rest(
    db: &Database,
    port: u16,
    method: &Method,
    path: &str,
    req: Request,
) -> Response {
    if method != Method::POST || !(path == "/sql" || path == "/") {
        return err(
            StatusCode::METHOD_NOT_ALLOWED,
            "use POST /sql with {\"query\": \"...\", \"params\": [...]}",
        );
    }
    let Some(body) = read_body(req).await else {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let sql_req: SqlReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("bad body (expected {{query, params?}}): {e}"),
            )
        }
    };

    let g = |k: &str| db.connection.get(k).cloned().unwrap_or_default();
    let (user, password, dbname) = (g("user"), g("password"), g("database"));

    match tokio::time::timeout(
        ENGINE_TIMEOUT,
        run_sql(port, &user, &password, &dbname, &sql_req),
    )
    .await
    {
        Ok(Ok(v)) => ok_json(v),
        Ok(Err(e)) => {
            // Surface the real Postgres error (it's the caller's own query failing —
            // syntax, constraint, type). Engine-unreachable stays a 502.
            let msg = e.to_string();
            if msg.contains("unreachable") || msg.contains("connect") {
                err(
                    StatusCode::BAD_GATEWAY,
                    &format!("postgres engine error: {msg}"),
                )
            } else {
                err(StatusCode::BAD_REQUEST, &msg)
            }
        }
        Err(_) => err(StatusCode::GATEWAY_TIMEOUT, "query timed out"),
    }
}

/// A parameter bound in Postgres TEXT wire format, whatever type the server
/// inferred for it. `prepare_typed` declares every param `UNKNOWN`, so the
/// server resolves each to its real context type (e.g. `$1::int`, `where id =
/// $1`) and reports THAT resolved type back — `Option<String>`'s `ToSql::accepts`
/// only matches text-ish types, so binding a resolved `int4`/`bool`/`jsonb`/...
/// param as a plain string FAILS ("error serializing parameter"), even though
/// the exact same text would work as a quoted SQL literal. `accepts` here
/// unconditionally returns true and `encode_format` forces `Format::Text`, so
/// the server parses the raw bytes with that type's own text input function —
/// i.e. behaves exactly like a literal, for every type, while keeping the bind
/// REAL (no string interpolation into the query text).
#[derive(Debug)]
struct TextParam(Option<String>);

impl tokio_postgres::types::ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &tokio_postgres::types::Type,
        out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            None => Ok(tokio_postgres::types::IsNull::Yes),
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(tokio_postgres::types::IsNull::No)
            }
        }
    }
    fn accepts(_ty: &tokio_postgres::types::Type) -> bool {
        true
    }
    fn encode_format(&self, _ty: &tokio_postgres::types::Type) -> tokio_postgres::types::Format {
        tokio_postgres::types::Format::Text
    }
    tokio_postgres::types::to_sql_checked!();
}

async fn run_sql(
    port: u16,
    user: &str,
    password: &str,
    dbname: &str,
    r: &SqlReq,
) -> anyhow::Result<Value> {
    use futures::TryStreamExt;
    use tokio_postgres::types::Type;

    let mut cfg = tokio_postgres::Config::new();
    cfg.host("127.0.0.1")
        .port(port)
        .user(user)
        .password(password)
        .dbname(dbname);
    cfg.connect_timeout(std::time::Duration::from_secs(10));
    let (client, connection) = cfg
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|e| anyhow::anyhow!("engine unreachable: {e}"))?;
    // Drive the connection; it ends when client drops.
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });

    // Params are declared `unknown` so Postgres infers each from query context —
    // exactly like a quoted literal ('42' works where an int is expected). This
    // keeps binding REAL (no interpolation) while accepting JSON scalars as text.
    let param_types = vec![Type::UNKNOWN; r.params.len()];
    let stmt = client.prepare_typed(&r.query, &param_types).await?;
    let params: Vec<TextParam> = r
        .params
        .iter()
        .map(|v| {
            TextParam(match v {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                other => Some(other.to_string()), // arrays/objects → JSON text (for json/jsonb params)
            })
        })
        .collect();

    let stream = client.query_raw(&stmt, params).await?;
    futures::pin_mut!(stream);
    let mut rows_json: Vec<Value> = Vec::new();
    let mut total_bytes = 0usize;
    while let Some(row) = stream.try_next().await? {
        let mut obj = serde_json::Map::with_capacity(row.len());
        for (i, col) in row.columns().iter().enumerate() {
            obj.insert(
                col.name().to_string(),
                col_to_json(&row, i, col.name(), col.type_())?,
            );
        }
        let val = Value::Object(obj);
        total_bytes += serde_json::to_vec(&val).map(|b| b.len()).unwrap_or(0);
        rows_json.push(val);
        if rows_json.len() >= 10_000 {
            anyhow::bail!(
                "result too large (max 10000 rows over REST — paginate with LIMIT/OFFSET)"
            );
        }
        if total_bytes >= REPLY_BYTE_BUDGET {
            anyhow::bail!(
                "result too large (max {}MiB over REST — narrow the projection or paginate)",
                REPLY_BYTE_BUDGET / (1024 * 1024)
            );
        }
    }
    // For INSERT/UPDATE/DELETE without RETURNING the row stream is empty but the
    // affected-count is real signal.
    let affected = stream.rows_affected().unwrap_or(rows_json.len() as u64);
    let fields: Vec<Value> = stmt
        .columns()
        .iter()
        .map(|c| json!({ "name": c.name(), "dataType": c.type_().name() }))
        .collect();
    drop(stream);
    drop(client);
    let _ = driver.await;
    Ok(json!({ "fields": fields, "rows": rows_json, "rowCount": affected }))
}

/// A permissive text decode used ONLY for types whose binary wire format is
/// known to be exactly UTF-8 bytes (enum labels — verified: Postgres encodes an
/// enum value's binary representation as its label text, same as the text
/// format). Do NOT reuse this for arbitrary unknown types: most binary formats
/// (interval, money, inet, time, composite...) are NOT text, and mis-decoding
/// them as UTF-8 would silently return garbled data — worse than an error.
struct AnyUtf8(String);
impl<'a> tokio_postgres::types::FromSql<'a> for AnyUtf8 {
    fn from_sql(
        _ty: &tokio_postgres::types::Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(AnyUtf8(std::str::from_utf8(raw)?.to_owned()))
    }
    fn accepts(_ty: &tokio_postgres::types::Type) -> bool {
        true
    }
}

/// Decode one column to JSON by its Postgres type. A real SQL NULL decodes as
/// `Value::Null`; a type this gateway genuinely can't decode (arrays, ranges,
/// composites, money/inet/interval/time/oid, ...) FAILS LOUD with the column
/// name + Postgres type name in the error (visible in the HTTP response) rather
/// than silently returning null indistinguishable from a real NULL — cast it in
/// the query (e.g. `::text`, `::float8`) to bridge.
fn col_to_json(
    row: &tokio_postgres::Row,
    i: usize,
    name: &str,
    ty: &tokio_postgres::types::Type,
) -> anyhow::Result<Value> {
    use tokio_postgres::types::{Kind, Type};
    let wrong_type = |e: tokio_postgres::Error| {
        anyhow::anyhow!("column \"{name}\" has unsupported type \"{}\" over REST ({e}) — cast it in the query (e.g. ::text, ::float8)", ty.name())
    };
    // Enum labels are UTF-8 text on the wire regardless of the specific enum
    // type's OID, so check Kind before the static-Type match below.
    if matches!(ty.kind(), Kind::Enum(_)) {
        return match row.try_get::<_, Option<AnyUtf8>>(i) {
            Ok(v) => Ok(v.map(|s| Value::String(s.0)).unwrap_or(Value::Null)),
            Err(e) => Err(wrong_type(e)),
        };
    }
    match *ty {
        Type::BOOL => row
            .try_get::<_, Option<bool>>(i)
            .map(|v| v.map(Value::Bool).unwrap_or(Value::Null))
            .map_err(wrong_type),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(i)
            .map(|v| v.map(|n| json!(n)).unwrap_or(Value::Null))
            .map_err(wrong_type),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(i)
            .map(|v| v.map(|n| json!(n)).unwrap_or(Value::Null))
            .map_err(wrong_type),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(i)
            .map(|v| v.map(|n| json!(n)).unwrap_or(Value::Null))
            .map_err(wrong_type),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(i)
            .map(|v| v.map(|n| json!(n)).unwrap_or(Value::Null))
            .map_err(wrong_type),
        Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(i)
            .map(|v| v.map(|n| json!(n)).unwrap_or(Value::Null))
            .map_err(wrong_type),
        // Serialized as a STRING (not f64): NUMERIC routinely exceeds f64
        // precision (money/quantities), and Neon/Upstash-style REST APIs do the
        // same to avoid silent precision loss.
        Type::NUMERIC => row
            .try_get::<_, Option<rust_decimal::Decimal>>(i)
            .map(|v| {
                v.map(|d| Value::String(d.to_string()))
                    .unwrap_or(Value::Null)
            })
            .map_err(wrong_type),
        Type::JSON | Type::JSONB => row
            .try_get::<_, Option<Value>>(i)
            .map(|v| v.unwrap_or(Value::Null))
            .map_err(wrong_type),
        Type::UUID => row
            .try_get::<_, Option<uuid::Uuid>>(i)
            .map(|v| {
                v.map(|u| Value::String(u.to_string()))
                    .unwrap_or(Value::Null)
            })
            .map_err(wrong_type),
        Type::TIMESTAMPTZ => row
            .try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(i)
            .map(|v| {
                v.map(|t| Value::String(t.to_rfc3339()))
                    .unwrap_or(Value::Null)
            })
            .map_err(wrong_type),
        Type::TIMESTAMP => row
            .try_get::<_, Option<chrono::NaiveDateTime>>(i)
            .map(|v| {
                v.map(|t| Value::String(t.to_string()))
                    .unwrap_or(Value::Null)
            })
            .map_err(wrong_type),
        Type::DATE => row
            .try_get::<_, Option<chrono::NaiveDate>>(i)
            .map(|v| {
                v.map(|t| Value::String(t.to_string()))
                    .unwrap_or(Value::Null)
            })
            .map_err(wrong_type),
        Type::BYTEA => row
            .try_get::<_, Option<Vec<u8>>>(i)
            .map(|v| {
                v.map(|b| {
                    use base64::Engine;
                    Value::String(base64::engine::general_purpose::STANDARD.encode(b))
                })
                .unwrap_or(Value::Null)
            })
            .map_err(wrong_type),
        // TEXT/VARCHAR/NAME/BPCHAR (+ citext/ltree family) decode as text; any
        // other type falls through to the same call, which fails loud via
        // `wrong_type` rather than swallowing the error as null.
        _ => row
            .try_get::<_, Option<String>>(i)
            .map(|v| v.map(Value::String).unwrap_or(Value::Null))
            .map_err(wrong_type),
    }
}

// ---- Shared plumbing -------------------------------------------------------------

/// Which of this DB's credentials the REST bearer may match. When a DEDICATED,
/// independently-revocable REST token exists (`UPSTASH_REDIS_REST_TOKEN` for
/// redis, `DB_REST_TOKEN` for postgres) ONLY that token authorizes REST — the
/// raw engine `password` is NOT accepted, so REST access is separated from and
/// revocable without the engine credential. The `password` fallback applies
/// only to legacy databases provisioned before dedicated tokens existed.
/// Constant-time comparison throughout.
pub(crate) fn credential_matches(db: &Database, bearer: &str) -> bool {
    let dedicated: Vec<&String> = ["UPSTASH_REDIS_REST_TOKEN", "DB_REST_TOKEN"]
        .iter()
        .filter_map(|k| db.connection.get(*k))
        .filter(|v| !v.is_empty())
        .collect();
    if !dedicated.is_empty() {
        return dedicated
            .iter()
            .any(|v| ct_eq(v.as_bytes(), bearer.as_bytes()));
    }
    // Legacy DB with no dedicated REST token: fall back to the engine password.
    match db.connection.get("password") {
        Some(v) if !v.is_empty() => ct_eq(v.as_bytes(), bearer.as_bytes()),
        _ => false,
    }
}

/// Constant-time byte equality (length leak only — lengths are not secret here).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// Supabase Studio edge arm (DbKind::Supabase)
// ---------------------------------------------------------------------------

/// The whole `<slug>.{db_domain}` hostname of a Supabase database IS the
/// Studio dashboard: basic-auth gate (the generated STUDIO_USERNAME /
/// STUDIO_PASSWORD — the `DASHBOARD_USERNAME`/`DASHBOARD_PASSWORD` mechanism
/// the upstream stack's Kong enforces on its `/` catch-all, `hide_credentials`
/// included: the Authorization header is checked here and never forwarded),
/// then a streaming reverse proxy to the studio container's loopback port.
///
/// Runs on the database's host node (per-DB DNS points the slug there), so
/// the proxy target is always loopback — no cross-node forwarding in v1; a
/// stale-DNS landing gets an honest 421 like the engine arms.
async fn supabase_studio_proxy(cloud: &Arc<CloudState>, db: &Database, req: Request) -> Response {
    // ---- AuthN: HTTP Basic against this database's generated studio creds ---
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Basic ")
                .or_else(|| v.strip_prefix("basic "))
                .map(str::to_string)
        })
        .and_then(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()
        })
        .and_then(|raw| String::from_utf8(raw).ok())
        .map(|pair| {
            let (u, p) = pair.split_once(':').unwrap_or((pair.as_str(), ""));
            let eu = db
                .connection
                .get("STUDIO_USERNAME")
                .map(String::as_str)
                .unwrap_or("");
            let ep = db
                .connection
                .get("STUDIO_PASSWORD")
                .map(String::as_str)
                .unwrap_or("");
            !eu.is_empty()
                && ct_eq(u.as_bytes(), eu.as_bytes())
                && ct_eq(p.as_bytes(), ep.as_bytes())
        })
        .unwrap_or(false);
    if !ok {
        let mut resp = err(
            StatusCode::UNAUTHORIZED,
            "Supabase Studio credentials required (see this database's connection details)",
        );
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Basic realm=\"supabase-studio\", charset=\"UTF-8\""),
        );
        return resp;
    }

    // ---- Proxy target: this node's loopback studio port ---------------------
    let Some(port) = db
        .connection
        .get("studio_port")
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<u16>().ok())
    else {
        return err(
            StatusCode::MISDIRECTED_REQUEST,
            "this Supabase database is not hosted on this node (stale DNS?) or has no live Studio",
        );
    };
    let method = req.method().clone();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let headers_vec: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            let name = k.as_str().to_lowercase();
            // Hop-by-hop + the gate itself: Kong's hide_credentials equivalent.
            if matches!(
                name.as_str(),
                "host"
                    | "connection"
                    | "content-length"
                    | "transfer-encoding"
                    | "keep-alive"
                    | "upgrade"
                    | "authorization"
                    | "x-hive-proxied"
                    | "x-hive-request-id"
            ) {
                return None;
            }
            v.to_str().ok().map(|s| (name, s.to_string()))
        })
        .collect();
    // Buffered with a Studio-appropriate cap (SQL/CSV imports ride this body);
    // the RESPONSE is streamed (Next.js asset bundles are multi-MB).
    const STUDIO_BODY_CAP: usize = 64 * 1024 * 1024;
    let body = match axum::body::to_bytes(req.into_body(), STUDIO_BODY_CAP).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let url = format!("http://127.0.0.1:{port}{path_q}");
    let mut rb = cloud
        .http
        .request(method, &url)
        .header("host", &host)
        .header("x-forwarded-host", &host)
        .header("x-forwarded-proto", "https")
        .timeout(std::time::Duration::from_secs(120))
        .body(body);
    for (k, v) in &headers_vec {
        rb = rb.header(k, v);
    }
    match rb.send().await {
        Ok(r) => {
            let status = r.status();
            let rheaders = r.headers().clone();
            let mut builder = Response::builder().status(status.as_u16());
            for (k, v) in rheaders.iter() {
                let lk = k.as_str().to_lowercase();
                if matches!(
                    lk.as_str(),
                    "transfer-encoding" | "connection" | "content-length"
                ) {
                    continue;
                }
                // Studio must never navigate the browser to an internal origin.
                if lk == "location" {
                    if let Ok(loc) = v.to_str() {
                        let rewritten = loc
                            .replace(
                                &format!("http://127.0.0.1:{port}"),
                                &format!("https://{host}"),
                            )
                            .replace("http://meta:8080", &format!("https://{host}"));
                        if let Ok(hv) = header::HeaderValue::from_str(&rewritten) {
                            builder = builder.header(header::LOCATION, hv);
                        }
                        continue;
                    }
                }
                if let (Ok(name), Ok(val)) = (
                    header::HeaderName::from_bytes(k.as_str().as_bytes()),
                    header::HeaderValue::from_bytes(v.as_bytes()),
                ) {
                    builder = builder.header(name, val);
                }
            }
            builder
                .body(axum::body::Body::from_stream(r.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => {
            tracing::warn!(db = %db.id, error = %e, "supabase studio proxy upstream error");
            err(
                StatusCode::BAD_GATEWAY,
                "Studio is still starting (or stopped) — retry in a few seconds",
            )
        }
    }
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let t = rest.trim();
    (!t.is_empty()).then(|| t.to_string())
}

async fn read_body(req: Request) -> Option<axum::body::Bytes> {
    let (_parts, body) = req.into_parts();
    axum::body::to_bytes(body, BODY_CAP).await.ok()
}

fn ok_json(v: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

fn err(code: StatusCode, msg: &str) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "error": msg }).to_string(),
    )
        .into_response()
}

fn with_cors(mut resp: Response) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basics() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"secret", b"secre"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    fn db_with(conn: serde_json::Value) -> crate::databases::Database {
        serde_json::from_value(serde_json::json!({
            "id": "db_test01", "name": "t", "project": "p",
            "kind": "postgres", "region": "iad", "status": "ready",
            "connection": conn,
        }))
        .unwrap()
    }

    #[test]
    fn postgres_rest_token_is_separate_from_and_supersedes_engine_password() {
        // REGRESSION TEST for the confirmed finding: the Postgres SQL-over-HTTP
        // surface used to accept the raw engine `password` as its REST bearer, so
        // REST access could not be revoked without rotating the DB password.
        // A DB with a dedicated DB_REST_TOKEN must accept ONLY that token —
        // never the engine password — so REST is independently revocable.
        let db = db_with(serde_json::json!({
            "password": "engine-pw-abc",
            "DB_REST_TOKEN": "pgrest_tok_xyz",
        }));
        assert!(
            credential_matches(&db, "pgrest_tok_xyz"),
            "dedicated REST token must authorize"
        );
        assert!(
            !credential_matches(&db, "engine-pw-abc"),
            "engine password must NOT authorize once a REST token exists"
        );
        assert!(!credential_matches(&db, "wrong"));

        // Redis's existing dedicated token behaves the same way.
        let redis = db_with(serde_json::json!({
            "password": "engine-pw", "UPSTASH_REDIS_REST_TOKEN": "urest_tok",
        }));
        assert!(credential_matches(&redis, "urest_tok"));
        assert!(!credential_matches(&redis, "engine-pw"));

        // Legacy DB with no dedicated token falls back to the engine password.
        let legacy = db_with(serde_json::json!({ "password": "legacy-pw" }));
        assert!(credential_matches(&legacy, "legacy-pw"));
        assert!(!credential_matches(&legacy, "nope"));
    }

    #[test]
    fn admin_commands_are_denied() {
        for c in ["SHUTDOWN", "config", "Debug", "ACL", "replicaof"] {
            assert!(command_denied(c).is_some(), "{c} must be denied");
        }
        for c in ["GET", "SET", "INCR", "HGETALL", "ping"] {
            assert!(command_denied(c).is_none(), "{c} must be allowed");
        }
    }

    fn redis_db(port: &str, password: &str) -> Database {
        Database {
            id: "db_testredis000000000000".into(),
            name: "resttest".into(),
            project: "proj".into(),
            team: "acme".into(),
            kind: DbKind::Redis,
            region: "san-jose".into(),
            status: crate::databases::DbStatus::Ready,
            provider: "Upstash".into(),
            mode: "live".into(),
            created_ms: 0,
            connection: [("local_port", port), ("password", password)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            container: None,
            note: String::new(),
            replicas: vec![],
            role: "primary".into(),
            primary_node: String::new(),
            db_host: "resttest.downstash.xyz".into(),
            host_node: "fc-virginia".into(),
        }
    }

    /// Real Redis round-trip through the ACTUAL HTTP dispatch layer
    /// (`redis_rest`) — the GET/<CMD> path form, the POST / single-command
    /// form, and the POST /pipeline form — against a live engine. Requires:
    /// `podman run -d -p 127.0.0.1:54444:6379 docker.io/library/redis:7-alpine
    /// --requirepass resttestpw123`.
    #[tokio::test]
    #[ignore = "requires a live redis on 127.0.0.1:54444 with --requirepass resttestpw123 (see doc comment)"]
    async fn redis_rest_dispatch_all_three_entry_forms() {
        let db = redis_db("54444", "resttestpw123");
        let req_body = |b: &'static str| {
            Request::builder()
                .method(Method::POST)
                .body(axum::body::Body::from(b))
                .unwrap()
        };

        // GET /<CMD>/<args> form.
        let resp = redis_rest(
            &db,
            54444,
            &Method::POST,
            "/",
            req_body(r#"["SET","k","v1"]"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = redis_rest(
            &db,
            54444,
            &Method::GET,
            "/GET/k",
            Request::builder()
                .method(Method::GET)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["result"],
            json!("v1"),
            "GET /<CMD>/<arg> form must dispatch and return the real value"
        );

        // POST /pipeline form: order must match input order (real engine, real TCP).
        let resp = redis_rest(
            &db,
            54444,
            &Method::POST,
            "/pipeline",
            req_body(r#"[["SET","p1","1"],["SET","p2","2"],["GET","p1"],["GET","p2"]]"#),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v[2]["result"], json!("1"));
        assert_eq!(v[3]["result"], json!("2"));

        // Denied command must 403 through the real dispatch, not just the unit-level helper.
        let resp = redis_rest(
            &db,
            54444,
            &Method::POST,
            "/",
            req_body(r#"["EVAL","return 1","0"]"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Real Postgres round-trip proving the TextParam fix: params inferred as
    /// non-text types (int/bool/jsonb/timestamptz/NULL) — the exact class that
    /// failed with "error serializing parameter" under the old `Option<String>`
    /// binding, including the module's own doc example `select $1::int + 1`.
    /// Requires a live engine: `podman run -d -e POSTGRES_PASSWORD=testpw -e
    /// POSTGRES_USER=testuser -e POSTGRES_DB=testdb -p 127.0.0.1:55433:5432
    /// docker.io/library/postgres:16-alpine`.
    #[tokio::test]
    #[ignore = "requires a live postgres on 127.0.0.1:55433 (see doc comment)"]
    async fn sql_over_http_binds_non_text_params() {
        let q = |query: &str, params: Vec<Value>| SqlReq {
            query: query.to_string(),
            params,
        };

        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select $1::int + 1 as v", vec![json!("41")]),
        )
        .await
        .unwrap();
        assert_eq!(
            r["rows"][0]["v"],
            json!(42),
            "doc-example int param must bind"
        );

        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select $1::boolean as v", vec![json!(true)]),
        )
        .await
        .unwrap();
        assert_eq!(r["rows"][0]["v"], json!(true));

        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select $1::jsonb as v", vec![json!({"a": 1})]),
        )
        .await
        .unwrap();
        assert_eq!(r["rows"][0]["v"], json!({"a": 1}));

        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select $1::int as v", vec![Value::Null]),
        )
        .await
        .unwrap();
        assert_eq!(r["rows"][0]["v"], Value::Null);

        // A bare (non-cast) comparison against an int column — the everyday
        // `where id = $1` shape the old binding broke. A REGULAR table, not
        // TEMP: run_sql opens a fresh connection per call (no pooling), and a
        // temp table is connection-scoped — it would vanish before the next call.
        run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("drop table if exists t_binds_test", vec![]),
        )
        .await
        .unwrap();
        run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("create table t_binds_test(id int)", vec![]),
        )
        .await
        .unwrap();
        run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("insert into t_binds_test values (7)", vec![]),
        )
        .await
        .unwrap();
        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select id from t_binds_test where id = $1", vec![json!(7)]),
        )
        .await
        .unwrap();
        assert_eq!(r["rows"][0]["id"], json!(7));
        run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("drop table t_binds_test", vec![]),
        )
        .await
        .unwrap();
    }

    /// Real Postgres round-trip proving col_to_json's NUMERIC/enum decode +
    /// fail-loud fallback (vs. the old silent-null-on-WrongType behavior).
    #[tokio::test]
    #[ignore = "requires a live postgres on 127.0.0.1:55433 (see doc comment on sql_over_http_binds_non_text_params)"]
    async fn sql_over_http_decodes_numeric_and_enum_and_fails_loud_on_unsupported() {
        let q = |query: &str| SqlReq {
            query: query.to_string(),
            params: vec![],
        };

        // NUMERIC (e.g. avg()/sum() over numeric columns) decodes as a string,
        // not silently null, and preserves precision beyond f64.
        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select 12345678901234567890.123456789::numeric as v"),
        )
        .await
        .unwrap();
        assert_eq!(r["rows"][0]["v"], json!("12345678901234567890.123456789"));

        // Enum: binary wire format is the label text.
        run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("drop type if exists mood"),
        )
        .await
        .unwrap();
        run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("create type mood as enum ('sad','ok','happy')"),
        )
        .await
        .unwrap();
        let r = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select 'happy'::mood as v"),
        )
        .await
        .unwrap();
        assert_eq!(r["rows"][0]["v"], json!("happy"));

        // A genuinely unsupported type (interval — binary format is 3 ints, NOT
        // UTF-8 text) must error rather than silently return null.
        let err = run_sql(
            55433,
            "testuser",
            "testpw",
            "testdb",
            &q("select interval '1 day' as v"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("unsupported type"),
            "expected a loud type error, got: {err}"
        );
    }

    #[test]
    fn scripting_and_subscribe_commands_are_denied() {
        // Scripting: EVAL/FUNCTION can persist/run arbitrary Lua and reach
        // non-noscript denied commands (e.g. MIGRATE) as a bypass.
        for c in ["EVAL", "evalsha", "FCALL", "function", "SCRIPT", "Reset"] {
            assert!(command_denied(c).is_some(), "{c} must be denied");
        }
        // Subscribe family: multi-reply / unsolicited-push commands desync
        // run_pipeline's one-reply-per-command assumption.
        for c in ["SUBSCRIBE", "psubscribe", "UNSUBSCRIBE"] {
            assert!(command_denied(c).is_some(), "{c} must be denied");
        }
    }

    #[test]
    fn json_args_coerce_to_redis_strings() {
        assert_eq!(json_arg(serde_json::json!("v")), "v");
        assert_eq!(json_arg(serde_json::json!(42)), "42");
        assert_eq!(json_arg(serde_json::json!(true)), "true");
        assert_eq!(json_arg(serde_json::json!(null)), "");
        assert_eq!(json_arg(serde_json::json!({"a":1})), "{\"a\":1}");
    }
}
