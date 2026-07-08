#![cfg(feature = "sql")]
//! End-to-end tests for the three PostgreSQL foreign-key parity gaps closed
//! here: `MATCH FULL`, `DEFERRABLE`/`INITIALLY DEFERRED`, and `SET
//! CONSTRAINTS`. See `src/sql/fk.rs`'s module doc comment for the semantics
//! these pin down, and `docs/postgres-compat.md`'s "Foreign keys" section for
//! the compatibility summary. `tests/sql_conformance.rs` covers the
//! remaining, still-`0A000` gap (`MATCH PARTIAL`, and `DEFERRABLE` on
//! `UNIQUE`/`PRIMARY KEY`).

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

async fn session() -> Session<MemoryStorage> {
    let db = Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"));
    Session::new(db, "guardian")
}

/// Execute SQL and return the SQLSTATE of the resulting error (panics if it
/// unexpectedly succeeds).
async fn err_code(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match s.execute(sql).await {
        Ok(_) => panic!("expected `{sql}` to fail, but it succeeded"),
        Err(e) => e.sqlstate().to_string(),
    }
}

async fn ok(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<ExecResult> {
    s.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
}

/// First column of the first row, as i64 (for `SELECT count(*) ...`).
async fn scalar_i64(s: &mut Session<MemoryStorage>, sql: &str) -> i64 {
    let r = ok(s, sql).await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => rows[0][0]
            .to_text()
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(|| panic!("`{sql}` did not return an integer scalar")),
        _ => panic!("expected rows from `{sql}`"),
    }
}

// ---------------------------------------------------------------------------
// MATCH FULL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn match_full_partial_null_is_rejected() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE mp (a INT, b INT, PRIMARY KEY (a, b))").await;
    ok(
        &mut s,
        "CREATE TABLE mc (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES mp (a, b) MATCH FULL)",
    )
    .await;
    ok(&mut s, "INSERT INTO mp VALUES (1, 2)").await;
    // Some but not all NULL: a MATCH FULL violation in its own right, even
    // though one component (y) would dangle-or-not is beside the point.
    assert_eq!(
        err_code(&mut s, "INSERT INTO mc VALUES (1, 1, NULL)").await,
        "23503"
    );
    assert_eq!(
        err_code(&mut s, "INSERT INTO mc VALUES (2, NULL, 2)").await,
        "23503"
    );
    // Confirm nothing was inserted by either rejected row.
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mc").await, 0);
}

#[tokio::test]
async fn match_full_all_null_is_exempt() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mp2 (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mc2 (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES mp2 (a, b) MATCH FULL)",
    )
    .await;
    // No parent rows exist at all; an all-NULL key is still exempt under
    // every MATCH kind, including MATCH FULL.
    ok(&mut s, "INSERT INTO mc2 VALUES (1, NULL, NULL)").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mc2").await, 1);
}

#[tokio::test]
async fn match_full_all_non_null_checks_against_parent() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mp3 (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mc3 (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES mp3 (a, b) MATCH FULL)",
    )
    .await;
    ok(&mut s, "INSERT INTO mp3 VALUES (1, 2)").await;
    // A fully non-NULL key matching a real parent row succeeds.
    ok(&mut s, "INSERT INTO mc3 VALUES (1, 1, 2)").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mc3").await, 1);
    // A fully non-NULL key with no matching parent row is the ordinary
    // "not present" violation, not the MATCH FULL null-mixing one.
    assert_eq!(
        err_code(&mut s, "INSERT INTO mc3 VALUES (2, 9, 9)").await,
        "23503"
    );
}

#[tokio::test]
async fn pg_constraint_reports_match_mode_via_confmatchtype() {
    // `pg_constraint.confmatchtype` ('f'/'p'/'s' in real PostgreSQL) is how a
    // FK's MATCH mode is introspected; `MATCH PARTIAL` can never appear since
    // it's rejected at DDL time (see `match_partial_rejected_deferrable_and_match_full_accepted`
    // in `tests/sql_conformance.rs`).
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mfx_p (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mfx_full (id INT PRIMARY KEY, x INT, y INT, \
         CONSTRAINT mfx_full_fk FOREIGN KEY (x, y) REFERENCES mfx_p (a, b) MATCH FULL)",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mfx_simple (id INT PRIMARY KEY, x INT, y INT, \
         CONSTRAINT mfx_simple_fk FOREIGN KEY (x, y) REFERENCES mfx_p (a, b))", // MATCH SIMPLE (PG default)
    )
    .await;
    let r = ok(
        &mut s,
        "SELECT conname, confmatchtype FROM pg_constraint \
         WHERE contype = 'f' ORDER BY conname",
    )
    .await;
    let rows = match &r[0] {
        ExecResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    let text = |row: &[guardian_db::relational::SqlValue]| -> Vec<String> {
        row.iter()
            .map(|v| v.to_text().unwrap_or_default())
            .collect()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(
        text(&rows[0]),
        ["mfx_full_fk", "f"].map(str::to_string).to_vec()
    );
    assert_eq!(
        text(&rows[1]),
        ["mfx_simple_fk", "s"].map(str::to_string).to_vec()
    );
}

// ---------------------------------------------------------------------------
// MATCH FULL x DEFERRABLE (the interaction between the two features, not
// just each in isolation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn match_full_null_mix_defers_to_commit_under_deferrable_initially_deferred() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mfd_p (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mfd_c (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES mfd_p (a, b) MATCH FULL \
         DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    // A partial-NULL MATCH FULL key is an immediate `23503` under a NOT
    // DEFERRABLE (or INITIALLY IMMEDIATE) constraint — see
    // `match_full_partial_null_is_rejected` — but this constraint is
    // DEFERRABLE INITIALLY DEFERRED, so the INSERT succeeds immediately
    // here instead. This matches real PostgreSQL (verified against a live
    // PostgreSQL 16 instance): `RI_FKey_check` tests the MATCH FULL shape
    // and the parent lookup in the very same deferrable AFTER ROW trigger
    // invocation, so the null-mix check shares that trigger's deferred
    // timing, not a fixed always-immediate rule.
    ok(&mut s, "INSERT INTO mfd_c VALUES (1, 1, NULL)").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mfd_c").await, 1);
    // Never fixed: COMMIT itself raises the MATCH FULL violation.
    assert_eq!(err_code(&mut s, "COMMIT").await, "23503");
    // The failed COMMIT rolled the whole transaction back: nothing was
    // written, and the session is immediately usable again.
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mfd_c").await, 0);
}

#[tokio::test]
async fn match_full_null_mix_fixed_before_commit_succeeds() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mfd_p2 (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mfd_c2 (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES mfd_p2 (a, b) MATCH FULL \
         DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO mfd_c2 VALUES (1, 1, NULL)").await;
    // Rewrite the row to an all-NULL shape before COMMIT: like any other
    // UPDATE that changes an FK column's value, this runs its own fresh
    // (immediately-passing, all-NULL-exempt) check, so the deferred check
    // queued by the INSERT above is satisfied at COMMIT — the row it was
    // watching no longer has that partial-NULL shape.
    ok(&mut s, "UPDATE mfd_c2 SET y = NULL, x = NULL WHERE id = 1").await;
    ok(&mut s, "COMMIT").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mfd_c2").await, 1);
}

#[tokio::test]
async fn match_full_null_mix_set_constraints_immediate_forces_failure_now() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mfd_p3 (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mfd_c3 (id INT PRIMARY KEY, x INT, y INT, \
         CONSTRAINT mfd_c3_fk FOREIGN KEY (x, y) REFERENCES mfd_p3 (a, b) \
         MATCH FULL DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO mfd_c3 VALUES (1, 1, NULL)").await; // deferred, no error yet
    // Forces the queued MATCH FULL check to run right now.
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS mfd_c3_fk IMMEDIATE").await,
        "23503"
    );
    // Like any other statement error inside an explicit block, this aborted
    // the transaction.
    assert_eq!(err_code(&mut s, "SELECT 1").await, "25P02");
    ok(&mut s, "ROLLBACK").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM mfd_c3").await, 0);
}

#[tokio::test]
async fn match_full_null_mix_still_immediate_without_deferrable() {
    // Sanity check for the contrast above: without DEFERRABLE, the MATCH
    // FULL null-mix check is still an immediate `23503`, even inside an
    // explicit transaction block (it is `INITIALLY IMMEDIATE`, PostgreSQL's
    // default, and cannot be deferred at all since it isn't DEFERRABLE).
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE mfd_p4 (a INT, b INT, PRIMARY KEY (a, b))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE mfd_c4 (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES mfd_p4 (a, b) MATCH FULL)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    assert_eq!(
        err_code(&mut s, "INSERT INTO mfd_c4 VALUES (1, 1, NULL)").await,
        "23503"
    );
    ok(&mut s, "ROLLBACK").await;
}

// ---------------------------------------------------------------------------
// DEFERRABLE / INITIALLY DEFERRED
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deferred_fk_violated_then_fixed_before_commit_succeeds() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE dp (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE dc (id INT PRIMARY KEY, pid INT \
         REFERENCES dp(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    // The parent row doesn't exist yet — the check is deferred, not an
    // immediate error.
    ok(&mut s, "INSERT INTO dc VALUES (1, 999)").await;
    // Fix it before COMMIT.
    ok(&mut s, "INSERT INTO dp VALUES (999)").await;
    ok(&mut s, "COMMIT").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM dc").await, 1);
}

#[tokio::test]
async fn deferred_fk_left_violated_fails_at_commit_and_aborts() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE dp2 (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE dc2 (id INT PRIMARY KEY, pid INT \
         REFERENCES dp2(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO dc2 VALUES (1, 999)").await;
    // Never fixed: COMMIT itself fails with the FK violation.
    assert_eq!(err_code(&mut s, "COMMIT").await, "23503");
    // The failed COMMIT rolled the whole transaction back: nothing was
    // written, and the session is immediately usable again (no leftover
    // transaction/aborted state to clean up with ROLLBACK).
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM dc2").await, 0);
    ok(&mut s, "INSERT INTO dc2 VALUES (2, NULL)").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM dc2").await, 1);
}

#[tokio::test]
async fn rollback_discards_pending_deferred_checks() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE rbp (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE rbc (id INT PRIMARY KEY, pid INT \
         REFERENCES rbp(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    // Queues a deferred check that would fail if ever validated.
    ok(&mut s, "INSERT INTO rbc VALUES (1, 999)").await;
    ok(&mut s, "ROLLBACK").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM rbc").await, 0);
    // A later, otherwise-empty transaction commits cleanly: the discarded
    // check does not resurface and does not fail this COMMIT.
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "SELECT 1").await;
    ok(&mut s, "COMMIT").await;
}

#[tokio::test]
async fn no_action_defers_but_restrict_never_does_under_the_same_deferrable() {
    // NO ACTION: deferred, and satisfiable again before COMMIT.
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE np (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE ncn (id INT PRIMARY KEY, pid INT \
         REFERENCES np(id) ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "INSERT INTO np VALUES (1)").await;
    ok(&mut s, "INSERT INTO ncn VALUES (1, 1)").await;
    ok(&mut s, "BEGIN").await;
    // Deleting the still-referenced parent succeeds immediately: the
    // resulting dangling reference is deferred, not checked per-statement.
    ok(&mut s, "DELETE FROM np WHERE id = 1").await;
    ok(&mut s, "INSERT INTO np VALUES (1)").await;
    ok(&mut s, "COMMIT").await;

    // RESTRICT: verified against PostgreSQL's own source
    // (`src/backend/utils/adt/ri_triggers.c`) to never defer, regardless of
    // DEFERRABLE/INITIALLY DEFERRED — so the same shape of DELETE fails
    // immediately, inside the transaction, not at COMMIT.
    let mut s2 = session().await;
    ok(&mut s2, "CREATE TABLE rp (id INT PRIMARY KEY)").await;
    ok(
        &mut s2,
        "CREATE TABLE rc (id INT PRIMARY KEY, pid INT \
         REFERENCES rp(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s2, "INSERT INTO rp VALUES (1)").await;
    ok(&mut s2, "INSERT INTO rc VALUES (1, 1)").await;
    ok(&mut s2, "BEGIN").await;
    assert_eq!(
        err_code(&mut s2, "DELETE FROM rp WHERE id = 1").await,
        "23503"
    );
    ok(&mut s2, "ROLLBACK").await;
    assert_eq!(scalar_i64(&mut s2, "SELECT count(*) FROM rp").await, 1);
}

// ---------------------------------------------------------------------------
// SET CONSTRAINTS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_constraints_deferred_postpones_an_initially_immediate_deferrable_fk() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE sip (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE sic (id INT PRIMARY KEY, pid INT \
         CONSTRAINT sic_fk REFERENCES sip(id) DEFERRABLE)", // INITIALLY IMMEDIATE (PG default)
    )
    .await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "SET CONSTRAINTS sic_fk DEFERRED").await;
    // Without the SET CONSTRAINTS above, this INSERT would fail immediately
    // (23503, the parent row doesn't exist yet); deferred, it succeeds now.
    ok(&mut s, "INSERT INTO sic VALUES (1, 999)").await;
    ok(&mut s, "INSERT INTO sip VALUES (999)").await;
    ok(&mut s, "COMMIT").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM sic").await, 1);
}

#[tokio::test]
async fn set_constraints_immediate_forces_failure_now() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE scp (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE scc (id INT PRIMARY KEY, pid INT \
         CONSTRAINT scc_fk REFERENCES scp(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO scc VALUES (1, 999)").await; // deferred, no error yet
    // Forces the queued check to run right now.
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS scc_fk IMMEDIATE").await,
        "23503"
    );
    // Like any other statement error inside an explicit block, this aborted
    // the transaction.
    assert_eq!(err_code(&mut s, "SELECT 1").await, "25P02");
    ok(&mut s, "ROLLBACK").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM scc").await, 0);
}

#[tokio::test]
async fn set_constraints_all_immediate_also_forces_failure() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE scp2 (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE scc2 (id INT PRIMARY KEY, pid INT \
         REFERENCES scp2(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO scc2 VALUES (1, 999)").await;
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS ALL IMMEDIATE").await,
        "23503"
    );
    ok(&mut s, "ROLLBACK").await;
}

#[tokio::test]
async fn set_constraints_on_not_deferrable_errors_wrong_object_type() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE scp3 (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE scc3 (id INT PRIMARY KEY, \
         pid INT CONSTRAINT scc3_fk REFERENCES scp3(id))", // NOT DEFERRABLE (PG default)
    )
    .await;
    ok(&mut s, "BEGIN").await;
    // PostgreSQL's actual error for this is 42809 ("constraint ... is not
    // deferrable"), not 42704 ("does not exist") — verified against
    // `AfterTriggerSetState` in `src/backend/commands/trigger.c`.
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS scc3_fk DEFERRED").await,
        "42809"
    );
    ok(&mut s, "ROLLBACK").await;
    // IMMEDIATE on a NOT DEFERRABLE constraint is a silent no-op (it is
    // already always immediate) — PostgreSQL only raises the error on the
    // DEFERRED branch.
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "SET CONSTRAINTS scc3_fk IMMEDIATE").await;
    ok(&mut s, "COMMIT").await;
}

#[tokio::test]
async fn set_constraints_unknown_name_is_undefined_object() {
    let mut s = session().await;
    ok(&mut s, "BEGIN").await;
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS nope DEFERRED").await,
        "42704"
    );
    ok(&mut s, "ROLLBACK").await;
}

#[tokio::test]
async fn set_constraints_outside_transaction_is_a_no_op_only_when_valid() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE sop (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE soc (id INT PRIMARY KEY, pid INT \
         CONSTRAINT soc_fk REFERENCES sop(id) DEFERRABLE)",
    )
    .await;
    // No BEGIN: PostgreSQL "emits a warning and otherwise has no effect"
    // outside a transaction block, but only for a request that is itself
    // valid -- ALL, or a real, appropriately-deferrable name. There is no
    // per-transaction mode for these to change (autocommit has none) and no
    // deferred check could possibly be pending yet, so they are true no-ops.
    ok(&mut s, "SET CONSTRAINTS ALL DEFERRED").await;
    ok(&mut s, "SET CONSTRAINTS soc_fk DEFERRED").await;
    ok(&mut s, "SET CONSTRAINTS soc_fk IMMEDIATE").await;
    // Verified against a live PostgreSQL 16 instance: outside a transaction
    // block, PostgreSQL still validates the name and raises the same
    // `42704`/`42809` it would inside one (alongside its own `WARNING: SET
    // CONSTRAINTS can only be used in transaction blocks`) -- an unknown
    // name or a non-deferrable name is not silently accepted just because
    // there's no explicit transaction open.
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS nonexistent_constraint IMMEDIATE").await,
        "42704"
    );
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS nonexistent_constraint DEFERRED").await,
        "42704"
    );
    ok(
        &mut s,
        "CREATE TABLE soc2 (id INT PRIMARY KEY, \
         pid INT CONSTRAINT soc2_fk REFERENCES sop(id))", // NOT DEFERRABLE (PG default)
    )
    .await;
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS soc2_fk DEFERRED").await,
        "42809"
    );
    // IMMEDIATE on a NOT DEFERRABLE constraint is a no-op even outside a
    // transaction block (it is already always immediate).
    ok(&mut s, "SET CONSTRAINTS soc2_fk IMMEDIATE").await;
}

#[tokio::test]
async fn set_constraints_error_inside_transaction_aborts_it() {
    // A `SET CONSTRAINTS` error inside an explicit transaction block aborts
    // it exactly like any other statement error: the transaction is left in
    // a failed state where every subsequent statement (other than
    // ROLLBACK) raises `25P02` until it is rolled back.
    let mut s = session().await;
    ok(&mut s, "BEGIN").await;
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS nope DEFERRED").await,
        "42704"
    );
    assert_eq!(err_code(&mut s, "SELECT 1").await, "25P02");
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS ALL IMMEDIATE").await,
        "25P02"
    );
    ok(&mut s, "ROLLBACK").await;
    // The session is immediately usable again after the ROLLBACK.
    ok(&mut s, "SELECT 1").await;

    // Same check for the other error this statement can raise (`42809`).
    ok(&mut s, "CREATE TABLE seit_p (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE seit_c (id INT PRIMARY KEY, \
         pid INT CONSTRAINT seit_fk REFERENCES seit_p(id))", // NOT DEFERRABLE
    )
    .await;
    ok(&mut s, "BEGIN").await;
    assert_eq!(
        err_code(&mut s, "SET CONSTRAINTS seit_fk DEFERRED").await,
        "42809"
    );
    assert_eq!(err_code(&mut s, "SELECT 1").await, "25P02");
    ok(&mut s, "ROLLBACK").await;
    ok(&mut s, "SELECT 1").await;
}
