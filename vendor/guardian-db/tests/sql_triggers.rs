#![cfg(feature = "sql")]
//! End-to-end tests for triggers: `CREATE TRIGGER` / `DROP TRIGGER`,
//! `ALTER TABLE ... ENABLE/DISABLE TRIGGER`, BEFORE/AFTER × row/statement
//! firing semantics, `WHEN` conditions, `UPDATE OF`, NEW/OLD and `TG_*`
//! binding, atomicity, the FK-cascade divergence pin, and `pg_trigger`
//! introspection. See `docs/postgres-compat.md` § Triggers.

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

fn db() -> Arc<Database<MemoryStorage>> {
    Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"))
}

fn session(db: &Arc<Database<MemoryStorage>>) -> Session<MemoryStorage> {
    Session::new(db.clone(), "guardian")
}

async fn ok(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<ExecResult> {
    s.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
}

async fn err_code(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match s.execute(sql).await {
        Ok(_) => panic!("expected `{sql}` to fail, but it succeeded"),
        Err(e) => e.sqlstate().to_string(),
    }
}

async fn err_message(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match s.execute(sql).await {
        Ok(_) => panic!("expected `{sql}` to fail, but it succeeded"),
        Err(e) => e.to_string(),
    }
}

/// First row/column of the last result, as text.
async fn scalar(s: &mut Session<MemoryStorage>, sql: &str) -> Option<String> {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Rows { rows, .. }) => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.to_text()),
        other => panic!("`{sql}` did not produce rows: {other:?}"),
    }
}

async fn count(s: &mut Session<MemoryStorage>, sql: &str) -> i64 {
    scalar(s, sql).await.unwrap().parse().unwrap()
}

/// The command-completion tag of the last result.
async fn tag(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Command { tag }) => tag,
        other => panic!("`{sql}` did not produce a command tag: {other:?}"),
    }
}

/// All rows of the last result rendered as text (`NULL` for SQL NULL).
async fn rows_text(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<Vec<String>> {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Rows { rows, .. }) => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or_else(|| "NULL".into()))
                    .collect()
            })
            .collect(),
        other => panic!("`{sql}` did not produce rows: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// BEFORE ROW: NEW modification, suppression, PK relocation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_insert_row_modifies_new() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, qty int, total int)",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION double_total() RETURNS trigger AS $$ BEGIN \
         NEW.total := NEW.qty * 2; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION double_total()",
    )
    .await;
    // Both the stored row and RETURNING see the trigger-modified value.
    assert_eq!(
        scalar(
            &mut s,
            "INSERT INTO t (id, qty) VALUES (1, 5) RETURNING total"
        )
        .await
        .unwrap(),
        "10"
    );
    assert_eq!(
        scalar(&mut s, "SELECT total FROM t WHERE id = 1")
            .await
            .unwrap(),
        "10"
    );
}

#[tokio::test]
async fn before_insert_row_return_null_suppresses_row() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE FUNCTION suppress() RETURNS trigger AS $$ BEGIN RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION suppress()",
    )
    .await;
    assert_eq!(tag(&mut s, "INSERT INTO t VALUES (1)").await, "INSERT 0 0");
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 0);
    // Suppressed rows are absent from RETURNING too.
    assert!(
        scalar(&mut s, "INSERT INTO t VALUES (2) RETURNING id")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn before_insert_pk_modification_relocates_row() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION bump_pk() RETURNS trigger AS $$ BEGIN \
         NEW.id := NEW.id + 100; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION bump_pk()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1, 7)").await;
    // The row id derives from the *post-trigger* primary key.
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM t WHERE id = 1").await,
        0
    );
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 101")
            .await
            .unwrap(),
        "7"
    );
    ok(&mut s, "ALTER TABLE t DISABLE TRIGGER trg").await;
    ok(&mut s, "UPDATE t SET v = 8 WHERE id = 101").await;
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 101")
            .await
            .unwrap(),
        "8"
    );
}

#[tokio::test]
async fn before_update_row_modifies_new() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 1)").await;
    ok(
        &mut s,
        "CREATE FUNCTION tenfold() RETURNS trigger AS $$ BEGIN \
         NEW.v := NEW.v * 10; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION tenfold()",
    )
    .await;
    assert_eq!(
        scalar(&mut s, "UPDATE t SET v = 2 WHERE id = 1 RETURNING v")
            .await
            .unwrap(),
        "20"
    );
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 1")
            .await
            .unwrap(),
        "20"
    );
}

#[tokio::test]
async fn before_update_row_return_null_suppresses_update() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 1)").await;
    ok(
        &mut s,
        "CREATE FUNCTION suppress() RETURNS trigger AS $$ BEGIN RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION suppress()",
    )
    .await;
    assert_eq!(tag(&mut s, "UPDATE t SET v = 99").await, "UPDATE 0");
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 1")
            .await
            .unwrap(),
        "1"
    );
}

#[tokio::test]
async fn before_delete_row_return_null_suppresses_delete() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(&mut s, "INSERT INTO t VALUES (1)").await;
    ok(
        &mut s,
        "CREATE FUNCTION suppress() RETURNS trigger AS $$ BEGIN RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE DELETE ON t FOR EACH ROW EXECUTE FUNCTION suppress()",
    )
    .await;
    assert_eq!(tag(&mut s, "DELETE FROM t").await, "DELETE 0");
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 1);
}

// ---------------------------------------------------------------------------
// AFTER ROW: final values, OLD/NEW
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_insert_row_sees_final_values() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "CREATE TABLE audit (id int, v int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION bump() RETURNS trigger AS $$ BEGIN \
         NEW.v := NEW.v + 1; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION log_new() RETURNS trigger AS $$ BEGIN \
         INSERT INTO audit (id, v) VALUES (NEW.id, NEW.v); RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER a_before BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION bump()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER b_after AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_new()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1, 10)").await;
    // The AFTER trigger observed the post-BEFORE-trigger value.
    assert_eq!(
        scalar(&mut s, "SELECT v FROM audit WHERE id = 1")
            .await
            .unwrap(),
        "11"
    );
}

#[tokio::test]
async fn after_update_row_sees_old_and_new() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "CREATE TABLE audit (old_v int, new_v int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 5)").await;
    ok(
        &mut s,
        "CREATE FUNCTION log_change() RETURNS trigger AS $$ BEGIN \
         INSERT INTO audit (old_v, new_v) VALUES (OLD.v, NEW.v); RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    )
    .await;
    ok(&mut s, "UPDATE t SET v = 6 WHERE id = 1").await;
    assert_eq!(
        rows_text(&mut s, "SELECT old_v, new_v FROM audit").await,
        vec![vec!["5".to_string(), "6".to_string()]]
    );
}

#[tokio::test]
async fn after_delete_row_sees_old() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "CREATE TABLE audit (old_id int, old_v int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 42)").await;
    ok(
        &mut s,
        "CREATE FUNCTION log_old() RETURNS trigger AS $$ BEGIN \
         INSERT INTO audit (old_id, old_v) VALUES (OLD.id, OLD.v); RETURN OLD; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER DELETE ON t FOR EACH ROW EXECUTE FUNCTION log_old()",
    )
    .await;
    ok(&mut s, "DELETE FROM t WHERE id = 1").await;
    assert_eq!(
        rows_text(&mut s, "SELECT old_id, old_v FROM audit").await,
        vec![vec!["1".to_string(), "42".to_string()]]
    );
}

// ---------------------------------------------------------------------------
// Statement-level triggers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statement_triggers_fire_once_for_multirow_statement() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(&mut s, "CREATE TABLE tally (kind text)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (kind) VALUES (TG_WHEN || ' ' || TG_LEVEL); RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER s1 BEFORE INSERT ON t FOR EACH STATEMENT EXECUTE FUNCTION note()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER s2 AFTER INSERT ON t FOR EACH STATEMENT EXECUTE FUNCTION note()",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION note_row() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (kind) VALUES ('ROW'); RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER r1 BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION note_row()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1), (2), (3)").await;
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE kind = 'BEFORE STATEMENT'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE kind = 'AFTER STATEMENT'"
        )
        .await,
        1
    );
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM tally WHERE kind = 'ROW'").await,
        3
    );
}

#[tokio::test]
async fn statement_triggers_fire_on_zero_rows() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "CREATE TABLE tally (kind text)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (kind) VALUES (TG_WHEN || ' ' || TG_OP); RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION note_row() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (kind) VALUES ('ROW'); RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    for ddl in [
        "CREATE TRIGGER s1 BEFORE UPDATE ON t FOR EACH STATEMENT EXECUTE FUNCTION note()",
        "CREATE TRIGGER s2 AFTER UPDATE ON t FOR EACH STATEMENT EXECUTE FUNCTION note()",
        "CREATE TRIGGER s3 BEFORE INSERT ON t FOR EACH STATEMENT EXECUTE FUNCTION note()",
        "CREATE TRIGGER s4 AFTER INSERT ON t FOR EACH STATEMENT EXECUTE FUNCTION note()",
        "CREATE TRIGGER r1 BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION note_row()",
        "CREATE TRIGGER r2 BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION note_row()",
    ] {
        ok(&mut s, ddl).await;
    }
    // Zero matched rows still fire BEFORE+AFTER STATEMENT exactly once each.
    ok(&mut s, "UPDATE t SET v = 1 WHERE false").await;
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE kind = 'BEFORE UPDATE'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE kind = 'AFTER UPDATE'"
        )
        .await,
        1
    );
    // Empty INSERT ... SELECT source: same.
    ok(&mut s, "INSERT INTO t SELECT * FROM t WHERE false").await;
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE kind = 'BEFORE INSERT'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE kind = 'AFTER INSERT'"
        )
        .await,
        1
    );
    // No row triggers fired at all.
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM tally WHERE kind = 'ROW'").await,
        0
    );
}

#[tokio::test]
async fn before_statement_writes_visible_to_statement() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, flag boolean)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, false)").await;
    ok(
        &mut s,
        "CREATE FUNCTION add_marker() RETURNS trigger AS $$ BEGIN \
         INSERT INTO t (id, flag) VALUES (99, false); RETURN NULL; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH STATEMENT EXECUTE FUNCTION add_marker()",
    )
    .await;
    // The BEFORE STATEMENT trigger's insert is visible to the UPDATE's own
    // scan: both rows — the original and the marker — get flagged.
    ok(&mut s, "UPDATE t SET flag = true").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM t WHERE flag").await, 2);
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 2);
}

// ---------------------------------------------------------------------------
// UPDATE OF / WHEN
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_of_fires_on_listed_assignment_only() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, a int, b int)").await;
    ok(&mut s, "CREATE TABLE tally (x int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 1, 1), (2, 2, 2)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (x) VALUES (1); RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION note()",
    )
    .await;
    // `SET b` does not mention `a`: no fire.
    ok(&mut s, "UPDATE t SET b = 5").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 0);
    // `SET a = a` mentions `a` even though no value changes (PostgreSQL
    // matches the assignment list, not value diffs): fires per row.
    ok(&mut s, "UPDATE t SET a = a").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 2);
    // Mixed assignment fires once per row, not once per matched column.
    ok(&mut s, "UPDATE t SET a = 1, b = 2 WHERE id = 1").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 3);
}

#[tokio::test]
async fn when_condition_filters_row_trigger() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, amount int)").await;
    ok(&mut s, "CREATE TABLE tally (x int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (x) VALUES (NEW.id); RETURN NULL; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW WHEN (NEW.amount > 100) \
         EXECUTE FUNCTION note()",
    )
    .await;
    // 50 → row written, trigger skipped.
    ok(&mut s, "INSERT INTO t VALUES (1, 50)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 0);
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 1);
    // 200 → fires.
    ok(&mut s, "INSERT INTO t VALUES (2, 200)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 1);
    // NULL comparison result → does not fire (PostgreSQL: WHEN NULL skips).
    ok(&mut s, "INSERT INTO t VALUES (3, NULL)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 1);
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 3);
}

#[tokio::test]
async fn when_references_new_and_old() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, a int, b int)").await;
    ok(&mut s, "CREATE TABLE tally (x int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 1, 1)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (x) VALUES (1); RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER UPDATE ON t FOR EACH ROW \
         WHEN (NEW.a IS DISTINCT FROM OLD.a) EXECUTE FUNCTION note()",
    )
    .await;
    // Same value: no real change of `a`, no fire.
    ok(&mut s, "UPDATE t SET a = 1, b = 9 WHERE id = 1").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 0);
    // Actual change fires.
    ok(&mut s, "UPDATE t SET a = 2 WHERE id = 1").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 1);
}

// ---------------------------------------------------------------------------
// Ordering, chaining, TG_* variables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn triggers_fire_in_alphabetical_order_and_chain_new() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, tag text)").await;
    ok(
        &mut s,
        "CREATE FUNCTION append_a() RETURNS trigger AS $$ BEGIN \
         NEW.tag := NEW.tag || 'a'; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION append_b() RETURNS trigger AS $$ BEGIN \
         NEW.tag := NEW.tag || 'b'; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    // Created b first, a second: firing is by name, not creation order, and
    // each trigger's returned NEW feeds the next.
    ok(
        &mut s,
        "CREATE TRIGGER b_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION append_b()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER a_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION append_a()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1, '')").await;
    assert_eq!(
        scalar(&mut s, "SELECT tag FROM t WHERE id = 1")
            .await
            .unwrap(),
        "ab"
    );

    // Suppression in the alphabetically-first trigger skips the rest of the
    // chain: `b_trg`'s side effect never happens.
    ok(&mut s, "CREATE TABLE u (id int PRIMARY KEY)").await;
    ok(&mut s, "CREATE TABLE tally (x int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION suppress() RETURNS trigger AS $$ BEGIN RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (x) VALUES (1); RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER a_null BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION suppress()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER b_mark BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION note()",
    )
    .await;
    ok(&mut s, "INSERT INTO u VALUES (1)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM u").await, 0);
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 0);
}

#[tokio::test]
async fn tg_variables_bound() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE tlog (op text, whenv text, levelv text, trg text, tab text, sch text)",
    )
    .await;
    // One shared trigger function serving row and statement triggers across
    // all three operations — TG_OP is what disambiguates.
    ok(
        &mut s,
        "CREATE FUNCTION shared() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tlog VALUES (TG_OP, TG_WHEN, TG_LEVEL, TG_NAME, TG_TABLE_NAME, \
         TG_TABLE_SCHEMA); \
         IF TG_LEVEL = 'STATEMENT' THEN RETURN NULL; END IF; \
         IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; \
         RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER ins_row BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION shared()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER upd_row AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION shared()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER del_stmt BEFORE DELETE ON t FOR EACH STATEMENT EXECUTE FUNCTION shared()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1)").await;
    ok(&mut s, "UPDATE t SET id = id WHERE id = 1").await;
    ok(&mut s, "DELETE FROM t WHERE id = 1").await;
    assert_eq!(
        rows_text(
            &mut s,
            "SELECT op, whenv, levelv, trg, tab, sch FROM tlog ORDER BY op"
        )
        .await,
        vec![
            vec!["DELETE", "BEFORE", "STATEMENT", "del_stmt", "t", "public"],
            vec!["INSERT", "BEFORE", "ROW", "ins_row", "t", "public"],
            vec!["UPDATE", "AFTER", "ROW", "upd_row", "t", "public"],
        ]
        .into_iter()
        .map(|r| r.into_iter().map(String::from).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn trigger_writes_to_another_table() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v text)").await;
    // The audit table has a serial key: two firings in one statement must
    // draw distinct sequence values (trigger side effects fold back).
    ok(
        &mut s,
        "CREATE TABLE audit (n serial PRIMARY KEY, id int, v text)",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION log_new() RETURNS trigger AS $$ BEGIN \
         INSERT INTO audit (id, v) VALUES (NEW.id, NEW.v); RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_new()",
    )
    .await;
    // A fresh session proves preload: `audit` is never named in the DML text.
    let mut s2 = session(&db);
    ok(&mut s2, "INSERT INTO t VALUES (1, 'x'), (2, 'y')").await;
    assert_eq!(
        rows_text(&mut s2, "SELECT n, id, v FROM audit ORDER BY n").await,
        vec![
            vec!["1".to_string(), "1".to_string(), "x".to_string()],
            vec!["2".to_string(), "2".to_string(), "y".to_string()],
        ]
    );
}

// ---------------------------------------------------------------------------
// Recursion, atomicity, transactions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trigger_self_recursion_depth_guard_54001() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE FUNCTION recurse() RETURNS trigger AS $$ BEGIN \
         INSERT INTO t (id) VALUES (NEW.id + 1); RETURN NULL; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION recurse()",
    )
    .await;
    assert_eq!(err_code(&mut s, "INSERT INTO t VALUES (1)").await, "54001");
    // The failed statement persisted nothing, not even the outer row.
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 0);
}

#[tokio::test]
async fn trigger_error_aborts_statement_atomically() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(&mut s, "CREATE TABLE audit (id int)").await;
    // The trigger writes to the audit table for every row *before* raising
    // on id = 2 — proving the earlier rows' side effects roll back too.
    ok(
        &mut s,
        "CREATE FUNCTION guard() RETURNS trigger AS $$ BEGIN \
         INSERT INTO audit (id) VALUES (NEW.id); \
         IF NEW.id = 2 THEN RAISE EXCEPTION 'boom'; END IF; \
         RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION guard()",
    )
    .await;
    assert_eq!(
        err_code(&mut s, "INSERT INTO t VALUES (1), (2), (3)").await,
        "P0001"
    );
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 0);
    assert_eq!(count(&mut s, "SELECT count(*) FROM audit").await, 0);
}

#[tokio::test]
async fn trigger_error_inside_txn_aborts_transaction() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE FUNCTION boom() RETURNS trigger AS $$ BEGIN \
         RAISE EXCEPTION 'boom'; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION boom()",
    )
    .await;
    ok(&mut s, "BEGIN").await;
    assert_eq!(err_code(&mut s, "INSERT INTO t VALUES (1)").await, "P0001");
    // The transaction is aborted: further statements are refused ...
    assert_eq!(err_code(&mut s, "SELECT 1").await, "25P02");
    // ... and COMMIT rolls back.
    assert_eq!(tag(&mut s, "COMMIT").await, "ROLLBACK");
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 0);
}

// ---------------------------------------------------------------------------
// Foreign-key cascade interplay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fk_cascade_fires_child_triggers() {
    // FK CASCADE DELETE fires the child table's own BEFORE/AFTER DELETE
    // triggers for each cascaded row, matching PostgreSQL behaviour.
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE parent (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id int PRIMARY KEY, \
         pid int REFERENCES parent (id) ON DELETE CASCADE)",
    )
    .await;
    ok(&mut s, "CREATE TABLE audit (id int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION log_del() RETURNS trigger AS $$ BEGIN \
         INSERT INTO audit (id) VALUES (OLD.id); RETURN OLD; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER DELETE ON child FOR EACH ROW EXECUTE FUNCTION log_del()",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 1), (2, 1)").await;
    // Cascade removes the child rows AND fires the child's triggers (two rows).
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM child").await, 0);
    assert_eq!(count(&mut s, "SELECT count(*) FROM audit").await, 2);
    // Direct DELETE on the child also fires them.
    ok(&mut s, "INSERT INTO parent VALUES (2)").await;
    ok(&mut s, "INSERT INTO child VALUES (3, 2), (4, 2)").await;
    ok(&mut s, "DELETE FROM child").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM audit").await, 4);
}

#[tokio::test]
async fn after_delete_on_parent_observes_cascade() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE parent (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id int PRIMARY KEY, \
         pid int REFERENCES parent (id) ON DELETE CASCADE)",
    )
    .await;
    ok(&mut s, "CREATE TABLE snap (c int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION snap_children() RETURNS trigger AS $$ BEGIN \
         INSERT INTO snap (c) SELECT count(*) FROM child; RETURN OLD; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg AFTER DELETE ON parent FOR EACH ROW EXECUTE FUNCTION snap_children()",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 1), (2, 1)").await;
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    // AFTER row triggers fire after referential actions: the parent's
    // trigger already sees the cascaded child deletions.
    assert_eq!(
        rows_text(&mut s, "SELECT c FROM snap").await,
        vec![vec!["0".to_string()]]
    );
}

// ---------------------------------------------------------------------------
// ON CONFLICT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_conflict_do_update_fires_update_triggers() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "CREATE TABLE tally (op text, whenv text)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (op, whenv) VALUES (TG_OP, TG_WHEN); \
         IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; \
         RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    for ddl in [
        "CREATE TRIGGER bi BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION note()",
        "CREATE TRIGGER ai AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION note()",
        "CREATE TRIGGER bu BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION note()",
        "CREATE TRIGGER au AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION note()",
    ] {
        ok(&mut s, ddl).await;
    }
    ok(&mut s, "INSERT INTO t VALUES (1, 1)").await;
    ok(&mut s, "DELETE FROM tally").await;
    // The upsert hits the conflict path: the INSERT attempt fires BEFORE
    // INSERT row triggers, then the conflict resolution fires BEFORE/AFTER
    // UPDATE — and no AFTER INSERT (no row was inserted).
    ok(
        &mut s,
        "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET v = excluded.v",
    )
    .await;
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 1")
            .await
            .unwrap(),
        "5"
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE op = 'INSERT' AND whenv = 'BEFORE'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE op = 'UPDATE' AND whenv = 'BEFORE'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE op = 'UPDATE' AND whenv = 'AFTER'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM tally WHERE op = 'INSERT' AND whenv = 'AFTER'"
        )
        .await,
        0
    );
    // DO NOTHING: only the BEFORE INSERT attempt fires.
    ok(&mut s, "DELETE FROM tally").await;
    ok(
        &mut s,
        "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO NOTHING",
    )
    .await;
    assert_eq!(
        rows_text(&mut s, "SELECT op, whenv FROM tally").await,
        vec![vec!["INSERT".to_string(), "BEFORE".to_string()]]
    );
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 1")
            .await
            .unwrap(),
        "5"
    );
}

// ---------------------------------------------------------------------------
// Introspection and lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_trigger_introspection() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, a int, b int, v int)",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER bi_row BEFORE INSERT ON t FOR EACH ROW WHEN (NEW.v > 0) \
         EXECUTE FUNCTION tfn()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER au_stmt AFTER UPDATE ON t FOR EACH STATEMENT EXECUTE FUNCTION tfn()",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER of_row BEFORE UPDATE OF b, a ON t FOR EACH ROW EXECUTE FUNCTION tfn()",
    )
    .await;
    // tgtype bitmask: BEFORE INSERT ROW = 1|2|4 = 7; AFTER UPDATE STATEMENT = 16.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT tgtype FROM pg_trigger WHERE tgname = 'bi_row'"
        )
        .await
        .unwrap(),
        "7"
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT tgtype FROM pg_trigger WHERE tgname = 'au_stmt'"
        )
        .await
        .unwrap(),
        "16"
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT tgenabled FROM pg_trigger WHERE tgname = 'bi_row'"
        )
        .await
        .unwrap(),
        "O"
    );
    // tgrelid joins to pg_class.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT c.relname FROM pg_trigger g JOIN pg_class c ON c.oid = g.tgrelid \
             WHERE g.tgname = 'bi_row'"
        )
        .await
        .unwrap(),
        "t"
    );
    // tgfoid joins to pg_proc.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT p.proname FROM pg_trigger g JOIN pg_proc p ON p.oid = g.tgfoid \
             WHERE g.tgname = 'bi_row'"
        )
        .await
        .unwrap(),
        "tfn"
    );
    // The WHEN text round-trips.
    let qual = scalar(
        &mut s,
        "SELECT tgqual FROM pg_trigger WHERE tgname = 'bi_row'",
    )
    .await
    .unwrap();
    assert!(qual.to_lowercase().contains("new.v > 0"), "tgqual = {qual}");
    // tgattr: 1-based ordinals of the UPDATE OF columns, in list order
    // (t = id a b v → b = 3, a = 2).
    assert_eq!(
        scalar(
            &mut s,
            "SELECT tgattr FROM pg_trigger WHERE tgname = 'of_row'"
        )
        .await
        .unwrap(),
        "3 2"
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT tgattr FROM pg_trigger WHERE tgname = 'bi_row'"
        )
        .await
        .unwrap(),
        ""
    );
}

#[tokio::test]
async fn drop_table_drops_its_triggers() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NULL; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn()",
    )
    .await;
    ok(&mut s, "DROP TABLE t").await;
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    // The recreated table carries no triggers: inserts are not suppressed.
    ok(&mut s, "INSERT INTO t VALUES (1)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 1);
    assert_eq!(count(&mut s, "SELECT count(*) FROM pg_trigger").await, 0);
    // And the function became droppable with the trigger gone.
    ok(&mut s, "DROP FUNCTION tfn()").await;
}

#[tokio::test]
async fn drop_function_used_by_trigger_is_2bp01() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn()",
    )
    .await;
    assert_eq!(err_code(&mut s, "DROP FUNCTION tfn()").await, "2BP01");
    assert_eq!(err_code(&mut s, "DROP FUNCTION tfn").await, "2BP01");
    // Replacing it with a non-trigger function is refused for the same reason.
    assert_eq!(
        err_code(
            &mut s,
            "CREATE OR REPLACE FUNCTION tfn() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql"
        )
        .await,
        "2BP01"
    );
    ok(&mut s, "DROP TRIGGER trg ON t").await;
    ok(&mut s, "DROP FUNCTION tfn()").await;
}

#[tokio::test]
async fn create_or_replace_function_swaps_live_trigger_body() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION setv() RETURNS trigger AS $$ BEGIN \
         NEW.v := 1; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION setv()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1, 0)").await;
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 1")
            .await
            .unwrap(),
        "1"
    );
    // Bodies re-parse per call: the live trigger picks up the new body.
    ok(
        &mut s,
        "CREATE OR REPLACE FUNCTION setv() RETURNS trigger AS $$ BEGIN \
         NEW.v := 2; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (2, 0)").await;
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 2")
            .await
            .unwrap(),
        "2"
    );
}

#[tokio::test]
async fn create_or_replace_trigger_replaces_definition() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "CREATE TABLE tally (x int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION setv() RETURNS trigger AS $$ BEGIN \
         NEW.v := 111; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (x) VALUES (1); RETURN NULL; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION setv()",
    )
    .await;
    let oid_before = scalar(&mut s, "SELECT oid FROM pg_trigger WHERE tgname = 'trg'")
        .await
        .unwrap();
    // Replace: different timing and function. Old behavior must be gone.
    ok(
        &mut s,
        "CREATE OR REPLACE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION note()",
    )
    .await;
    ok(&mut s, "INSERT INTO t VALUES (1, 0)").await;
    assert_eq!(
        scalar(&mut s, "SELECT v FROM t WHERE id = 1")
            .await
            .unwrap(),
        "0"
    );
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 1);
    // The oid is preserved across OR REPLACE.
    let oid_after = scalar(&mut s, "SELECT oid FROM pg_trigger WHERE tgname = 'trg'")
        .await
        .unwrap();
    assert_eq!(oid_before, oid_after);
    assert_eq!(count(&mut s, "SELECT count(*) FROM pg_trigger").await, 1);
}

#[tokio::test]
async fn drop_trigger_error_shapes() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int)").await;
    assert_eq!(err_code(&mut s, "DROP TRIGGER nope ON t").await, "42704");
    ok(&mut s, "DROP TRIGGER IF EXISTS nope ON t").await;
    // A missing table errors even under IF EXISTS (PostgreSQL).
    assert_eq!(
        err_code(&mut s, "DROP TRIGGER IF EXISTS x ON missing").await,
        "42P01"
    );
    // The bare (MySQL-style) form without ON is rejected as syntax.
    assert_eq!(err_code(&mut s, "DROP TRIGGER trg").await, "42601");
}

#[tokio::test]
async fn enable_disable_trigger() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(&mut s, "CREATE TABLE tally (x int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION note() RETURNS trigger AS $$ BEGIN \
         INSERT INTO tally (x) VALUES (1); RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION note()",
    )
    .await;
    ok(&mut s, "ALTER TABLE t DISABLE TRIGGER trg").await;
    ok(&mut s, "INSERT INTO t VALUES (1)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 0);
    assert_eq!(
        scalar(
            &mut s,
            "SELECT tgenabled FROM pg_trigger WHERE tgname = 'trg'"
        )
        .await
        .unwrap(),
        "D"
    );
    ok(&mut s, "ALTER TABLE t ENABLE TRIGGER trg").await;
    ok(&mut s, "INSERT INTO t VALUES (2)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 1);
    // ALL / USER forms.
    ok(&mut s, "ALTER TABLE t DISABLE TRIGGER ALL").await;
    ok(&mut s, "INSERT INTO t VALUES (3)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 1);
    ok(&mut s, "ALTER TABLE t ENABLE TRIGGER ALL").await;
    ok(&mut s, "INSERT INTO t VALUES (4)").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM tally").await, 2);
    // Unknown trigger name.
    assert_eq!(
        err_code(&mut s, "ALTER TABLE t ENABLE TRIGGER nope").await,
        "42704"
    );
    // Replication-role variants are named-unsupported.
    assert_eq!(
        err_code(&mut s, "ALTER TABLE t ENABLE ALWAYS TRIGGER trg").await,
        "0A000"
    );
    assert_eq!(
        err_code(&mut s, "ALTER TABLE t ENABLE REPLICA TRIGGER trg").await,
        "0A000"
    );
}

// ---------------------------------------------------------------------------
// DDL error shapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_trigger_name_is_42710() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int)").await;
    ok(&mut s, "CREATE TABLE u (id int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn()",
    )
    .await;
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn()"
        )
        .await,
        "42710"
    );
    // The namespace is per table: the same name elsewhere is fine.
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION tfn()",
    )
    .await;
}

#[tokio::test]
async fn create_trigger_resolution_errors() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int)").await;
    // Missing function → 42883.
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION nofn()"
        )
        .await,
        "42883"
    );
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    // Missing table → 42P01.
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON missing FOR EACH ROW EXECUTE FUNCTION tfn()"
        )
        .await,
        "42P01"
    );
    // A view target → 42809 (INSTEAD OF is the only view-trigger form).
    ok(&mut s, "CREATE VIEW v AS SELECT * FROM t").await;
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON v FOR EACH ROW EXECUTE FUNCTION tfn()"
        )
        .await,
        "42809"
    );
    // WHEN referencing an unknown column → 42703; unqualified → 42703;
    // OLD on an INSERT trigger → 42P17.
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW WHEN (NEW.nope > 0) \
             EXECUTE FUNCTION tfn()"
        )
        .await,
        "42703"
    );
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW WHEN (id > 0) \
             EXECUTE FUNCTION tfn()"
        )
        .await,
        "42703"
    );
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW WHEN (OLD.id > 0) \
             EXECUTE FUNCTION tfn()"
        )
        .await,
        "42P17"
    );
    // UPDATE OF with an unknown column → 42703.
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE UPDATE OF nope ON t FOR EACH ROW EXECUTE FUNCTION tfn()"
        )
        .await,
        "42703"
    );
}

#[tokio::test]
async fn create_trigger_on_non_trigger_function_is_42p17() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION plain() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql",
    )
    .await;
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION plain()"
        )
        .await,
        "42P17"
    );
}

#[tokio::test]
async fn trigger_function_direct_call_is_0a000() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    assert_eq!(err_code(&mut s, "SELECT tfn()").await, "0A000");
    assert_eq!(
        err_message(&mut s, "SELECT tfn()").await,
        "trigger functions can only be called as triggers"
    );
}

#[tokio::test]
async fn trigger_function_definition_shapes_are_42p13() {
    let db = db();
    let mut s = session(&db);
    // RETURNS trigger + LANGUAGE sql (PostgreSQL: 42P13).
    assert_eq!(
        err_code(
            &mut s,
            "CREATE FUNCTION tfn() RETURNS trigger AS $$ SELECT 1 $$ LANGUAGE sql"
        )
        .await,
        "42P13"
    );
    // Declared arguments (PostgreSQL: 42P13).
    assert_eq!(
        err_code(
            &mut s,
            "CREATE FUNCTION tfn(a int) RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
             LANGUAGE plpgsql"
        )
        .await,
        "42P13"
    );
}

#[tokio::test]
async fn trigger_record_misuse_shapes() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY, v int)").await;
    ok(&mut s, "INSERT INTO t VALUES (1, 1)").await;
    // `RETURN NEW` where NEW is unbound (DELETE) is PostgreSQL's 55000.
    ok(
        &mut s,
        "CREATE FUNCTION ret_new() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE DELETE ON t FOR EACH ROW EXECUTE FUNCTION ret_new()",
    )
    .await;
    assert_eq!(
        err_code(&mut s, "DELETE FROM t WHERE id = 1").await,
        "55000"
    );
    ok(&mut s, "DROP TRIGGER trg ON t").await;
    // Reading a NEW field in a firing that does not bind it fails 42703
    // (documented divergence: PostgreSQL uses a 55000-class error here).
    ok(
        &mut s,
        "CREATE FUNCTION read_new() RETURNS trigger AS $$ BEGIN \
         INSERT INTO t (id, v) VALUES (99, NEW.v); RETURN OLD; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE DELETE ON t FOR EACH ROW EXECUTE FUNCTION read_new()",
    )
    .await;
    assert_eq!(
        err_code(&mut s, "DELETE FROM t WHERE id = 1").await,
        "42703"
    );
    ok(&mut s, "DROP TRIGGER trg ON t").await;
    // Assigning an OLD field is a named 0A000 (silently accepting a no-op
    // assignment would violate the truthfulness contract).
    ok(
        &mut s,
        "CREATE FUNCTION set_old() RETURNS trigger AS $$ BEGIN \
         OLD.v := 5; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION set_old()",
    )
    .await;
    let msg = err_message(&mut s, "UPDATE t SET v = 2 WHERE id = 1").await;
    assert!(msg.contains("assignment to OLD"), "got: {msg}");
}

#[tokio::test]
async fn unsupported_trigger_forms_are_typed_0a000() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (id int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION tfn() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    )
    .await;
    // INSTEAD OF on a plain table (not a view) → 42809 wrong_object_type.
    // INSTEAD OF on a view is now supported; TRUNCATE, CONSTRAINT TRIGGER, and
    // REFERENCING are all supported as of this milestone — they are exercised
    // in tests/sql_triggers_extended.rs.
    {
        let sql = "CREATE TRIGGER trg INSTEAD OF INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn()";
        let (code, msg) = match s.execute(sql).await {
            Ok(_) => panic!("expected `{sql}` to fail"),
            Err(e) => (e.sqlstate().to_string(), e.to_string()),
        };
        assert_eq!(
            code, "42809",
            "INSTEAD OF on table should be 42809 (got {code}: {msg})"
        );
        assert!(
            msg.contains("not a view"),
            "message `{msg}` should mention \"not a view\""
        );
    }
    for (sql, needle) in [
        (
            "CREATE TEMPORARY TRIGGER trg BEFORE INSERT ON t FOR EACH ROW \
             EXECUTE FUNCTION tfn()",
            "TEMPORARY",
        ),
        (
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH STATEMENT WHEN (1 = 1) \
             EXECUTE FUNCTION tfn()",
            "statement-level",
        ),
        (
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW \
             WHEN (NEW.id IN (SELECT id FROM t)) EXECUTE FUNCTION tfn()",
            "subqueries",
        ),
        (
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn(int)",
            "TG_ARGV",
        ),
    ] {
        let (code, msg) = match s.execute(sql).await {
            Ok(_) => panic!("expected `{sql}` to fail"),
            Err(e) => (e.sqlstate().to_string(), e.to_string()),
        };
        assert_eq!(code, "0A000", "for `{sql}` (got {code}: {msg})");
        assert!(
            msg.contains(needle),
            "message `{msg}` should name `{needle}`"
        );
    }
    // These return 42601 (syntax_error) matching PostgreSQL exactly.
    // DEFERRABLE on a non-CONSTRAINT TRIGGER is a syntax-level rejection.
    // PG-style literal trigger arguments do not parse in sqlparser 0.62 either.
    for sql in [
        "CREATE TRIGGER trg AFTER INSERT ON t DEFERRABLE INITIALLY DEFERRED \
         FOR EACH ROW EXECUTE FUNCTION tfn()",
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tfn('x')",
    ] {
        let code = match s.execute(sql).await {
            Ok(_) => panic!("expected `{sql}` to fail"),
            Err(e) => e.sqlstate().to_string(),
        };
        assert_eq!(code, "42601", "for `{sql}` (got {code})");
    }
}
