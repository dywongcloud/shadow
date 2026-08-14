//! Hrana-over-HTTP server — the libsql wire protocol for the `sqlite` database
//! kind, so `@libsql/client` (and the Rust/Go/Python libsql clients) connect to
//! a platform-managed SQLite database with no adapter in between.
//!
//! # Addressing
//!
//! Two mount points, both serving the identical handler:
//!
//! * `https://<slug>.{HIVE_DB_DOMAIN}/v2/pipeline` — the per-database hostname
//!   every other managed engine already gets (wildcard ACME cert, per-DB A
//!   record pointing at the owning node). This is the DSN the platform
//!   publishes, as `libsql://<slug>.{db_domain}`.
//! * `https://api.{platform_domain}/v1/sqlite/<db_id>/v2/pipeline` — the same
//!   thing without a DB gateway domain, so the lane works on a node with
//!   `HIVE_DB_DOMAIN` unset. Its DSN is published WITH A TRAILING SLASH
//!   (`libsql://api.host/v1/sqlite/<id>/`) and that is load-bearing:
//!   `hrana-client-ts` resolves `v2/pipeline` RELATIVELY against the base URL
//!   (`new URL(pipelinePath, baseUrl)`), so a base without the trailing slash
//!   silently drops its last segment and posts to `/v1/sqlite/v2/pipeline` —
//!   a different database, or none.
//!
//! The version path is NOT negotiated by the JS client: `hrana-client-ts`
//! hardcodes `version: 2, encoding: "json"` and never probes. Both `v2` and
//! `v3` pipelines are therefore mounted, JSON only, and the `v3-protobuf`
//! probe is deliberately NOT served — answering it 2xx is the one thing that
//! would push a client onto the protobuf encoding this server does not speak.
//!
//! # Owner routing
//!
//! A SQLite database is a FILE on `Database::host_node`. Every other node
//! PROXIES to that owner (HTTP admin when known, else the iroh mesh) instead of
//! opening a local file — opening one would silently create a second, empty
//! database and diverge forever. This is the `proxy_to_owner` /
//! `fetch_artifact_from_host` shape: the owner re-runs the credential check
//! against its own replicated record, and a `local` marker in the envelope
//! stops it re-proxying. When the owner cannot be reached the answer is an
//! honest 421, never a locally-invented empty database.
//!
//! Because every node proxies, all stream (baton) state lives on the owner —
//! so an interactive transaction spanning several POSTs is pinned to one
//! connection by construction, whichever node the round-robin DNS picked.
//!
//! # Auth
//!
//! The bearer is the database's own dedicated `DB_REST_TOKEN`, compared
//! constant-time by `db_rest::credential_matches` (the same function the
//! Postgres/Redis REST surface uses). Scope is exactly one database; there is
//! no team-wide token on this surface and no tenant name is ever read from the
//! request.

use crate::databases::{Database, DbKind};
use crate::hrana_proto::{execute_pipeline, SqlCache};
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

/// How long a stream may sit between requests before it is reaped and its
/// connection returned to the pool. An abandoned interactive transaction
/// otherwise holds a pooled connection (and, mid-`BEGIN`, the file's write
/// lock) forever.
fn stream_idle_ms() -> u64 {
    std::env::var("HIVE_SQLITE_STREAM_IDLE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30_000)
        .clamp(1_000, 600_000)
}

/// Max request body for a pipeline POST.
const BODY_CAP: usize = 4 * 1024 * 1024;

struct Stream {
    conn: PooledConn,
    cache: SqlCache,
    db_id: String,
    last_ms: u64,
}

type Streams = Mutex<HashMap<String, Stream>>;

fn streams() -> &'static Streams {
    static S: OnceLock<Streams> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Unforgeable, single-use stream handle: 32 bytes from the OS CSPRNG,
/// base64url-nopad. A client can neither guess another stream's baton nor
/// reuse its own — every response mints a fresh one and retires the old.
fn mint_baton() -> String {
    use ring::rand::SecureRandom;
    static RNG: OnceLock<ring::rand::SystemRandom> = OnceLock::new();
    let mut buf = [0u8; 32];
    let rng = RNG.get_or_init(ring::rand::SystemRandom::new);
    // A CSPRNG failure must not silently downgrade to a predictable baton.
    if rng.fill(&mut buf).is_err() {
        return String::new();
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Reap streams idle past the budget. Runs on every request rather than on a
/// timer: a stream can only be starved of capacity by traffic, and traffic is
/// what runs this.
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
    // Drop outside the lock: `PooledConn::drop` rolls back an open transaction
    // and touches the pool.
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

// ---- HTTP surface ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Endpoint {
    Pipeline(u8),
    Cursor(u8),
    Probe(u8),
}

fn route(method: &Method, path: &str) -> Option<Endpoint> {
    let p = path.trim_end_matches('/');
    match (method, p) {
        (&Method::POST, "/v2/pipeline") => Some(Endpoint::Pipeline(2)),
        (&Method::POST, "/v3/pipeline") => Some(Endpoint::Pipeline(3)),
        (&Method::POST, "/v3/cursor") => Some(Endpoint::Cursor(3)),
        // Capability probes: 2xx means "this version is supported". v2 and v3
        // are; `v3-protobuf` deliberately is not routed at all.
        (&Method::GET, "/v2") => Some(Endpoint::Probe(2)),
        (&Method::GET, "/v3") => Some(Endpoint::Probe(3)),
        _ => None,
    }
}

/// Serve one Hrana request for `db`. `path` is the endpoint path RELATIVE to
/// the database's base URL (`/v2/pipeline`, `/v3`, …) — the two mount points
/// normalise to it before calling.
pub async fn serve(
    cloud: Arc<CloudState>,
    db: Database,
    bearer: String,
    method: Method,
    path: String,
    body: axum::body::Bytes,
) -> Response {
    if db.kind != DbKind::Sqlite {
        // BYTE-IDENTICAL to `api_serve`'s unknown-id answer. A distinct
        // "does not speak the libsql protocol" message here was readable
        // WITHOUT any credential (the kind check precedes the bearer check), so
        // an unauthenticated caller holding a leaked id could confirm both that
        // the id existed and which engine it ran — the existence leak this
        // surface documents as closed.
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

    // OWNER ROUTING. The file lives on exactly one node; everyone else proxies.
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

fn is_owner(cloud: &Arc<CloudState>, db: &Database) -> bool {
    // An empty `host_node` is a pre-sqlite-lane record shape; refusing to guess
    // is the only safe answer, so treat it as "not us" and let the proxy fail
    // honestly rather than create an empty file here.
    !db.host_node.is_empty() && db.host_node == cloud.node_name
}

async fn serve_local(
    cloud: &Arc<CloudState>,
    db: &Database,
    endpoint: Endpoint,
    body: &[u8],
) -> Response {
    let _ = cloud;
    match endpoint {
        Endpoint::Probe(v) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({ "version": v, "encoding": "json" }).to_string(),
        )
            .into_response(),
        Endpoint::Pipeline(v) => pipeline(db, v, body).await,
        Endpoint::Cursor(v) => cursor(db, v, body).await,
    }
}

/// Take the stream named by `baton`, or open a fresh one. Returns the stream
/// plus whether it was newly opened (only used for logging).
async fn checkout(db: &Database, baton: Option<&str>) -> Result<Stream, Response> {
    sweep();
    if let Some(b) = baton {
        // ONE guard covers take-and-restore. Writing this as
        // `match streams().lock().remove(b) { … }` is a self-deadlock: the
        // scrutinee's `MutexGuard` temporary lives until the END of the match,
        // so re-locking inside an arm blocks a non-reentrant
        // `parking_lot::Mutex` against a guard this very thread holds. It never
        // unblocks, the guard is never dropped, and because EVERY request
        // starts with `sweep()` (which locks the same map) the whole node's
        // HTTP runtime is consumed one wedged worker at a time — witnessed
        // live: one cross-database baton took down `/healthz` and every other
        // route on the node, permanently.
        let taken = {
            let mut map = streams().lock();
            match map.remove(b) {
                // A baton is single-use AND database-scoped: presenting another
                // database's baton must not reach this database's connection.
                Some(s) if s.db_id == db.id => Some(s),
                Some(other) => {
                    // Belongs to a different database — put it straight back
                    // under the guard we already hold, and answer exactly as if
                    // it had never existed (below), so a baton cannot be used to
                    // probe for live streams on another database.
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
    let Some(path) = crate::databases::sqlite_file_path(&db.id) else {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "database id is not a platform-issued id",
            None,
        ));
    };
    let pool = crate::sqlite_pool::pool_for(&db.id, &path);
    match pool.acquire().await {
        Ok(conn) => Ok(Stream {
            conn,
            cache: SqlCache::new(),
            db_id: db.id.clone(),
            last_ms: hive_core::now_ms(),
        }),
        Err(PoolError::Busy) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            &PoolError::Busy.to_string(),
            Some("SQLITE_BUSY"),
        )),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
            Some("SQLITE_ERROR"),
        )),
    }
}

/// Re-insert a live stream under a FRESH baton and return it. `None` when the
/// stream was closed (or lost its connection), which the client reads as a
/// null baton = stream closed.
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

async fn pipeline(db: &Database, version: u8, body: &[u8]) -> Response {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}"), None),
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

    let mut stream = match checkout(db, baton.as_deref()).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let cache = std::mem::take(&mut stream.cache);
    let out = stream
        .conn
        .call(move |c| execute_pipeline(c, requests, cache, version, false))
        .await;
    let (results, closed) = match out {
        Ok(o) => {
            stream.cache = o.cache;
            (o.results, o.closed)
        }
        Err(e) => {
            // The connection is gone; the stream cannot continue. Report it as
            // a whole-pipeline failure rather than pretending the requests ran.
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
        // `null` keeps the client on the URL it already has. Handing out a
        // node-specific absolute URL would pin the stream to one node's
        // address; every node proxies to the owner instead.
        "base_url": Value::Null,
        "results": results,
    }))
}

async fn cursor(db: &Database, version: u8, body: &[u8]) -> Response {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}"), None),
    };
    let baton = parsed
        .get("baton")
        .and_then(|b| b.as_str())
        .filter(|b| !b.is_empty())
        .map(|b| b.to_string());
    let batch = parsed.get("batch").cloned().unwrap_or(Value::Null);

    let mut stream = match checkout(db, baton.as_deref()).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let cache = stream.cache.clone();
    let out = stream
        .conn
        .call(move |c| crate::hrana_proto::execute_cursor(c, batch, &cache, version, false))
        .await;
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
    // The cursor response is newline-delimited JSON: a `CursorRespBody` line,
    // then one line per entry.
    let mut out = serde_json::to_string(&json!({ "baton": next, "base_url": Value::Null }))
        .unwrap_or_default();
    for e in entries {
        out.push('\n');
        out.push_str(&e.to_string());
    }
    out.push('\n');
    cors((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        out,
    )
        .into_response())
}

// ---- Owner proxy ----------------------------------------------------------

/// The internal envelope carried to the owning node. Bodies ride base64 so the
/// same envelope carries a JSON pipeline body and an NDJSON cursor response.
fn envelope(bearer: &str, path: &str, body: &[u8]) -> Value {
    json!({
        "path": path,
        "auth": bearer,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
    })
}

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

    // Every exit below is logged. An unlogged `None` here surfaces to the
    // client as a bare 421 with no way to tell "no admin URL for the owner"
    // from "the mesh dial failed" — the two have completely different fixes,
    // and diagnosing it from the outside means reading this function's source.
    let mut why = String::new();

    // HTTP admin first (same two-tier shape as `admin::fetch_from_host`).
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
            if let Ok(tok) = crate::auth::issue("mesh-internal", "", "service", false, 60) {
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

    // Mesh fallback.
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

/// The owner half of the proxy: re-resolve the database from THIS node's own
/// replicated record, re-check the presented credential, refuse to re-proxy,
/// and serve locally. Shared by the HTTP admin route and the gossip arm.
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
        return refuse(StatusCode::UNAUTHORIZED, "invalid auth token for this database");
    }
    // NO RE-PROXY: if this node is not the owner either, the caller's routing
    // information is stale. Answering honestly beats a proxy loop.
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

// ---- Route wiring ---------------------------------------------------------

/// `/v1/sqlite/:id/*` — the platform-API mount. Deliberately NOT tenant-JWT
/// gated: the database's own token is the credential (the `db_rest` posture),
/// and `auth::require_auth` exempts this prefix for exactly that reason.
pub fn routes() -> axum::Router<Arc<CloudState>> {
    use axum::routing::{get, post};
    // `@libsql/client` runs in browsers too, and its POST carries
    // `authorization` + `content-type: application/json` — both preflight
    // triggers. Without an OPTIONS arm the router answers 405 and the browser
    // never sends the real request, so the preflight is part of the endpoint,
    // not an extra. (The `<slug>.{db_domain}` mount already gets this from
    // `db_rest::handle`'s own OPTIONS short-circuit.)
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
        .route("/v1/databases/:id/hrana-mesh", post(mesh_route))
        .route("/v1/databases/sqlite-pools", get(pool_stats))
}

/// Operator view of this NODE's live SQLite pools + open streams. Node-local by
/// nature (a pool is a set of open file handles on one machine), so it is
/// deliberately not aggregated — read it per node, the way `/v1/dns/stats` is
/// read per node.
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
    api_serve(
        c,
        id,
        headers,
        Method::GET,
        "/v2",
        axum::body::Bytes::new(),
    )
    .await
}
async fn api_probe_v3(
    State(c): State<Arc<CloudState>>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    api_serve(
        c,
        id,
        headers,
        Method::GET,
        "/v3",
        axum::body::Bytes::new(),
    )
    .await
}

async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn api_serve(
    cloud: Arc<CloudState>,
    id: String,
    headers: HeaderMap,
    method: Method,
    path: &str,
    body: axum::body::Bytes,
) -> Response {
    let Some(db) = cloud.databases.get_raw(&id) else {
        // Unknown id and wrong-kind id are the identical answer — no existence
        // leak across tenants (the `browser_artifacts::resolve_for_tenant`
        // posture).
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

// ---- Response helpers -----------------------------------------------------

fn ok_json(v: Value) -> Response {
    cors((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response())
}

/// Hrana's HTTP error body is `{message, code}` — clients surface `message`
/// verbatim, so it must say what actually went wrong.
fn err(code: StatusCode, msg: &str, hrana_code: Option<&str>) -> Response {
    let body = match hrana_code {
        Some(c) => json!({ "message": msg, "code": c }),
        None => json!({ "message": msg }),
    };
    cors((
        code,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response())
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
