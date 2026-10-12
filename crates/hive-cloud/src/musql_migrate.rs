//! musql migration: convert one managed SQLite database from a `.db` file to a
//! `.musq` file, prove it row-for-row, and be able to go back.
//!
//! # The rule this module exists to enforce
//!
//! **No original file is deleted, truncated or rewritten in place until its
//! migrated copy has been verified against it.** Every step is therefore
//! ordered: pre-flight the source, quiesce it, COPY it, convert the DATABASE
//! (not the copy — the copy is the insurance), then compare the two engines
//! table by table, and only then let the record point at the new engine.
//!
//! # Why conversion is safe even when a tenant's data is only in the WAL
//!
//! Measured, not assumed: `musql-convert import` reads an un-checkpointed
//! `-wal`. A source built with 500 inserts and 71 deletes, killed without
//! closing so the main file was 4096 B and the WAL held every committed row,
//! converted to a 429-row `.musq` whose canonical digest matched SQLite's
//! exactly — rows, `typeof` per value and rowids all preserved.
//!
//! # The one thing that can stop a migration
//!
//! A non-finite REAL. SQLite stores `Inf`/`-Inf`/`NaN`; musql stores the value
//! but returns it as `{"type":"null"}` on the wire, where Hive today raises a
//! loud, column-naming error. That is an observable change for that tenant, so
//! [`preflight`] counts them per table/column and migration REFUSES a database
//! holding one. Refusing a migration is recoverable; silently turning a
//! tenant's `Inf` into `NULL` is not.

use crate::musql::{convert_bin, Engine, MusqlEngine};
use crate::sqlite_engine::{Budget, SqlEngine, SqlValue};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Backups live beside the store, never inside it: `musqld -dir` serves every
/// `.musq` in `sqlite-dbs`, and a backup directory inside it would either be
/// ignored (fine) or, if it ever held a `.musq`, be served as a database.
pub fn backup_root() -> PathBuf {
    crate::persist::data_dir().join("sqlite-dbs-backup")
}

/// A budget for verification reads only: a migration must see every row, and
/// the Hrana pipeline cap (20k rows) exists to bound a CLIENT's response, not
/// an operator's integrity check.
fn verify_budget() -> Budget {
    Budget {
        max_rows: usize::MAX / 4,
        max_bytes: usize::MAX / 4,
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Canonical rendering of one row: `typeof` first, so two engines that agree on
/// a value's bytes but disagree on its type cannot produce the same digest.
fn row_string(row: &[SqlValue]) -> String {
    let mut out = String::new();
    for v in row {
        out.push_str(v.typeof_name());
        out.push('|');
        match v {
            SqlValue::Null => out.push_str("NULL"),
            SqlValue::Integer(i) => out.push_str(&i.to_string()),
            SqlValue::Real(f) => out.push_str(&format!("{f:?}")),
            SqlValue::Text(s) => out.push_str(s),
            SqlValue::Blob(b) => out.push_str(&b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        }
        out.push('\u{1f}');
    }
    out
}

fn digest_rows(rows: &[String]) -> String {
    let mut h = Sha256::new();
    for r in rows {
        h.update(r.as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

/// Every row of one table, in a stable order. `ORDER BY rowid` when the table
/// has one; a `WITHOUT ROWID` table has no rowid, so its rows are SORTED before
/// digesting rather than compared in whatever order each engine happened to
/// return them — an unsorted comparison would report divergence between two
/// identical databases.
async fn dump<E: SqlEngine>(eng: &mut E, table: &str) -> Result<(Vec<String>, bool), String> {
    let budget = verify_budget();
    let ordered = format!("SELECT * FROM {} ORDER BY rowid", quote_ident(table));
    match eng.run(&ordered, true, &[], &[], budget).await {
        Ok(r) => Ok((r.rows.iter().map(|row| row_string(row)).collect(), true)),
        Err(_) => {
            let r = eng
                .run(
                    &format!("SELECT * FROM {}", quote_ident(table)),
                    true,
                    &[],
                    &[],
                    budget,
                )
                .await?;
            let mut rows: Vec<String> = r.rows.iter().map(|row| row_string(row)).collect();
            rows.sort();
            Ok((rows, false))
        }
    }
}

/// User tables only: `sqlite_*` internals are engine-private and differ by
/// construction, so comparing them would report a divergence that is not one.
async fn tables<E: SqlEngine>(eng: &mut E) -> Result<Vec<String>, String> {
    let r = eng
        .run(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            true,
            &[],
            &[],
            verify_budget(),
        )
        .await?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect())
}

// ---- Pre-flight ------------------------------------------------------------

pub struct NonFinite {
    pub table: String,
    pub column: String,
    pub count: u64,
}

pub struct Preflight {
    pub integrity: String,
    pub tables: Vec<String>,
    pub non_finite: Vec<NonFinite>,
}

impl Preflight {
    pub fn json(&self) -> Value {
        json!({
            "integrity_check": self.integrity,
            "tables": self.tables,
            "non_finite": self.non_finite.iter().map(|n| json!({
                "table": n.table, "column": n.column, "count": n.count,
            })).collect::<Vec<Value>>(),
        })
    }
}

/// Read-only inspection of a rusqlite database before anything is copied.
/// `integrity_check` must be `ok`: `musql-convert` refuses a corrupt source with
/// `ErrSourceCorrupt`, and `-force` would push through a broken file, so the
/// only correct move is to stop here and say why.
pub async fn preflight(id: &str) -> Result<Preflight, String> {
    let path = crate::databases::sqlite_file_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    tokio::task::spawn_blocking(move || -> Result<Preflight, String> {
        let conn = hive_crsql::rusqlite::Connection::open_with_flags(
            &path,
            hive_crsql::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("cannot open the source database: {e}"))?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| format!("integrity_check failed: {e}"))?;
        let mut names: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .map_err(|e| e.to_string())?;
            let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                names.push(r.get::<_, String>(0).map_err(|e| e.to_string())?);
            }
        }
        let mut non_finite = Vec::new();
        for t in &names {
            let cols: Vec<String> = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info({})", quote_ident(t)))
                    .map_err(|e| e.to_string())?;
                let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
                let mut c = Vec::new();
                while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                    c.push(r.get::<_, String>(1).map_err(|e| e.to_string())?);
                }
                c
            };
            for col in cols {
                // text renders Inf/-Inf/NaN as exactly those three strings and
                // nothing else does, so this is the scan that separates "this
                // database cannot migrate yet" from "it can".
                let sql = format!(
                    "SELECT count(*) FROM {} WHERE CAST({} AS TEXT) IN ('Inf','-Inf','NaN')",
                    quote_ident(t),
                    quote_ident(&col)
                );
                let n: i64 = conn
                    .query_row(&sql, [], |r| r.get(0))
                    .map_err(|e| format!("non-finite scan of {t}.{col} failed: {e}"))?;
                if n > 0 {
                    non_finite.push(NonFinite {
                        table: t.clone(),
                        column: col,
                        count: n as u64,
                    });
                }
            }
        }
        Ok(Preflight {
            integrity,
            tables: names,
            non_finite,
        })
    })
    .await
    .map_err(|e| format!("preflight task failed: {e}"))?
}

// ---- Backup ----------------------------------------------------------------

/// Crash-safe copy of the source: the `.db` AND its `-wal`/`-shm`, because a
/// live database's committed rows can live only in the WAL, and a backup of the
/// main file alone would be a backup of a stale snapshot.
fn backup(id: &str) -> Result<PathBuf, String> {
    let src = crate::databases::sqlite_file_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    let dir = backup_root().join(format!("{}-{}", id, hive_core::now_ms()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create backup dir: {e}"))?;
    let mut copied = 0usize;
    for suffix in ["", "-wal", "-shm"] {
        let from = if suffix.is_empty() {
            src.clone()
        } else {
            PathBuf::from(format!("{}{suffix}", src.display()))
        };
        if !from.exists() {
            continue;
        }
        let name = from
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        std::fs::copy(&from, dir.join(name)).map_err(|e| {
            format!("cannot back up {}: {e} — refusing to migrate without a verified copy", from.display())
        })?;
        copied += 1;
    }
    if copied == 0 {
        return Err(format!(
            "nothing to back up at {} — refusing to migrate a database with no file",
            src.display()
        ));
    }
    Ok(dir)
}

// ---- Convert + verify ------------------------------------------------------

pub struct TableCheck {
    pub table: String,
    pub source_rows: usize,
    pub target_rows: usize,
    pub source_digest: String,
    pub target_digest: String,
    pub ordered: bool,
    pub ok: bool,
}

pub struct Report {
    pub db_id: String,
    pub engine: &'static str,
    pub backup_dir: String,
    pub musq_bytes: u64,
    pub tables: Vec<TableCheck>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.tables.iter().all(|t| t.ok)
    }
    pub fn json(&self) -> Value {
        json!({
            "db_id": self.db_id,
            "engine": self.engine,
            "backup_dir": self.backup_dir,
            "musq_bytes": self.musq_bytes,
            "ok": self.ok(),
            "tables": self.tables.iter().map(|t| json!({
                "table": t.table,
                "source_rows": t.source_rows,
                "target_rows": t.target_rows,
                "source_digest": t.source_digest,
                "target_digest": t.target_digest,
                "ordered": t.ordered,
                "ok": t.ok,
            })).collect::<Vec<Value>>(),
        })
    }
}

fn run_convert(args: &[&str]) -> Result<String, String> {
    let bin = convert_bin();
    let out = std::process::Command::new(&bin)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {bin}: {e} (set HIVE_MUSQL_CONVERT_BIN)"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("{bin} {} failed: {text}", args.join(" ")));
    }
    Ok(text)
}

/// Convert `id` to musql and prove it. Steps, in order:
/// pre-flight -> refuse new writers -> quiesce -> back up -> convert -> verify
/// -> (caller flips the record). The record flip is deliberately NOT here: a
/// migration that has not been verified must not be able to make itself live.
///
/// `live_is_musql` says whether the record ALREADY points at musql, i.e.
/// whether the `.musq` on disk is the live database or only a staged artifact.
pub async fn migrate(id: &str, live_is_musql: bool) -> Result<Report, String> {
    let pre = preflight(id).await?;
    if pre.integrity != "ok" {
        return Err(format!(
            "refusing to migrate {id}: PRAGMA integrity_check says {:?}",
            pre.integrity
        ));
    }
    if !pre.non_finite.is_empty() {
        let where_: Vec<String> = pre
            .non_finite
            .iter()
            .map(|n| format!("{}.{} ({} rows)", n.table, n.column, n.count))
            .collect();
        return Err(format!(
            "refusing to migrate {id}: it holds non-finite REAL values, which musql returns as \
             NULL on the wire while SQLite returns Inf/-Inf — {}; cast or clean them, then retry",
            where_.join(", ")
        ));
    }

    let src = crate::databases::sqlite_file_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    let dst = crate::databases::sqlite_musql_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    if dst.exists() && live_is_musql {
        return Err(format!(
            "{} already exists and this database is ALREADY served by musql — overwriting the \
             live file would replace tenant data with a re-conversion of a stale snapshot; roll \
             back to rusqlite first if that is what you want",
            dst.display()
        ));
    }

    // CONVERSION WINDOW. Held until this function returns, by `Drop`: from here
    // on no new Hrana stream can open this database, so nothing can write to the
    // source after the snapshot below and be lost when the record flips.
    // `close_streams` + `sqlite_pool::close` alone only quiesce what is already
    // open — a client arriving a second later reopens the file.
    let _converting = crate::hrana::begin_conversion(id);

    // QUIESCE. Every stream holds a pooled connection and one mid-`BEGIN` holds
    // the file's write lock; closing them checkpoints and removes `-wal`, so
    // what is converted is a self-contained file.
    crate::hrana::close_streams(id);
    crate::sqlite_pool::close(id);

    let backup_dir = backup(id)?;

    // A `.musq` already on disk while the record still says rusqlite is a STAGED
    // artifact from an earlier run, not the live database: nothing serves it, so
    // it is moved aside into this run's backup (never deleted) and re-converted
    // from the current source. Refusing here blocked every real flip, because a
    // prior verified run had already staged all of them.
    if dst.exists() {
        let staged = backup_dir.join(format!(
            "{}.staged-{}",
            crate::databases::sqlite_musql_file_name(id),
            hive_core::now_ms()
        ));
        std::fs::rename(&dst, &staged).map_err(|e| {
            format!("cannot move the staged {} aside: {e}", dst.display())
        })?;
        tracing::warn!(
            db = %id,
            staged = %staged.display(),
            "a staged .musq was already present; moved it aside and re-converting from the source"
        );
    }

    // A `.musq.delta` beside it is the same stale artifact, and leaving one
    // behind makes the import fail outright — measured live, flipping a database
    // back to musql after a rollback:
    //   `musql-convert: sqlite: the source database fails integrity_check:
    //    engine: segment delta was built against a different segment file
    //    (delta was built for counter 2 pages 1; this file is counter 1 pages 2)`
    // i.e. the delta still describes the segment that was just moved aside.
    // Move them into this run's backup as well (never delete): the destination
    // must be a clean one, and everything removed is inside `backup_dir`.
    let stamped = hive_core::now_ms();
    for suffix in [".lock", ".delta"] {
        let from = PathBuf::from(format!("{}{suffix}", dst.display()));
        if !from.exists() {
            continue;
        }
        let to = backup_dir.join(format!(
            "{}{suffix}.staged-{stamped}",
            crate::databases::sqlite_musql_file_name(id)
        ));
        if let Err(e) = std::fs::rename(&from, &to) {
            return Err(format!(
                "cannot move the stale {} aside: {e}",
                from.display()
            ));
        }
        tracing::warn!(db = %id, moved = %to.display(), "a stale musql sidecar was present; moved it aside before converting");
    }

    let convert = {
        let src = src.clone();
        let dst = dst.clone();
        tokio::task::spawn_blocking(move || {
            run_convert(&[
                "import",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ])
        })
        .await
        .map_err(|e| format!("convert task failed: {e}"))?
    };
    convert.map_err(|e| format!("migration of {id} failed (backup kept at {}): {e}", backup_dir.display()))?;
    let musq_bytes = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    // musql keeps a 0-byte `.musq.lock` beside the database and, transiently, a
    // `.musq.delta`. Which states leave committed data in the delta is not
    // documented, so the backup rule is: keep `.musq` AND every `*.musq.*`
    // sidecar, and verify by OPENING the database, never by comparing bytes.
    if let Err(e) = backup_musq_sidecars(id, &backup_dir) {
        tracing::warn!(db = %id, error = %e, "could not back up musql sidecar files");
    }

    let mut checks = Vec::new();
    {
        let mut target = MusqlEngine::connect(reqwest::Client::new(), id)
            .await
            .map_err(|e| format!("cannot open the migrated database: {e}"))?;
        let src2 = src.clone();
        let tables = tables(&mut target).await?;
        let mut source = tokio::task::spawn_blocking(move || {
            hive_crsql::rusqlite::Connection::open_with_flags(
                &src2,
                hive_crsql::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|e| format!("cannot reopen the source for verification: {e}"))
        })
        .await
        .map_err(|e| format!("verification task failed: {e}"))??;

        for t in tables {
            let (s_rows, ordered) = dump(&mut source, &t).await?;
            let (t_rows, _) = dump(&mut target, &t).await?;
            let s_digest = digest_rows(&s_rows);
            let t_digest = digest_rows(&t_rows);
            checks.push(TableCheck {
                table: t,
                source_rows: s_rows.len(),
                target_rows: t_rows.len(),
                ok: s_digest == t_digest && s_rows.len() == t_rows.len(),
                source_digest: s_digest,
                target_digest: t_digest,
                ordered,
            });
        }
        // A table present in the source but missing from the target is the
        // failure a per-table walk cannot see: it just never gets compared.
        let source_tables: Vec<String> = {
            let mut stmt = source
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .map_err(|e| e.to_string())?;
            let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
            let mut v = Vec::new();
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                v.push(r.get::<_, String>(0).map_err(|e| e.to_string())?);
            }
            v
        };
        for t in source_tables {
            if !checks.iter().any(|c| c.table == t) {
                checks.push(TableCheck {
                    table: t,
                    source_rows: 0,
                    target_rows: 0,
                    source_digest: String::new(),
                    target_digest: String::new(),
                    ordered: false,
                    ok: false,
                });
            }
        }
    }

    let report = Report {
        db_id: id.to_string(),
        engine: Engine::Musql.label(),
        backup_dir: backup_dir.display().to_string(),
        musq_bytes,
        tables: checks,
    };
    if !report.ok() {
        return Err(format!(
            "migration of {id} VERIFIED AS DIVERGENT — the record still points at rusqlite and \
             the original file is untouched: {}",
            report.json()
        ));
    }
    Ok(report)
}

fn backup_musq_sidecars(id: &str, dir: &Path) -> Result<(), String> {
    let dst = crate::databases::sqlite_musql_path(id)
        .ok_or_else(|| "bad id".to_string())?;
    for suffix in [".lock", ".delta"] {
        let from = PathBuf::from(format!("{}{suffix}", dst.display()));
        if !from.exists() {
            continue;
        }
        let name = from
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        std::fs::copy(&from, dir.join(name)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Roll back: `musql-convert export` the `.musq` to a NEW file, verify it the
/// same way, and only then put it in place. The pre-migration `.db` is renamed
/// aside, never deleted, so the rollback is itself reversible.
pub async fn rollback(id: &str) -> Result<Report, String> {
    let live = crate::databases::sqlite_file_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    let musq = crate::databases::sqlite_musql_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    if !musq.exists() {
        return Err(format!("{} does not exist — nothing to roll back", musq.display()));
    }
    // Same conversion window as `migrate`: the live file is being replaced at
    // the end of this function, so nothing may be writing to it in between.
    let _converting = crate::hrana::begin_conversion(id);
    crate::hrana::close_streams(id);
    crate::sqlite_pool::close(id);

    let dir = backup_root().join(format!("{}-rollback-{}", id, hive_core::now_ms()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create backup dir: {e}"))?;
    let restored = dir.join(crate::databases::sqlite_file_name(id));
    {
        let musq = musq.clone();
        let restored = restored.clone();
        tokio::task::spawn_blocking(move || {
            run_convert(&[
                "export",
                &musq.to_string_lossy(),
                &restored.to_string_lossy(),
            ])
        })
        .await
        .map_err(|e| format!("export task failed: {e}"))??;
    }

    // Verify the export BEFORE it becomes the live file.
    let mut target = {
        let restored_name = restored.clone();
        tokio::task::spawn_blocking(move || {
            hive_crsql::rusqlite::Connection::open(&restored_name)
                .map_err(|e| format!("cannot open the exported database: {e}"))
        })
        .await
        .map_err(|e| format!("verification task failed: {e}"))??
    };
    let mut source = MusqlEngine::connect(reqwest::Client::new(), id)
        .await
        .map_err(|e| format!("cannot open the musql database: {e}"))?;
    let names = {
        let mut stmt = target
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            v.push(r.get::<_, String>(0).map_err(|e| e.to_string())?);
        }
        v
    };
    let mut checks = Vec::new();
    for t in names {
        let (s_rows, _) = dump(&mut source, &t).await?;
        let (t_rows, ordered) = dump(&mut target, &t).await?;
        let s_digest = digest_rows(&s_rows);
        let t_digest = digest_rows(&t_rows);
        checks.push(TableCheck {
            table: t,
            source_rows: s_rows.len(),
            target_rows: t_rows.len(),
            ok: s_digest == t_digest && s_rows.len() == t_rows.len(),
            source_digest: s_digest,
            target_digest: t_digest,
            ordered,
        });
    }
    let report = Report {
        db_id: id.to_string(),
        engine: Engine::Rusqlite.label(),
        backup_dir: dir.display().to_string(),
        musq_bytes: std::fs::metadata(&musq).map(|m| m.len()).unwrap_or(0),
        tables: checks,
    };
    if !report.ok() {
        return Err(format!(
            "rollback of {id} VERIFIED AS DIVERGENT — the live file was NOT replaced: {}",
            report.json()
        ));
    }
    // Only now does the live file move: the old one is renamed aside first, so a
    // crash between the two operations leaves both files present and neither
    // lost.
    if live.exists() {
        let aside = dir.join(format!("{}.pre-musql", crate::databases::sqlite_file_name(id)));
        std::fs::rename(&live, &aside)
            .map_err(|e| format!("cannot move the live file aside: {e}"))?;
    }
    std::fs::copy(&restored, &live).map_err(|e| format!("cannot install the rolled-back file: {e}"))?;
    Ok(report)
}

/// Provision a NEW musql database: build an empty SQLite file and convert it, so
/// the `.musq` exists before any request can reach it. (musqld's `-create` would
/// also do this, but it creates a database for ANY unknown name, which is the
/// silent-empty-database hazard; provisioning is deliberate, `-create` is not.)
pub async fn provision(id: &str) -> Result<(), String> {
    let dst = crate::databases::sqlite_musql_path(id)
        .ok_or_else(|| "database id is not a platform-issued id".to_string())?;
    if dst.exists() {
        return Ok(());
    }
    let dir = crate::databases::sqlite_store_dir();
    let seed = dir.join(format!(".seed-{}.db", crate::git::sanitize_tag(id)));
    let musq_name = crate::databases::sqlite_musql_file_name(id);
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&seed);
        {
            let conn = hive_crsql::rusqlite::Connection::open(&seed).map_err(|e| e.to_string())?;
            let _: String = conn
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
        }
        let out = dir.join(&musq_name);
        run_convert(&["import", &seed.to_string_lossy(), &out.to_string_lossy()])?;
        let _ = std::fs::remove_file(&seed);
        Ok(())
    })
    .await
    .map_err(|e| format!("musql provisioning task failed: {e}"))?
}

/// Used by the operator endpoint to report what a migration WOULD do.
pub async fn plan(id: &str) -> Result<Value, String> {
    let pre = preflight(id).await?;
    Ok(json!({
        "db_id": id,
        "preflight": pre.json(),
        "blocked": !pre.non_finite.is_empty() || pre.integrity != "ok",
    }))
}

// ---- Operator diagnostic ---------------------------------------------------

/// `--sqlite-engine-probe`: run the SAME Hrana pipeline through BOTH engines on
/// the SAME data and print both envelopes, so "the musql swap is wire-identical"
/// is a measurement and not a claim.
///
/// This exists because asking the question with a second implementation is how
/// a diagnostic and the decision it informs quietly diverge (the reason
/// `--dns-probe` runs `dns_probe::probe_nameserver` rather than its own probe).
/// Here that means: the pipeline below is executed by
/// `hrana_proto::execute_pipeline` — the production function — once against
/// `rusqlite` and once against `musql`, and the two JSON envelopes are diffed.
///
/// Requires `HIVE_SQLITE_ENGINE=musql`, `HIVE_MUSQLD_BIN`, `HIVE_MUSQL_CONVERT_BIN`
/// and a scratch `HIVE_DATA`. It touches nothing but that scratch directory.
pub async fn run_probe() -> anyhow::Result<()> {
    use crate::hrana_proto::execute_pipeline;

    let id = "db_6b1e261dc28a47eaaec0364c8f440591";
    let src = crate::databases::sqlite_file_path(id)
        .ok_or_else(|| anyhow::anyhow!("bad probe id"))?;
    let dst = crate::databases::sqlite_musql_path(id)
        .ok_or_else(|| anyhow::anyhow!("bad probe id"))?;
    if crate::musql::default_engine() != Engine::Musql {
        anyhow::bail!("run this with HIVE_SQLITE_ENGINE=musql");
    }
    std::fs::create_dir_all(crate::databases::sqlite_store_dir())?;
    for p in [
        src.clone(),
        PathBuf::from(format!("{}-wal", src.display())),
        PathBuf::from(format!("{}-shm", src.display())),
        dst.clone(),
        PathBuf::from(format!("{}.lock", dst.display())),
        PathBuf::from(format!("{}.delta", dst.display())),
    ] {
        let _ = std::fs::remove_file(p);
    }

    // 1. A source database with every value type plus deletions, so a migration
    //    that dropped a type or a rowid would show up in the digest.
    {
        let src = src.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = hive_crsql::rusqlite::Connection::open(&src)?;
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
            conn.execute_batch(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, score REAL, blob BLOB, note TEXT);",
            )?;
            for i in 1..=50i64 {
                conn.execute(
                    "INSERT INTO users(id,name,score,blob,note) VALUES(?1,?2,?3,?4,?5)",
                    hive_crsql::rusqlite::params![
                        i,
                        format!("n{i}"),
                        i as f64 * 1.5,
                        vec![(i % 256) as u8, 1u8, 2u8],
                        if i % 3 == 0 {
                            None
                        } else {
                            Some(format!("note-{i}"))
                        },
                    ],
                )?;
            }
            conn.execute("DELETE FROM users WHERE id % 7 = 0", [])?;
            Ok(())
        })
        .await??;
    }

    // 2. Snapshot the PRISTINE source. The pipeline below MUTATES (insert,
    //    update, delete, create table), so running it twice against the same
    //    file would compare two different databases. Both engines must start
    //    from these exact bytes; the snapshot is what makes that true.
    let pristine = {
        let src = src.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            {
                let conn = hive_crsql::rusqlite::Connection::open(&src)?;
                // Checkpoint so the file alone is the whole database. (It
                // returns three INTEGER columns, so it is a batch, not a
                // one-row string query — unlike `journal_mode`.)
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            }
            Ok(std::fs::read(&src)?)
        })
        .await??
    };
    let restore = |bytes: &[u8]| -> anyhow::Result<()> {
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                src.clone()
            } else {
                PathBuf::from(format!("{}{suffix}", src.display()))
            };
            let _ = std::fs::remove_file(p);
        }
        std::fs::write(&src, bytes)?;
        Ok(())
    };

    // 3. The pipeline. Every request type the Hrana surface serves, including a
    //    v3-only one and a statement that must fail.
    let requests: Vec<Value> = vec![
        json!({"type":"execute","stmt":{"sql":"SELECT id,name,score,blob,note FROM users WHERE id<=3 ORDER BY id","args":[],"want_rows":true}}),
        json!({"type":"execute","stmt":{"sql":"SELECT count(*) c, min(id) lo, max(id) hi FROM users","args":[],"want_rows":true}}),
        json!({"type":"execute","stmt":{"sql":"INSERT INTO users(id,name,score,blob,note) VALUES(900,'ins',1.25,X'010203',NULL)","args":[],"want_rows":true}}),
        json!({"type":"execute","stmt":{"sql":"UPDATE users SET name='upd' WHERE id>40","args":[],"want_rows":false}}),
        json!({"type":"execute","stmt":{"sql":"DELETE FROM users WHERE id=1","args":[],"want_rows":false}}),
        json!({"type":"execute","stmt":{"sql":"CREATE TABLE probe_t(a TEXT)","args":[],"want_rows":false}}),
        json!({"type":"execute","stmt":{"sql":"INSERT INTO probe_t(a) VALUES(?)","args":[{"type":"text","value":"bound"}],"want_rows":true}}),
        json!({"type":"execute","stmt":{"sql":"INSERT INTO users(id) VALUES(2)","args":[],"want_rows":true}}),
        json!({"type":"describe","sql":"SELECT id,name FROM users WHERE id=?"}),
        json!({"type":"sequence","sql":"CREATE TABLE probe_s(x); INSERT INTO probe_s VALUES(7);"}),
        json!({"type":"batch","batch":{"steps":[
            {"stmt":{"sql":"INSERT INTO probe_s(x) VALUES(?1)","args":[{"type":"integer","value":"11"}],"want_rows":true}},
            {"condition":{"type":"ok","step":0},"stmt":{"sql":"INSERT INTO probe_s(x) VALUES(?1)","args":[{"type":"integer","value":"12"}],"want_rows":true}},
            {"condition":{"type":"is_autocommit"},"stmt":{"sql":"SELECT count(*) c FROM probe_s","args":[],"want_rows":true}}
        ]}}),
        // `rows_read` for a row-returning statement the caller did NOT ask rows
        // for: the rusqlite engine steps it and counts it, so musql must too.
        json!({"type":"execute","stmt":{"sql":"SELECT count(*) AS c FROM users","args":[],"want_rows":false}}),
        // A computed non-finite: SQLite yields Inf, `hrana_proto::encode_value`
        // refuses to encode it, and musql must raise the SAME error rather than
        // hand back the NULL its row encoder substitutes.
        json!({"type":"execute","stmt":{"sql":"SELECT 1e308*10 AS overflow","args":[],"want_rows":true}}),
        // Negative zero as a literal (the stored-value case is a documented
        // musqld normalisation, not something this lane can see here).
        json!({"type":"execute","stmt":{"sql":"SELECT -0.0 AS nz","args":[],"want_rows":true}}),
        json!({"type":"store_sql","sql_id":7,"sql":"SELECT 42 AS answer"}),
        json!({"type":"execute","stmt":{"sql_id":7,"args":[],"want_rows":true}}),
        json!({"type":"close_sql","sql_id":7}),
        json!({"type":"get_autocommit"}),
    ];

    // 4. rusqlite pass.
    let before = {
        let src = src.clone();
        let requests = requests.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
            let mut conn = hive_crsql::rusqlite::Connection::open(&src)?;
            let out = futures::executor::block_on(execute_pipeline(
                &mut conn,
                requests,
                crate::hrana_proto::SqlCache::new(),
                3,
                false,
            ));
            Ok(out.results)
        })
        .await??
    };

    // 5. Put the source back exactly as it was, so the migration converts the
    //    same bytes the rusqlite pass started from.
    restore(&pristine)?;

    // 6. Start the sidecar and convert + verify. The conversion window is
    //    watched from another task: `migrate` must hold it for the whole
    //    conversion, because that is the only thing stopping a client from
    //    reopening the file after the snapshot and losing the write.
    let watching = id.to_string();
    let window_watch = tokio::spawn(async move {
        for _ in 0..2000 {
            if crate::hrana::is_quiescing(&watching) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    });
    if !crate::musql::spawn() {
        anyhow::bail!("musqld is not usable here (check HIVE_MUSQLD_BIN)");
    }
    for _ in 0..80 {
        if crate::musql::ready() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let report = migrate(id, false)
        .await
        .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;

    // 6b. Re-run the migration with the `.musq` from the pass above STILL ON
    //     DISK. This is the case that used to be refused outright ("refusing to
    //     overwrite a musql database"), which meant a database that had ever
    //     been staged could never actually be flipped. A staged file is not a
    //     live one — the record still points at rusqlite — so it is moved aside
    //     and re-converted, and this proves the flip is runnable.
    let staged = crate::databases::sqlite_musql_path(id)
        .map(|p| p.exists())
        .unwrap_or(false);
    let report2 = migrate(id, false)
        .await
        .map_err(|e| anyhow::anyhow!("re-migration over a staged .musq failed: {e}"))?;
    println!("\n--- re-migration with a staged .musq already present ---");
    println!("staged .musq present before this pass: {staged}");
    println!("{}", serde_json::to_string_pretty(&report2.json())?);
    if !staged || !report2.ok() {
        anyhow::bail!("the operator flip is not runnable over a staged .musq");
    }
    let window_held = window_watch.await.unwrap_or(false);
    println!("\n--- conversion window ---");
    println!("new Hrana streams refused for the whole conversion: {window_held}");
    if !window_held {
        anyhow::bail!("the conversion window was never held; a write could land after the snapshot");
    }

    // 7. musql pass, on the migrated file.
    let mut eng = MusqlEngine::connect(reqwest::Client::new(), id)
        .await
        .map_err(|e| anyhow::anyhow!("cannot open the migrated database: {e}"))?;
    let after = execute_pipeline(&mut eng, requests, crate::hrana_proto::SqlCache::new(), 3, false)
        .await
        .results;

    // 8b. Rollback, in the real direction and on the real files. It is the half
    //     of this swap that has only ever been written, never run: export the
    //     `.musq` back to SQLite, verify it against the engine BEFORE it
    //     becomes the live file, install it, then prove the file an app would
    //     open is the same database the engine still serves.
    let rolled = rollback(id)
        .await
        .map_err(|e| anyhow::anyhow!("rollback failed: {e}"))?;
    println!("\n--- rollback (musql -> rusqlite), verified before install ---");
    println!("{}", serde_json::to_string_pretty(&rolled.json())?);
    if !rolled.ok() {
        anyhow::bail!("rollback verified as divergent");
    }
    let live_path = crate::databases::sqlite_file_path(id)
        .ok_or_else(|| anyhow::anyhow!("bad probe id"))?;
    let mut live = tokio::task::spawn_blocking(move || {
        hive_crsql::rusqlite::Connection::open_with_flags(
            &live_path,
            hive_crsql::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| anyhow::anyhow!("cannot reopen the rolled-back file: {e}"))
    })
    .await??;
    let mut engine = MusqlEngine::connect(reqwest::Client::new(), id)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reopen the musql database: {e}"))?;
    let mut post = Vec::new();
    let names = tables(&mut engine)
        .await
        .map_err(|e| anyhow::anyhow!("cannot list the engine's tables: {e}"))?;
    for t in names {
        let (l, _) = dump(&mut live, &t)
            .await
            .map_err(|e| anyhow::anyhow!("cannot read {t} from the rolled-back file: {e}"))?;
        let (m, _) = dump(&mut engine, &t)
            .await
            .map_err(|e| anyhow::anyhow!("cannot read {t} from the engine: {e}"))?;
        let (ld, md) = (digest_rows(&l), digest_rows(&m));
        post.push(json!({"table": t, "live_rows": l.len(), "musql_rows": m.len(),
                         "live_digest": ld, "musql_digest": md, "match": ld == md}));
        if ld != md {
            anyhow::bail!("after rollback the live file does not match the engine: {t}");
        }
    }
    println!("\n--- app-visible data after rollback (rusqlite file vs musql engine) ---");
    println!("{}", serde_json::to_string_pretty(&post)?);

    // 8c. Diff. `query_duration_ms` is wall-clock and is the one field that must
    //    differ between two runs; everything else is the contract. Walked
    //    recursively so a nested result (batch `step_results`) is covered too.
    fn strip(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| k.as_str() != "query_duration_ms")
                    .map(|(k, v)| (k.clone(), strip(v)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(strip).collect()),
            other => other.clone(),
        }
    }
    let (a, b) = (strip(&Value::Array(before.clone())), strip(&Value::Array(after.clone())));
    let identical = a == b;

    println!("=== musql engine probe ===");
    println!("store:   {}", crate::databases::sqlite_store_dir().display());
    println!("sidecar: {}", crate::musql::status_json());
    println!("\n--- migration (verified row-for-row vs the backup) ---");
    println!("{}", serde_json::to_string_pretty(&report.json())?);
    println!("\n--- rusqlite envelope ({} results) ---", before.len());
    println!("{}", serde_json::to_string_pretty(&a)?);
    println!("\n--- musql envelope ({} results) ---", after.len());
    println!("{}", serde_json::to_string_pretty(&b)?);
    println!("\nIDENTICAL (query_duration_ms excluded): {identical}");
    if !identical {
        for (i, (x, y)) in before.iter().zip(after.iter()).enumerate() {
            if strip(x) != strip(y) {
                println!("  result[{i}] differs:\n    rusqlite: {}\n    musql:    {}", strip(x), strip(y));
            }
        }
        anyhow::bail!("the two engines produced different Hrana envelopes");
    }

    // 9. The exported env surface for this kind — names and shapes an app reads.
    let mut conn = std::collections::HashMap::new();
    conn.insert("LIBSQL_URL".into(), "libsql://api.example.host/v1/sqlite/".to_string() + id + "/");
    conn.insert("DB_REST_TOKEN".into(), "dbt_probe".into());
    conn.insert("engine".into(), Engine::Musql.label().into());
    let db = crate::databases::Database {
        id: id.to_string(),
        name: "probe".into(),
        project: "probe".into(),
        team: String::new(),
        kind: crate::databases::DbKind::Sqlite,
        region: "probe".into(),
        status: crate::databases::DbStatus::Ready,
        provider: String::new(),
        mode: "live".into(),
        created_ms: 0,
        connection: conn,
        container: None,
        note: String::new(),
        replicas: vec![],
        role: "primary".into(),
        primary_node: String::new(),
        db_host: String::new(),
        host_node: "probe".into(),
    };
    // The env surface an app reads is produced by `env_exports`, which does not
    // read `engine` at all — but "should be unchanged" is not evidence, so print
    // both the pre-swap record shape (no `engine` key) and the post-swap one and
    // compare them here.
    let mut legacy = db.clone();
    legacy.connection.remove("engine");
    let before_env = crate::databases::env_exports(&legacy, "https://api.example.host");
    let after_env = crate::databases::env_exports(&db, "https://api.example.host");
    println!("\n--- exported env for DbKind::Sqlite ---");
    for (k, v) in &after_env {
        println!("{k}={v}");
    }
    println!("\nENV SURFACE IDENTICAL pre/post swap: {}", before_env == after_env);
    if before_env != after_env {
        println!("  pre:  {before_env:?}");
        println!("  post: {after_env:?}");
        anyhow::bail!("the exported env surface changed");
    }

    // 10. The record is the ONLY thing that makes a database musql. With
    //     HIVE_SQLITE_ENGINE=musql the default IS musql, so this is the check
    //     that proves an env var alone cannot flip an existing database: the
    //     pre-change provisioner wrote `engine: "sqlite"`, and a database with
    //     no `engine` key at all predates the field.
    let mut legacy = db.connection.clone();
    legacy.insert("engine".into(), "sqlite".into());
    let mut no_key = db.connection.clone();
    no_key.remove("engine");
    println!("\n--- engine resolution under HIVE_SQLITE_ENGINE=musql ---");
    println!("default (new databases):  {}", crate::musql::default_engine().label());
    println!("record engine=musql:      {}", crate::musql::engine_for_db(&db.connection).label());
    println!("record engine=sqlite:     {}", crate::musql::engine_for_db(&legacy).label());
    println!("record engine absent:     {}", crate::musql::engine_for_db(&no_key).label());
    if crate::musql::engine_for_db(&legacy) != Engine::Rusqlite
        || crate::musql::engine_for_db(&no_key) != Engine::Rusqlite
    {
        anyhow::bail!("an unverified database record can become musql by env var alone");
    }
    Ok(())
}
