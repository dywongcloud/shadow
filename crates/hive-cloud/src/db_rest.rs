//! HTTP REST surface of the per-tenant DB gateway, the third protocol next to the Postgres/Redis
//! wire proxies in [`crate::db_gateway`], served on the SAME public HTTPS listener as tenant apps:
//! the edge pipeline branches here for any `Host: <slug>.{db_domain}` (the `*.{db_domain}` wildcard
//! cert is already in the shared SNI resolver). Two dialects:
//!
//! - Redis kinds, Upstash-compatible: `POST /` with `["SET","k","v"]`, `POST /pipeline` with
//! `[[...],[...]]`, or `GET /<CMD>/<arg>/<arg>` (path segments are the URL-decoded command);
//! replies `{"result": ...}` / `[{"result": ...}, ...]`.
//! - Postgres kinds, SQL-over-HTTP (Neon-style): `POST /sql` with `{"query": "select $1::int + 1",
//! "params": ["41"]}`; replies `{"fields": [...], "rows": [...], "rowCount": n}`.
//!
//! SECURITY (ZeroTrust): every request must present `Authorization: Bearer` matching THIS
//! database's own credential (Redis: its REST token or password; Postgres: its password), compared
//! constant-time; the bearer scopes to exactly one DB. Queries use REAL parameter binding (extended
//! protocol, params typed `unknown` so Postgres infers like literals), never string interpolation.
//! The engine is reachable only on the host node's loopback, and this handler runs there because
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
/// Max cumulative RESPONSE bytes buffered for one request (a redis pipeline's replies, or SQL
/// result rows): bounds a bounded-request-to-unbounded-response amplification (1000x GET on large
/// values) regardless of per-item/per-row caps.
const REPLY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
/// Whole-request budget against the backing engine.
const ENGINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Entry point from the edge pipeline. `host` is the port-stripped lowercased
/// Host header, already verified to end with `.{db_domain}`.
///
/// The order is the design. CORS preflight answers first (browsers call this surface, Upstash
/// parity; auth still applies to the real request). The DB is resolved FIRST so auth compares
/// against per-DB credentials, and an unknown slug is a 404 that leaks nothing (the wildcard DNS
/// makes every label resolvable). A Supabase database is its Studio dashboard, gated by HTTP BASIC
/// auth in its own arm BEFORE the bearer check (Studio speaks basic-auth, not the REST bearer): its
/// GET/HEAD are proven bodyless before any cache lookup or owner hop (HTTP/2 DATA needs no
/// Content-Length), the pinned shell and immutable assets come from a node-local cache, and
/// everything else takes the owner route. Then the constant-time bearer check. SQLite is Hrana with
/// its own owner routing (its engine is a FILE on `Database::host_node`, so no `local_port`, and
/// routing beats opening a second, empty file). A missing `local_port` is an honest 421 (stale
/// DNS), not a 500. A per-database cap on concurrent REST engine connections follows (every call
/// opens a fresh connection, so a burst could exhaust the container's `max_connections` and starve
/// the app's own `DATABASE_URL` clients; it reserves headroom, it is not a pool). Last, dispatch by
/// kind; native HTTP kinds answer 501 because their REST surface is the team-scoped platform API.
pub async fn handle(cloud: Arc<CloudState>, host: String, mut req: Request) -> Response {
    if req.method() == Method::OPTIONS {
        return with_cors(StatusCode::NO_CONTENT.into_response());
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();

    let Some(db) = cloud.databases.by_db_host(&host) else {
        return with_cors(err(StatusCode::NOT_FOUND, "no database at this hostname"));
    };

    if db.kind == DbKind::Supabase {
        if matches!(*req.method(), Method::GET | Method::HEAD) {
            req = match studio_require_empty_body(req).await {
                Ok(req) => req,
                Err(response) => return response,
            };
        }
        if let Some(kind) = studio_cache_candidate(&req) {
            return studio_cached_route(&cloud, &db, req, kind).await;
        }
        if !db.host_node.is_empty() && db.host_node != cloud.node_name {
            return match studio_forward_to_owner(&cloud, &db, req).await {
                Some(resp) => resp,
                None => err(
                    StatusCode::MISDIRECTED_REQUEST,
                    "this Supabase database's owner node is unreachable",
                ),
            };
        }
        return supabase_studio_proxy(&cloud, &db, req).await;
    }

    let local = db
        .connection
        .get("local_port")
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<u16>().ok());

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

    let Some(_permit) = rest_conn_permit(&db.id) else {
        return with_cors(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "database REST concurrency limit reached; retry",
        ));
    };

    let out = match db.kind {
        DbKind::Redis => redis_rest(&db, local_port, &method, &path, req).await,
        DbKind::Postgres => postgres_rest(&db, local_port, &method, &path, req).await,
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

async fn redis_rest(
    db: &Database,
    port: u16,
    method: &Method,
    path: &str,
    req: Request,
) -> Response {
    let password = db.connection.get("password").cloned().unwrap_or_default();
    let b64 = req
        .headers()
        .get("upstash-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("base64"))
        .unwrap_or(false);

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

/// Mirror of the @upstash/redis client's response decode, in the ENCODE direction (its `decode()`:
/// numbers/undefined unchanged, arrays decoded per element recursively, strings "OK" passthrough
/// else base64-decoded). `Upstash-Encoding: base64` is sent BY DEFAULT by @upstash/redis >=1.x, so
/// ignoring it corrupts every read for those clients (a stored value that happens to be
/// base64-alphabet text decodes to garbage, e.g. "flow" to "~Z0", which broke a workflow app's
/// queue route and every CBOR blob); encoding every non-"OK" string, numeric-looking ones included,
/// is the correct round-trip.
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

/// Commands that reconfigure/kill the engine or leak host state are refused: the REST bearer proves
/// access to the DATA, not engine administration (Upstash blocks the same class). Also denied:
/// server-side scripting (EVAL/FUNCTION LOAD persist and run arbitrary Lua, and EVAL can reach
/// commands like MIGRATE even though MIGRATE itself is denied: a real bypass of this list) and the
/// SUBSCRIBE family (RESP2 emits more than one reply frame per command, which desyncs
/// `run_pipeline`'s one-reply-per-command assumption and misattributes later replies).
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

#[derive(serde::Deserialize)]
struct SqlReq {
    query: String,
    #[serde(default)]
    params: Vec<Value>,
}

/// SQL over HTTP (`POST /sql`): the caller's own failing query (syntax, constraint, type) surfaces
/// verbatim as a 400, an unreachable engine is a 502 and a query past `ENGINE_TIMEOUT` a 504.
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

/// A parameter bound in Postgres TEXT wire format, whatever type the server inferred for it.
/// `prepare_typed` declares every param `UNKNOWN`, so the server resolves each to its real context
/// type (`$1::int`, `where id = $1`) and reports THAT type back; `Option<String>`'s
/// `ToSql::accepts` only matches text-ish types, so binding a resolved `int4`/`bool`/`jsonb` param
/// as a plain string FAILS even though the same text works as a quoted literal. `accepts` here
/// always returns true and `encode_format` forces `Format::Text`, so the server parses the raw
/// bytes with that type's own text input function: it behaves exactly like a literal for every type
/// while the bind stays REAL (no interpolation into the query text).
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
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });

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
                other => Some(other.to_string()),
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

/// A permissive text decode used ONLY for types whose binary wire format is exactly UTF-8 bytes
/// (enum labels: Postgres encodes an enum's binary form as its label text). Never reuse it for
/// arbitrary unknown types: most binary formats (interval, money, inet, time, composite...) are NOT
/// text, and decoding them as UTF-8 would silently return garbled data, worse than an error.
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

/// Decodes one column to JSON by its Postgres type. A real SQL NULL is `Value::Null`; a type this
/// gateway cannot decode (arrays, ranges, composites, money/inet/interval/time/oid, ...) FAILS LOUD
/// with the column name and type name in the HTTP response rather than returning a null
/// indistinguishable from a real NULL: cast it in the query (`::text`, `::float8`) to bridge. Enum
/// labels are UTF-8 on the wire whatever the enum's OID, so `Kind` is checked before the static
/// type match; NUMERIC is serialized as a STRING, since it routinely exceeds f64 precision (as
/// Neon/Upstash-style APIs do).
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
        _ => row
            .try_get::<_, Option<String>>(i)
            .map(|v| v.map(Value::String).unwrap_or(Value::Null))
            .map_err(wrong_type),
    }
}

/// Which of this DB's credentials the REST bearer may match: when a DEDICATED, independently
/// revocable REST token exists (`UPSTASH_REDIS_REST_TOKEN` for redis, `DB_REST_TOKEN` for postgres)
/// ONLY that token authorizes REST, never the raw engine `password`, so REST access is separate
/// from and revocable without the engine credential. The `password` fallback is for legacy
/// databases provisioned before dedicated tokens. Constant-time comparison throughout.
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

/// The exact image provisioned by `databases::supabase_stack_args`, and part of the cache identity:
/// changing the pin there requires changing this discriminator (a restart starts with an empty
/// map).
const STUDIO_IMAGE_PIN: &str = "2026.02.16-sha-26c615c";
const STUDIO_EDGE_CACHE_MAX_ENTRIES: usize = 512;
const STUDIO_EDGE_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const STUDIO_EDGE_CACHE_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const STUDIO_EDGE_CACHE_MAX_ENTRIES_PER_DB: usize = 128;
const STUDIO_EDGE_CACHE_MAX_BYTES_PER_DB: usize = 32 * 1024 * 1024;
const STUDIO_FAVICON_CACHE_MAX_ENTRIES: usize = 128;
const STUDIO_FAVICON_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
const STUDIO_FAVICON_CACHE_MAX_ENTRIES_PER_DB: usize = 8;
const STUDIO_AUTH_CACHE_MAX_ENTRIES: usize =
    STUDIO_EDGE_CACHE_MAX_ENTRIES - STUDIO_FAVICON_CACHE_MAX_ENTRIES;
const STUDIO_AUTH_CACHE_MAX_BYTES: usize =
    STUDIO_EDGE_CACHE_MAX_BYTES - STUDIO_FAVICON_CACHE_MAX_BYTES;

#[derive(Clone, Copy, PartialEq, Eq)]
enum StudioCacheKind {
    Shell,
    NextStatic,
    Image,
    Favicon,
    Monaco,
}

#[derive(Clone)]
struct StudioCachedResponse {
    status: StatusCode,
    headers: Vec<(header::HeaderName, header::HeaderValue)>,
    body: axum::body::Bytes,
    etag: String,
    kind: StudioCacheKind,
}

struct StudioCacheEntry {
    response: Arc<StudioCachedResponse>,
    db_id: String,
    used: u64,
}

#[derive(Default)]
struct StudioEdgeCache {
    entries: std::collections::HashMap<String, StudioCacheEntry>,
    bytes: usize,
    clock: u64,
}

static STUDIO_EDGE_CACHE: std::sync::OnceLock<parking_lot::Mutex<StudioEdgeCache>> =
    std::sync::OnceLock::new();
static STUDIO_EDGE_FILL_LOCKS: std::sync::OnceLock<[tokio::sync::Mutex<()>; 64]> =
    std::sync::OnceLock::new();

/// Percent escapes are refused: reqwest's URL parser can normalize encoded dot-segments after this
/// edge classified the original path, which must never turn a public favicon route into a different
/// upstream route.
fn studio_safe_asset_path(path: &str) -> bool {
    path.len() <= 512
        && !path.split('/').any(|segment| segment == "..")
        && path.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.' | b'@' | b'+' | b'~')
        })
}

/// Positive allowlist derived from the pinned image's Next.js build and public tree: `/api`,
/// `/_next/data`, the login exchange, mutations and every unknown route never enter this lane and
/// keep ordinary owner routing. The shell and immutable assets are cached node-locally only after
/// this edge repeats the ordinary Studio auth gate, before the owner hop and deliberately not as a
/// CCN lookup.
fn studio_cache_candidate(req: &Request) -> Option<StudioCacheKind> {
    if !matches!(*req.method(), Method::GET | Method::HEAD)
        || req.uri().query().is_some()
        || req.headers().contains_key(header::RANGE)
        || req.headers().contains_key(header::UPGRADE)
    {
        return None;
    }
    let path = req.uri().path();
    if path == "/project/default" {
        return Some(StudioCacheKind::Shell);
    }
    if !studio_safe_asset_path(path) {
        return None;
    }
    if path.starts_with("/_next/static/") {
        Some(StudioCacheKind::NextStatic)
    } else if path.starts_with("/img/") {
        Some(StudioCacheKind::Image)
    } else if path == "/favicon.ico" || path.starts_with("/favicon/") {
        Some(StudioCacheKind::Favicon)
    } else if path.starts_with("/monaco-editor/") {
        Some(StudioCacheKind::Monaco)
    } else {
        None
    }
}

fn studio_session_ok(db: &Database, headers: &HeaderMap) -> bool {
    let studio_password = db
        .connection
        .get("STUDIO_PASSWORD")
        .map(String::as_str)
        .unwrap_or("");
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .filter(|(name, _)| *name == STUDIO_SESSION_COOKIE)
        .any(|(_, token)| verify_studio_session(studio_password, &db.id, token))
}

fn studio_basic_ok(db: &Database, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Basic ")
                .or_else(|| v.strip_prefix("basic "))
        })
        .and_then(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()
        })
        .and_then(|raw| String::from_utf8(raw).ok())
        .is_some_and(|pair| {
            let (user, password) = pair.split_once(':').unwrap_or((pair.as_str(), ""));
            let expected_user = db
                .connection
                .get("STUDIO_USERNAME")
                .map(String::as_str)
                .unwrap_or("");
            let expected_password = db
                .connection
                .get("STUDIO_PASSWORD")
                .map(String::as_str)
                .unwrap_or("");
            !expected_user.is_empty()
                && ct_eq(user.as_bytes(), expected_user.as_bytes())
                && ct_eq(password.as_bytes(), expected_password.as_bytes())
        })
}

fn studio_unauthorized() -> Response {
    let mut response = err(
        StatusCode::UNAUTHORIZED,
        "Supabase Studio credentials required — they are the STUDIO_USERNAME/STUDIO_PASSWORD pair under this database's Connection details on the dashboard's Storage page, or use its Open Studio button for one-click login",
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Basic realm=\"supabase-studio\", charset=\"UTF-8\""),
    );
    response
}

/// A Studio-owned cookie may affect the representation: only requests with no cookies, or solely
/// this edge's session cookie (stripped before upstream), are eligible for a shared per-database
/// cache entry.
fn studio_has_upstream_cookie(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .any(|cookie| {
            cookie
                .trim()
                .split_once('=')
                .map(|(name, _)| name != STUDIO_SESSION_COOKIE)
                .unwrap_or(true)
        })
}

/// Collapses arbitrary Accept-Encoding syntax to the one encoding this edge offers upstream, so the
/// cache has exactly four representation variants instead of one attacker-controlled variant per
/// raw header spelling.
fn studio_encoding_variant(headers: &HeaderMap) -> Option<&'static str> {
    const SUPPORTED: [&str; 4] = ["br", "gzip", "deflate", "identity"];
    let mut quality = [None; 4];
    let mut wildcard = None;
    let mut saw_header = false;
    for value in headers.get_all(header::ACCEPT_ENCODING).iter() {
        let Ok(value) = value.to_str() else {
            return None;
        };
        saw_header = true;
        for item in value.split(',') {
            let mut fields = item.trim().split(';');
            let name = fields.next().unwrap_or("").trim().to_ascii_lowercase();
            let mut q = 1000u16;
            for field in fields {
                let Some((key, value)) = field.trim().split_once('=') else {
                    return None;
                };
                if !key.trim().eq_ignore_ascii_case("q") {
                    return None;
                }
                let parsed = value.trim().parse::<f32>().ok()?;
                if !parsed.is_finite() || !(0.0..=1.0).contains(&parsed) {
                    return None;
                }
                q = (parsed * 1000.0).round() as u16;
            }
            if name == "*" {
                wildcard = Some(wildcard.unwrap_or(0).max(q));
            } else if let Some(index) = SUPPORTED.iter().position(|supported| *supported == name) {
                quality[index] = Some(quality[index].unwrap_or(0).max(q));
            }
        }
    }
    if !saw_header {
        return Some("identity");
    }
    let mut selected = None;
    for (index, encoding) in SUPPORTED.iter().enumerate() {
        let default = if *encoding == "identity" {
            1000
        } else {
            wildcard.unwrap_or(0)
        };
        let q = quality[index].unwrap_or(default);
        if q > 0 && selected.is_none_or(|(_, best_q)| q > best_q) {
            selected = Some((*encoding, q));
        }
    }
    selected.map(|(encoding, _)| encoding)
}

fn studio_cache_key(db: &Database, path: &str, encoding: &str) -> String {
    format!("{STUDIO_IMAGE_PIN}\0{}\0{path}\0{encoding}", db.id)
}

fn studio_cache_get(key: &str) -> Option<Arc<StudioCachedResponse>> {
    let cache = STUDIO_EDGE_CACHE.get_or_init(Default::default);
    let mut cache = cache.lock();
    cache.clock = cache.clock.wrapping_add(1);
    let used = cache.clock;
    let entry = cache.entries.get_mut(key)?;
    entry.used = used;
    Some(entry.response.clone())
}

/// Per-database pressure evicts only that database; global favicon pressure evicts only public
/// favicon entries, never another database's authenticated shells or assets.
fn studio_cache_insert(key: String, db_id: &str, response: StudioCachedResponse) {
    let size = response.body.len();
    if size > STUDIO_EDGE_CACHE_MAX_ENTRY_BYTES {
        return;
    }
    let favicon = response.kind == StudioCacheKind::Favicon;
    let cache = STUDIO_EDGE_CACHE.get_or_init(Default::default);
    let mut cache = cache.lock();
    if let Some(old) = cache.entries.remove(&key) {
        cache.bytes = cache.bytes.saturating_sub(old.response.body.len());
    }

    let class_totals = |cache: &StudioEdgeCache, only_db: bool| {
        cache
            .entries
            .values()
            .filter(|entry| {
                (entry.response.kind == StudioCacheKind::Favicon) == favicon
                    && (!only_db || entry.db_id == db_id)
            })
            .fold((0usize, 0usize), |(entries, bytes), entry| {
                (entries + 1, bytes + entry.response.body.len())
            })
    };
    loop {
        let (db_entries, db_bytes) = class_totals(&cache, true);
        let (class_entries, class_bytes) = class_totals(&cache, false);
        let over_db = if favicon {
            db_entries >= STUDIO_FAVICON_CACHE_MAX_ENTRIES_PER_DB
        } else {
            db_entries >= STUDIO_EDGE_CACHE_MAX_ENTRIES_PER_DB
                || db_bytes.saturating_add(size) > STUDIO_EDGE_CACHE_MAX_BYTES_PER_DB
        };
        let over_class = if favicon {
            class_entries >= STUDIO_FAVICON_CACHE_MAX_ENTRIES
                || class_bytes.saturating_add(size) > STUDIO_FAVICON_CACHE_MAX_BYTES
        } else {
            class_entries >= STUDIO_AUTH_CACHE_MAX_ENTRIES
                || class_bytes.saturating_add(size) > STUDIO_AUTH_CACHE_MAX_BYTES
        };
        if !over_db && !over_class {
            break;
        }
        let victim_db = over_db.then_some(db_id);
        let Some(oldest) = cache
            .entries
            .iter()
            .filter(|(_, entry)| {
                (entry.response.kind == StudioCacheKind::Favicon) == favicon
                    && victim_db.is_none_or(|id| entry.db_id == id)
            })
            .min_by_key(|(_, entry)| entry.used)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if let Some(old) = cache.entries.remove(&oldest) {
            cache.bytes = cache.bytes.saturating_sub(old.response.body.len());
        }
    }
    if cache.bytes.saturating_add(size) > STUDIO_EDGE_CACHE_MAX_BYTES {
        return;
    }
    cache.clock = cache.clock.wrapping_add(1);
    let used = cache.clock;
    cache.bytes += size;
    cache.entries.insert(
        key,
        StudioCacheEntry {
            response: Arc::new(response),
            db_id: db_id.to_string(),
            used,
        },
    );
}

/// The shell URL is stable rather than content-addressed: it is cached at the edge under the image
/// pin, but browsers must revalidate promptly.
fn studio_cache_control(kind: StudioCacheKind) -> &'static str {
    match kind {
        StudioCacheKind::Shell => "private, max-age=60, must-revalidate",
        StudioCacheKind::NextStatic => "private, max-age=31536000, immutable",
        StudioCacheKind::Image => "private, max-age=2592000",
        StudioCacheKind::Favicon | StudioCacheKind::Monaco => "private, max-age=86400",
    }
}

fn studio_cached_response(
    cached: &StudioCachedResponse,
    method: &Method,
    request_headers: &HeaderMap,
) -> Response {
    let not_modified = request_headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|values| values.split(','))
        .map(str::trim)
        .any(|value| {
            value == "*" || value.trim_start_matches("W/") == cached.etag.trim_start_matches("W/")
        });
    let mut builder = Response::builder()
        .status(if not_modified {
            StatusCode::NOT_MODIFIED
        } else {
            cached.status
        })
        .header(header::ETAG, cached.etag.as_str())
        .header(header::CACHE_CONTROL, studio_cache_control(cached.kind))
        .header(header::VARY, "Cookie, Authorization, Accept-Encoding");
    for (name, value) in &cached.headers {
        builder = builder.header(name, value);
    }
    let body = if not_modified || *method == Method::HEAD {
        axum::body::Body::empty()
    } else {
        axum::body::Body::from(cached.body.clone())
    };
    builder.body(body).unwrap_or_else(|_| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Studio cache response build",
        )
    })
}

fn studio_cacheable_content(kind: StudioCacheKind, headers: &HeaderMap) -> bool {
    if headers.contains_key(header::SET_COOKIE)
        || headers
            .get(header::VARY)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|field| field.trim() == "*"))
    {
        return false;
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    match kind {
        StudioCacheKind::Shell => content_type.starts_with("text/html"),
        _ => !content_type.is_empty() && !content_type.starts_with("text/html"),
    }
}

fn studio_end_to_end_headers(
    headers: &HeaderMap,
) -> Vec<(header::HeaderName, header::HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "connection"
                    | "transfer-encoding"
                    | "content-length"
                    | "keep-alive"
                    | "upgrade"
                    | "set-cookie"
                    | "cache-control"
                    | "etag"
                    | "vary"
                    | "date"
            )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn studio_rebuild_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if !matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length" | "keep-alive" | "upgrade"
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| err(StatusCode::BAD_GATEWAY, "Studio response build"))
}

async fn studio_fetch_from_owner(cloud: &Arc<CloudState>, db: &Database, req: Request) -> Response {
    if !db.host_node.is_empty() && db.host_node != cloud.node_name {
        return studio_forward_to_owner(cloud, db, req)
            .await
            .unwrap_or_else(|| {
                err(
                    StatusCode::MISDIRECTED_REQUEST,
                    "this Supabase database's owner node is unreachable",
                )
            });
    }
    supabase_studio_proxy(cloud, db, req).await
}

async fn studio_require_empty_body(req: Request) -> Result<Request, Response> {
    let (parts, body) = req.into_parts();
    match axum::body::to_bytes(body, 0).await {
        Ok(body) if body.is_empty() => Ok(Request::from_parts(parts, axum::body::Body::empty())),
        _ => Err(err(
            StatusCode::BAD_REQUEST,
            "Supabase GET/HEAD requests must not carry a body",
        )),
    }
}

/// Buffers a cache candidate only while it fits; once the cap is crossed the already-read prefix is
/// replayed and the untouched remainder streamed to the client, so cache admission never turns a
/// valid large Studio asset into a 502.
async fn studio_buffer_cache_candidate(response: Response) -> Result<axum::body::Bytes, Response> {
    use futures::StreamExt;

    let (parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut chunks = Vec::new();
    let mut bytes = 0usize;
    while let Some(next) = stream.next().await {
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(_) => return Err(err(StatusCode::BAD_GATEWAY, "Studio response body failed")),
        };
        bytes = bytes.saturating_add(chunk.len());
        if bytes > STUDIO_EDGE_CACHE_MAX_ENTRY_BYTES {
            let prefix =
                futures::stream::iter(chunks.into_iter().map(Ok::<axum::body::Bytes, axum::Error>))
                    .chain(futures::stream::once(async move { Ok(chunk) }))
                    .chain(stream);
            return Err(Response::from_parts(
                parts,
                axum::body::Body::from_stream(prefix),
            ));
        }
        chunks.push(chunk);
    }
    let mut body = Vec::with_capacity(bytes);
    for chunk in chunks {
        body.extend_from_slice(&chunk);
    }
    Ok(axum::body::Bytes::from(body))
}

/// A HEAD miss cannot populate a GET representation and keeps the owner's original status and
/// validators. A fill forces the representation promised by the bounded cache key and suppresses
/// client validators only for the fill: the client's own conditions are evaluated against the
/// completed representation.
async fn studio_cached_route(
    cloud: &Arc<CloudState>,
    db: &Database,
    mut req: Request,
    kind: StudioCacheKind,
) -> Response {
    let is_public = kind == StudioCacheKind::Favicon;
    if !is_public && !studio_session_ok(db, req.headers()) && !studio_basic_ok(db, req.headers()) {
        return studio_unauthorized();
    }
    if studio_has_upstream_cookie(req.headers()) {
        return studio_fetch_from_owner(cloud, db, req).await;
    }
    let method = req.method().clone();
    let request_headers = req.headers().clone();
    let Some(encoding) = studio_encoding_variant(req.headers()) else {
        return studio_fetch_from_owner(cloud, db, req).await;
    };
    let key = studio_cache_key(db, req.uri().path(), encoding);
    if let Some(cached) = studio_cache_get(&key) {
        return studio_cached_response(&cached, &method, &request_headers);
    }
    if method == Method::HEAD {
        req.headers_mut().insert(
            header::ACCEPT_ENCODING,
            header::HeaderValue::from_static(encoding),
        );
        return studio_fetch_from_owner(cloud, db, req).await;
    }

    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let locks =
        STUDIO_EDGE_FILL_LOCKS.get_or_init(|| std::array::from_fn(|_| tokio::sync::Mutex::new(())));
    let _fill = locks[(hasher.finish() as usize) % locks.len()].lock().await;
    if let Some(cached) = studio_cache_get(&key) {
        return studio_cached_response(&cached, &method, &request_headers);
    }

    req.headers_mut().insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static(encoding),
    );
    req.headers_mut().remove(header::IF_NONE_MATCH);
    req.headers_mut().remove(header::IF_MODIFIED_SINCE);
    let response = studio_fetch_from_owner(cloud, db, req).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = match studio_buffer_cache_candidate(response).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    if status == StatusCode::OK && studio_cacheable_content(kind, &headers) {
        use sha2::Digest as _;
        let etag = headers
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("\"{}\"", hex::encode(sha2::Sha256::digest(&body))));
        let cached = StudioCachedResponse {
            status,
            headers: studio_end_to_end_headers(&headers),
            body: body.clone(),
            etag,
            kind,
        };
        studio_cache_insert(key, &db.id, cached.clone());
        return studio_cached_response(&cached, &method, &request_headers);
    }
    studio_rebuild_response(status, &headers, body)
}

/// HMAC-SHA256 over length-delimited ticket/session fields, keyed with this database's generated
/// STUDIO_PASSWORD. Tickets also bind a CSPRNG nonce; sessions use a separate domain and carry no
/// replay state.
fn studio_mac(
    studio_password: &str,
    domain: &str,
    db_id: &str,
    exp_ms: u64,
    authority_node: Option<&str>,
    nonce: Option<&str>,
) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(studio_password.as_bytes())
        .expect("hmac accepts any key length");
    for field in [domain, db_id] {
        mac.update(&(field.len() as u64).to_be_bytes());
        mac.update(field.as_bytes());
    }
    mac.update(&exp_ms.to_be_bytes());
    if let Some(authority_node) = authority_node {
        mac.update(&(authority_node.len() as u64).to_be_bytes());
        mac.update(authority_node.as_bytes());
    }
    if let Some(nonce) = nonce {
        mac.update(&(nonce.len() as u64).to_be_bytes());
        mac.update(nonce.as_bytes());
    }
    hex::encode(mac.finalize().into_bytes())
}

fn lowercase_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .as_bytes()
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// Mints a nonce-bearing one-use ticket bound to the exact node that must durably consume it
/// (`Uuid::new_v4` is the OS CSPRNG, the repository's credential/token nonce source).
pub(crate) fn mint_studio_ticket(
    studio_password: &str,
    db_id: &str,
    authority_node: &str,
    ttl_ms: u64,
) -> String {
    use base64::Engine as _;

    let exp = hive_core::now_ms().saturating_add(ttl_ms);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let authority = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(authority_node);
    let mac = studio_mac(
        studio_password,
        STUDIO_TICKET_DOMAIN,
        db_id,
        exp,
        Some(authority_node),
        Some(&nonce),
    );
    format!("{STUDIO_TICKET_VERSION}.{exp}.{nonce}.{authority}.{mac}")
}

fn mint_studio_session(studio_password: &str, db_id: &str) -> String {
    let exp = hive_core::now_ms().saturating_add(STUDIO_SESSION_TTL_MS);
    format!(
        "{exp}.{}",
        studio_mac(
            studio_password,
            STUDIO_SESSION_DOMAIN,
            db_id,
            exp,
            None,
            None,
        )
    )
}

/// Strictly parses and authenticates a versioned, issuer-bound ticket. Legacy tickets fail closed:
/// without a signed authority there is no linearizable node to consult during a control-leader
/// flap.
fn verify_studio_ticket(
    studio_password: &str,
    db_id: &str,
    token: &str,
) -> Option<(String, u64, String)> {
    use base64::Engine as _;

    if studio_password.is_empty() || token.len() > STUDIO_TICKET_MAX_BYTES {
        return None;
    }
    let mut fields = token.split('.');
    let (Some(version), Some(exp_s), Some(nonce), Some(authority_b64), Some(mac_hex), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return None;
    };
    if version != STUDIO_TICKET_VERSION
        || exp_s.is_empty()
        || exp_s.len() > 20
        || !exp_s.bytes().all(|b| b.is_ascii_digit())
        || !lowercase_hex(nonce, 32)
        || authority_b64.is_empty()
        || authority_b64.len() > STUDIO_AUTHORITY_B64_MAX_BYTES
        || !lowercase_hex(mac_hex, 64)
    {
        return None;
    }
    let authority_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(authority_b64)
        .ok()?;
    let authority_node = std::str::from_utf8(&authority_bytes).ok()?;
    if authority_node.is_empty()
        || authority_node.len() > STUDIO_AUTHORITY_MAX_BYTES
        || base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(authority_node) != authority_b64
    {
        return None;
    }
    let exp = exp_s.parse::<u64>().ok()?;
    let now = hive_core::now_ms();
    if now > exp || exp > now.saturating_add(STUDIO_TICKET_TTL_MS) {
        return None;
    }
    let expected = studio_mac(
        studio_password,
        STUDIO_TICKET_DOMAIN,
        db_id,
        exp,
        Some(authority_node),
        Some(nonce),
    );
    ct_eq(mac_hex.as_bytes(), expected.as_bytes())
        .then(|| (nonce.to_string(), exp, authority_node.to_string()))
}

fn verify_studio_session(studio_password: &str, db_id: &str, token: &str) -> bool {
    if studio_password.is_empty() || token.len() > STUDIO_SESSION_MAX_BYTES {
        return false;
    }
    let mut fields = token.split('.');
    let (Some(exp_s), Some(mac_hex), None) = (fields.next(), fields.next(), fields.next()) else {
        return false;
    };
    if exp_s.is_empty()
        || exp_s.len() > 20
        || !exp_s.bytes().all(|b| b.is_ascii_digit())
        || !lowercase_hex(mac_hex, 64)
    {
        return false;
    }
    let Ok(exp) = exp_s.parse::<u64>() else {
        return false;
    };
    if hive_core::now_ms() > exp {
        return false;
    }
    ct_eq(
        mac_hex.as_bytes(),
        studio_mac(
            studio_password,
            STUDIO_SESSION_DOMAIN,
            db_id,
            exp,
            None,
            None,
        )
        .as_bytes(),
    )
}

fn consume_studio_ticket_digest_local(
    cloud: &Arc<CloudState>,
    db_id: &str,
    digest: &str,
    exp_ms: u64,
) -> bool {
    if !cloud.databases.consume_studio_ticket_digest(
        db_id,
        digest,
        exp_ms,
        crate::databases::STUDIO_REPLAY_STATE_MAX,
    ) {
        return false;
    }
    crate::persist::persist_durable(cloud)
}

fn studio_replay_accepted(envelope: &Value) -> Option<bool> {
    use base64::Engine as _;

    if envelope.get("status").and_then(Value::as_u64) != Some(200) {
        return Some(false);
    }
    let body = envelope.get("body_b64")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("accepted")?
        .as_bool()
}

/// Studio envelopes can carry Basic credentials, session cookies or a replay decision, so they
/// never ride a plaintext cross-node admin hop nor an HTTPS endpoint that may redirect them to one;
/// iroh is E2E encrypted, and a non-redirecting HTTPS admin URL remains an authenticated fallback.
fn studio_secure_admin_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .is_some_and(|url| url.scheme() == "https")
}

fn studio_admin_http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client")
    })
}

/// Consume at the immutable authority signed into the ticket. Recomputing the
/// control leader here would create overlapping writers during a leader flap.
/// A success header means the immutable authority may already have durably consumed the nonce, so a
/// missing or malformed body is ambiguous and fails closed: the write is never retried over the
/// second transport.
async fn consume_studio_ticket_nonce(
    cloud: &Arc<CloudState>,
    db_id: &str,
    nonce: &str,
    exp_ms: u64,
    authority_node: &str,
) -> bool {
    use sha2::Digest as _;

    let digest = hex::encode(sha2::Sha256::digest(
        [
            db_id.as_bytes(),
            b"\0" as &[u8],
            authority_node.as_bytes(),
            b"\0" as &[u8],
            nonce.as_bytes(),
        ]
        .concat(),
    ));
    if authority_node == cloud.node_name {
        return consume_studio_ticket_digest_local(cloud, db_id, &digest, exp_ms);
    }
    let route = format!("/v1/databases/{db_id}/studio-mesh");
    let env = json!({
        "replay_digest": digest,
        "replay_exp_ms": exp_ms,
        "replay_authority": authority_node,
    });
    let admin = {
        let admins = cloud.node_admins.read();
        admins
            .get(authority_node)
            .filter(|url| studio_secure_admin_url(url))
            .cloned()
    };
    if let Some(admin) = admin {
        let mut request = studio_admin_http()
            .post(format!("{admin}{route}"))
            .json(&env)
            .timeout(std::time::Duration::from_secs(15));
        if crate::auth::enforced() {
            if let Ok(token) = crate::auth::issue(
                "mesh-internal",
                "",
                "service",
                false,
                crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
            ) {
                request = request.bearer_auth(token);
            }
        }
        if let Ok(response) = request.send().await {
            if response.status().is_success() {
                return studio_read_http_envelope(response)
                    .await
                    .ok()
                    .as_ref()
                    .and_then(studio_replay_accepted)
                    .unwrap_or(false);
            }
        }
    }
    let Some((peer_id, addr)) = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|node| node.name == authority_node)
        .and_then(|node| Some((node.peer_id?, node.iroh_addr?)))
    else {
        return false;
    };
    let payload = match serde_json::to_vec(&env) {
        Ok(payload) => payload,
        Err(_) => return false,
    };
    let Some(response) = crate::gossip::request_to(
        cloud,
        &peer_id,
        &addr,
        hive_p2p::GOSSIP_POST,
        &route,
        &payload,
        15,
    )
    .await
    else {
        return false;
    };
    serde_json::from_slice::<Value>(&response)
        .ok()
        .as_ref()
        .and_then(studio_replay_accepted)
        .unwrap_or(false)
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// Public, credential-free receiver for the dashboard's cross-origin `postMessage`: the ticket
/// stays out of every URL and only this same-origin page creates the POST form, so the dashboard's
/// `form-action 'self'` CSP does not block the exchange.
fn studio_login_bootstrap(head: bool) -> Response {
    const HTML: &str = r#"<!doctype html><meta charset="utf-8"><meta name="referrer" content="no-referrer"><title>Opening Supabase Studio…</title><style>body{font:14px system-ui;margin:32px;color:#555}</style><p id="status">Opening Supabase Studio…</p><script>(()=>{let used=false;addEventListener("message",event=>{const data=event.data;if(used||!data||data.type!=="hive-studio-login-ticket"||typeof data.ticket!=="string")return;used=true;try{event.source.postMessage({type:"hive-studio-login-received"},event.origin)}catch{}const form=document.createElement("form");form.method="POST";form.action=location.pathname;form.enctype="application/x-www-form-urlencoded";const ticket=document.createElement("input");ticket.type="hidden";ticket.name="ticket";ticket.value=data.ticket;form.appendChild(ticket);document.body.replaceChildren(form);form.submit()})})()</script>"#;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::PRAGMA, "no-cache")
        .header("referrer-policy", "no-referrer")
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'")
        .body(if head {
            axum::body::Body::empty()
        } else {
            axum::body::Body::from(HTML)
        })
        .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Studio login page build"))
}

const STUDIO_TICKET_VERSION: &str = "v3";
const STUDIO_TICKET_DOMAIN: &str = "hive-studio-ticket-v3";
const STUDIO_SESSION_DOMAIN: &str = "hive-studio-session-v2";
pub(crate) const STUDIO_TICKET_TTL_MS: u64 = 60_000;
const STUDIO_SESSION_TTL_MS: u64 = 12 * 3600 * 1000;
const STUDIO_LOGIN_PATH: &str = "/_hive/studio-login";
const STUDIO_SESSION_COOKIE: &str = "__Host-hive_studio_session";
const STUDIO_AUTHORITY_MAX_BYTES: usize = 128;
const STUDIO_AUTHORITY_B64_MAX_BYTES: usize = 172;
const STUDIO_TICKET_MAX_BYTES: usize = 320;
const STUDIO_SESSION_MAX_BYTES: usize = 96;
const STUDIO_LOGIN_BODY_CAP: usize = STUDIO_TICKET_MAX_BYTES + "ticket=".len();

/// The whole `<slug>.{db_domain}` hostname of a Supabase database IS the Studio dashboard: a
/// basic-auth gate (the generated STUDIO_USERNAME/STUDIO_PASSWORD, the upstream Kong's
/// `DASHBOARD_*` mechanism with `hide_credentials`: the Authorization header is checked here and
/// never forwarded), then a streaming reverse proxy to the studio container's loopback port. It
/// runs on the database's host node (the per-database DNS record is owner-affine); a request
/// landing on any OTHER node is forwarded to `Database::host_node` by `studio_forward_to_owner` as
/// a bounded base64 JSON envelope of the whole exchange, and the owner re-runs the same Basic-auth
/// check against its own credentials: an edge forwards bytes, it never authenticates on the
/// tenant's behalf.
///
/// The platform edge IS this stack's Kong: `/auth/v1`, `/rest/v1`, `/storage/v1` and `/graphql/v1`
/// (mapped to PostgREST's `/rpc/graphql`) route to the sidecars' loopback ports with the prefix
/// STRIPPED, authenticated by the services' own apikey/JWT (the public data-plane model of hosted
/// Supabase), so the Studio Basic gate does not apply there and the Authorization header is
/// stripped ONLY on the Studio plane (stripping it on the data plane would 401 every authenticated
/// call). `/favicon/` PWA assets are served ungated (Chrome fetches them WITHOUT credentials by
/// spec, so they could never pass Basic; they are static image bytes disclosing nothing the 401's
/// realm does not). The one-time login exchange is intercepted at its reserved path for every
/// method, so neither a token nor a malformed exchange reaches Studio, and the edge session cookie
/// authenticates only this proxy (never leaked upstream; Studio-owned cookies pass verbatim).
///
/// The request body is buffered to a Studio-appropriate cap (SQL/CSV imports) while the response is
/// streamed (multi-MB bundles). A reverse proxy must RELAY redirects, never follow them: reqwest's
/// default policy swallowed Studio's own `GET / -> 307 /project/default`, so Next.js hydration
/// threw before first paint (hence the `NO_REDIRECT` client), and a `Location` is rewritten so the
/// browser is never sent to an internal origin.
async fn supabase_studio_proxy(cloud: &Arc<CloudState>, db: &Database, req: Request) -> Response {
    let raw_path = req.uri().path().to_string();
    let raw_q = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let strip = |rest: &str| {
        let p = if rest.is_empty() { "/" } else { rest };
        format!("{p}{raw_q}")
    };
    let data_plane: Option<(&str, String)> = if let Some(rest) = raw_path.strip_prefix("/auth/v1") {
        Some(("auth_port", strip(rest)))
    } else if let Some(rest) = raw_path.strip_prefix("/rest/v1") {
        Some(("rest_port", strip(rest)))
    } else if let Some(rest) = raw_path.strip_prefix("/storage/v1") {
        Some(("storage_port", strip(rest)))
    } else if raw_path == "/graphql/v1" {
        Some(("rest_port", format!("/rpc/graphql{raw_q}")))
    } else if (raw_path.starts_with("/favicon/") || raw_path == "/favicon.ico")
        && studio_safe_asset_path(&raw_path)
        && (req.method() == axum::http::Method::GET || req.method() == axum::http::Method::HEAD)
    {
        Some(("studio_port", format!("{raw_path}{raw_q}")))
    } else {
        None
    };

    let studio_password = db
        .connection
        .get("STUDIO_PASSWORD")
        .map(String::as_str)
        .unwrap_or("");
    if data_plane.is_none() && raw_path == STUDIO_LOGIN_PATH {
        if req.uri().query().is_some() {
            return no_store(err(
                StatusCode::BAD_REQUEST,
                "Studio login exchange does not accept a query string",
            ));
        }
        if req.method() == axum::http::Method::GET {
            return studio_login_bootstrap(false);
        }
        if req.method() == axum::http::Method::HEAD {
            return studio_login_bootstrap(true);
        }
        if req.method() != axum::http::Method::POST {
            return no_store(err(
                StatusCode::METHOD_NOT_ALLOWED,
                "Studio login exchange requires GET, HEAD, or POST",
            ));
        }
        let form_content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| {
                v.trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            });
        if !form_content_type {
            return no_store(err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Studio login exchange requires a form body",
            ));
        }
        let body = match axum::body::to_bytes(req.into_body(), STUDIO_LOGIN_BODY_CAP).await {
            Ok(body) => body,
            Err(_) => {
                return no_store(err(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Studio login form is too large",
                ))
            }
        };
        let token = std::str::from_utf8(&body)
            .ok()
            .and_then(|form| form.strip_prefix("ticket="))
            .filter(|token| !token.is_empty() && !token.contains('&'));
        let accepted =
            match token.and_then(|token| verify_studio_ticket(studio_password, &db.id, token)) {
                Some((nonce, exp, authority_node)) => {
                    consume_studio_ticket_nonce(cloud, &db.id, &nonce, exp, &authority_node).await
                }
                None => false,
            };
        if !accepted {
            return no_store(err(
                StatusCode::UNAUTHORIZED,
                "invalid, expired, or already-used Studio login ticket",
            ));
        }
        let session = mint_studio_session(studio_password, &db.id);
        let response = Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, "/project/default")
            .header(header::CACHE_CONTROL, "no-store")
            .header(
                header::SET_COOKIE,
                format!(
                    "{STUDIO_SESSION_COOKIE}={session}; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
                    STUDIO_SESSION_TTL_MS / 1000
                ),
            )
            .body(axum::body::Body::empty())
            .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "redirect build"));
        return response;
    }

    let ok = data_plane.is_some()
        || studio_session_ok(db, req.headers())
        || studio_basic_ok(db, req.headers());
    if !ok {
        return studio_unauthorized();
    }

    let port_key = data_plane
        .as_ref()
        .map(|(k, _)| *k)
        .unwrap_or("studio_port");
    let Some(port) = db
        .connection
        .get(port_key)
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<u16>().ok())
    else {
        if data_plane.is_some() {
            return err(
                StatusCode::NOT_IMPLEMENTED,
                "this Supabase stack's API sidecars are still being provisioned — retry shortly",
            );
        }
        return err(
            StatusCode::MISDIRECTED_REQUEST,
            "this Supabase database is not hosted on this node (stale DNS?) or has no live Studio",
        );
    };
    let method = req.method().clone();
    let path_q = match &data_plane {
        Some((_, stripped)) => stripped.clone(),
        None => req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".into()),
    };
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
            if matches!(
                name.as_str(),
                "host"
                    | "connection"
                    | "content-length"
                    | "transfer-encoding"
                    | "keep-alive"
                    | "upgrade"
                    | "x-hive-proxied"
                    | "x-hive-request-id"
            ) || (name == "authorization" && data_plane.is_none())
            {
                return None;
            }
            let value = v.to_str().ok()?;
            if name == "cookie" {
                let kept = value
                    .split(';')
                    .filter(|cookie| {
                        cookie
                            .trim()
                            .split_once('=')
                            .map(|(cookie_name, _)| cookie_name != STUDIO_SESSION_COOKIE)
                            .unwrap_or(true)
                    })
                    .map(str::trim)
                    .filter(|cookie| !cookie.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                return (!kept.is_empty()).then_some((name, kept));
            }
            Some((name, value.to_string()))
        })
        .collect();
    const STUDIO_BODY_CAP: usize = 64 * 1024 * 1024;
    let body = match axum::body::to_bytes(req.into_body(), STUDIO_BODY_CAP).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let url = format!("http://127.0.0.1:{port}{path_q}");
    static NO_REDIRECT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let http = NO_REDIRECT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client")
    });
    let mut rb = http
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

/// Cap on the buffered body in either direction of a mesh-relayed Studio exchange (the mesh
/// envelope cannot stream: the forwarded request and the owner's response are fully buffered);
/// matches `STUDIO_BODY_CAP` on the direct-serve path.
const STUDIO_MESH_BODY_CAP: usize = 64 * 1024 * 1024;
const STUDIO_MESH_BODY_B64_CAP: usize = 4 * STUDIO_MESH_BODY_CAP.div_ceil(3);
/// A 64 MiB body expands to ~85.4 MiB in the JSON base64 envelope. Keep the
/// route-specific QUIC allocation bounded while leaving room for status/headers.
const STUDIO_MESH_RESPONSE_FRAME_CAP: usize = 96 * 1024 * 1024;

fn studio_mesh_failure(code: &'static str, message: &'static str) -> Value {
    use base64::Engine as _;

    let body = json!({ "error": message, "code": code }).to_string();
    json!({
        "status": 502,
        "headers": [[header::CONTENT_TYPE.as_str(), "application/json"]],
        "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
    })
}

fn studio_mesh_failure_response(code: &'static str, message: &'static str) -> Response {
    studio_decode_envelope(&studio_mesh_failure(code, message)).unwrap_or_else(|| {
        err(
            StatusCode::BAD_GATEWAY,
            "Studio mesh response failed before it could be relayed",
        )
    })
}

#[derive(Clone, Copy)]
enum StudioEnvelopeReadError {
    BodyRead,
    TooLarge,
    BufferUnavailable,
    TransportAmbiguous,
}

impl StudioEnvelopeReadError {
    fn response(self) -> Response {
        match self {
            Self::BodyRead => studio_mesh_failure_response(
                "STUDIO_MESH_BODY_READ_FAILED",
                "Studio owner response failed while streaming to the ingress node",
            ),
            Self::TooLarge => studio_mesh_failure_response(
                "STUDIO_MESH_RESPONSE_TOO_LARGE",
                "Studio owner response exceeds the bounded mesh transport",
            ),
            Self::BufferUnavailable => studio_mesh_failure_response(
                "STUDIO_MESH_BUFFER_UNAVAILABLE",
                "Studio owner response could not be buffered for the mesh transport",
            ),
            Self::TransportAmbiguous => studio_mesh_failure_response(
                "STUDIO_MESH_TRANSPORT_AMBIGUOUS",
                "Studio owner transport failed after the request may have been delivered; it was not retried",
            ),
        }
    }
}

/// Forwards a Studio exchange to its owner: HTTPS admin first when configured (never plain HTTP:
/// the envelope carries Basic/session credentials), then the E2E-encrypted iroh mesh, the real path
/// on this fleet (loopback-only admin ports). An outcome that may already have executed a mutation
/// (the owner sent success headers, then the envelope failed or was oversized) is answered with an
/// honest typed 502 and NEVER replayed over the mesh; only reads, and statuses emitted before the
/// owner's Studio handler, may fall through to the second transport.
async fn studio_forward_to_owner(
    cloud: &Arc<CloudState>,
    db: &Database,
    req: Request,
) -> Option<Response> {
    let node = db.host_node.clone();
    if node.is_empty() {
        return None;
    }
    let method = req.method().clone();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let hdrs: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    let (_parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, STUDIO_MESH_BODY_CAP)
        .await
        .ok()?;
    let body_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };
    let env = json!({
        "method": method.as_str(),
        "path": path_q,
        "headers": hdrs,
        "body_b64": body_b64,
    });
    let route = format!("/v1/databases/{}/studio-mesh", db.id);

    let admin = cloud
        .node_admins
        .read()
        .get(&node)
        .filter(|url| studio_secure_admin_url(url))
        .cloned();
    if let Some(admin) = admin {
        let mut rb = studio_admin_http()
            .post(format!("{admin}{route}"))
            .json(&env)
            .timeout(std::time::Duration::from_secs(60));
        if crate::auth::enforced() {
            if let Ok(tok) = crate::auth::issue(
                "mesh-internal",
                "",
                "service",
                false,
                crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
            ) {
                rb = rb.bearer_auth(tok);
            }
        }
        match rb.send().await {
            Ok(r) if r.status().is_success() => {
                return Some(
                    match studio_read_http_envelope(r).await.and_then(|v| {
                        studio_decode_envelope(&v).ok_or(StudioEnvelopeReadError::BodyRead)
                    }) {
                        Ok(response) => response,
                        Err(error) => error.response(),
                    },
                );
            }
            Ok(r)
                if matches!(
                    r.status(),
                    StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN
                        | StatusCode::NOT_FOUND
                        | StatusCode::METHOD_NOT_ALLOWED
                        | StatusCode::PAYLOAD_TOO_LARGE
                ) => {}
            Ok(_) => return Some(StudioEnvelopeReadError::TransportAmbiguous.response()),
            Err(_)
                if method == Method::GET || method == Method::HEAD || method == Method::OPTIONS => {
            }
            Err(_) => return Some(StudioEnvelopeReadError::TransportAmbiguous.response()),
        }
    }

    let target = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)))?;
    let payload = serde_json::to_vec(&env).ok()?;
    let bytes = crate::gossip::request_to_with_response_cap(
        cloud,
        &target.0,
        &target.1,
        hive_p2p::GOSSIP_POST,
        &route,
        &payload,
        60,
        STUDIO_MESH_RESPONSE_FRAME_CAP,
    )
    .await?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    studio_decode_envelope(&v)
}

async fn studio_read_http_envelope(
    response: reqwest::Response,
) -> Result<Value, StudioEnvelopeReadError> {
    use futures::StreamExt as _;

    let declared = response
        .content_length()
        .ok_or(StudioEnvelopeReadError::BodyRead)?;
    let declared = usize::try_from(declared).map_err(|_| StudioEnvelopeReadError::TooLarge)?;
    if declared > STUDIO_MESH_RESPONSE_FRAME_CAP {
        return Err(StudioEnvelopeReadError::TooLarge);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(declared)
        .map_err(|_| StudioEnvelopeReadError::BufferUnavailable)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StudioEnvelopeReadError::BodyRead)?;
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|size| size > declared)
        {
            return Err(StudioEnvelopeReadError::BodyRead);
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != declared {
        return Err(StudioEnvelopeReadError::BodyRead);
    }
    serde_json::from_slice(&bytes).map_err(|_| StudioEnvelopeReadError::BodyRead)
}

fn studio_decode_envelope(v: &Value) -> Option<Response> {
    use base64::Engine;
    let status = v.get("status")?.as_u64()? as u16;
    let status = StatusCode::from_u16(status).ok()?;
    let mut builder = Response::builder().status(status);
    if let Some(hdrs) = v.get("headers").and_then(|h| h.as_array()) {
        for h in hdrs {
            let (Some(k), Some(val)) = (
                h.get(0).and_then(|x| x.as_str()),
                h.get(1).and_then(|x| x.as_str()),
            ) else {
                continue;
            };
            if let (Ok(name), Ok(hv)) = (
                header::HeaderName::from_bytes(k.as_bytes()),
                header::HeaderValue::from_str(val),
            ) {
                builder = builder.header(name, hv);
            }
        }
    }
    let body_b64 = v.get("body_b64")?.as_str()?;
    if body_b64.len() > STUDIO_MESH_BODY_B64_CAP {
        return None;
    }
    let body = base64::engine::general_purpose::STANDARD
        .decode(body_b64)
        .ok()?;
    if body.len() > STUDIO_MESH_BODY_CAP {
        return None;
    }
    builder.body(axum::body::Body::from(body)).ok()
}

/// Owner-side handler for a forwarded Studio exchange (the mesh and HTTP-admin entry points both
/// call it): re-validates that THIS node is still the owner before touching the container, since a
/// stale caller-side record must never make a second node answer for a database it does not host.
pub(crate) async fn studio_mesh_serve(cloud: &Arc<CloudState>, id: &str, env: &Value) -> Value {
    use base64::Engine;
    let Some(db) = cloud.databases.get_raw(id) else {
        return json!({ "status": 404, "headers": [], "body_b64": "" });
    };
    if db.kind != DbKind::Supabase {
        return json!({ "status": 400, "headers": [], "body_b64": "" });
    }
    if let (Some(digest), Some(exp_ms), Some(authority_node)) = (
        env.get("replay_digest").and_then(Value::as_str),
        env.get("replay_exp_ms").and_then(Value::as_u64),
        env.get("replay_authority").and_then(Value::as_str),
    ) {
        let now = hive_core::now_ms();
        let valid = lowercase_hex(digest, 64)
            && exp_ms >= now
            && exp_ms <= now.saturating_add(STUDIO_TICKET_TTL_MS)
            && authority_node == cloud.node_name;
        let accepted = valid && consume_studio_ticket_digest_local(cloud, id, digest, exp_ms);
        let body = serde_json::to_vec(&json!({ "accepted": accepted })).unwrap_or_default();
        return json!({
            "status": if valid { 200 } else { 421 },
            "headers": [],
            "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
        });
    }
    if db.host_node != cloud.node_name {
        return json!({
            "status": 421,
            "headers": [],
            "body_b64": base64::engine::general_purpose::STANDARD
                .encode(b"not the current owner (stale forward)"),
        });
    }
    let Some(method_str) = env.get("method").and_then(Value::as_str) else {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_INVALID",
            "Studio mesh request is missing its method",
        );
    };
    let Ok(method) = Method::from_bytes(method_str.as_bytes()) else {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_INVALID",
            "Studio mesh request has an invalid method",
        );
    };
    let Some(path_q) = env.get("path").and_then(Value::as_str) else {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_INVALID",
            "Studio mesh request is missing its path",
        );
    };
    let Some(body_b64) = env.get("body_b64").and_then(Value::as_str) else {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_INVALID",
            "Studio mesh request is missing its body",
        );
    };
    if body_b64.len() > STUDIO_MESH_BODY_B64_CAP {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_TOO_LARGE",
            "Studio mesh request exceeds the bounded mesh transport",
        );
    }
    let Ok(body) = base64::engine::general_purpose::STANDARD.decode(body_b64) else {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_INVALID",
            "Studio mesh request body is not valid base64",
        );
    };
    if body.len() > STUDIO_MESH_BODY_CAP {
        return studio_mesh_failure(
            "STUDIO_MESH_REQUEST_TOO_LARGE",
            "Studio mesh request exceeds the bounded mesh transport",
        );
    }
    let mut builder = Request::builder().method(method).uri(path_q);
    if let Some(hdrs) = env.get("headers").and_then(|h| h.as_array()) {
        for h in hdrs {
            let (Some(k), Some(v)) = (
                h.get(0).and_then(|x| x.as_str()),
                h.get(1).and_then(|x| x.as_str()),
            ) else {
                continue;
            };
            builder = builder.header(k, v);
        }
    }
    let req = match builder.body(axum::body::Body::from(body)) {
        Ok(r) => r,
        Err(_) => return json!({ "status": 400, "headers": [], "body_b64": "" }),
    };
    let resp = supabase_studio_proxy(cloud, &db, req).await;
    let status = resp.status().as_u16();
    let hdrs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    use futures::StreamExt as _;

    let mut stream = resp.into_body().into_data_stream();
    let mut body_bytes = Vec::new();
    while let Some(next) = stream.next().await {
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(error) => {
                tracing::warn!(
                    db = %id,
                    error = %error,
                    "Studio owner response body failed while relaying over the mesh"
                );
                return studio_mesh_failure(
                    "STUDIO_MESH_BODY_READ_FAILED",
                    "Studio owner response failed while streaming to the mesh",
                );
            }
        };
        let Some(next_len) = body_bytes.len().checked_add(chunk.len()) else {
            return studio_mesh_failure(
                "STUDIO_MESH_RESPONSE_TOO_LARGE",
                "Studio owner response exceeds the bounded mesh transport",
            );
        };
        if next_len > STUDIO_MESH_BODY_CAP {
            tracing::warn!(
                db = %id,
                observed_bytes = next_len,
                cap_bytes = STUDIO_MESH_BODY_CAP,
                "Studio owner response exceeded the bounded mesh transport"
            );
            return studio_mesh_failure(
                "STUDIO_MESH_RESPONSE_TOO_LARGE",
                "Studio owner response exceeds the bounded mesh transport",
            );
        }
        if body_bytes.try_reserve_exact(chunk.len()).is_err() {
            tracing::warn!(
                db = %id,
                requested_bytes = chunk.len(),
                cap_bytes = STUDIO_MESH_BODY_CAP,
                "Studio owner response buffer allocation failed"
            );
            return studio_mesh_failure(
                "STUDIO_MESH_BUFFER_UNAVAILABLE",
                "Studio owner response could not be buffered for the mesh transport",
            );
        }
        body_bytes.extend_from_slice(&chunk);
    }
    json!({
        "status": status,
        "headers": hdrs,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&body_bytes),
    })
}

async fn studio_mesh_route(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(env): axum::Json<Value>,
) -> axum::Json<Value> {
    axum::Json(studio_mesh_serve(&cloud, &id, &env).await)
}

/// `/v1/databases/:id/studio-mesh`, the owner-proxy mount mirroring `hrana::routes()`'s
/// `/hrana-mesh` (same `owner_routed` exemption in `main.rs`, same gossip dispatch-arm shape in
/// `gossip.rs`).
pub fn routes() -> axum::Router<Arc<CloudState>> {
    axum::Router::new().route(
        "/v1/databases/:id/studio-mesh",
        axum::routing::post(studio_mesh_route),
    )
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
