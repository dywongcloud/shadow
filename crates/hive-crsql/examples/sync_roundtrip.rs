//! Live, no-mocks witness for hive-crsql's sync seam: REAL cr-sqlite peers
//! (in-memory AND file-backed), exercising the per-origin-site sync protocol
//! end to end. Run:
//!   cargo run -p hive-crsql --example sync_roundtrip
//! Not a test file (see AGENTS.md's no-test-files rule) -- a real, one-off
//! executable proof, same category as this repo's other `examples/*.rs`.
//!
//! Proves, in order:
//!   [1] divergent peers converge via per-site export/apply, ts survives
//!       merge verbatim both directions, and the bounded wire encoding is
//!       deterministic (two exports byte-identical, decode(encode(b)) == b)
//!   [2] replay/idempotency: applying the same batches twice is a no-op
//!   [3] interruption between batches: apply batch 1 of 3, drop+reopen the
//!       FILE-BACKED peer (progress is durable), resume redelivers batch 1
//!       as a no-op, batches 2-3 apply, final state == uninterrupted peer
//!   [4] gap detection: a batch arriving before its predecessor is refused
//!       with SyncGap and leaves zero trace; the chain then applies cleanly

use anyhow::Context;
use hive_crsql::{ApplyOutcome, ChangeBatch, SyncGap};

const ITEMS_DDL: &str = "CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);";

fn mem_peer() -> anyhow::Result<rusqlite::Connection> {
    let conn = hive_crsql::open_in_memory()?;
    conn.execute_batch(ITEMS_DDL)?;
    hive_crsql::as_crr(&conn, "items")?;
    Ok(conn)
}

fn rows(conn: &rusqlite::Connection) -> anyhow::Result<Vec<String>> {
    Ok(conn
        .prepare("SELECT label FROM items ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Full per-site export of everything this peer holds for `site`, one batch.
fn full_export(conn: &rusqlite::Connection, site: &[u8]) -> anyhow::Result<Vec<ChangeBatch>> {
    hive_crsql::changes_since_site(conn, site, 0, usize::MAX)
}

fn main() -> anyhow::Result<()> {
    // ---------------------------------------------------------------
    // [1] Divergent peers, per-site sync round trip, ts + wire determinism
    // ---------------------------------------------------------------
    let a = mem_peer()?;
    let b = mem_peer()?;

    println!("a.crsql_version() = {}", hive_crsql::version(&a)?);
    let a_site = hive_crsql::site_id(&a)?;
    let b_site = hive_crsql::site_id(&b)?;
    assert_ne!(a_site, b_site, "distinct peers must have distinct site ids");

    // Per-site selection must isolate: before any sync, A holds nothing from B.
    assert!(
        full_export(&a, &b_site)?.is_empty(),
        "a must hold zero b-site changes before sync"
    );

    // Diverge: peer A writes row 1, peer B writes row 2 -- neither knows
    // about the other's write yet. Each sets its transaction ts first: the
    // v0.17 fork resets ext_data.timestamp to 0 on commit, so writes without
    // `crsql_set_ts` record ts '0' -- useless on the wire.
    hive_crsql::set_ts(&a, 1_754_000_000_001)?;
    a.execute("INSERT INTO items (id, label) VALUES (1, 'from-a')", [])?;
    hive_crsql::set_ts(&b, 1_754_000_000_002)?;
    b.execute("INSERT INTO items (id, label) VALUES (2, 'from-b')", [])?;

    // Outbound half: per-site bounded export. One change each -> one batch.
    let a_out =
        hive_crsql::changes_since_site(&a, &a_site, 0, hive_crsql::DEFAULT_MAX_BATCH_CHANGES)?;
    let b_out =
        hive_crsql::changes_since_site(&b, &b_site, 0, hive_crsql::DEFAULT_MAX_BATCH_CHANGES)?;
    assert_eq!(a_out.len(), 1);
    assert_eq!(b_out.len(), 1);
    assert_eq!(
        a_out[0].since_db_version, 0,
        "first batch chains from the request watermark"
    );
    let ac = &a_out[0].changes[0];
    println!(
        "a change: table={} cid={} col_v={} db_v={} cl={} seq={} ts={}",
        ac.table, ac.cid, ac.col_version, ac.db_version, ac.cl, ac.seq, ac.ts
    );
    assert_eq!(
        ac.ts, "1754000000001",
        "a's write must carry the ts its transaction set"
    );
    assert_eq!(
        b_out[0].changes[0].ts, "1754000000002",
        "b's write must carry the ts its transaction set"
    );

    // Bounded DETERMINISTIC serialization: encoding is stable across exports
    // and round-trips exactly.
    let enc1: Vec<u8> = a_out[0].encode();
    let a_out2 =
        hive_crsql::changes_since_site(&a, &a_site, 0, hive_crsql::DEFAULT_MAX_BATCH_CHANGES)?;
    assert_eq!(
        enc1,
        a_out2[0].encode(),
        "two exports of the same state must be byte-identical"
    );
    let decoded = ChangeBatch::decode(&enc1)?;
    assert_eq!(
        decoded, a_out[0],
        "decode(encode(batch)) must round-trip exactly"
    );
    println!("[1] wire encoding deterministic: {} byte frame, export-encode-export stable, round-trip exact", enc1.len());

    // Inbound half over the WIRE FORM (not the in-memory structs): real sync.
    let b_wire: Vec<ChangeBatch> = b_out
        .iter()
        .map(|bb| ChangeBatch::decode(&bb.encode()))
        .collect::<anyhow::Result<_>>()?;
    let a_wire: Vec<ChangeBatch> = a_out
        .iter()
        .map(|bb| ChangeBatch::decode(&bb.encode()))
        .collect::<anyhow::Result<_>>()?;
    let outcomes_b = hive_crsql::apply_batches(&b, &a_wire)?;
    let outcomes_a = hive_crsql::apply_batches(&a, &b_wire)?;
    println!(
        "[1] apply outcomes: b<-a {:?} ; a<-b {:?}",
        outcomes_b, outcomes_a
    );
    assert!(matches!(
        outcomes_b[0],
        ApplyOutcome::Applied { applied: 1, .. }
    ));
    assert!(matches!(
        outcomes_a[0],
        ApplyOutcome::Applied { applied: 1, .. }
    ));

    // ts must survive the merge verbatim -- the winner-clock insert binds the
    // inbound change's own ts (changes_vtab_write.rs `set_winner_clock`).
    let a_holds_b = full_export(&a, &b_site)?;
    let b_holds_a = full_export(&b, &a_site)?;
    let merged_from_b = &a_holds_b[0].changes[0];
    let merged_from_a = &b_holds_a[0].changes[0];
    assert_eq!(
        merged_from_b.ts, "1754000000002",
        "ts must survive merge into a verbatim"
    );
    assert_eq!(
        merged_from_a.ts, "1754000000001",
        "ts must survive merge into b verbatim"
    );
    println!(
        "[1] ts survived merge on both peers: {} / {}",
        merged_from_a.ts, merged_from_b.ts
    );

    let a_labels = rows(&a)?;
    let b_labels = rows(&b)?;
    println!("[1] a rows after sync: {a_labels:?}");
    println!("[1] b rows after sync: {b_labels:?}");
    assert_eq!(a_labels, vec!["from-a".to_string(), "from-b".to_string()]);
    assert_eq!(
        a_labels, b_labels,
        "divergent peers must converge to the identical row set"
    );
    println!("[1] OK divergent peers converged via per-site sync");

    // ---------------------------------------------------------------
    // [2] Replay/idempotency: same batch twice = deterministic no-op
    // ---------------------------------------------------------------
    let c_mem = mem_peer()?;
    let d_mem = mem_peer()?;
    let c_mem_site = hive_crsql::site_id(&c_mem)?;
    hive_crsql::set_ts(&c_mem, 1_754_000_001_001)?;
    c_mem.execute("INSERT INTO items (id, label) VALUES (1, 'c1')", [])?;
    hive_crsql::set_ts(&c_mem, 1_754_000_001_002)?;
    c_mem.execute("INSERT INTO items (id, label) VALUES (2, 'c2')", [])?;
    // max_changes_per_batch=1 forces one batch per commit -> a 2-batch chain.
    let cd_batches = hive_crsql::changes_since_site(&c_mem, &c_mem_site, 0, 1)?;
    assert_eq!(
        cd_batches.len(),
        2,
        "bound of 1 change must split two commits into two batches"
    );
    assert_eq!(
        cd_batches[1].since_db_version,
        cd_batches[0].max_db_version().unwrap(),
        "batch chain must be contiguous: batch 2 continues from batch 1's max"
    );

    let first = hive_crsql::apply_batches(&d_mem, &cd_batches)?;
    assert!(first
        .iter()
        .all(|o| matches!(o, ApplyOutcome::Applied { .. })));
    let state_after_first = rows(&d_mem)?;
    let export_after_first = full_export(&d_mem, &c_mem_site)?;
    let d_watermark = hive_crsql::watermark_for(&d_mem, &c_mem_site)?;

    let second = hive_crsql::apply_batches(&d_mem, &cd_batches)?;
    println!("[2] second apply of identical batches: {second:?}");
    assert!(
        second
            .iter()
            .all(|o| matches!(o, ApplyOutcome::Replay { skipped: 1, .. })),
        "re-applying an already-applied batch must be a Replay no-op"
    );
    assert_eq!(
        rows(&d_mem)?,
        state_after_first,
        "replay must not change rows"
    );
    assert_eq!(
        full_export(&d_mem, &c_mem_site)?,
        export_after_first,
        "replay must not change the change-store"
    );
    assert_eq!(
        hive_crsql::watermark_for(&d_mem, &c_mem_site)?,
        d_watermark,
        "replay must not move the watermark"
    );
    assert_eq!(
        state_after_first.len(),
        2,
        "no duplicated rows from redelivery"
    );
    println!("[2] OK replay is a deterministic no-op (watermark {d_watermark} unchanged, state byte-identical)");

    // ---------------------------------------------------------------
    // [3] Interruption between batches + durable per-site progress
    // ---------------------------------------------------------------
    let dir = std::env::temp_dir().join(format!(
        "hive-crsql-roundtrip-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let result = scenario_interruption(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    result?;

    // ---------------------------------------------------------------
    // [4] Gap detection: missing/out-of-order batch is refused loudly
    // ---------------------------------------------------------------
    scenario_gap(&cd_batches, &c_mem_site)?;

    println!("REAL_SYNC_ROUNDTRIP_OK");
    Ok(())
}

/// [3] File-backed peers: source C makes 3 commits -> a 3-batch chain. Peer D
/// applies batch 1, "restarts" (connection dropped, file reopened), and MUST
/// still know its watermark; resuming redelivers batch 1 (Replay no-op) then
/// applies 2-3. Peer E applies all three uninterrupted. D and E must end
/// byte-identical.
fn scenario_interruption(dir: &std::path::Path) -> anyhow::Result<()> {
    let c = hive_crsql::open(dir.join("c.db"))?;
    c.execute_batch(ITEMS_DDL)?;
    hive_crsql::as_crr(&c, "items")?;
    let c_site = hive_crsql::site_id(&c)?;
    for (i, label) in ["r1", "r2", "r3"].iter().enumerate() {
        hive_crsql::set_ts(&c, 1_754_000_002_001 + i as u64)?;
        c.execute(
            &format!(
                "INSERT INTO items (id, label) VALUES ({}, '{}')",
                10 + i,
                label
            ),
            [],
        )?;
    }
    let batches = hive_crsql::changes_since_site(&c, &c_site, 0, 1)?;
    assert_eq!(batches.len(), 3, "3 commits at bound 1 -> 3 batches");
    println!(
        "[3] source exported 3 batches: since/max = {}",
        batches
            .iter()
            .map(|bb| format!("{}/{}", bb.since_db_version, bb.max_db_version().unwrap()))
            .collect::<Vec<_>>()
            .join(" , ")
    );

    // D: apply batch 1 only, then "crash".
    let applied_before_crash;
    {
        let d = hive_crsql::open(dir.join("d.db"))?;
        d.execute_batch(ITEMS_DDL)?;
        hive_crsql::as_crr(&d, "items")?;
        let out = hive_crsql::apply_batch(&d, &batches[0])?;
        applied_before_crash = hive_crsql::watermark_for(&d, &c_site)?;
        println!(
            "[3] d applied batch 1/3 -> {out:?}; watermark before crash = {applied_before_crash}"
        );
        assert!(matches!(out, ApplyOutcome::Applied { applied: 1, .. }));
    } // d dropped here: the "process" is gone.

    // Reopen from the file: progress must have survived the restart.
    let d = hive_crsql::open(dir.join("d.db"))?;
    let durable = hive_crsql::watermark_for(&d, &c_site)?;
    assert_eq!(
        durable, applied_before_crash,
        "per-origin-site progress must be durable across a restart"
    );
    let sites = hive_crsql::known_sites(&d)?;
    println!(
        "[3] after reopen, d's durable per-site progress: {}",
        sites
            .iter()
            .map(|(s, v)| format!("{}@{}", hive_crsql::hex(s), v))
            .collect::<Vec<_>>()
            .join(" , ")
    );
    assert!(
        sites.iter().any(|(s, _)| s == &c_site),
        "reopened peer must still know the source site"
    );

    // Resume: redelivery of batch 1 is a Replay no-op, then 2 and 3 apply.
    let resume0 = hive_crsql::apply_batch(&d, &batches[0])?;
    assert!(
        matches!(resume0, ApplyOutcome::Replay { skipped: 1, .. }),
        "redelivered batch after restart must be a Replay no-op, got {resume0:?}"
    );
    let resume_rest = hive_crsql::apply_batches(&d, &batches[1..])?;
    assert!(resume_rest
        .iter()
        .all(|o| matches!(o, ApplyOutcome::Applied { .. })));
    println!("[3] resume: redelivered batch 1 -> Replay; batches 2-3 -> {resume_rest:?}");

    // E: uninterrupted baseline, same three batches in one go.
    let e = hive_crsql::open(dir.join("e.db"))?;
    e.execute_batch(ITEMS_DDL)?;
    hive_crsql::as_crr(&e, "items")?;
    hive_crsql::apply_batches(&e, &batches)?;

    assert_eq!(
        rows(&d)?,
        rows(&e)?,
        "interrupted peer must equal uninterrupted peer"
    );
    assert_eq!(rows(&d)?, rows(&c)?, "synced peers must equal the source");
    assert_eq!(
        full_export(&d, &c_site)?,
        full_export(&e, &c_site)?,
        "interrupted and uninterrupted change-stores must be identical"
    );
    println!(
        "[3] OK interrupted resume converged to the uninterrupted state {:?}",
        rows(&d)?
    );
    Ok(())
}

/// [4] A batch whose predecessor never landed must be REFUSED with SyncGap
/// and leave zero trace; applying the proper chain afterwards converges.
fn scenario_gap(batches: &[ChangeBatch], c_site: &[u8]) -> anyhow::Result<()> {
    let f = mem_peer()?;

    // Batch 2 of 2 first (its predecessor never arrived).
    let err = hive_crsql::apply_batch(&f, &batches[1])
        .expect_err("batch 2 before batch 1 must be refused");
    let gap = err
        .downcast_ref::<SyncGap>()
        .context("refusal must be a SyncGap")?;
    println!(
        "[4] out-of-order refusal: SyncGap {{ watermark {}, batch_since {} }}",
        gap.watermark, gap.batch_since
    );
    assert_eq!(gap.watermark, 0, "fresh peer's watermark is 0");
    assert_eq!(
        gap.batch_since, batches[1].since_db_version,
        "the gap must name the point the batch tried to continue from"
    );
    assert!(rows(&f)?.is_empty(), "a refused batch must leave zero rows");
    assert_eq!(
        hive_crsql::watermark_for(&f, c_site)?,
        0,
        "a refused batch must not advance the durable watermark (transactional rollback)"
    );

    // In order now: batch 1 applies; re-trying batch 2 still works.
    let o1 = hive_crsql::apply_batch(&f, &batches[0])?;
    assert!(matches!(o1, ApplyOutcome::Applied { applied: 1, .. }));
    let o2 = hive_crsql::apply_batch(&f, &batches[1])?;
    assert!(matches!(o2, ApplyOutcome::Applied { .. }));
    assert_eq!(rows(&f)?, vec!["c1".to_string(), "c2".to_string()]);
    println!("[4] OK gap refused cleanly (no partial state), chain then applied and converged");
    Ok(())
}
