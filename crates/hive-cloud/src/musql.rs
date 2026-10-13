//! musql: the managed-SQLite storage engine, reached as a loopback Hrana sidecar behind Hive's own
//! Hrana handler.
//!
//! **Why a sidecar.** musql is Go (no C ABI, no Rust crate, a `GOOS=js` wasm that needs a JS host),
//! so it cannot be linked into this workspace or run under wasmtime. Tenants still talk only to
//! Hive's Rust Hrana handler, with `musqld` the engine behind it, on loopback, never exposed.
//!
//! **Not musql's job.** musqld has ONE global `-auth-token` and no per-database concept, so
//! per-database auth stays in Rust (`db_rest::credential_matches`) and NO `-auth-token` is passed:
//! a token that cannot express the scope we enforce looks like a second boundary while enforcing a
//! weaker one. Owner routing, the owner bearer re-check, the honest 421, batons, the 4 MiB body cap
//! and the wire codec stay in Rust; musqld answers one database at a time, selected by `Host`.
//!
//! **Two rules proved by measurement.** Never run with `-create`: an unknown name would silently
//! create a 147-byte empty database and serve it, the divergence the SQLite lane's owner routing
//! exists to prevent, whereas without it a missing file is a clean 404 that this module maps onto
//! Hive's own byte-identical "no such database". And ONE writer per file: two musqld instances on
//! one `.musq` both bound and answered 200 (upstream disclaims even a mixed C/musql pair), so this
//! supervisor is the only process that may start musqld and `crate::musql_migrate` quiesces the
//! file before converting.
//!
//! **Envelope discipline.** musqld's JSON never reaches a client: its error codes are normalised to
//! `SQLITE_ERROR`, its `base_url` is dropped, its `baton` stays internal (a fresh one per response
//! would cost every client a round trip) and its constant `rows_read: 0` is replaced with the rows
//! actually stepped (see [`MusqlEngine::run`]).
//!
//! **Measured divergences from SQLite.** `rows_read` is the length of the returned rows array (see
//! `run`), and a NULL in a REAL-capable column is re-checked against the engine because musqld's
//! encoder emits `{"type":"null"}` for `Inf`/`-Inf`/`NaN` (see `non_finite_probe`). Two are
//! ACCEPTED: musqld normalises a STORED `-0.0` to `0.0` (`-0.0 = 0.0` in SQL and they sort, compare
//! and sum identically; only their TEXT rendering differs, which nothing here reads, and closing it
//! would mean re-encoding every REAL both ways), and error message text differs (`SELECT FROM` is
//! `near "FROM": syntax error` on SQLite and `no such column: FROM` on musqld; message text is not
//! part of the wire contract, clients match the code and the envelope, so musqld's more specific
//! text passes through).

use crate::sqlite_engine::{Budget, Col, DescribeResult, NamedArg, RunResult, SqlEngine, SqlValue};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The engine used for managed SQLite databases. Default `rusqlite`, so a roll of this code changes
/// nothing until an operator says so (see [`engine_for_db`]).
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

/// Per-database override, read from the `engine` key the provisioner writes into the record's
/// `connection` map. **Every value other than `musql` resolves to [`Engine::Rusqlite`], never to
/// [`default_engine`]**: the pre-change provisioner wrote `engine: "sqlite"`, so a `_ =>
/// default_engine()` arm silently re-routed EVERY existing database (a never-converted,
/// never-verified `.db` file) onto musql the moment an operator set `HIVE_SQLITE_ENGINE=musql`, and
/// where no `.musq` existed the database answered "no such database" instead of its data. Only the
/// explicit, verified-after-the-fact flip in `musql_migrate` makes a database musql.
pub fn engine_for_db(connection: &HashMap<String, String>) -> Engine {
    let v = connection
        .get("engine")
        .map(|s| s.trim().to_ascii_lowercase());
    match v.as_deref() {
        Some("musql") => Engine::Musql,
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

/// Where the sidecar listens: loopback only. This is an engine, not a service, and AGENTS.md's "do
/// not open new inbound ports" applies to the world, not to a process talking to itself on
/// 127.0.0.1.
fn listen_addr() -> String {
    env_str("HIVE_MUSQL_LISTEN", "127.0.0.1:8099")
}

/// musqld's stream idle timeout. MUST exceed Hive's own `HIVE_SQLITE_STREAM_IDLE_MS` (up to 600 s),
/// or the engine kills an interactive transaction mid-stream while Hive still believes the stream
/// is live.
fn idle_timeout() -> String {
    env_str("HIVE_MUSQL_IDLE_TIMEOUT", "900s")
}

pub fn musqld_bin() -> String {
    env_str("HIVE_MUSQLD_BIN", "musqld")
}

/// Concurrent musql streams per database: a musqld stream is the analogue of a pooled SQLite
/// connection, so it gets the same per-DATABASE bound as `sqlite_pool`'s semaphore (without one,
/// every Hrana request would open an unbounded number of engine streams).
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

/// Probed, never assumed: does this host have a usable musqld? Run at boot and advertised on
/// `NodeInfo` like `wasm_runtime`/`bun_runtime`, so placement and the migration planner see it.
/// `None` means NOT REPORTED, which this lane treats as NOT CAPABLE (the `wasm_runtime` asymmetry):
/// a node that cannot run the engine must never be handed a `musql` database. `-h` prints the flag
/// list and exits 0, and the marker that this really is musqld (not some other `musqld` on PATH) is
/// `-dir`, the mode this lane depends on.
pub fn probe() -> Option<bool> {
    let bin = musqld_bin();
    let out = std::process::Command::new(&bin).arg("-h").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    Some(text.contains("-dir") && text.contains("-idle-timeout"))
}

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

/// One supervised musqld: run it, mark it ready once it answers, and restart it with backoff if it
/// exits. A sidecar that dies silently would turn every `musql` database into a 500 with no
/// operator-visible cause.
///
/// Readiness is gated on OUR child still being alive when the probe succeeded: the probe proves
/// only that SOMETHING answered on the port, and an ORPHANED musqld from a previous boot answers
/// identically (one survived its parent's shutdown, held 127.0.0.1:8099 and every supervised child
/// then died on `bind: address already in use` while the probe still saw 404s and reported ready).
/// Serving through it would hand tenant databases to a writer this node does not supervise,
/// breaking one writer per file. Another process owning the port (or musqld dying at startup) stays
/// NOT ready: an honest 503 beats serving a foreign writer, and the restart counter makes it
/// operator-visible.
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
            match child.try_wait() {
                Ok(None) => {
                    sc.ready.store(true, Ordering::SeqCst);
                    backoff = Duration::from_millis(500);
                    tracing::info!(addr = %addr, dir = %dir.display(), "musqld sidecar ready");
                }
                other => {
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

/// Readiness = the port answers AND speaks musqld's error shape: a bare TCP connect would also
/// succeed against anything else that grabbed the port. It proves that for SOME process, not the
/// one this supervisor spawned, so the caller must also confirm its own child is alive (see
/// `supervise`). The probe asks for an unknown database (no `-create`), the one request that cannot
/// touch tenant data and provably reached musqld's router.
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

/// True once the sidecar answered its readiness probe. Anything about to hand a request to the
/// engine waits on this, not on the process having been spawned: a spawned-but-not-listening musqld
/// is the window that would otherwise become a 500 for a real tenant.
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
    /// Released in `Drop`, never on an error branch: axum drops the request future when a client
    /// goes away, and a dropped future never returns `Err` (same rule as
    /// `sqlite_pool::PooledConn`).
    _permit: tokio::sync::OwnedSemaphorePermit,
    /// musqld's stream handle. It mints a fresh one on EVERY response, so this
    /// is updated after each call and never handed to a client.
    baton: Option<String>,
    /// The connection's last inserted rowid, tracked here because musqld reports a PER-STATEMENT
    /// rowid (0 for UPDATE/DELETE) while SQLite, and so Hive's rusqlite path, reports the
    /// CONNECTION's (what an UPDATE after an INSERT echoes); without it the engines disagree on
    /// `last_insert_rowid` for every non-INSERT write on a pooled connection.
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
    /// An unknown or expired baton answers 400 `HRANA_PROTO_ERROR`; one retry with a fresh stream
    /// is correct, since Hive's own stream is still live and only the engine's copy lapsed. A 404
    /// maps to the byte-identical `no such database`, so a musql database whose file is absent
    /// looks like any other missing database.
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

        if status == reqwest::StatusCode::BAD_REQUEST
            && v.get("code").and_then(|c| c.as_str()) == Some("HRANA_PROTO_ERROR")
            && self.baton.is_some()
        {
            self.baton = None;
            return Box::pin(self.post(requests)).await;
        }
        if status == reqwest::StatusCode::NOT_FOUND {
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

    /// Unwraps one result envelope: `Ok(response)` or the engine's error. musqld names errors with
    /// codes Hive never emitted (`SQLITE_CONSTRAINT_UNIQUE`, ...); `crate::hrana_proto` re-tags
    /// everything `SQLITE_ERROR` on the way out, so the client-visible code is unchanged and only
    /// the message (genuinely more specific than SQLite's) moves. That message text is a DECISION:
    /// it is deliberately NOT rewritten to imitate SQLite's, since text is not part of the Hrana
    /// wire contract (clients branch on the code and the envelope, both identical).
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

    /// The one place musqld's value encoding is trusted: identical to Hive's (measured: `integer`
    /// as a string, `float` as a number, `blob` under `base64`), with an unrecognised shape an
    /// error rather than a guess that would silently mangle a row.
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

    /// Whether a NULL musqld rendered in this column could be a non-finite REAL. Only a column the
    /// engine declares REAL (or does not declare at all: every computed expression, including
    /// `SELECT 1e308*10`) can hold one, so a NULL in an INTEGER or TEXT column is never probed
    /// (SQLite affinity: a declared type containing REAL, FLOA or DOUB takes REAL affinity).
    fn could_be_non_finite(col: Option<&Col>) -> bool {
        match col.and_then(|c| c.decltype.as_deref()) {
            None => true,
            Some(d) => {
                let u = d.to_ascii_uppercase();
                u.contains("REAL") || u.contains("FLOA") || u.contains("DOUB")
            }
        }
    }

    /// Decides whether a NULL musqld rendered is a non-finite REAL. The encoder emits
    /// `{"type":"null"}` for `Inf`/`-Inf`/`NaN`, byte-identical to a real NULL, but the ENGINE can
    /// still tell (`SELECT 1e308*10 IS NULL` answers 0 and `CAST(1e308*10 AS TEXT)` answers `Inf`),
    /// so the statement is re-run wrapped in a subquery that projects, per suspect column, the text
    /// rendering musqld itself would use.
    ///
    /// Three gates keep that safe and bounded: only for a statement the engine itself reports
    /// read-only (re-running a write would apply it twice; a statement it will not vouch for is not
    /// re-run and the NULL stays a NULL); only when the first pass rendered a NULL in a column that
    /// can hold a REAL; and one probe statement covering every suspect column, so the cost is one
    /// extra execution, never one per column (an unnamed column cannot be addressed from the
    /// wrapper, so it stays unchecked). The cost is accepted: a non-finite handed to a client as a
    /// NULL it never wrote is the divergence this lane raises a loud error for everywhere else
    /// (`hrana_proto::encode_value` refuses to encode one), and a silent wrong value is worse than
    /// a second read. The probe posts directly, never through `run`, which would probe its own
    /// probe.
    async fn non_finite_probe(
        &mut self,
        sql: &str,
        cols: &[Col],
        idxs: &[usize],
    ) -> Option<(String, f64)> {
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
    /// Runs one statement. The engine is asked for rows UNCONDITIONALLY: musqld answers `rows_read:
    /// 0` for every statement and with `want_rows: false` returns no row bodies, so the only honest
    /// step count is the length of the returned `rows` array, read before a single row is decoded
    /// (the reply is ONE JSON document, so the count is free). The rusqlite engine counts every
    /// stepped row before looking at `want_rows` and applies the row/byte budget only to rows it
    /// KEEPS, so a 25 000-row SELECT with `want_rows: false` answers `rows_read: 25000`, no rows
    /// and no error; enforcing the budget on a set about to be discarded would be a divergent
    /// envelope plus a 32 MiB decode thrown away. Nothing is decoded when the caller did not ask
    /// for rows (the non-finite re-check is skipped with them: it stops a lossy NULL reaching a
    /// client, and no row does), and over the ceiling the COUNT refuses before any decode (a result
    /// one row over must not cost 20 001 decodes to be told it is too large). `last_insert_rowid`
    /// tracks the connection's value like SQLite, gated on `rows_written` the way the rusqlite
    /// engine gates it on `changes`; a NULL in a REAL-capable column is re-checked against the
    /// engine before the row is handed out (see `non_finite_probe`).
    async fn run(
        &mut self,
        sql: &str,
        want_rows: bool,
        args: &[SqlValue],
        named: &[NamedArg],
        budget: Budget,
    ) -> Result<RunResult, String> {
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
        let rows_written = result
            .get("affected_row_count")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
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

        let returned = result.get("rows").and_then(|r| r.as_array());
        let rows_read = returned.map(|a| a.len() as u64).unwrap_or(0);
        if !want_rows {
            return Ok(RunResult {
                cols,
                rows: Vec::new(),
                rows_read,
                rows_written,
                last_insert_rowid,
            });
        }
        if rows_read > budget.max_rows as u64 {
            return Err(format!(
                "result set exceeds {} rows over the pipeline endpoint — paginate with \
                 LIMIT/OFFSET",
                budget.max_rows
            ));
        }
        let mut rows: Vec<Vec<SqlValue>> = Vec::with_capacity(returned.map_or(0, |a| a.len()));
        let mut bytes = 0usize;
        let mut realish_nulls: Vec<usize> = Vec::new();
        for row in returned.into_iter().flatten() {
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
            if bytes > budget.max_bytes {
                return Err(format!(
                    "result set exceeds {}MiB over the pipeline endpoint — narrow the projection \
                     or paginate",
                    budget.max_bytes / (1024 * 1024)
                ));
            }
        }
        if !realish_nulls.is_empty() {
            if let Some((col, value)) = self.non_finite_probe(sql, &cols, &realish_nulls).await {
                return Err(crate::hrana_proto::non_finite_message(&col, value));
            }
        }
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
        match self.post(vec![json!({ "type": "get_autocommit" })]).await {
            Ok(results) => {
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
