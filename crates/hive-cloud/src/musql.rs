//! musql: the managed-SQLite storage engine, reached as a loopback Hrana
//! sidecar behind Hive's own Hrana handler.
//!
//! # Why a sidecar and not a crate
//!
//! musql is Go. It has no C ABI, no Rust crate, and its `GOOS=js` wasm needs a
//! JS host, so it cannot be linked into this workspace and cannot run under
//! wasmtime. The only shape that works is the one here: **tenants still talk
//! only to Hive's Rust Hrana handler, and `musqld` is the storage engine behind
//! it**, on loopback, never exposed.
//!
//! # What is deliberately NOT musql's job
//!
//! musqld has exactly ONE global `-auth-token` and no per-database concept, so
//! per-database auth stays in Rust (`db_rest::credential_matches`), and no
//! `-auth-token` is passed at all — a token that cannot express the scope we
//! enforce is worse than no token, because it looks like a second boundary
//! while enforcing a weaker one. Owner routing, the owner bearer re-check, the
//! honest 421, batons, the 4 MiB body cap and the whole wire codec likewise
//! stay in Rust; musqld answers one database at a time, selected by `Host`.
//!
//! # Two rules proved by measurement, not by reading docs
//!
//! * **Never run with `-create`.** With it, an unknown name silently creates a
//!   147-byte empty database and serves it — a missing file becoming an empty
//!   database is exactly the divergence the SQLite lane's owner-routing was
//!   written to prevent. Without it, a missing file is a clean 404
//!   `{"message":"no such database"}`, which this module maps onto Hive's own
//!   byte-identical answer.
//! * **One writer per file.** Two musqld instances on the same `.musq` both
//!   bound and both answered 200 in testing; upstream disclaims even a mixed
//!   C/musql pair. This supervisor is therefore the only process that may start
//!   musqld, and `crate::musql_migrate` quiesces the file before converting.
//!
//! # Envelope discipline
//!
//! musqld's JSON never reaches a client. Its error codes
//! (`SQLITE_CONSTRAINT_UNIQUE`, `HRANA_PROTO_ERROR`, …) are normalised to
//! `SQLITE_ERROR`, its `base_url` is dropped, its `baton` is kept internal (it
//! hands out a fresh one on EVERY response, which would cost every client an
//! extra round trip), and its constant `rows_read: 0` is replaced with the rows
//! actually stepped — which is why the engine is asked for rows even when the
//! caller did not want them (see [`MusqlEngine::run`]).
//!
//! # Four measured divergences from SQLite, and what was decided about each
//!
//! * **`rows_read` with `want_rows: false`.** musqld returns no row bodies, so
//!   counting what came back reported 0 for a SELECT that stepped rows. Fixed:
//!   the engine is always asked for rows and the count is taken from the step,
//!   matching the rusqlite engine byte for byte.
//! * **A non-finite REAL.** musqld's row encoder has no representation for
//!   `Inf`/`-Inf`/`NaN` and emits `{"type":"null"}`, indistinguishable from a
//!   real NULL — yet `SELECT 1e308*10 IS NULL` answers 0 and
//!   `CAST(1e308*10 AS TEXT)` answers `Inf`, so the engine still knows. Fixed:
//!   a NULL in a REAL-capable column is re-checked against the engine and
//!   turned into the same loud error `hrana_proto::encode_value` raises, rather
//!   than handed to a client as a NULL it never wrote.
//! * **Negative zero.** musqld normalises a STORED `-0.0` to `0.0` (measured on
//!   the live sidecar: `INSERT INTO t VALUES(-0.0)` then `SELECT x` answers
//!   `{"type":"float","value":0}`; a literal `SELECT -0.0` is preserved).
//!   ACCEPTED, NOT FIXED: `-0.0 = 0.0` is true in SQL, the two sort, compare
//!   and sum identically, and the only observable difference is their TEXT
//!   rendering (`CAST(x AS TEXT)`), which nothing in this lane reads. Closing
//!   it would mean re-encoding every REAL on the way in and out.
//! * **Error message text.** `SELECT FROM` is `near "FROM": syntax error` on
//!   SQLite and `no such column: FROM` on musqld; both are `SQLITE_ERROR`.
//!   Decided: message text is NOT part of the wire contract (clients match on
//!   the code and the envelope, and `hrana_proto` re-tags every code to
//!   `SQLITE_ERROR` regardless), so musqld's more specific text is passed
//!   through unchanged instead of being rewritten to imitate SQLite's parser.

use crate::sqlite_engine::{Budget, Col, DescribeResult, NamedArg, RunResult, SqlEngine, SqlValue};
use async_trait::async_trait;
// `as _`: this module defines its own `Engine` enum; only the trait's
// methods are needed here.
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// ---- Configuration ---------------------------------------------------------

/// The engine used for managed SQLite databases. Default `rusqlite`, so a roll
/// of this code changes nothing until an operator says so — see
/// [`engine_for_db`].
pub fn default_engine() -> Engine {
    match std::env::var("HIVE_SQLITE_ENGINE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "musql" => Engine::Musql,
        _ => Engine::Rusqlite,
    }
}

/// Which engine one database runs on. A per-database `engine` value wins over
/// the node default, so migration is a per-database roll, not a flag day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Rusqlite,
    Musql,
}

impl Engine {
    pub fn label(&self) -> &'static str {
        match self {
            Engine::Rusqlite => "rusqlite",
            Engine::Musql => "musql",
        }
    }
}

/// Per-database override, read from the `engine` key the provisioner writes into
/// the database record's `connection` map.
///
/// **Every value other than `musql` resolves to [`Engine::Rusqlite`], never to
/// [`default_engine`].** That is the whole point of this function and it is a
/// correctness property, not a style choice: the pre-change provisioner wrote
/// `engine: "sqlite"`, so a `_ => default_engine()` arm silently re-routed
/// EVERY existing database — a `.db` file, never converted, never verified —
/// onto musql the moment an operator set `HIVE_SQLITE_ENGINE=musql`, and where
/// no `.musq` existed the database answered "no such database" instead of its
/// data. An unverified record must not be able to become musql by env var
/// alone; the only thing that makes a database musql is the explicit,
/// verified-after-the-fact flip in `musql_migrate`.
pub fn engine_for_db(connection: &HashMap<String, String>) -> Engine {
    let v = connection
        .get("engine")
        .map(|s| s.trim().to_ascii_lowercase());
    match v.as_deref() {
        Some("musql") => Engine::Musql,
        // "rusqlite", the legacy "sqlite", a record with no `engine` key at
        // all, and every unknown spelling: a rusqlite file by construction, and
        // it stays one until it is converted and proven.
        _ => Engine::Rusqlite,
    }
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Where the sidecar listens. Loopback only — this is an engine, not a service,
/// and AGENTS.md's "do not open new inbound ports" applies to the world, not to
/// a process talking to itself on 127.0.0.1.
fn listen_addr() -> String {
    env_str("HIVE_MUSQL_LISTEN", "127.0.0.1:8099")
}

/// musqld's stream idle timeout. MUST exceed Hive's own `HIVE_SQLITE_STREAM_IDLE_MS`
/// (up to 600 s) or an interactive transaction is killed mid-stream by the
/// engine while Hive still believes the stream is live.
fn idle_timeout() -> String {
    env_str("HIVE_MUSQL_IDLE_TIMEOUT", "900s")
}

pub fn musqld_bin() -> String {
    env_str("HIVE_MUSQLD_BIN", "musqld")
}

/// Concurrent musql streams per database. A musqld stream is the analogue of a
/// pooled SQLite connection, so it gets the same bound for the same reason:
/// without one, every Hrana request would open an unbounded number of engine
/// streams and a slow query would be able to multiply without limit. It is
/// per-DATABASE, exactly like `sqlite_pool`'s semaphore.
fn max_streams() -> usize {
    std::env::var("HIVE_SQLITE_POOL_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(8)
        .clamp(1, 256)
}

fn stream_wait_ms() -> u64 {
    std::env::var("HIVE_SQLITE_POOL_WAIT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(10_000)
        .clamp(100, 120_000)
}

pub fn convert_bin() -> String {
    env_str("HIVE_MUSQL_CONVERT_BIN", "musql-convert")
}

// ---- Capability probe ------------------------------------------------------

/// Probed, never assumed: does this host actually have a usable musqld? Run at
/// boot and advertised on `NodeInfo` like `wasm_runtime`/`bun_runtime`, so
/// placement and the migration planner can see it without an operator SSHing in.
/// `None` here means NOT REPORTED, which this lane treats as NOT CAPABLE — the
/// same asymmetry `wasm_runtime` uses, and for the same reason: a node that
/// cannot run the engine must never be handed a `musql` database.
pub fn probe() -> Option<bool> {
    let bin = musqld_bin();
    let out = std::process::Command::new(&bin).arg("-h").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    // `-h` prints the flag list to stderr and exits 0; the marker that this is
    // really musqld (and not some other `musqld` on PATH) is `-dir`, which is
    // the mode this lane depends on.
    Some(text.contains("-dir") && text.contains("-idle-timeout"))
}

// ---- Sidecar ---------------------------------------------------------------

pub struct Sidecar {
    pub base: String,
    ready: AtomicBool,
    restarts: AtomicU64,
}

static SIDECAR: OnceLock<Option<Arc<Sidecar>>> = OnceLock::new();

/// The sidecar, if this node runs one. `None` means musql is not in use here.
pub fn sidecar() -> Option<&'static Arc<Sidecar>> {
    SIDECAR.get().and_then(|s| s.as_ref())
}

/// Start (once) and supervise the sidecar. Returns whether musql is usable on
/// this node; a node with no musqld binary simply reports false and every
/// database stays on rusqlite.
pub fn spawn() -> bool {
    if default_engine() != Engine::Musql {
        let _ = SIDECAR.set(None);
        return false;
    }
    let bin = musqld_bin();
    if probe() != Some(true) {
        tracing::warn!(bin = %bin, "HIVE_SQLITE_ENGINE=musql but no usable musqld found; the SQLite lane stays on rusqlite");
        let _ = SIDECAR.set(None);
        return false;
    }
    let dir = crate::databases::sqlite_store_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot create the SQLite store directory; musql disabled");
        let _ = SIDECAR.set(None);
        return false;
    }
    let addr = listen_addr();
    let sc = Arc::new(Sidecar {
        base: format!("http://{addr}"),
        ready: AtomicBool::new(false),
        restarts: AtomicU64::new(0),
    });
    let _ = SIDECAR.set(Some(sc.clone()));
    tokio::spawn(supervise(sc, bin, dir, addr));
    true
}

/// One supervised musqld: run it, mark it ready once it answers, and restart it
/// with backoff if it ever exits. A sidecar that dies silently would turn every
/// `musql` database into a 500 with no operator-visible cause.
async fn supervise(sc: Arc<Sidecar>, bin: String, dir: std::path::PathBuf, addr: String) {
    let idle = idle_timeout();
    let mut backoff = Duration::from_millis(500);
    loop {
        sc.ready.store(false, Ordering::SeqCst);
        let child = tokio::process::Command::new(&bin)
            .arg("-dir")
            .arg(&dir)
            .arg("-listen")
            .arg(&addr)
            .arg("-idle-timeout")
            .arg(&idle)
            // NO `-create`: an unknown name must 404, never become an empty
            // database. Provisioning creates the file on purpose instead.
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(bin = %bin, error = %e, "musqld failed to start");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        if wait_ready(&sc.base).await {
            // The probe proves only that SOMETHING answered on the port — an
            // ORPHANED musqld from a previous boot answers it identically, and
            // one was witnessed live: it survived its parent's shutdown, kept
            // holding 127.0.0.1:8099, and every supervised child afterwards
            // died on `bind: address already in use` while the readiness probe
            // still saw 404s and reported ready. Serving tenant databases
            // through that process would hand them to a writer this node does
            // not supervise — exactly the "one writer per file" property the
            // module doc stakes out. So readiness is gated on OUR child still
            // being alive at the moment the probe succeeded.
            match child.try_wait() {
                Ok(None) => {
                    sc.ready.store(true, Ordering::SeqCst);
                    backoff = Duration::from_millis(500);
                    tracing::info!(addr = %addr, dir = %dir.display(), "musqld sidecar ready");
                }
                other => {
                    // Another process owns the port (or musqld died on startup).
                    // Stay NOT ready: an honest 503 beats serving a foreign
                    // writer, and the restart counter makes it operator-visible.
                    tracing::error!(
                        addr = %addr,
                        status = ?other,
                        "musqld is not the process serving this port (another musqld already \
                         holds it, or it exited on startup); the SQLite lane stays unready"
                    );
                }
            }
        } else {
            tracing::error!(addr = %addr, "musqld bound but never answered its readiness probe");
        }
        let status = child.wait().await;
        sc.ready.store(false, Ordering::SeqCst);
        sc.restarts.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            addr = %addr,
            status = ?status,
            "musqld sidecar exited; restarting"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Readiness = the port answers AND speaks musqld's error shape. A bare TCP
/// connect would also succeed against anything else that grabbed the port, and
/// the whole point of the probe is to know the ENGINE is there.
///
/// It proves that for SOME process, not for the one this supervisor spawned —
/// the caller must additionally confirm its own child is alive (see
/// `supervise`), because an orphaned musqld answers this probe identically.
async fn wait_ready(base: &str) -> bool {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    for _ in 0..40 {
        // An unknown database with no `-create` is the one request that cannot
        // touch tenant data and provably reached musqld's router.
        let r = client
            .post(format!("{base}/v2/pipeline"))
            .header("host", "hive-readiness-probe.localhost")
            .header("content-type", "application/json")
            .body("{\"baton\":null,\"requests\":[]}")
            .send()
            .await;
        if let Ok(resp) = r {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// True once the sidecar has answered its readiness probe. Anything that is
/// about to hand a request to the engine waits on this rather than on the
/// process having been spawned: a spawned-but-not-listening musqld is exactly
/// the window that would otherwise turn into a 500 for a real tenant.
pub fn ready() -> bool {
    sidecar()
        .map(|s| s.ready.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Operator view of the sidecar, for `/v1/admin/sqlite/engines`.
pub fn status_json() -> Value {
    match sidecar() {
        None => json!({ "engine": Engine::Rusqlite.label(), "sidecar": null }),
        Some(sc) => json!({
            "engine": default_engine().label(),
            "sidecar": {
                "base": sc.base,
                "ready": sc.ready.load(Ordering::SeqCst),
                "restarts": sc.restarts.load(Ordering::Relaxed),
                "idle_timeout": idle_timeout(),
            },
        }),
    }
}

// ---- Engine ----------------------------------------------------------------

/// Per-database admission for musql streams, the musql arm's equivalent of
/// `sqlite_pool`'s semaphore. Keyed by database id so one busy database cannot
/// starve another.
fn stream_sem(db_id: &str) -> Arc<tokio::sync::Semaphore> {
    static R: OnceLock<parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>> =
        OnceLock::new();
    R.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .entry(db_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(max_streams())))
        .clone()
}

/// The musql engine: one Hrana stream on the sidecar, addressed by `Host`.
pub struct MusqlEngine {
    client: reqwest::Client,
    base: String,
    /// Host header for this database: `hive-sqlite-<sanitize_tag(db_id)>.localhost`.
    db: String,
    /// Released in `Drop`, never on an error branch — axum drops the request
    /// future when a client goes away, and a dropped future never returns
    /// `Err`. Same rule as `sqlite_pool::PooledConn`.
    _permit: tokio::sync::OwnedSemaphorePermit,
    /// musqld's stream handle. It mints a fresh one on EVERY response, so this
    /// is updated after each call and never handed to a client.
    baton: Option<String>,
    /// The connection's last inserted rowid, tracked here because musqld reports
    /// a PER-STATEMENT rowid (0 for UPDATE/DELETE) while SQLite — and therefore
    /// Hive's rusqlite path — reports the CONNECTION's, which is what an UPDATE
    /// after an INSERT echoes. Without this the two engines disagree on
    /// `last_insert_rowid` for every non-INSERT write, which is a client-visible
    /// difference on a pooled connection.
    last_rowid: i64,
}

impl MusqlEngine {
    /// Open a stream, queueing for admission when the database is at its limit.
    pub async fn connect(client: reqwest::Client, db_id: &str) -> Result<MusqlEngine, String> {
        let sc = sidecar().ok_or_else(|| {
            "musql is not running on this node (no sidecar); this database cannot be served here"
                .to_string()
        })?;
        if !sc.ready.load(Ordering::SeqCst) {
            return Err("musql sidecar is not ready".to_string());
        }
        let Some(_) = crate::databases::sqlite_file_path(db_id) else {
            return Err("database id is not a platform-issued id".to_string());
        };
        let sem = stream_sem(db_id);
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => match tokio::time::timeout(
                Duration::from_millis(stream_wait_ms()),
                sem.clone().acquire_owned(),
            )
            .await
            {
                Ok(Ok(p)) => p,
                Ok(Err(_)) | Err(_) => {
                    return Err(format!(
                        "database is at its musql stream limit ({} concurrent) and the wait \
                         budget ({}ms) expired",
                        max_streams(),
                        stream_wait_ms()
                    ))
                }
            },
        };
        Ok(MusqlEngine {
            client,
            base: sc.base.clone(),
            db: host_label(db_id),
            baton: None,
            last_rowid: 0,
            _permit: permit,
        })
    }

    /// Post one pipeline and return its `results` array. Everything musql-shaped
    /// is normalised here so no caller downstream has to know it exists.
    async fn post(&mut self, requests: Vec<Value>) -> Result<Vec<Value>, String> {
        let body = json!({ "baton": self.baton.clone(), "requests": requests });
        let resp = self
            .client
            .post(format!("{}/v3/pipeline", self.base))
            .header("host", self.db.clone())
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("musql sidecar unreachable: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("musql sidecar returned an unreadable body: {e}"))?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        // An unknown/expired baton: musqld answers 400. Retrying once with a
        // fresh stream is correct — Hive's own stream is still live, only the
        // engine's copy of it lapsed.
        if status == reqwest::StatusCode::BAD_REQUEST
            && v.get("code").and_then(|c| c.as_str()) == Some("HRANA_PROTO_ERROR")
            && self.baton.is_some()
        {
            self.baton = None;
            return Box::pin(self.post(requests)).await;
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            // Byte-identical to Hive's own "no such database": a musql database
            // whose file is absent must not look different from any other
            // missing database.
            return Err("no such database".to_string());
        }
        if !status.is_success() {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or(&text)
                .to_string();
            return Err(format!("musql sidecar error ({status}): {msg}"));
        }
        self.baton = v.get("baton").and_then(|b| b.as_str()).map(|s| s.to_string());
        Ok(v
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Unwrap one result envelope: `Ok(response)` or the engine's error. musqld
    /// names errors with codes Hive has never emitted
    /// (`SQLITE_CONSTRAINT_UNIQUE`, `SQLITE_CONSTRAINT_FOREIGNKEY`, …);
    /// `crate::hrana_proto` re-tags everything `SQLITE_ERROR` on the way out, so
    /// the client-visible code is unchanged and only the message (which is
    /// genuinely more specific) moves.
    ///
    /// That message text is DECISION, not accident: it differs from SQLite's
    /// (`SELECT FROM` is `near "FROM": syntax error` there, `no such column:
    /// FROM` here) and it is deliberately NOT rewritten to imitate it. Message
    /// text is not part of the Hrana wire contract — clients branch on the code
    /// and the envelope, both of which are identical — and musqld's text is
    /// more specific than SQLite's parser's, so normalising it would trade real
    /// information for cosmetic agreement.
    fn unwrap(result: &Value) -> Result<Value, String> {
        match result.get("type").and_then(|t| t.as_str()) {
            Some("ok") => Ok(result.get("response").cloned().unwrap_or(Value::Null)),
            _ => Err(result
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("musql returned an error with no message")
                .to_string()),
        }
    }

    /// The one place musqld's value encoding is trusted. It is identical to
    /// Hive's (measured: `integer` as a string, `float` as a number, `blob`
    /// under `base64`), and an unrecognised shape is an error rather than a
    /// guess that would silently mangle a row.
    fn decode(v: &Value) -> Result<SqlValue, String> {
        let ty = v
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| "musql returned a value with no type".to_string())?;
        match ty {
            "null" => Ok(SqlValue::Null),
            "integer" => v
                .get("value")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "musql integer value is not a string".to_string())
                .and_then(|s| s.parse::<i64>().map_err(|_| "unparseable".to_string()))
                .map(SqlValue::Integer),
            "float" => v
                .get("value")
                .and_then(|x| x.as_f64())
                .map(SqlValue::Real)
                .ok_or_else(|| "musql float value is not a number".to_string()),
            "text" => v
                .get("value")
                .and_then(|x| x.as_str())
                .map(|s| SqlValue::Text(s.to_string()))
                .ok_or_else(|| "musql text value is not a string".to_string()),
            "blob" => {
                let b64 = v
                    .get("base64")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "musql blob value carries no base64".to_string())?;
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .map(SqlValue::Blob)
                    .map_err(|e| format!("musql blob is not valid base64: {e}"))
            }
            other => Err(format!("musql returned unknown value type {other:?}")),
        }
    }

    fn encode(v: &SqlValue) -> Value {
        match v {
            SqlValue::Null => json!({ "type": "null" }),
            SqlValue::Integer(i) => json!({ "type": "integer", "value": i.to_string() }),
            SqlValue::Real(f) => json!({ "type": "float", "value": f }),
            SqlValue::Text(s) => json!({ "type": "text", "value": s }),
            SqlValue::Blob(b) => json!({
                "type": "blob",
                "base64": base64::engine::general_purpose::STANDARD.encode(b),
            }),
        }
    }

    fn cols(v: &Value) -> Vec<Col> {
        v.get("cols")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|c| Col {
                        name: c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                        decltype: c
                            .get("decltype")
                            .and_then(|d| d.as_str())
                            .map(|s| s.to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether a NULL rendered by musqld in this column could be a non-finite
    /// REAL rather than a NULL. Only a column the engine declares REAL (or does
    /// not declare at all — every computed expression, including `SELECT
    /// 1e308*10`) can hold one, so a NULL in an INTEGER or TEXT column is never
    /// probed. SQLite's affinity rules: a declared type containing REAL, FLOA
    /// or DOUB takes REAL affinity.
    fn could_be_non_finite(col: Option<&Col>) -> bool {
        match col.and_then(|c| c.decltype.as_deref()) {
            None => true,
            Some(d) => {
                let u = d.to_ascii_uppercase();
                u.contains("REAL") || u.contains("FLOA") || u.contains("DOUB")
            }
        }
    }

    /// Decide whether a NULL musqld rendered is a non-finite REAL.
    ///
    /// musqld's row encoder has no representation for `Inf`/`-Inf`/`NaN` and
    /// emits `{"type":"null"}` — byte-identical to a real NULL, so the rendered
    /// row alone cannot tell them apart. The ENGINE still can, measured against
    /// the live sidecar: `SELECT 1e308*10 IS NULL` answers 0 and
    /// `CAST(1e308*10 AS TEXT)` answers `Inf`. So ask it: re-run the statement
    /// wrapped in a subquery and project, per suspect column, the text
    /// rendering musqld itself would have used.
    ///
    /// Three gates keep that safe and bounded:
    /// * only for a statement the engine itself reports read-only — re-running a
    ///   write would apply it twice;
    /// * only when the first pass actually rendered a NULL, and only in a column
    ///   that can hold a REAL (`could_be_non_finite`);
    /// * one probe statement covering every suspect column at once, so the cost
    ///   is one extra execution of the statement, never one per column.
    ///
    /// That cost is accepted: a non-finite handed to a client as a NULL it never
    /// wrote is the one divergence this lane raises a loud error for everywhere
    /// else (`hrana_proto::encode_value` refuses to encode one), and a silent
    /// wrong value is worse than a second read.
    async fn non_finite_probe(
        &mut self,
        sql: &str,
        cols: &[Col],
        idxs: &[usize],
    ) -> Option<(String, f64)> {
        // Read-only is the engine's own verdict; a statement it will not vouch
        // for is not re-run, and the NULL is left as a NULL.
        if !self.describe(sql).await.ok()?.is_readonly {
            return None;
        }
        let inner = sql.trim().trim_end_matches(';').trim();
        if inner.is_empty() {
            return None;
        }
        let mut guards: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for i in idxs {
            let name = cols.get(*i)?.name.as_str();
            // An unnamed column cannot be addressed from the wrapper; musqld
            // answers "no such column" for `t.""`. Skipping is honest — it
            // leaves that one column unchecked.
            if name.is_empty() {
                continue;
            }
            let q = format!("\"{}\"", name.replace('"', "\"\""));
            guards.push(format!(
                "MAX(CASE WHEN typeof(t.{q}) = 'real' AND CAST(t.{q} AS TEXT) \
                 IN ('Inf','-Inf','NaN') THEN CAST(t.{q} AS TEXT) ELSE NULL END)"
            ));
            names.push(name.to_string());
        }
        if guards.is_empty() {
            return None;
        }
        // `post` directly, never `run`: `run` would probe its own probe.
        let results = self
            .post(vec![json!({
                "type": "execute",
                "stmt": { "sql": format!("SELECT {} FROM ({}) AS t", guards.join(", "), inner),
                          "want_rows": true },
            })])
            .await
            .ok()?;
        let first = results.into_iter().next().unwrap_or(Value::Null);
        let res = Self::unwrap(&first).ok()?;
        let result = res.get("result").cloned().unwrap_or(Value::Null);
        let arr = result
            .get("rows")
            .and_then(|r| r.as_array())?
            .first()?
            .as_array()?;
        for (i, v) in arr.iter().enumerate() {
            let Some(s) = v.get("value").and_then(|s| s.as_str()) else {
                continue;
            };
            let f = match s {
                "Inf" => f64::INFINITY,
                "-Inf" => f64::NEG_INFINITY,
                "NaN" => f64::NAN,
                _ => continue,
            };
            return Some((names.get(i).cloned().unwrap_or_default(), f));
        }
        None
    }
}

/// The `Host` that selects a database in musqld's `-dir` mode: the file's own
/// name minus its extension, plus any domain (musqld reads the first label).
pub fn host_label(db_id: &str) -> String {
    format!("{}.localhost", crate::databases::sqlite_musql_stem(db_id))
}

#[async_trait]
impl SqlEngine for MusqlEngine {
    async fn run(
        &mut self,
        sql: &str,
        want_rows: bool,
        args: &[SqlValue],
        named: &[NamedArg],
        budget: Budget,
    ) -> Result<RunResult, String> {
        // The engine is asked for rows UNCONDITIONALLY. musqld answers
        // `rows_read: 0` for every statement, so the step count is the only
        // honest source for the number the wire publishes — and with
        // `want_rows: false` musqld returns no row bodies at all, which would
        // report 0 for a `SELECT` that stepped rows. The rusqlite engine counts
        // every stepped row BEFORE it looks at `want_rows`, and that is what
        // clients see today, so that is the contract. The rows are discarded
        // below when the caller did not ask for them.
        let mut stmt = json!({ "sql": sql, "want_rows": true });
        if !args.is_empty() {
            stmt["args"] = Value::Array(args.iter().map(Self::encode).collect());
        }
        if !named.is_empty() {
            stmt["named_args"] = Value::Array(
                named
                    .iter()
                    .map(|n| json!({ "name": n.name, "value": Self::encode(&n.value) }))
                    .collect(),
            );
        }
        let results = self
            .post(vec![json!({ "type": "execute", "stmt": stmt })])
            .await?;
        let first = results.into_iter().next().unwrap_or(Value::Null);
        let res = Self::unwrap(&first)?;
        let result = res.get("result").cloned().unwrap_or(Value::Null);

        let cols = Self::cols(&result);
        let mut rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut bytes = 0usize;
        let mut realish_nulls: Vec<usize> = Vec::new();
        for row in result.get("rows").and_then(|r| r.as_array()).unwrap_or(&vec![]) {
            let arr = row.as_array().ok_or("musql returned a non-array row")?;
            let mut out = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                let decoded = Self::decode(v)?;
                if matches!(decoded, SqlValue::Null)
                    && Self::could_be_non_finite(cols.get(i))
                    && !realish_nulls.contains(&i)
                {
                    realish_nulls.push(i);
                }
                bytes += decoded.approx_bytes();
                out.push(decoded);
            }
            rows.push(out);
            if rows.len() > budget.max_rows {
                return Err(format!(
                    "result set exceeds {} rows over the pipeline endpoint — paginate with \
                     LIMIT/OFFSET",
                    budget.max_rows
                ));
            }
            if bytes > budget.max_bytes {
                return Err(format!(
                    "result set exceeds {}MiB over the pipeline endpoint — narrow the projection \
                     or paginate",
                    budget.max_bytes / (1024 * 1024)
                ));
            }
        }
        // A NULL in a REAL-capable column is the one place musqld's encoder is
        // lossy, so it is re-checked against the engine before the row is
        // handed out. See `non_finite_probe`.
        if !realish_nulls.is_empty() {
            if let Some((col, value)) = self.non_finite_probe(sql, &cols, &realish_nulls).await {
                return Err(crate::hrana_proto::non_finite_message(&col, value));
            }
        }
        // musqld reports rows_read: 0 unconditionally, so the honest substitute
        // is what this statement stepped. Deliberately taken from every row the
        // statement produced, not from the rows the caller asked for:
        // `want_rows: false` still executes and still counts.
        let rows_read = rows.len() as u64;
        if !want_rows {
            // The rusqlite engine does not materialise a row it was told not to
            // return; the budget above is still applied to the set that had to
            // be materialised to count it, which is the safer direction (a
            // refusal, not an unbounded buffer).
            rows.clear();
        }
        let rows_written = result
            .get("affected_row_count")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        // musqld reports a per-statement rowid (0 for UPDATE/DELETE); SQLite
        // reports the connection's, which is the value an UPDATE after an
        // INSERT echoes and the value Hive has always published. Track it here
        // so both engines answer identically, and gate it on `rows_written`
        // exactly the way the rusqlite engine gates it on `changes`.
        let reported = result
            .get("last_insert_rowid")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        if reported != 0 {
            self.last_rowid = reported;
        }
        let last_insert_rowid =
            if rows_written > 0 { (self.last_rowid != 0).then_some(self.last_rowid) } else { None };
        Ok(RunResult {
            cols,
            rows,
            rows_read,
            rows_written,
            last_insert_rowid,
        })
    }

    async fn execute_batch(&mut self, sql: &str) -> Result<(), String> {
        let results = self
            .post(vec![json!({ "type": "sequence", "sql": sql })])
            .await?;
        let first = results.into_iter().next().unwrap_or(Value::Null);
        Self::unwrap(&first).map(|_| ())
    }

    async fn describe(&mut self, sql: &str) -> Result<DescribeResult, String> {
        let results = self
            .post(vec![json!({ "type": "describe", "sql": sql })])
            .await?;
        let first = results.into_iter().next().unwrap_or(Value::Null);
        let res = Self::unwrap(&first)?;
        let result = res.get("result").cloned().unwrap_or(Value::Null);
        Ok(DescribeResult {
            params: result
                .get("params")
                .and_then(|p| p.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|p| p.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            cols: Self::cols(&result),
            is_explain: result
                .get("is_explain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            is_readonly: result
                .get("is_readonly")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    async fn is_autocommit(&mut self) -> bool {
        // v3 only — the sidecar speaks v3 internally regardless of the client's
        // version, and `get_autocommit` is gated on the CLIENT's version by
        // `hrana_proto` before it ever reaches this.
        match self.post(vec![json!({ "type": "get_autocommit" })]).await {
            Ok(results) => {
                // Bind first: `into_iter().next()` yields an owned Value, and
                // chaining `.get()` off it would borrow a temporary.
                let first = results.into_iter().next();
                first
                    .as_ref()
                    .and_then(|r| r.get("response"))
                    .and_then(|r| r.get("is_autocommit"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true)
            }
            Err(e) => {
                tracing::warn!(error = %e, "musql get_autocommit failed; reporting autocommit");
                true
            }
        }
    }
}
