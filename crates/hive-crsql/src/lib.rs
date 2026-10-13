//! Thin `rusqlite` wrapper around the vendored `superfly/cr-sqlite` extension (`vendor/cr-sqlite`,
//! v0.17): per-origin-site sync progress (`crsql_db_versions`, durable in the same transaction),
//! explicit gap/replay handling ([`SyncGap`] refuses a batch chained from ahead of the watermark,
//! [`ApplyOutcome::Replay`] is a deterministic no-op), bounded deterministic batches
//! ([`ChangeBatch::encode`] is canonical bytes) and one transaction per batch.

use rusqlite::Connection;
use std::path::PathBuf;

/// Re-exported so consumers can name the connection type without pinning their own rusqlite —
/// one SQLite build links into the fleet binary, not two.
pub use rusqlite;

/// The loadable extension path WITHOUT platform suffix (SQLite appends `.dylib`/`.so`/`.dll`
/// itself); `HIVE_CRSQL_EXTENSION_PATH` overrides for a packaged fleet deploy.
pub fn extension_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIVE_CRSQL_EXTENSION_PATH") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/cr-sqlite/core/dist/crsqlite")
}

/// Open a connection with the cr-sqlite extension loaded; extension loading is enabled only for
/// the load call so later (possibly tenant-supplied) SQL can never load native code.
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
    conn.query_row("SELECT crsql_as_crr(?1)", [table], |_| Ok(()))?;
    Ok(())
}

/// This site's stable 16-byte identifier, the `site_id` this node's own writes carry; persisted
/// in `crsql_site_id`, so it survives reopening the file.
pub fn site_id(conn: &Connection) -> anyhow::Result<Vec<u8>> {
    Ok(conn.query_row("SELECT crsql_site_id()", [], |r| r.get(0))?)
}

/// The extension's version as `crsql_version()` reports it (170000 = v0.17.0
/// on the currently-vendored superfly/cr-sqlite commit — see VENDOR.md).
pub fn version(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT crsql_version()", [], |r| r.get(0))?)
}

/// Set this transaction's timestamp — the `ts` every change written by the transaction carries.
/// The v0.17 fork resets it to 0 on commit/rollback, so a write without it records `ts = '0'`
/// verbatim into every receiving peer; call once per write transaction, before the writes.
pub fn set_ts(conn: &Connection, ts: u64) -> anyhow::Result<()> {
    conn.query_row("SELECT crsql_set_ts(?1)", [ts.to_string()], |_| Ok(()))?;
    Ok(())
}

/// One row of the ten-column `crsql_changes` v0.17 wire format in the vtab's declared order; the
/// trailing `ts` is read unconditionally by `merge_insert`, so a peer that omits it poisons the
/// receiving site's clock tables. Never drop it.
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
    pub ts: String,
}

/// Recommended upper bound on changes per batch; a single origin commit larger than this still
/// travels as one oversize batch so it applies atomically.
pub const DEFAULT_MAX_BATCH_CHANGES: usize = 256;

/// The canonical size of one change's `val` — the number `max_value_bytes` is enforced against
/// on BOTH sides: HCB1 wire cost (1 tag byte + payload). `www/sqlite/hcb1.js` carries the same
/// formula.
pub fn val_payload_bytes(val: &rusqlite::types::Value) -> u64 {
    match val {
        rusqlite::types::Value::Null => 1,
        rusqlite::types::Value::Integer(_) | rusqlite::types::Value::Real(_) => 9,
        rusqlite::types::Value::Text(s) => 5 + s.len() as u64,
        rusqlite::types::Value::Blob(b) => 5 + b.len() as u64,
    }
}

/// One bounded, deterministically-ordered sync unit: changes from exactly ONE origin site
/// ascending by `(db_version, seq)`, chained to the receiver's progress via `since_db_version`
/// (the gap detector).
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeBatch {
    /// Origin site every change belongs to (16 raw bytes).
    pub site_id: Vec<u8>,
    /// The watermark this batch continues from: every change has `db_version > since_db_version`
    /// and the receiver must already hold this site's history up to here.
    pub since_db_version: i64,
    /// Ascending by `(db_version, seq)`; one origin `db_version` never
    /// spans two batches.
    pub changes: Vec<Change>,
}

impl ChangeBatch {
    pub fn min_db_version(&self) -> Option<i64> {
        self.changes.iter().map(|c| c.db_version).min()
    }

    pub fn max_db_version(&self) -> Option<i64> {
        self.changes.iter().map(|c| c.db_version).max()
    }

    /// Canonical binary encoding (`"HCB1"` magic, fixed field order, big-endian integers, explicit
    /// lengths, type-tagged `val`): the same change set always encodes to the same bytes.
    pub fn encode(&self) -> Vec<u8> {
        fn push_str(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        fn push_blob(out: &mut Vec<u8>, b: &[u8]) {
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
        let mut out = Vec::with_capacity(64 + self.changes.len() * 48);
        out.extend_from_slice(b"HCB1");
        out.push(self.site_id.len() as u8);
        out.extend_from_slice(&self.site_id);
        out.extend_from_slice(&self.since_db_version.to_be_bytes());
        out.extend_from_slice(&(self.changes.len() as u32).to_be_bytes());
        for c in &self.changes {
            push_str(&mut out, &c.table);
            push_blob(&mut out, &c.pk);
            push_str(&mut out, &c.cid);
            match &c.val {
                rusqlite::types::Value::Null => out.push(0),
                rusqlite::types::Value::Integer(i) => {
                    out.push(1);
                    out.extend_from_slice(&i.to_be_bytes());
                }
                rusqlite::types::Value::Real(f) => {
                    out.push(2);
                    out.extend_from_slice(&f.to_bits().to_be_bytes());
                }
                rusqlite::types::Value::Text(s) => {
                    out.push(3);
                    push_blob(&mut out, s.as_bytes());
                }
                rusqlite::types::Value::Blob(b) => {
                    out.push(4);
                    push_blob(&mut out, b);
                }
            }
            out.extend_from_slice(&c.col_version.to_be_bytes());
            out.extend_from_slice(&c.db_version.to_be_bytes());
            out.push(c.site_id.len() as u8);
            out.extend_from_slice(&c.site_id);
            out.extend_from_slice(&c.cl.to_be_bytes());
            out.extend_from_slice(&c.seq.to_be_bytes());
            push_str(&mut out, &c.ts);
        }
        out
    }

    /// Inverse of [`encode`][Self::encode]; refuses a frame whose changes are not all from the
    /// batch's `site_id` or whose `db_version`s decrease.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<ChangeBatch> {
        struct Cursor<'a> {
            b: &'a [u8],
            at: usize,
        }
        impl<'a> Cursor<'a> {
            fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
                if self.b.len() - self.at < n {
                    anyhow::bail!("truncated frame at offset {}", self.at);
                }
                let s = &self.b[self.at..self.at + n];
                self.at += n;
                Ok(s)
            }
            fn u8(&mut self) -> anyhow::Result<u8> {
                Ok(self.take(1)?[0])
            }
            fn u16(&mut self) -> anyhow::Result<u16> {
                Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
            }
            fn u32(&mut self) -> anyhow::Result<u32> {
                Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
            }
            fn i64(&mut self) -> anyhow::Result<i64> {
                Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
            }
            fn blob(&mut self) -> anyhow::Result<Vec<u8>> {
                let n = self.u32()? as usize;
                Ok(self.take(n)?.to_vec())
            }
            fn string(&mut self) -> anyhow::Result<String> {
                let n = self.u16()? as usize;
                Ok(String::from_utf8(self.take(n)?.to_vec())?)
            }
        }
        let mut cur = Cursor { b: bytes, at: 0 };
        if cur.take(4)? != b"HCB1" {
            anyhow::bail!("bad magic: not an HCB1 change-batch frame");
        }
        let site_len = cur.u8()? as usize;
        let site_id = cur.take(site_len)?.to_vec();
        let since_db_version = cur.i64()?;
        let count = cur.u32()? as usize;
        let mut changes = Vec::with_capacity(count.min(1024));
        let mut prev_db_version: Option<i64> = None;
        for _ in 0..count {
            let table = cur.string()?;
            let pk = cur.blob()?;
            let cid = cur.string()?;
            let val = match cur.u8()? {
                0 => rusqlite::types::Value::Null,
                1 => rusqlite::types::Value::Integer(cur.i64()?),
                2 => rusqlite::types::Value::Real(f64::from_bits(u64::from_be_bytes(
                    cur.take(8)?.try_into().unwrap(),
                ))),
                3 => rusqlite::types::Value::Text(String::from_utf8(cur.blob()?)?),
                4 => rusqlite::types::Value::Blob(cur.blob()?),
                t => anyhow::bail!("unknown val tag {t}"),
            };
            let col_version = cur.i64()?;
            let db_version = cur.i64()?;
            let clen = cur.u8()? as usize;
            let csite = cur.take(clen)?.to_vec();
            let cl = cur.i64()?;
            let seq = cur.i64()?;
            let ts = cur.string()?;
            if csite != site_id {
                anyhow::bail!(
                    "mixed-site batch: change carries a different site_id than the batch"
                );
            }
            if let Some(p) = prev_db_version {
                if db_version < p {
                    anyhow::bail!(
                        "batch out of deterministic order: db_version {db_version} after {p}"
                    );
                }
            }
            prev_db_version = Some(db_version);
            changes.push(Change {
                table,
                pk,
                cid,
                val,
                col_version,
                db_version,
                site_id: csite,
                cl,
                seq,
                ts,
            });
        }
        if cur.at != bytes.len() {
            anyhow::bail!("trailing bytes after frame");
        }
        Ok(ChangeBatch {
            site_id,
            since_db_version,
            changes,
        })
    }
}

/// A batch claimed continuity from AHEAD of this peer's durable watermark for its origin site —
/// an intermediate batch is missing. Refused and rolled back; re-request the export from
/// `watermark`.
#[derive(Debug)]
pub struct SyncGap {
    pub site_id: Vec<u8>,
    /// This peer's durable high-water mark for the origin site.
    pub watermark: i64,
    /// The `since_db_version` the refused batch chained from.
    pub batch_since: i64,
}

impl std::fmt::Display for SyncGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sync gap for site {}: batch continues from db_version {} but durable watermark is {} -- an intermediate batch is missing; re-request export from {}",
            hex(&self.site_id),
            self.batch_since,
            self.watermark,
            self.watermark
        )
    }
}

impl std::error::Error for SyncGap {}

/// What applying one batch did (see [`apply_batch`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    /// New changes merged and the durable per-site watermark advanced
    /// `from_db_version` → `to_db_version`, in one committed transaction.
    Applied {
        site_id: Vec<u8>,
        from_db_version: i64,
        to_db_version: i64,
        /// Changes actually inserted (the batch minus any already-seen
        /// overlap prefix with `db_version <= from_db_version`).
        applied: usize,
    },
    /// The batch's maximum `db_version` is at/below the durable watermark: already applied, nothing
    /// written — redelivery is a deterministic no-op.
    Replay {
        site_id: Vec<u8>,
        at_db_version: i64,
        skipped: usize,
    },
}

/// Lowercase hex, for diagnostics.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Every origin site this DB has durable progress for, ascending by site id, with its high-water
/// mark (the extension-maintained `crsql_db_versions`).
pub fn known_sites(conn: &Connection) -> anyhow::Result<Vec<(Vec<u8>, i64)>> {
    let mut stmt =
        conn.prepare("SELECT site_id, db_version FROM crsql_db_versions ORDER BY site_id ASC")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)
}

/// This peer's durable high-water mark for one origin site (0 = nothing received); the number
/// exports are requested with and the gap check chains on.
pub fn watermark_for(conn: &Connection, site_id: &[u8]) -> anyhow::Result<i64> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT db_version FROM crsql_db_versions WHERE site_id = ?1",
            [site_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// All tracked changes from ONE origin site with `db_version > since_db_version` as bounded,
/// deterministically-ordered [`ChangeBatch`]es — the outbound half of a sync round. Pass the
/// RECEIVER's [`watermark_for`] this site (0 = full export). Two exports of the same state are
/// byte-identical.
pub fn changes_since_site(
    conn: &Connection,
    site_id: &[u8],
    since_db_version: i64,
    max_changes_per_batch: usize,
) -> anyhow::Result<Vec<ChangeBatch>> {
    if max_changes_per_batch == 0 {
        anyhow::bail!("max_changes_per_batch must be >= 1");
    }
    if site_id.is_empty() {
        anyhow::bail!("site_id must be non-empty");
    }
    let mut stmt = conn.prepare(
        "SELECT \"table\", \"pk\", \"cid\", \"val\", \"col_version\", \"db_version\", \"site_id\", \"cl\", \"seq\", \"ts\"
         FROM crsql_changes WHERE site_id = ?1 AND db_version > ?2
         ORDER BY db_version, seq ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![site_id, since_db_version], |r| {
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
            ts: r.get(9)?,
        })
    })?;
    let mut batches: Vec<ChangeBatch> = Vec::new();
    let mut cur: Vec<Change> = Vec::new();
    let mut batch_since = since_db_version;
    let mut group_end_v: Option<i64> = None;
    for row in rows {
        let c = row?;
        let new_group = group_end_v != Some(c.db_version);
        if new_group && !cur.is_empty() && cur.len() >= max_changes_per_batch {
            let max = cur.last().map(|x| x.db_version).unwrap_or(batch_since);
            batches.push(ChangeBatch {
                site_id: site_id.to_vec(),
                since_db_version: batch_since,
                changes: std::mem::take(&mut cur),
            });
            batch_since = max;
        }
        group_end_v = Some(c.db_version);
        cur.push(c);
    }
    if !cur.is_empty() {
        batches.push(ChangeBatch {
            site_id: site_id.to_vec(),
            since_db_version: batch_since,
            changes: cur,
        });
    }
    Ok(batches)
}

/// Apply ONE received batch in a single transaction (watermark read, gap/replay checks, every
/// insert and the extension's watermark bump commit or roll back together). Given durable
/// watermark `w`: `max_db_version <= w` → [`ApplyOutcome::Replay`]; `since_db_version > w` →
/// [`SyncGap`], everything rolled back; otherwise merge the changes above `w` →
/// [`ApplyOutcome::Applied`].
pub fn apply_batch(conn: &Connection, batch: &ChangeBatch) -> anyhow::Result<ApplyOutcome> {
    if batch.site_id.is_empty() {
        anyhow::bail!("batch site_id must be non-empty");
    }
    for c in &batch.changes {
        if c.site_id != batch.site_id {
            anyhow::bail!(
                "mixed-site batch refused: change site {} != batch site {}",
                hex(&c.site_id),
                hex(&batch.site_id)
            );
        }
    }
    let tx = conn.unchecked_transaction()?;
    let w = watermark_for(&tx, &batch.site_id)?;
    let batch_max = batch.max_db_version();
    match batch_max {
        None => {
            if batch.since_db_version > w {
                return Err(anyhow::Error::new(SyncGap {
                    site_id: batch.site_id.clone(),
                    watermark: w,
                    batch_since: batch.since_db_version,
                }));
            }
            Ok(ApplyOutcome::Replay {
                site_id: batch.site_id.clone(),
                at_db_version: w,
                skipped: 0,
            })
        }
        Some(max) if max <= w => Ok(ApplyOutcome::Replay {
            site_id: batch.site_id.clone(),
            at_db_version: w,
            skipped: batch.changes.len(),
        }),
        Some(max) => {
            if batch.since_db_version > w {
                return Err(anyhow::Error::new(SyncGap {
                    site_id: batch.site_id.clone(),
                    watermark: w,
                    batch_since: batch.since_db_version,
                }));
            }
            let mut stmt = tx.prepare(
                "INSERT INTO crsql_changes
                 (\"table\", \"pk\", \"cid\", \"val\", \"col_version\", \"db_version\", \"site_id\", \"cl\", \"seq\", \"ts\")
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let mut applied = 0usize;
            for c in &batch.changes {
                if c.db_version <= w {
                    continue;
                }
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
                    c.ts,
                ])?;
                applied += 1;
            }
            drop(stmt);
            let to = watermark_for(&tx, &batch.site_id)?;
            anyhow::ensure!(
                to == std::cmp::max(w, max),
                "durable watermark advanced to {to}, expected {}",
                std::cmp::max(w, max)
            );
            let from = w;
            tx.commit()?;
            Ok(ApplyOutcome::Applied {
                site_id: batch.site_id.clone(),
                from_db_version: from,
                to_db_version: to,
                applied,
            })
        }
    }
}

/// Apply batches in order, one transaction each, stopping at the first error; committed batches
/// stay committed, so after fixing the cause the caller simply resumes (redeliveries come back as
/// [`ApplyOutcome::Replay`]).
pub fn apply_batches(
    conn: &Connection,
    batches: &[ChangeBatch],
) -> anyhow::Result<Vec<ApplyOutcome>> {
    let mut outcomes = Vec::with_capacity(batches.len());
    for b in batches {
        outcomes.push(apply_batch(conn, b)?);
    }
    Ok(outcomes)
}
