//! Bounded per-database SQLite connection pool for the libsql/Hrana lane.
//!
//! The Postgres SQL-over-HTTP surface (`db_rest::run_sql`) deliberately opens a
//! FRESH `tokio_postgres` connection per request and bounds concurrency with a
//! `try_acquire` semaphore that 503s on exhaustion. That is an admission cap,
//! not a pool, and it stays exactly as it is — this module is the sqlite lane's
//! own pool and changes nothing about the postgres lane's semantics.
//!
//! What is different here, and why a real pool is worth it: a SQLite database
//! is a FILE on this node, so "connecting" is an `sqlite3_open` plus the WAL
//! pragmas — cheap enough that per-request opens look fine in a microbenchmark
//! and pathological under a Hrana workload, because an interactive Hrana stream
//! holds its connection across MANY HTTP requests (that is what the baton is
//! for). Reopening per request would break transactions outright.
//!
//! Shape:
//! * capacity is a `tokio::sync::Semaphore` per database — `acquire_owned` with
//!   a timeout, so an exhausted pool makes callers QUEUE (FIFO, the semaphore's
//!   own waiter order) instead of getting an immediate 503. Only a caller that
//!   waits out `HIVE_SQLITE_POOL_WAIT_MS` is refused, and it is refused with a
//!   typed error the caller renders as a Hrana error rather than a bare 503.
//! * the idle set is BOUNDED (`HIVE_SQLITE_POOL_MAX_IDLE`): a returned
//!   connection is kept for reuse only while the idle set is under the bound,
//!   otherwise it is closed. An unbounded idle set is just a leak with a nicer
//!   name — every connection holds an fd and a private page cache.
//! * [`PooledConn`] releases its permit in `Drop`, never on an error branch.
//!   axum drops a request future when the client goes away (curl timeout,
//!   browser navigation); a dropped future never returns `Err`, so any
//!   "release on failure" living in a caller is silently skipped and the pool
//!   wedges permanently. This is the `ColdStartGuard` rule applied to a second
//!   reservation surface.
//!
//! Connections never cross the pool boundary as `&Connection`: `rusqlite`'s
//! `Connection` is `Send + !Sync`, and every real call blocks on disk, so
//! [`PooledConn::call`] MOVES the connection into `spawn_blocking` and moves it
//! back. That keeps the async runtime free of blocking IO and makes it
//! impossible to share one connection across two tasks.

use hive_crsql::rusqlite::{Connection, OpenFlags};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Live connections per database. Deliberately small: SQLite serialises
/// writers on the file anyway, so extra connections buy read concurrency (WAL
/// readers don't block) and nothing else.
fn max_conns() -> usize {
    env_usize("HIVE_SQLITE_POOL_MAX", 8).clamp(1, 256)
}
/// How many of those may sit idle waiting for reuse.
fn max_idle() -> usize {
    env_usize("HIVE_SQLITE_POOL_MAX_IDLE", 4).clamp(1, 256)
}
/// How long a caller queues for a permit before being refused.
fn wait_ms() -> u64 {
    env_u64("HIVE_SQLITE_POOL_WAIT_MS", 10_000).clamp(100, 120_000)
}
/// SQLite's own lock wait (a writer blocked by another writer on the file).
fn busy_ms() -> u64 {
    env_u64("HIVE_SQLITE_BUSY_MS", 5_000).clamp(0, 120_000)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

#[derive(Debug)]
pub enum PoolError {
    /// Every permit was held for the whole wait budget.
    Busy,
    /// `sqlite3_open` (or the boot pragmas) failed.
    Open(String),
    /// The blocking task carrying the connection panicked or was cancelled —
    /// the connection is gone, the permit is still released by `Drop`.
    Lost,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Busy => write!(
                f,
                "database is at its connection limit ({} concurrent) and the wait budget ({}ms) expired",
                max_conns(),
                wait_ms()
            ),
            PoolError::Open(e) => write!(f, "cannot open database file: {e}"),
            PoolError::Lost => write!(f, "database connection was lost mid-request"),
        }
    }
}

pub struct SqlitePool {
    path: PathBuf,
    sem: Arc<Semaphore>,
    idle: Mutex<Vec<Connection>>,
    /// Counters for the operator view — never policy inputs.
    opened: AtomicU64,
    reused: AtomicU64,
    waited: AtomicU64,
    refused: AtomicU64,
}

impl SqlitePool {
    fn new(path: PathBuf) -> SqlitePool {
        SqlitePool {
            path,
            sem: Arc::new(Semaphore::new(max_conns())),
            idle: Mutex::new(Vec::new()),
            opened: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }
    }

    /// Open a connection with the pragmas every pooled connection must carry.
    /// WAL is what makes concurrent readers non-blocking; `busy_timeout` is
    /// what turns a contended write from an instant `SQLITE_BUSY` error into a
    /// bounded wait.
    fn open(path: &Path) -> Result<Connection, PoolError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| PoolError::Open(e.to_string()))?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| PoolError::Open(e.to_string()))?;
        // `journal_mode` returns a row, so it is a query, not an `execute`.
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| PoolError::Open(e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_millis(busy_ms()))
            .map_err(|e| PoolError::Open(e.to_string()))?;
        // NORMAL is the documented safe pairing with WAL (durable across
        // process crash; only a host power loss can lose the last commits).
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = conn.pragma_update(None, "foreign_keys", "ON");
        Ok(conn)
    }

    /// Take a connection, queueing for a permit when the pool is full.
    pub async fn acquire(self: &Arc<Self>) -> Result<PooledConn, PoolError> {
        let permit = match self.sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.waited.fetch_add(1, Ordering::Relaxed);
                match tokio::time::timeout(
                    std::time::Duration::from_millis(wait_ms()),
                    self.sem.clone().acquire_owned(),
                )
                .await
                {
                    Ok(Ok(p)) => p,
                    _ => {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                        return Err(PoolError::Busy);
                    }
                }
            }
        };
        let pooled = self.idle.lock().pop();
        let conn = match pooled {
            Some(c) => {
                self.reused.fetch_add(1, Ordering::Relaxed);
                c
            }
            None => {
                let path = self.path.clone();
                // Opening touches disk; keep it off the runtime threads.
                let opened = tokio::task::spawn_blocking(move || Self::open(&path))
                    .await
                    .map_err(|_| PoolError::Lost)??;
                self.opened.fetch_add(1, Ordering::Relaxed);
                opened
            }
        };
        Ok(PooledConn {
            pool: self.clone(),
            conn: Some(conn),
            _permit: permit,
        })
    }

    /// Return a connection to the idle set, or close it when the set is full.
    /// A connection that is still inside a transaction is NEVER pooled: the
    /// next borrower would inherit an open write lock and a half-finished unit
    /// of work.
    fn release(&self, conn: Connection) {
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        if !conn.is_autocommit() {
            // Rollback failed — the connection's state is not knowable, so
            // close it rather than hand it to the next caller.
            return;
        }
        let mut idle = self.idle.lock();
        if idle.len() < max_idle() {
            idle.push(conn);
        }
    }

    fn stats_json(&self, id: &str) -> serde_json::Value {
        serde_json::json!({
            "database": id,
            "max": max_conns(),
            "available": self.sem.available_permits(),
            "idle": self.idle.lock().len(),
            "opened": self.opened.load(Ordering::Relaxed),
            "reused": self.reused.load(Ordering::Relaxed),
            "waited": self.waited.load(Ordering::Relaxed),
            "refused": self.refused.load(Ordering::Relaxed),
        })
    }
}

/// A borrowed connection. The permit is released in `Drop` — including when the
/// request future is dropped mid-await, which is the only reason this type
/// exists as a guard instead of a plain return value.
pub struct PooledConn {
    pool: Arc<SqlitePool>,
    conn: Option<Connection>,
    _permit: OwnedSemaphorePermit,
}

impl PooledConn {
    /// Run `f` against the connection on the blocking pool. The connection is
    /// moved in and moved back out; a panic inside `f` loses the connection
    /// (reported as [`PoolError::Lost`]) but never the permit.
    pub async fn call<F, R>(&mut self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let Some(mut conn) = self.conn.take() else {
            return Err(PoolError::Lost);
        };
        match tokio::task::spawn_blocking(move || {
            let out = f(&mut conn);
            (conn, out)
        })
        .await
        {
            Ok((conn, out)) => {
                self.conn = Some(conn);
                Ok(out)
            }
            Err(_) => Err(PoolError::Lost),
        }
    }

    /// True while this guard still holds a usable connection.
    pub fn is_live(&self) -> bool {
        self.conn.is_some()
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
        // `_permit` drops right after this, freeing capacity — order matters:
        // the connection must be back in the idle set before a queued waiter
        // is woken, or the waiter opens a new one while this sits unused.
    }
}

type Registry = Mutex<HashMap<String, Arc<SqlitePool>>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pool for `db_id`, created on first use. `path` is derived server-side
/// from the database id (never from tenant input — see
/// `crate::databases::sqlite_file_path`); it is recorded on the first call and
/// a later call with a different path is ignored, so nothing can re-point a
/// live pool at another file.
pub fn pool_for(db_id: &str, path: &Path) -> Arc<SqlitePool> {
    let mut reg = registry().lock();
    reg.entry(db_id.to_string())
        .or_insert_with(|| Arc::new(SqlitePool::new(path.to_path_buf())))
        .clone()
}

/// Drop a database's pool (on delete). In-flight guards keep their `Arc` alive
/// and release normally; the idle connections close as the last `Arc` drops.
pub fn close(db_id: &str) {
    if let Some(pool) = registry().lock().remove(db_id) {
        pool.idle.lock().clear();
    }
}

/// Operator view of every live pool on this node.
pub fn stats() -> Vec<serde_json::Value> {
    let reg = registry().lock();
    let mut out: Vec<serde_json::Value> = reg.iter().map(|(id, p)| p.stats_json(id)).collect();
    out.sort_by(|a, b| a["database"].as_str().cmp(&b["database"].as_str()));
    out
}
