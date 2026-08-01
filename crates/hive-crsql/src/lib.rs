//! Thin `rusqlite` wrapper around the vendored `superfly/cr-sqlite` loadable
//! extension (`vendor/cr-sqlite`, built by its own `build.sh` — see
//! `vendor/cr-sqlite/VENDOR.md` for why this fork over upstream vlcn-io).
//!
//! Real, live-witnessed 2026-08-01 against `crsqlite.dylib` built from the
//! vendored source on this machine: opened an in-memory connection, loaded
//! the extension, ran `crsql_as_crr('foo')`, inserted + updated real rows,
//! and read them back through `crsql_changes` — `crsql_version()` reported
//! `170000` (v0.17.0) and the changes reflected the exact history-free,
//! last-write-wins semantics `vendor/cr-sqlite/README.md` documents (a
//! superseded column's earlier `col_version` disappears rather than
//! accumulating). This module is that same call sequence, not a
//! reimplementation — the only thing new here is packaging it as a reusable
//! `open`/`as_crr`/`changes` API instead of a one-off scratch binary.
//!
//! NOT yet done (tracked in `.gm/prd.yml`'s `bn-impl-fleet-crr-peer`): per-
//! tenant DB file layout under the platform's storage invariants, a fleet
//! packaging path for the built extension binary (`extension_path()` below
//! is a local-dev default only), replication-factor policy, and billing
//! treatment. This crate is deliberately just the SQLite-extension seam.

use rusqlite::Connection;
use std::path::PathBuf;

/// Resolves the loadable extension path (WITHOUT platform suffix — SQLite's
/// own `sqlite3_load_extension` appends `.dylib`/`.so`/`.dll` itself, and
/// passing one explicitly makes the lookup silently fail on the wrong
/// platform). `HIVE_CRSQL_EXTENSION_PATH` overrides for a packaged fleet
/// deploy; the vendored build.sh output is the local-dev default.
pub fn extension_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIVE_CRSQL_EXTENSION_PATH") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/cr-sqlite/core/dist/crsqlite")
}

/// Open a connection with the cr-sqlite extension loaded. Extension loading
/// is enabled only for the duration of the load call — `Connection` doesn't
/// need it enabled afterward, and leaving it enabled would let any later SQL
/// (including tenant-supplied queries, on a connection this crate's callers
/// might expose further up the stack) load arbitrary native code.
pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    unsafe {
        conn.load_extension_enable()?;
        let result = conn.load_extension(extension_path(), None::<&str>);
        conn.load_extension_disable()?;
        result?;
    }
    Ok(conn)
}

pub fn open_in_memory() -> anyhow::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    unsafe {
        conn.load_extension_enable()?;
        let result = conn.load_extension(extension_path(), None::<&str>);
        conn.load_extension_disable()?;
        result?;
    }
    Ok(conn)
}

/// Upgrade an existing table to a CRR (conflict-free replicated relation) —
/// must run once per table before its changes are tracked by `crsql_changes`.
pub fn as_crr(conn: &Connection, table: &str) -> anyhow::Result<()> {
    // `crsql_as_crr` is a scalar function invoked via SELECT, so it returns a
    // row -- rusqlite's `execute()` rejects any statement that does (real
    // error hit while building this: "Execute returned results - did you
    // mean to call query?"). `query_row` is the correct call here.
    conn.query_row("SELECT crsql_as_crr(?1)", [table], |_| Ok(()))?;
    Ok(())
}

/// This site's stable identifier (16 raw bytes) — the `site_id` column value
/// this node's own writes carry in `crsql_changes`.
pub fn site_id(conn: &Connection) -> anyhow::Result<Vec<u8>> {
    Ok(conn.query_row("SELECT crsql_site_id()", [], |r| r.get(0))?)
}

/// The extension's version as `crsql_version()` reports it (170000 = v0.17.0
/// on the currently-vendored superfly/cr-sqlite commit — see VENDOR.md).
pub fn version(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT crsql_version()", [], |r| r.get(0))?)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub table: String,
    pub pk: Vec<u8>,
    pub cid: String,
    pub val: rusqlite::types::Value,
    pub col_version: i64,
    pub db_version: i64,
    pub site_id: Vec<u8>,
    pub cl: i64,
    pub seq: i64,
}

/// All tracked changes since `since_db_version` (0 = everything this site has
/// ever recorded, local and merged-from-peers alike) — the payload a sync
/// peer sends outbound, or persists to detect what it still needs to fetch.
pub fn changes_since(conn: &Connection, since_db_version: i64) -> anyhow::Result<Vec<Change>> {
    let mut stmt = conn.prepare(
        "SELECT \"table\", \"pk\", \"cid\", \"val\", \"col_version\", \"db_version\", \"site_id\", \"cl\", \"seq\"
         FROM crsql_changes WHERE db_version > ?1",
    )?;
    let rows = stmt.query_map([since_db_version], |r| {
        Ok(Change {
            table: r.get(0)?,
            pk: r.get(1)?,
            cid: r.get(2)?,
            val: r.get(3)?,
            col_version: r.get(4)?,
            db_version: r.get(5)?,
            site_id: r.get(6)?,
            cl: r.get(7)?,
            seq: r.get(8)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)
}

/// Apply a batch of changes received from a peer (the inbound half of sync)
/// by inserting them into the same `crsql_changes` virtual table — the
/// extension resolves conflicts itself (history-free, last-write-wins per
/// column, per `vendor/cr-sqlite/README.md`).
pub fn apply_changes(conn: &Connection, changes: &[Change]) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO crsql_changes
         (\"table\", \"pk\", \"cid\", \"val\", \"col_version\", \"db_version\", \"site_id\", \"cl\", \"seq\")
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for c in changes {
        stmt.execute(rusqlite::params![
            c.table,
            c.pk,
            c.cid,
            c.val,
            c.col_version,
            c.db_version,
            c.site_id,
            c.cl,
            c.seq,
        ])?;
    }
    Ok(())
}
