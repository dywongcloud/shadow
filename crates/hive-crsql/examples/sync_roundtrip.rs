//! Live, no-mocks witness for hive-crsql: two REAL in-memory cr-sqlite peers,
//! diverging local writes, a real changes_since/apply_changes sync round
//! trip, and a printed proof both sides converge. Run:
//!   cargo run -p hive-crsql --example sync_roundtrip
//! Not a test file (see AGENTS.md's no-test-files rule) -- a real, one-off
//! executable proof, same category as this repo's other `examples/*.rs`.

fn main() -> anyhow::Result<()> {
    let a = hive_crsql::open_in_memory()?;
    let b = hive_crsql::open_in_memory()?;

    for conn in [&a, &b] {
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);")?;
        hive_crsql::as_crr(conn, "items")?;
    }

    println!("a.crsql_version() = {}", hive_crsql::version(&a)?);
    println!("a.site_id() = {} bytes", hive_crsql::site_id(&a)?.len());
    println!("b.site_id() = {} bytes", hive_crsql::site_id(&b)?.len());
    assert_ne!(
        hive_crsql::site_id(&a)?,
        hive_crsql::site_id(&b)?,
        "two independently-opened connections must have distinct site ids"
    );

    // Diverge: peer A writes row 1, peer B writes row 2 -- neither knows
    // about the other's write yet.
    a.execute("INSERT INTO items (id, label) VALUES (1, 'from-a')", [])?;
    b.execute("INSERT INTO items (id, label) VALUES (2, 'from-b')", [])?;

    let a_changes = hive_crsql::changes_since(&a, 0)?;
    let b_changes = hive_crsql::changes_since(&b, 0)?;
    println!("a has {} local change(s), b has {} local change(s)", a_changes.len(), b_changes.len());
    assert_eq!(a_changes.len(), 1);
    assert_eq!(b_changes.len(), 1);

    // Real sync round trip: apply each side's changes to the other.
    hive_crsql::apply_changes(&b, &a_changes)?;
    hive_crsql::apply_changes(&a, &b_changes)?;

    let a_labels: Vec<String> = a
        .prepare("SELECT label FROM items ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let b_labels: Vec<String> = b
        .prepare("SELECT label FROM items ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    println!("a rows after sync: {a_labels:?}");
    println!("b rows after sync: {b_labels:?}");
    assert_eq!(a_labels, vec!["from-a".to_string(), "from-b".to_string()]);
    assert_eq!(a_labels, b_labels, "both peers must converge to the identical row set");

    println!("REAL_SYNC_ROUNDTRIP_OK");
    Ok(())
}
