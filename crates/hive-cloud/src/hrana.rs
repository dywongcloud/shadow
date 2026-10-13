//! Hrana-over-HTTP server: the libsql wire protocol for the `sqlite` database kind, so
//! `@libsql/client` (and the Rust, Go and Python libsql clients) connect to a platform-managed
//! SQLite database with no adapter in between.
//!
//! # Addressing
//!
//! Two mount points serve the identical handler:
//!
//! * `https://<slug>.{HIVE_DB_DOMAIN}/v2/pipeline`: the per-database hostname every other managed
//!   engine already gets (wildcard ACME cert, per-DB A record pointing at the owning node). This is
//!   the DSN the platform publishes, as `libsql://<slug>.{db_domain}`.
//! * `https://api.{platform_domain}/v1/sqlite/<db_id>/v2/pipeline`: the same thing without a DB
//!   gateway domain, so the lane works on a node with `HIVE_DB_DOMAIN` unset. Its DSN is published
//!   WITH A TRAILING SLASH (`libsql://api.host/v1/sqlite/<id>/`) and that is load-bearing:
//!   `hrana-client-ts` resolves `v2/pipeline` RELATIVELY against the base URL (`new
//!   URL(pipelinePath, baseUrl)`), so a base without the trailing slash silently drops its last
//!   segment and posts to `/v1/sqlite/v2/pipeline`, a different database or none.
//!
//! The version path is NOT negotiated by the JS client: `hrana-client-ts` hardcodes `version: 2,
//! encoding: "json"` and never probes. Both `v2` and `v3` pipelines are therefore mounted, JSON
//! only, and the `v3-protobuf` probe is deliberately NOT served: answering it 2xx is the one thing
//! that would push a client onto the protobuf encoding this server does not speak.
//!
//! # Owner routing
//!
//! A SQLite database is a FILE on `Database::host_node`. Every other node PROXIES to that owner
//! (HTTP admin when known, else the iroh mesh) instead of opening a local file, which would
//! silently create a second, empty database and diverge forever. It is the shape of
//! `browser_artifacts::fetch_artifact_from_host`: the owner re-runs the credential check against
//! its own replicated record and refuses to re-proxy (`mesh_serve`). When the owner cannot be
//! reached the answer is an honest 421, never a locally invented empty database.
//!
//! Because every node proxies, all stream (baton) state lives on the owner, so an interactive
//! transaction spanning several POSTs is pinned to one connection by construction, whichever node
//! the round-robin DNS picked.
//!
//! # Auth
//!
//! The bearer is the database's own dedicated `DB_REST_TOKEN`, compared in constant time by
//! `db_rest::credential_matches` (the same function the Postgres/Redis REST surface uses). Scope is
//! exactly one database; there is no team-wide token on this surface and no tenant name is ever
//! read from the request.

use crate::databases::{Database, DbKind};
use crate::hrana_proto::{execute_pipeline, SqlCache};
use crate::musql::{Engine as DbEngine, MusqlEngine};
use crate::sqlite_pool::{PoolError, PooledConn};
use crate::state::CloudState;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// How long a stream may sit between requests (`HIVE_SQLITE_STREAM_IDLE_MS`, default 30 s, clamped
/// to 1-600 s) before it is reaped and its connection returned to the pool. An abandoned
/// interactive transaction otherwise holds a pooled connection (and, mid-`BEGIN`, the file's write
/// lock) forever.
fn stream_idle_ms() -> u64 {
    std::env::var("HIVE_SQLITE_STREAM_IDLE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30_000)
        .clamp(1_000, 600_000)
}

/// Max request body for a pipeline POST, ENFORCED TWICE ON PURPOSE because the two mounts differ in
/// where the bytes arrive. The routes carry `DefaultBodyLimit::max(BODY_CAP)` because axum's own 2
/// MiB default would otherwise reject first, so the documented cap is the enforced one. That limit
/// fires in the framework, before `serve` runs: an over-cap body gets 413 with axum's text/plain
/// message and no `code`, and it does so BEFORE the bearer is checked (an over-cap body with a
/// wrong token answers 413, not 401). The check in `serve` remains the chokepoint for every caller
/// that reaches the handler with bytes already in hand, notably `mesh_serve`, whose body arrives
/// base64 inside a JSON envelope and is checked after decoding.
const BODY_CAP: usize = 4 * 1024 * 1024;

/// The mesh hop carries the same bytes base64-encoded inside a JSON envelope, so its transport
/// limit has to clear the encoded size of a full-cap body (a third larger) or a pipeline the owner
/// would accept could not reach it.
const MESH_BODY_CAP: usize = BODY_CAP * 2;

/// One Hrana stream's connection to whichever engine this database runs on. Both arms execute the
/// SAME pipeline implementation (`hrana_proto::execute_pipeline`), so the wire contract cannot
/// drift between engines. They differ only in where the work happens: the rusqlite arm is moved
/// onto `spawn_blocking` (its engine is blocking file IO and must never run on a serving tokio
/// worker), the musql arm is awaited on the runtime (its engine is loopback HTTP).
enum EngineConn {
    Rusqlite(PooledConn),
    Musql(MusqlEngine),
}

impl EngineConn {
    /// Whether the stream can continue: a rusqlite stream asks its pooled connection, while a musql
    /// stream is live as long as it is checked out (the sidecar owns the underlying file handle and
    /// reports its own failures as request errors).
    fn is_live(&self) -> bool {
        match self {
            EngineConn::Rusqlite(c) => c.is_live(),
            EngineConn::Musql(_) => true,
        }
    }

    async fn pipeline(
        &mut self,
        requests: Vec<Value>,
        cache: SqlCache,
        version: u8,
    ) -> Result<crate::hrana_proto::ExecOutput, String> {
        match self {
            EngineConn::Rusqlite(c) => c
                .call_blocking(move |conn| {
                    Box::pin(execute_pipeline(conn, requests, cache, version, false))
                })
                .await
                .map_err(|e| e.to_string()),
            EngineConn::Musql(e) => Ok(execute_pipeline(e, requests, cache, version, false).await),
        }
    }

    /// Runs a cursor batch. On the rusqlite arm the closure must own everything the future borrows:
    /// it is `for<'a>` over the connection's lifetime, so a borrow of a local could not be shown to
    /// outlive it.
    async fn cursor(
        &mut self,
        batch: Value,
        cache: &SqlCache,
        version: u8,
    ) -> Result<Vec<Value>, String> {
        match self {
            EngineConn::Rusqlite(c) => {
                let cache = cache.clone();
                c.call_blocking(move |conn| {
                    let cache = cache.clone();
                    Box::pin(async move {
                        crate::hrana_proto::execute_cursor(conn, batch, &cache, version, false)
                            .await
                    })
                })
                .await
                .map_err(|e| e.to_string())
            }
            EngineConn::Musql(e) => {
                Ok(crate::hrana_proto::execute_cursor(e, batch, cache, version, false).await)
            }
        }
    }
}

struct Stream {
    conn: EngineConn,
    cache: SqlCache,
    db_id: String,
    last_ms: u64,
}

type Streams = Mutex<HashMap<String, Stream>>;

fn streams() -> &'static Streams {
    static S: OnceLock<Streams> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Unforgeable, single-use stream handle: 32 bytes from the OS CSPRNG, base64url-nopad. A client
/// can neither guess another stream's baton nor reuse its own: every response mints a fresh one and
/// retires the old. A CSPRNG failure yields an empty string, which `checkin` treats as a closed
/// stream, never a predictable baton.
fn mint_baton() -> String {
    use ring::rand::SecureRandom;
    static RNG: OnceLock<ring::rand::SystemRandom> = OnceLock::new();
    let mut buf = [0u8; 32];
    let rng = RNG.get_or_init(ring::rand::SystemRandom::new);
    if rng.fill(&mut buf).is_err() {
        return String::new();
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Reaps streams idle past the budget. Runs on every request rather than on a timer: a stream can
/// only be starved of capacity by traffic, and traffic is what runs this. The expired streams are
/// dropped outside the lock, because `PooledConn::drop` rolls back an open transaction and touches
/// the pool.
fn sweep() {
    let cutoff = hive_core::now_ms().saturating_sub(stream_idle_ms());
    let expired: Vec<Stream> = {
        let mut map = streams().lock();
        let keys: Vec<String> = map
            .iter()
            .filter(|(_, s)| s.last_ms < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        keys.iter().filter_map(|k| map.remove(k)).collect()
    };
    for s in expired {
        tracing::debug!(db = %s.db_id, "hrana stream expired; connection returned to pool");
    }
}

/// Drop every stream belonging to a database (on delete).
pub fn close_streams(db_id: &str) {
    let dropped: Vec<Stream> = {
        let mut map = streams().lock();
        let keys: Vec<String> = map
            .iter()
            .filter(|(_, s)| s.db_id == db_id)
            .map(|(k, _)| k.clone())
            .collect();
        keys.iter().filter_map(|k| map.remove(k)).collect()
    };
    drop(dropped);
}

/// Databases whose file is mid-conversion, keyed by database id. `close_streams` closes what is
/// ALREADY open, which is not enough: a client that dials in a second later reopens the file behind
/// the migration's back, and a write landing after `musql_migrate` snapshotted the source is lost
/// the moment the record flips. So the whole conversion holds this set and `checkout` REFUSES new
/// streams for it (refuses, never queues: a queued stream would open onto a file that has already
/// been replaced).
fn quiescing() -> &'static Mutex<std::collections::HashSet<String>> {
    static Q: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Held for the duration of a conversion. The set entry is removed by `Drop`, never on a success or
/// error branch: a conversion that panics or returns early must not leave a database refusing every
/// request forever.
pub struct ConversionGuard(String);

impl Drop for ConversionGuard {
    fn drop(&mut self) {
        quiescing().lock().remove(&self.0);
        tracing::info!(db = %self.0, "storage-engine conversion window closed");
    }
}

/// Refuse new streams on `db_id` until the returned guard is dropped.
pub fn begin_conversion(db_id: &str) -> ConversionGuard {
    quiescing().lock().insert(db_id.to_string());
    tracing::info!(db = %db_id, "storage-engine conversion window open; new Hrana streams refused");
    ConversionGuard(db_id.to_string())
}

/// Whether a conversion is holding `db_id`: while it is, `checkout` refuses to
/// open a new stream on that database.
pub fn is_quiescing(db_id: &str) -> bool {
    quiescing().lock().contains(db_id)
}

#[derive(Clone, Copy, PartialEq)]
enum Endpoint {
    Pipeline(u8),
    Cursor(u8),
    Probe(u8),
}

/// Maps method and path to an endpoint. The GET probes answer 2xx for each supported version (v2
/// and v3); `v3-protobuf` is deliberately not routed at all.
fn route(method: &Method, path: &str) -> Option<Endpoint> {
    let p = path.trim_end_matches('/');
    match (method, p) {
        (&Method::POST, "/v2/pipeline") => Some(Endpoint::Pipeline(2)),
        (&Method::POST, "/v3/pipeline") => Some(Endpoint::Pipeline(3)),
        (&Method::POST, "/v3/cursor") => Some(Endpoint::Cursor(3)),
        (&Method::GET, "/v2") => Some(Endpoint::Probe(2)),
        (&Method::GET, "/v3") => Some(Endpoint::Probe(3)),
        _ => None,
    }
}

/// Serves one Hrana request for `db`. `path` is the endpoint path RELATIVE to the database's base
/// URL (`/v2/pipeline`, `/v3`, ...); the two mount points normalise to it before calling. A
/// wrong-kind database answers BYTE-IDENTICALLY to `api_serve`'s unknown-id answer: a distinct
/// 'does not speak the libsql protocol' message would be readable WITHOUT any credential (the kind
/// check precedes the bearer check), letting an unauthenticated caller holding a leaked id confirm
/// both that the id existed and which engine it ran. The file lives on exactly one node: everyone
/// else proxies to the owner (`forward_to_owner`).
pub async fn serve(
    cloud: Arc<CloudState>,
    db: Database,
    bearer: String,
    method: Method,
    path: String,
    body: axum::body::Bytes,
) -> Response {
    if db.kind != DbKind::Sqlite {
        return err(StatusCode::NOT_FOUND, "no such database", None);
    }
    if !crate::db_rest::credential_matches(&db, &bearer) {
        let mut r = err(
            StatusCode::UNAUTHORIZED,
            "missing or invalid auth token for this database",
            Some("UNAUTHORIZED"),
        );
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return r;
    }
    let Some(endpoint) = route(&method, &path) else {
        return err(
            StatusCode::NOT_FOUND,
            "unsupported libsql endpoint — this server speaks Hrana v2/v3 over JSON \
             (POST v2/pipeline, POST v3/pipeline, POST v3/cursor)",
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

    if !is_owner(&cloud, &db) {
        return match forward_to_owner(&cloud, &db, &bearer, &path, &body).await {
            Some(r) => r,
            None => err(
                StatusCode::MISDIRECTED_REQUEST,
                "this database is hosted on another node and that node could not be reached",
                Some("HOST_UNREACHABLE"),
            ),
        };
    }
    serve_local(&cloud, &db, endpoint, &body).await
}

/// Whether this node hosts the database file. An empty `host_node` is a pre-sqlite-lane record
/// shape, and refusing to guess is the only safe answer: it counts as 'not us', so the proxy fails
/// honestly instead of creating an empty file here.
fn is_owner(cloud: &Arc<CloudState>, db: &Database) -> bool {
    !db.host_node.is_empty() && db.host_node == cloud.node_name
}

async fn serve_local(
    cloud: &Arc<CloudState>,
    db: &Database,
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
        Endpoint::Pipeline(v) => pipeline(cloud, db, v, body).await,
        Endpoint::Cursor(v) => cursor(cloud, db, v, body).await,
    }
}

/// Takes the stream named by `baton`, or opens a fresh one; while a conversion holds the database
/// (`begin_conversion`) it refuses with a retryable 503 `SQLITE_BUSY` instead of opening a
/// connection onto a database that is being replaced. A baton is single-use AND database-scoped:
/// another database's baton is put straight back and answered exactly as if it had never existed,
/// so batons cannot probe other databases' streams. Take-and-restore happens under ONE lock guard:
/// `match streams().lock().remove(b) { ... }` self-deadlocks, because the scrutinee's `MutexGuard`
/// temporary lives to the END of the match and re-locking inside an arm blocks a non-reentrant
/// `parking_lot::Mutex` against a guard this thread holds; every request starts with `sweep()` (the
/// same map), so the node's HTTP runtime would be consumed one wedged worker at a time. A musql
/// database whose engine is not serving is an honest 503, never an invented empty database or a
/// silent fall-through to rusqlite: those are different files, and serving one as the other is the
/// divergence owner routing exists to prevent.
async fn checkout(
    cloud: &Arc<CloudState>,
    db: &Database,
    baton: Option<&str>,
) -> Result<Stream, Response> {
    sweep();
    if is_quiescing(&db.id) {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this database is being converted between storage engines; retry shortly",
            Some("SQLITE_BUSY"),
        ));
    }
    if let Some(b) = baton {
        let taken = {
            let mut map = streams().lock();
            match map.remove(b) {
                Some(s) if s.db_id == db.id => Some(s),
                Some(other) => {
                    map.insert(b.to_string(), other);
                    None
                }
                None => None,
            }
        };
        return taken.ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "stream expired or was already used — open a new one (baton: null)",
                Some("STREAM_EXPIRED"),
            )
        });
    }
    let engine = crate::musql::engine_for_db(&db.connection);
    let conn = match engine {
        DbEngine::Musql => {
            let e = MusqlEngine::connect(cloud.http.clone(), &db.id).await.map_err(|e| {
                err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!("musql engine unavailable for this database: {e}"),
                    Some("SQLITE_BUSY"),
                )
            })?;
            EngineConn::Musql(e)
        }
        DbEngine::Rusqlite => {
            let Some(path) = crate::databases::sqlite_file_path(&db.id) else {
                return Err(err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "database id is not a platform-issued id",
                    None,
                ));
            };
            let pool = crate::sqlite_pool::pool_for(&db.id, &path);
            match pool.acquire().await {
                Ok(conn) => EngineConn::Rusqlite(conn),
                Err(PoolError::Busy) => {
                    return Err(err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &PoolError::Busy.to_string(),
                        Some("SQLITE_BUSY"),
                    ))
                }
                Err(e) => {
                    return Err(err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &e.to_string(),
                        Some("SQLITE_ERROR"),
                    ))
                }
            }
        }
    };
    Ok(Stream {
        conn,
        cache: SqlCache::new(),
        db_id: db.id.clone(),
        last_ms: hive_core::now_ms(),
    })
}

/// Re-inserts a live stream under a FRESH baton and returns it. `None` when the stream was closed
/// (or lost its connection), which the client reads as a null baton = stream closed.
fn checkin(mut stream: Stream) -> Option<String> {
    if !stream.conn.is_live() {
        return None;
    }
    let baton = mint_baton();
    if baton.is_empty() {
        return None;
    }
    stream.last_ms = hive_core::now_ms();
    streams().lock().insert(baton.clone(), stream);
    Some(baton)
}

/// Runs one pipeline POST on the stream named by the baton (or a fresh one). When the connection is
/// gone the stream cannot continue, so the whole pipeline fails (500 `SQLITE_ERROR`) rather than
/// pretending the requests ran. The response's `base_url` is `null`, which keeps the client on the
/// URL it already has: a node-specific absolute URL would pin the stream to one node's address, and
/// every node proxies to the owner instead.
async fn pipeline(cloud: &Arc<CloudState>, db: &Database, version: u8, body: &[u8]) -> Response {
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
    let baton = parsed
        .get("baton")
        .and_then(|b| b.as_str())
        .filter(|b| !b.is_empty())
        .map(|b| b.to_string());
    let requests: Vec<Value> = parsed
        .get("requests")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    let mut stream = match checkout(cloud, db, baton.as_deref()).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let cache = std::mem::take(&mut stream.cache);
    let out = stream.conn.pipeline(requests, cache, version).await;
    let (results, closed) = match out {
        Ok(o) => {
            stream.cache = o.cache;
            (o.results, o.closed)
        }
        Err(e) => {
            drop(stream);
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
                Some("SQLITE_ERROR"),
            );
        }
    };
    let next = if closed { None } else { checkin(stream) };
    ok_json(json!({
        "baton": next,
        "base_url": Value::Null,
        "results": results,
    }))
}

/// Runs one cursor POST. The response is newline-delimited JSON: a `CursorRespBody` line (`baton`,
/// `base_url`), then one line per entry.
async fn cursor(cloud: &Arc<CloudState>, db: &Database, version: u8, body: &[u8]) -> Response {
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
    let baton = parsed
        .get("baton")
        .and_then(|b| b.as_str())
        .filter(|b| !b.is_empty())
        .map(|b| b.to_string());
    let batch = parsed.get("batch").cloned().unwrap_or(Value::Null);

    let mut stream = match checkout(cloud, db, baton.as_deref()).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let out = stream.conn.cursor(batch, &stream.cache, version).await;
    let entries = match out {
        Ok(e) => e,
        Err(e) => {
            drop(stream);
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
                Some("SQLITE_ERROR"),
            );
        }
    };
    let next = checkin(stream);
    let mut out = serde_json::to_string(&json!({ "baton": next, "base_url": Value::Null }))
        .unwrap_or_default();
    for e in entries {
        out.push('\n');
        out.push_str(&e.to_string());
    }
    out.push('\n');
    cors(
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            out,
        )
            .into_response(),
    )
}

/// The internal envelope carried to the owning node. Bodies ride base64 so the
/// same envelope carries a JSON pipeline body and an NDJSON cursor response.
fn envelope(bearer: &str, path: &str, body: &[u8]) -> Value {
    json!({
        "path": path,
        "auth": bearer,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
    })
}

/// Proxies a request to the owning node: HTTP admin first (the same two-tier shape as
/// `admin::fetch_from_host`), the mesh gossip route as fallback. `None` when neither answered,
/// which the caller turns into the honest 421. Every failing exit is logged with its reason: an
/// unlogged `None` would surface to the client as a bare 421 with no way to tell 'no admin URL for
/// the owner' from 'the mesh dial failed', two problems with completely different fixes.
async fn forward_to_owner(
    cloud: &Arc<CloudState>,
    db: &Database,
    bearer: &str,
    path: &str,
    body: &[u8],
) -> Option<Response> {
    let node = db.host_node.clone();
    if node.is_empty() {
        tracing::warn!(
            db = %db.id,
            "hrana owner proxy: database record carries no host_node; cannot route"
        );
        return None;
    }
    let env = envelope(bearer, path, body);
    let route = format!("/v1/databases/{}/hrana-mesh", db.id);

    let mut why = String::new();

    let admin = cloud.node_admins.read().get(&node).cloned();
    if admin.is_none() {
        why.push_str("no HTTP admin URL known for owner; ");
    }
    if let Some(admin) = admin {
        let mut rb = cloud
            .http
            .post(format!("{admin}{route}"))
            .json(&env)
            .timeout(std::time::Duration::from_secs(30));
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
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    let Some(target) = target else {
        tracing::warn!(
            db = %db.id, owner = %node,
            reason = %why,
            "hrana owner proxy failed: owner has no gossip address (peer_id/iroh_addr) in this \
             node's registry, so the mesh fallback could not be attempted"
        );
        return None;
    };
    let payload = serde_json::to_vec(&env).ok()?;
    let bytes = match crate::gossip::request_to(
        cloud,
        &target.0,
        &target.1,
        hive_p2p::GOSSIP_POST,
        &route,
        &payload,
        30,
    )
    .await
    {
        Some(b) => b,
        None => {
            tracing::warn!(
                db = %db.id, owner = %node, peer_id = %target.0,
                reason = %why,
                "hrana owner proxy failed: mesh gossip request to the owner timed out or errored"
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

/// The owner half of the proxy: re-resolves the database from THIS node's own replicated record,
/// re-checks the presented credential, refuses to re-proxy, and serves locally. Shared by the HTTP
/// admin route and the gossip arm. A node that is not the owner either means the caller's routing
/// information is stale, and answering 421 honestly beats a proxy loop.
pub async fn mesh_serve(cloud: &Arc<CloudState>, id: &str, env: &Value) -> Value {
    let refuse = |status: StatusCode, msg: &str| {
        json!({
            "status": status.as_u16(),
            "content_type": "application/json",
            "body_b64": base64::engine::general_purpose::STANDARD
                .encode(json!({ "message": msg }).to_string()),
        })
    };
    let Some(db) = cloud.databases.get_raw(id) else {
        return refuse(StatusCode::NOT_FOUND, "no such database");
    };
    if db.kind != DbKind::Sqlite {
        return refuse(StatusCode::NOT_FOUND, "no such database");
    }
    let bearer = env.get("auth").and_then(|a| a.as_str()).unwrap_or_default();
    if !crate::db_rest::credential_matches(&db, bearer) {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "invalid auth token for this database",
        );
    }
    if !is_owner(cloud, &db) {
        return refuse(
            StatusCode::MISDIRECTED_REQUEST,
            "this node does not host that database",
        );
    }
    let path = env.get("path").and_then(|p| p.as_str()).unwrap_or_default();
    let Some(endpoint) = route(&Method::POST, path).or_else(|| route(&Method::GET, path)) else {
        return refuse(StatusCode::NOT_FOUND, "unsupported libsql endpoint");
    };
    let body = base64::engine::general_purpose::STANDARD
        .decode(env.get("body_b64").and_then(|b| b.as_str()).unwrap_or(""))
        .unwrap_or_default();
    if body.len() > BODY_CAP {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let resp = serve_local(cloud, &db, endpoint, &body).await;
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), BODY_CAP * 16)
        .await
        .unwrap_or_default();
    json!({
        "status": status,
        "content_type": ct,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

/// The routes of this lane: the `/v1/sqlite/:id/*` platform-API mount, the owner-proxy hop
/// `/v1/databases/:id/hrana-mesh`, and the operator routes for pool stats and storage-engine
/// switching. The `/v1/sqlite/:id/*` mount is deliberately NOT tenant-JWT gated: the database's own
/// token is the credential (the `db_rest` posture), and `auth::require_auth` exempts this prefix
/// for that reason. Its routes carry an OPTIONS arm because `@libsql/client` runs in browsers and
/// its POST (`authorization` + `content-type: application/json`) triggers a preflight (the
/// `<slug>.{db_domain}` mount gets it from `db_rest::handle`). The body cap is enforced by
/// `DefaultBodyLimit` where the bytes arrive (see `BODY_CAP`), and `add_cors` rides along because
/// the framework's over-cap rejection never reaches `cors()` inside a handler, which would leave a
/// browser client with an opaque network error instead of the 413.
pub fn routes() -> axum::Router<Arc<CloudState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/v1/sqlite/:id/v2/pipeline",
            post(api_pipeline_v2).options(preflight),
        )
        .route(
            "/v1/sqlite/:id/v3/pipeline",
            post(api_pipeline_v3).options(preflight),
        )
        .route(
            "/v1/sqlite/:id/v3/cursor",
            post(api_cursor_v3).options(preflight),
        )
        .route("/v1/sqlite/:id/v2", get(api_probe_v2).options(preflight))
        .route("/v1/sqlite/:id/v3", get(api_probe_v3).options(preflight))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_CAP))
        .layer(axum::middleware::map_response(add_cors))
        .route(
            "/v1/databases/:id/hrana-mesh",
            post(mesh_route).layer(axum::extract::DefaultBodyLimit::max(MESH_BODY_CAP)),
        )
        .route("/v1/databases/sqlite-pools", get(pool_stats))
        .route("/v1/databases/sqlite-engines", get(engine_stats))
        .route("/v1/databases/:id/sqlite-engine", post(set_engine_route))
        .route("/v1/databases/:id/sqlite-engine/plan", get(engine_plan_route))
}

/// Dry run: what a switch to musql WOULD do, including the blockers (`integrity_check` and the
/// non-finite REAL scan), so an operator can see a refusal before asking for the real thing.
async fn engine_plan_route(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Response {
    if let Err(e) = crate::admin::require_operator(claims.as_ref().map(|e| &e.0)) {
        return e.into_response();
    }
    let Some(db) = c.databases.get_raw(&id) else {
        return err(StatusCode::NOT_FOUND, "no such database", None);
    };
    if db.kind != DbKind::Sqlite {
        return err(StatusCode::NOT_FOUND, "no such database", None);
    }
    match crate::musql_migrate::plan(&id).await {
        Ok(v) => ok_json(v),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e, Some("SQLITE_ERROR")),
    }
}

/// Operator view of every managed SQLite database's storage engine on this node, plus the sidecar's
/// own state. Node-local like `sqlite-pools` (a pool and a sidecar are both things on one machine),
/// read per node.
async fn engine_stats(
    State(c): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::Json<Value>, (StatusCode, String)> {
    crate::admin::require_operator(claims.as_ref().map(|e| &e.0))?;
    let dbs: Vec<Value> = c
        .databases
        .snapshot()
        .into_iter()
        .filter(|d| d.kind == DbKind::Sqlite)
        .map(|d| {
            json!({
                "id": d.id,
                "engine": crate::musql::engine_for_db(&d.connection).label(),
                "host_node": d.host_node,
                "owned_here": d.host_node == c.node_name,
                "file": d.connection.get("file").cloned().unwrap_or_default(),
            })
        })
        .collect();
    Ok(axum::Json(json!({
        "node": c.node_name,
        "sidecar": crate::musql::status_json(),
        "databases": dbs,
    })))
}

/// Switches one managed SQLite database's engine, with the conversion VERIFIED before the record
/// moves. `{"engine":"musql"}` converts and checks row-for-row; `{"engine":"rusqlite"}` exports
/// back and checks the same way. The record flips (`set_engine`) only after a VERIFIED conversion,
/// since `migrate` and `rollback` both refuse to return `Ok` with a divergent table; a `.musq`
/// staged by an earlier verified run is moved aside rather than blocking the flip (`live_is_musql`
/// is false here by construction, because an equal target returned earlier).
///
/// Owner-only by design: the file lives on one node and a non-owner has nothing to convert. It
/// answers 421 rather than proxying: this is an operator action, not tenant traffic, and an honest
/// refusal beats a silent no-op.
async fn set_engine_route(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if let Err(e) = crate::admin::require_operator(claims.as_ref().map(|e| &e.0)) {
        return e.into_response();
    }
    let Some(db) = c.databases.get_raw(&id) else {
        return err(StatusCode::NOT_FOUND, "no such database", None);
    };
    if db.kind != DbKind::Sqlite {
        return err(StatusCode::NOT_FOUND, "no such database", None);
    }
    if !is_owner(&c, &db) {
        return err(
            StatusCode::MISDIRECTED_REQUEST,
            "this database is hosted on another node; run the engine switch there",
            Some("HOST_UNREACHABLE"),
        );
    }
    let want = body.get("engine").and_then(|v| v.as_str()).unwrap_or("");
    let target = match want {
        "musql" => DbEngine::Musql,
        "rusqlite" => DbEngine::Rusqlite,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "engine must be \"musql\" or \"rusqlite\"",
                None,
            )
        }
    };
    let current = crate::musql::engine_for_db(&db.connection);
    if current == target {
        return ok_json(json!({ "db_id": id, "engine": target.label(), "changed": false }));
    }
    let result = if target == DbEngine::Musql {
        crate::musql_migrate::migrate(&id, current == DbEngine::Musql).await
    } else {
        crate::musql_migrate::rollback(&id).await
    };
    match result {
        Ok(report) => {
            c.databases.set_engine(&id, target);
            crate::hrana::close_streams(&id);
            crate::sqlite_pool::close(&id);
            ok_json(json!({ "changed": true, "report": report.json() }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e, Some("SQLITE_ERROR")),
    }
}

/// Operator view of this NODE's live SQLite pools and open streams. Node-local by nature (a pool is
/// a set of open file handles on one machine), so it is deliberately not aggregated: read it per
/// node, the way `/v1/dns/stats` is read per node.
async fn pool_stats(
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::Json<Value>, (StatusCode, String)> {
    crate::admin::require_operator(claims.as_ref().map(|e| &e.0))?;
    let streams = streams().lock();
    let mut per_db: HashMap<String, usize> = HashMap::new();
    for s in streams.values() {
        *per_db.entry(s.db_id.clone()).or_default() += 1;
    }
    drop(streams);
    Ok(axum::Json(json!({
        "pools": crate::sqlite_pool::stats(),
        "open_streams": per_db,
        "stream_idle_ms": stream_idle_ms(),
    })))
}

async fn api_pipeline_v2(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    api_serve(c, id, headers, Method::POST, "/v2/pipeline", body).await
}
async fn api_pipeline_v3(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    api_serve(c, id, headers, Method::POST, "/v3/pipeline", body).await
}
async fn api_cursor_v3(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    api_serve(c, id, headers, Method::POST, "/v3/cursor", body).await
}
async fn api_probe_v2(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    api_serve(c, id, headers, Method::GET, "/v2", axum::body::Bytes::new()).await
}
async fn api_probe_v3(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    api_serve(c, id, headers, Method::GET, "/v3", axum::body::Bytes::new()).await
}

async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

/// `cors` as a response-mapping middleware, so a response produced OUTSIDE the handlers (the
/// framework's over-cap 413) still carries the headers the browser preflight was promised.
async fn add_cors(resp: Response) -> Response {
    cors(resp)
}

/// Shared handler of the `/v1/sqlite/:id/*` routes: extracts the bearer and calls `serve`. An
/// unknown id and a wrong-kind id get the identical answer, so no existence leaks across tenants
/// (the `browser_artifacts::resolve_for_tenant` posture).
async fn api_serve(
    cloud: Arc<CloudState>,
    id: String,
    headers: HeaderMap,
    method: Method,
    path: &str,
    body: axum::body::Bytes,
) -> Response {
    let Some(db) = cloud.databases.get_raw(&id) else {
        return err(StatusCode::NOT_FOUND, "no such database", None);
    };
    let bearer = crate::db_rest::bearer_token(&headers).unwrap_or_default();
    serve(cloud, db, bearer, method, path.to_string(), body).await
}

async fn mesh_route(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    axum::Json(env): axum::Json<Value>,
) -> axum::Json<Value> {
    axum::Json(mesh_serve(&c, &id, &env).await)
}

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

/// Hrana's HTTP error body is `{message, code}` — clients surface `message`
/// verbatim, so it must say what actually went wrong.
fn err(code: StatusCode, msg: &str, hrana_code: Option<&str>) -> Response {
    let body = match hrana_code {
        Some(c) => json!({ "message": msg, "code": c }),
        None => json!({ "message": msg }),
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
        header::HeaderValue::from_static("authorization, content-type, x-turso-encryption-key"),
    );
    resp
}
