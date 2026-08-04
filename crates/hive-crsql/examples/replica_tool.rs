//! Fleet-side replica file tool for the browser↔fleet CRR exchange witness
//! (bn-browser-fleet-crr-exchange): REAL writes and reads against a fleet
//! replica file through the exact same `hive_crsql` seam the exchange's
//! `browser_db` module uses — a fleet-local writer is precisely what sets
//! the rows the exchange then carries to browsers (merges bind the inbound
//! change's own ts, so this local-write path is also the only one that must
//! set `crsql_set_ts` per write transaction, contract §7).
//!
//!   replica_tool write <db> <id> <label> <ts>
//!   replica_tool rows <db>
//!   replica_tool watermarks <db>

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write") => {
            let db = args.get(2).context("usage: write <db> <id> <label> <ts>")?;
            let id: i64 = args.get(3).context("id")?.parse()?;
            let label = args.get(4).context("label")?;
            let ts: u64 = args.get(5).context("ts")?.parse()?;
            let conn = hive_crsql::open(db)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);",
            )?;
            hive_crsql::as_crr(&conn, "items")?;
            hive_crsql::set_ts(&conn, ts)?;
            conn.execute(
                "INSERT INTO items (id, label) VALUES (?1, ?2)",
                rusqlite::params![id, label],
            )?;
            println!(
                "write: id={id} label={label:?} ts={ts} site={}",
                hive_crsql::hex(&hive_crsql::site_id(&conn)?)
            );
            Ok(())
        }
        Some("rows") => {
            let db = args.get(2).context("usage: rows <db>")?;
            let conn = hive_crsql::open(db)?;
            let mut stmt = conn.prepare("SELECT id, label FROM items ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        match r.get::<_, rusqlite::types::Value>(1)? {
                            rusqlite::types::Value::Text(s) => s,
                            rusqlite::types::Value::Blob(b) => format!("blob:{}", b.len()),
                            other => format!("{other:?}"),
                        },
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            println!("rows: {}", serde_json::to_string(&rows)?);
            Ok(())
        }
        Some("watermarks") => {
            let db = args.get(2).context("usage: watermarks <db>")?;
            let conn = hive_crsql::open(db)?;
            let sites = hive_crsql::known_sites(&conn)?
                .iter()
                .map(|(s, v)| format!("{}@{}", hive_crsql::hex(s), v))
                .collect::<Vec<_>>();
            println!("watermarks: {}", sites.join(" , "));
            Ok(())
        }
        other => anyhow::bail!("unknown subcommand {other:?}; want write | rows | watermarks"),
    }
}
