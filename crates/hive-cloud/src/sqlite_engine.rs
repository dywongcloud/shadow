//! The managed-SQLite engine seam: everything the Hrana codec needs from a SQL
//! engine, and nothing else.
//!
//! This module exists so the Hrana wire implementation has exactly ONE copy.
//! Before it, `hrana_proto.rs` called `rusqlite` directly, so a second engine
//! could only be added by reimplementing statement semantics — and a second
//! implementation of "which rows changed" or "is this request allowed at this
//! protocol version" is the kind of duplication that diverges silently and
//! takes tenant data with it. The seam is narrow on purpose: a value type, one
//! `run` per statement, `execute_batch` for `sequence`, `describe`, and
//! `is_autocommit`. Everything else (batching, conditions, batons, SQL cache,
//! read-only gating, the wire codec) stays in `hrana_proto.rs` and is written
//! once against this trait.
//!
//! # Why `async`
//!
//! The musql engine speaks HTTP to a loopback sidecar, which is async IO; the
//! rusqlite engine is blocking file IO. Both run through the same trait, and
//! the CALLER decides where each one executes: `sqlite_pool::PooledConn` moves
//! the rusqlite connection onto `spawn_blocking` and drives the future to
//! completion there (`futures::executor::block_on`), so no blocking file IO
//! ever lands on a serving tokio worker, while the musql engine is awaited
//! directly on the runtime. One semantic implementation, two correct
//! schedulings.
//!
//! # The musql engine's three known, bounded divergences
//!
//! These are properties of musql measured live (v0.3.0), not guesses, and they
//! are all narrower than "wrong data":
//! 1. `rows_read` — musqld reports 0, so the engine reports the rows it
//!    actually materialised. Still a real count, never a fabrication.
//! 2. `rows_written` — musqld reports `affected_row_count` per statement, which
//!    is already the `total_changes`-delta value rusqlite derives. Equivalent.
//! 3. A stored non-finite REAL comes back NULL from musqld (rusqlite raises).
//!    That is why migration refuses a database holding one (see
//!    `crate::musql::preflight`) instead of silently changing a tenant's rows.

use async_trait::async_trait;
use hive_crsql::rusqlite::types::{Value as RValue, ValueRef};
use hive_crsql::rusqlite::{Connection, Statement};

/// A value in, or out of, an engine. Deliberately not `rusqlite::Value`: that
/// type belongs to one engine and leaking it into the codec is what would make
/// a second engine impossible.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    /// SQLite's own `typeof()` spelling. Used for the migration digest, where
    /// two engines agreeing on a row's bytes but not its type is a real
    /// divergence.
    pub fn typeof_name(&self) -> &'static str {
        match self {
            SqlValue::Null => "null",
            SqlValue::Integer(_) => "integer",
            SqlValue::Real(_) => "real",
            SqlValue::Text(_) => "text",
            SqlValue::Blob(_) => "blob",
        }
    }

    /// Rough wire size, so a result set can be bounded WITHOUT serialising it
    /// first. A bound that requires the bytes it is bounding is no bound at
    /// all — that is the whole reason this is an estimate and not
    /// `to_string().len()`.
    pub fn approx_bytes(&self) -> usize {
        match self {
            SqlValue::Null => 16,
            SqlValue::Integer(_) => 40,
            SqlValue::Real(_) => 40,
            SqlValue::Text(s) => s.len() + 24,
            SqlValue::Blob(b) => (b.len() * 4 / 3 + 4) + 24,
        }
    }
}

impl From<ValueRef<'_>> for SqlValue {
    fn from(v: ValueRef<'_>) -> SqlValue {
        match v {
            ValueRef::Null => SqlValue::Null,
            ValueRef::Integer(i) => SqlValue::Integer(i),
            ValueRef::Real(f) => SqlValue::Real(f),
            ValueRef::Text(t) => SqlValue::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
        }
    }
}

/// A bound positional or named argument.
pub struct NamedArg {
    pub name: String,
    pub value: SqlValue,
}

/// One result column's metadata.
pub struct Col {
    pub name: String,
    pub decltype: Option<String>,
}

/// One statement's outcome.
pub struct RunResult {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<SqlValue>>,
    /// Rows this statement stepped — counted whether or not they were kept.
    pub rows_read: u64,
    /// Rows this statement changed. 0 for DDL, `PRAGMA` and every read, which
    /// is what stops a `CREATE TABLE` from echoing another request's insert
    /// count on a pooled connection.
    pub rows_written: u64,
    /// Reported only for a statement that actually wrote.
    pub last_insert_rowid: Option<i64>,
}

/// A `describe` outcome.
pub struct DescribeResult {
    pub params: Vec<Option<String>>,
    pub cols: Vec<Col>,
    pub is_explain: bool,
    pub is_readonly: bool,
}

/// Buffer ceiling for one statement: rows, then bytes across the row values.
#[derive(Clone, Copy)]
pub struct Budget {
    pub max_rows: usize,
    pub max_bytes: usize,
}

#[async_trait]
pub trait SqlEngine: Send {
    /// Execute one statement and buffer its result. `want_rows: false` still
    /// executes (Hrana says so) and still counts `rows_read`, it just keeps no
    /// row bodies.
    async fn run(
        &mut self,
        sql: &str,
        want_rows: bool,
        args: &[SqlValue],
        named: &[NamedArg],
        budget: Budget,
    ) -> Result<RunResult, String>;

    /// Multi-statement `execute_batch` (Hrana's `sequence` request).
    async fn execute_batch(&mut self, sql: &str) -> Result<(), String>;

    /// Column/parameter metadata without executing.
    async fn describe(&mut self, sql: &str) -> Result<DescribeResult, String>;

    /// False inside an open transaction. The v3 `is_autocommit` batch
    /// condition reads this.
    async fn is_autocommit(&mut self) -> bool;
}

// ---- rusqlite engine -------------------------------------------------------

fn to_rusqlite(v: &SqlValue) -> RValue {
    match v {
        SqlValue::Null => RValue::Null,
        SqlValue::Integer(i) => RValue::Integer(*i),
        SqlValue::Real(f) => RValue::Real(*f),
        SqlValue::Text(s) => RValue::Text(s.clone()),
        SqlValue::Blob(b) => RValue::Blob(b.clone()),
    }
}

/// Bind every parameter, or fail naming the one that could not be bound.
/// Unbound parameters default to NULL in SQLite, which turns a client-side
/// arity bug into a query that "works" and returns the wrong rows — so this
/// refuses instead. (The musql engine gets the same guarantee from musqld,
/// which rejects `expected 1 arguments, got 0` — measured, not assumed.)
fn bind(stmt: &mut Statement<'_>, args: &[SqlValue], named: &[NamedArg]) -> Result<(), String> {
    let expected = stmt.parameter_count();
    if args.len() > expected {
        return Err(format!(
            "statement takes {expected} parameters but {} positional arguments were supplied",
            args.len()
        ));
    }
    let mut bound = vec![false; expected];
    for (i, a) in args.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, to_rusqlite(a))
            .map_err(|e| format!("cannot bind positional argument {}: {e}", i + 1))?;
        bound[i] = true;
    }
    for na in named {
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
        stmt.raw_bind_parameter(idx, to_rusqlite(&na.value))
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

#[async_trait]
impl SqlEngine for Connection {
    async fn run(
        &mut self,
        sql: &str,
        want_rows: bool,
        args: &[SqlValue],
        named: &[NamedArg],
        budget: Budget,
    ) -> Result<RunResult, String> {
        let mut stmt = self.prepare(sql).map_err(|e| e.to_string())?;
        bind(&mut stmt, args, named)?;

        // Column metadata is read from the statement itself (immutable borrow)
        // BEFORE stepping, because `raw_query` takes the statement mutably.
        let cols: Vec<Col> = stmt
            .columns()
            .iter()
            .map(|c| Col {
                name: c.name().to_string(),
                decltype: c.decl_type().map(|s| s.to_string()),
            })
            .collect();
        let want_rows = want_rows && !cols.is_empty();
        // `sqlite3_changes` is per-CONNECTION and is only updated by DML, so
        // after `INSERT` then `SELECT` it still reports the insert's count.
        // `sqlite3_total_changes` moves for exactly the statements
        // `sqlite3_changes` is meaningful for, so its delta is the precise
        // discriminator — and it is also what keeps DDL/`PRAGMA` at 0.
        let total_before = self.total_changes();

        let mut rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut rows_read: u64 = 0;
        let mut bytes: usize = 0;
        {
            let mut stream = stmt.raw_query();
            while let Some(row) = stream.next().map_err(|e| e.to_string())? {
                rows_read += 1;
                if !want_rows {
                    continue;
                }
                let mut out = Vec::with_capacity(cols.len());
                for i in 0..cols.len() {
                    let v = SqlValue::from(row.get_ref(i).map_err(|e| e.to_string())?);
                    bytes += v.approx_bytes();
                    out.push(v);
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
                        "result set exceeds {}MiB over the pipeline endpoint — narrow the \
                         projection or paginate",
                        budget.max_bytes / (1024 * 1024)
                    ));
                }
            }
        }
        let wrote_rows = self.total_changes() != total_before;
        let changes = if !wrote_rows { 0 } else { self.changes() };
        // SQLite keeps the connection's last inserted rowid indefinitely, so it
        // is reported only for a statement that actually wrote. Deliberately
        // NOT additionally gated on "the rowid moved": re-inserting a rowid
        // that was just deleted leaves it unchanged, and suppressing a real
        // insert's rowid is a worse failure than an UPDATE/DELETE echoing the
        // connection's last one.
        let last_insert_rowid = if changes > 0 {
            let id = self.last_insert_rowid();
            (id != 0).then_some(id)
        } else {
            None
        };
        Ok(RunResult {
            cols,
            rows,
            rows_read,
            rows_written: changes,
            last_insert_rowid,
        })
    }

    async fn execute_batch(&mut self, sql: &str) -> Result<(), String> {
        Connection::execute_batch(self, sql).map_err(|e| e.to_string())
    }

    async fn describe(&mut self, sql: &str) -> Result<DescribeResult, String> {
        let stmt = self.prepare(sql).map_err(|e| e.to_string())?;
        let params: Vec<Option<String>> = (1..=stmt.parameter_count())
            .map(|i| stmt.parameter_name(i).map(|s| s.to_string()))
            .collect();
        let cols: Vec<Col> = stmt
            .columns()
            .iter()
            .map(|c| Col {
                name: c.name().to_string(),
                decltype: c.decl_type().map(|s| s.to_string()),
            })
            .collect();
        Ok(DescribeResult {
            params,
            cols,
            is_explain: stmt.is_explain() != 0,
            is_readonly: stmt.readonly(),
        })
    }

    async fn is_autocommit(&mut self) -> bool {
        Connection::is_autocommit(self)
    }
}
