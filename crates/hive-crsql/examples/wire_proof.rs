//! Live, no-mocks wire-format witness for the ten-column `crsql_changes` v0.17 format: the NATIVE
//! half of a two-runtime sync round trip whose other peer is the browser crsqlite wasm build under
//! Node, orchestrated by `crates/hive-browser/www/sqlite/prove-wire.sh` (`export-a`, `apply-final`,
//! `export-hcb1`, `apply-hcb1`); the changes that cross the wire must arrive byte-identical.

use anyhow::Context;
use hive_crsql::Change;

/// Tagged JSON shape for the `val` ANY column — the exact shape the Node side
/// (`wire-proof-node.mjs`) emits and consumes, so a byte-level comparison is meaningful.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    changes: Vec<WireChange>,
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct WireChange {
    table: String,
    /// hex of the packed-pk blob
    pk: String,
    cid: String,
    val: WireVal,
    col_version: i64,
    db_version: i64,
    /// hex of the 16-byte site id
    site_id: String,
    cl: i64,
    seq: i64,
    ts: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
enum WireVal {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    /// hex
    Blob(String),
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> anyhow::Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        anyhow::bail!("odd-length hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(anyhow::Error::from))
        .collect()
}

fn to_wire(c: &Change) -> WireChange {
    let val = match &c.val {
        rusqlite::types::Value::Null => WireVal::Null,
        rusqlite::types::Value::Integer(i) => WireVal::Integer(*i),
        rusqlite::types::Value::Real(f) => WireVal::Real(*f),
        rusqlite::types::Value::Text(s) => WireVal::Text(s.clone()),
        rusqlite::types::Value::Blob(b) => WireVal::Blob(hex(b)),
    };
    WireChange {
        table: c.table.clone(),
        pk: hex(&c.pk),
        cid: c.cid.clone(),
        val,
        col_version: c.col_version,
        db_version: c.db_version,
        site_id: hex(&c.site_id),
        cl: c.cl,
        seq: c.seq,
        ts: c.ts.clone(),
    }
}

fn from_wire(w: &WireChange) -> anyhow::Result<Change> {
    let val = match &w.val {
        WireVal::Null => rusqlite::types::Value::Null,
        WireVal::Integer(i) => rusqlite::types::Value::Integer(*i),
        WireVal::Real(f) => rusqlite::types::Value::Real(*f),
        WireVal::Text(s) => rusqlite::types::Value::Text(s.clone()),
        WireVal::Blob(h) => rusqlite::types::Value::Blob(unhex(h)?),
    };
    Ok(Change {
        table: w.table.clone(),
        pk: unhex(&w.pk)?,
        cid: w.cid.clone(),
        val,
        col_version: w.col_version,
        db_version: w.db_version,
        site_id: unhex(&w.site_id)?,
        cl: w.cl,
        seq: w.seq,
        ts: w.ts.clone(),
    })
}

fn dump(conn: &rusqlite::Connection) -> anyhow::Result<WireFile> {
    let mut changes: Vec<Change> = Vec::new();
    for (site, _) in hive_crsql::known_sites(conn)? {
        for batch in hive_crsql::changes_since_site(conn, &site, 0, usize::MAX)? {
            changes.extend(batch.changes);
        }
    }
    let rows: Vec<Vec<serde_json::Value>> = conn
        .prepare("SELECT id, label FROM items ORDER BY id")?
        .query_map([], |r| {
            Ok(vec![
                serde_json::Value::from(r.get::<_, i64>(0)?),
                serde_json::Value::from(r.get::<_, String>(1)?),
            ])
        })?
        .collect::<Result<_, _>>()?;
    Ok(WireFile {
        changes: changes.iter().map(to_wire).collect(),
        rows,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("export-a") => {
            let db = args.get(2).context("usage: export-a <db> <out.json>")?;
            let out = args.get(3).context("usage: export-a <db> <out.json>")?;
            let conn = hive_crsql::open(db)?;
            conn.execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);",
            )?;
            hive_crsql::as_crr(&conn, "items")?;
            hive_crsql::set_ts(&conn, 1_754_000_000_001)?;
            conn.execute("INSERT INTO items (id, label) VALUES (1, 'from-a')", [])?;
            let file = dump(&conn)?;
            std::fs::write(out, serde_json::to_string_pretty(&file)?)?;
            println!(
                "export-a: {} change(s), site_id={}, ts={}",
                file.changes.len(),
                file.changes[0].site_id,
                file.changes[0].ts
            );
            Ok(())
        }
        Some("apply-final") => {
            let db = args
                .get(2)
                .context("usage: apply-final <db> <b.json> <a-final.json>")?;
            let bin = args
                .get(3)
                .context("usage: apply-final <db> <b.json> <a-final.json>")?;
            let out = args
                .get(4)
                .context("usage: apply-final <db> <b.json> <a-final.json>")?;
            let conn = hive_crsql::open(db)?;
            let b: WireFile =
                serde_json::from_str(&std::fs::read_to_string(bin)?).context("parse b.json")?;
            let changes: Vec<Change> = b
                .changes
                .iter()
                .map(from_wire)
                .collect::<anyhow::Result<_>>()?;
            let mut by_site: std::collections::BTreeMap<Vec<u8>, Vec<Change>> =
                std::collections::BTreeMap::new();
            for c in changes {
                by_site.entry(c.site_id.clone()).or_default().push(c);
            }
            let batches: Vec<hive_crsql::ChangeBatch> = by_site
                .into_iter()
                .map(|(site, mut cs)| {
                    cs.sort_by_key(|c| (c.db_version, c.seq));
                    hive_crsql::ChangeBatch {
                        site_id: site,
                        since_db_version: 0,
                        changes: cs,
                    }
                })
                .collect();
            let outcomes = hive_crsql::apply_batches(&conn, &batches)?;
            for o in &outcomes {
                println!("apply-final: {o:?}");
            }
            let file = dump(&conn)?;
            std::fs::write(out, serde_json::to_string_pretty(&file)?)?;
            println!(
                "apply-final: applied {} batch(es) from wasm peer; a now holds {} change(s), {} row(s)",
                outcomes.len(),
                file.changes.len(),
                file.rows.len()
            );
            Ok(())
        }
        Some("export-hcb1") => {
            let db = args.get(2).context("usage: export-hcb1 <db> <out.hex>")?;
            let out = args.get(3).context("usage: export-hcb1 <db> <out.hex>")?;
            let conn = hive_crsql::open(db)?;
            let mut lines = String::new();
            let mut total = 0usize;
            for (site, _) in hive_crsql::known_sites(&conn)? {
                for batch in hive_crsql::changes_since_site(
                    &conn,
                    &site,
                    0,
                    hive_crsql::DEFAULT_MAX_BATCH_CHANGES,
                )? {
                    lines.push_str(&hex(&batch.encode()));
                    lines.push('\n');
                    total += 1;
                }
            }
            std::fs::write(out, lines)?;
            println!("export-hcb1: {total} batch(es) encoded");
            Ok(())
        }
        Some("apply-hcb1") => {
            let db = args
                .get(2)
                .context("usage: apply-hcb1 <db> <in.hex> <reencoded.hex>")?;
            let input = args
                .get(3)
                .context("usage: apply-hcb1 <db> <in.hex> <reencoded.hex>")?;
            let out = args
                .get(4)
                .context("usage: apply-hcb1 <db> <in.hex> <reencoded.hex>")?;
            let conn = hive_crsql::open(db)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);",
            )?;
            hive_crsql::as_crr(&conn, "items")?;
            let text = std::fs::read_to_string(input)?;
            let mut batches = Vec::new();
            let mut lines = String::new();
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let batch = hive_crsql::ChangeBatch::decode(&unhex(line)?)?;
                lines.push_str(&hex(&batch.encode()));
                lines.push('\n');
                batches.push(batch);
            }
            std::fs::write(out, lines)?;
            let outcomes = hive_crsql::apply_batches(&conn, &batches)?;
            for o in &outcomes {
                println!("apply-hcb1: {o:?}");
            }
            println!(
                "apply-hcb1: applied {} batch(es) from JS-encoded frames",
                outcomes.len()
            );
            Ok(())
        }
        other => {
            anyhow::bail!(
                "unknown subcommand {other:?}; want export-a | apply-final | export-hcb1 | apply-hcb1 (see prove-wire.sh)"
            )
        }
    }
}
