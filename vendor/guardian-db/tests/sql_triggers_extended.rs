#![cfg(feature = "sql")]
//! Extended PostgreSQL-parity trigger tests:
//! INSTEAD OF, TRUNCATE, CONSTRAINT TRIGGER / DEFERRABLE,
//! REFERENCING transition tables, and FK-cascade trigger firing.

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage, SqlValue};
use std::sync::Arc;

async fn session() -> Session<MemoryStorage> {
    let db = Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"));
    Session::new(db, "guardian")
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

async fn query_count(s: &mut Session<MemoryStorage>, sql: &str) -> i64 {
    let mut results = ok(s, sql).await;
    let res = results.pop().expect("no result");
    match res {
        ExecResult::Rows { rows, .. } => {
            let row = rows.into_iter().next().expect("no row");
            let val = row.into_iter().next().expect("no col");
            match &val {
                SqlValue::Int8(n) => *n,
                SqlValue::Int4(n) => *n as i64,
                SqlValue::Int2(n) => *n as i64,
                _ => val
                    .as_i64()
                    .unwrap_or_else(|| panic!("unexpected: {val:?}")),
            }
        }
        other => panic!("expected Rows, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. INSTEAD OF INSERT routes DML to the base table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn instead_of_insert_routes_to_base_table() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE base (id INT, val TEXT)").await;
    ok(&mut s, "CREATE VIEW v AS SELECT id, val FROM base").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION handle_insert() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO base (id, val) VALUES (NEW.id, NEW.val);
  RETURN NEW;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER tr INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION handle_insert()",
    )
    .await;

    // INSERT through the view; the trigger routes it to base.
    ok(&mut s, "INSERT INTO v VALUES (1, 'hello')").await;

    let cnt = query_count(&mut s, "SELECT count(*) FROM base WHERE val = 'hello'").await;
    assert_eq!(cnt, 1, "row should be in base table via INSTEAD OF trigger");
}

// ---------------------------------------------------------------------------
// 2. INSTEAD OF UPDATE with OLD and NEW available in body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn instead_of_update_with_old_and_new() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE base (id INT, val TEXT)").await;
    ok(&mut s, "CREATE VIEW v AS SELECT id, val FROM base").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION handle_update() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  UPDATE base SET val = NEW.val WHERE id = OLD.id;
  RETURN NEW;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER tr INSTEAD OF UPDATE ON v FOR EACH ROW EXECUTE FUNCTION handle_update()",
    )
    .await;

    ok(&mut s, "INSERT INTO base VALUES (1, 'original')").await;
    ok(&mut s, "UPDATE v SET val = 'updated' WHERE id = 1").await;

    // The trigger should have applied the update to the base table.
    let cnt = query_count(&mut s, "SELECT count(*) FROM base WHERE val = 'updated'").await;
    assert_eq!(cnt, 1, "base table should reflect the updated value");

    // Old value must be gone.
    let old_cnt = query_count(&mut s, "SELECT count(*) FROM base WHERE val = 'original'").await;
    assert_eq!(old_cnt, 0);
}

// ---------------------------------------------------------------------------
// 3. BEFORE TRUNCATE fires before the table is cleared
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_truncate_fires_before_clear() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE data (id INT)").await;
    ok(&mut s, "CREATE TABLE audit (op TEXT)").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION on_before_truncate() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit VALUES ('before_truncate');
  RETURN NULL;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER tr BEFORE TRUNCATE ON data FOR EACH STATEMENT EXECUTE FUNCTION on_before_truncate()",
    )
    .await;

    ok(&mut s, "INSERT INTO data VALUES (1), (2)").await;
    ok(&mut s, "TRUNCATE data").await;

    // Trigger fired.
    let audit_cnt = query_count(&mut s, "SELECT count(*) FROM audit").await;
    assert_eq!(audit_cnt, 1, "BEFORE TRUNCATE trigger should have fired");

    // Table was truncated.
    let data_cnt = query_count(&mut s, "SELECT count(*) FROM data").await;
    assert_eq!(data_cnt, 0, "data table should be empty after TRUNCATE");
}

// ---------------------------------------------------------------------------
// 4. AFTER TRUNCATE fires after the table is cleared
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_truncate_fires_after_clear() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE data (id INT)").await;
    ok(&mut s, "CREATE TABLE audit (op TEXT)").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION on_after_truncate() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit VALUES ('after_truncate');
  RETURN NULL;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER tr AFTER TRUNCATE ON data FOR EACH STATEMENT EXECUTE FUNCTION on_after_truncate()",
    )
    .await;

    ok(&mut s, "INSERT INTO data VALUES (1), (2), (3)").await;
    ok(&mut s, "TRUNCATE data").await;

    // Trigger fired.
    let audit_cnt = query_count(&mut s, "SELECT count(*) FROM audit").await;
    assert_eq!(audit_cnt, 1, "AFTER TRUNCATE trigger should have fired");

    // Table was truncated.
    let data_cnt = query_count(&mut s, "SELECT count(*) FROM data").await;
    assert_eq!(data_cnt, 0, "data table should be empty after TRUNCATE");
}

// ---------------------------------------------------------------------------
// 5. FOR EACH ROW on TRUNCATE events is a DDL-time error (42601)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn for_each_row_on_truncate_is_ddl_error() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE data (id INT)").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION noop() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  RETURN NULL;
END; $$"#,
    )
    .await;

    // TRUNCATE triggers must be FOR EACH STATEMENT; FOR EACH ROW is a syntax error.
    let code = err_code(
        &mut s,
        "CREATE TRIGGER tr BEFORE TRUNCATE ON data FOR EACH ROW EXECUTE FUNCTION noop()",
    )
    .await;
    assert_eq!(
        code, "0A000",
        "FOR EACH ROW on TRUNCATE should be SQLSTATE 0A000 (feature_not_supported)"
    );
}

// ---------------------------------------------------------------------------
// 6. CONSTRAINT TRIGGER with DEFERRABLE INITIALLY DEFERRED fires at COMMIT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn constraint_trigger_basics_and_deferred() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE data (id INT)").await;
    ok(&mut s, "CREATE TABLE audit (op TEXT)").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION deferred_check() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit VALUES ('deferred');
  RETURN NEW;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        r#"CREATE CONSTRAINT TRIGGER ct
AFTER INSERT ON data
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION deferred_check()"#,
    )
    .await;

    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO data VALUES (1)").await;

    // Trigger is deferred — audit must still be empty inside the transaction.
    let mid_cnt = query_count(&mut s, "SELECT count(*) FROM audit").await;
    assert_eq!(
        mid_cnt, 0,
        "deferred trigger must not have fired yet mid-transaction"
    );

    ok(&mut s, "COMMIT").await;

    // After commit the deferred trigger fires and the audit row is written.
    let post_cnt = query_count(&mut s, "SELECT count(*) FROM audit").await;
    assert_eq!(
        post_cnt, 1,
        "deferred trigger should fire at COMMIT and insert audit row"
    );
}

// ---------------------------------------------------------------------------
// 7. REFERENCING NEW TABLE in an AFTER STATEMENT trigger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn referencing_new_table_in_statement_trigger() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE data (id INT)").await;
    ok(&mut s, "CREATE TABLE audit (cnt INT)").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION snapshot_count() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit SELECT count(*) FROM new_table;
  RETURN NULL;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        r#"CREATE TRIGGER tr
AFTER INSERT ON data
REFERENCING NEW TABLE AS new_table
FOR EACH STATEMENT
EXECUTE FUNCTION snapshot_count()"#,
    )
    .await;

    // Insert two rows in one statement.
    ok(&mut s, "INSERT INTO data VALUES (1), (2)").await;

    // The statement trigger should have received the transition table with 2 rows.
    let cnt = query_count(&mut s, "SELECT cnt FROM audit LIMIT 1").await;
    assert_eq!(
        cnt, 2,
        "REFERENCING NEW TABLE should capture both inserted rows"
    );
}

// ---------------------------------------------------------------------------
// 8. FK CASCADE DELETE fires BEFORE/AFTER DELETE triggers on the child table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fk_cascade_delete_fires_child_trigger() {
    let mut s = session().await;

    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .await;
    ok(&mut s, "CREATE TABLE audit (op TEXT)").await;
    ok(
        &mut s,
        r#"CREATE FUNCTION on_child_delete() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit VALUES ('child_deleted');
  RETURN OLD;
END; $$"#,
    )
    .await;
    ok(
        &mut s,
        "CREATE TRIGGER tr BEFORE DELETE ON child FOR EACH ROW EXECUTE FUNCTION on_child_delete()",
    )
    .await;

    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 1)").await;

    // Deleting the parent cascades to the child and fires the child's trigger.
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;

    let cnt = query_count(&mut s, "SELECT count(*) FROM audit").await;
    assert_eq!(
        cnt, 1,
        "child BEFORE DELETE trigger should have fired during FK cascade"
    );

    // Both parent and child rows must be gone.
    let child_cnt = query_count(&mut s, "SELECT count(*) FROM child").await;
    assert_eq!(child_cnt, 0);
}

// ---------------------------------------------------------------------------
// 9. FK cascade depth guard: 54001 at depth > 25
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fk_cascade_depth_guard() {
    let mut s = session().await;

    // Self-referential table that allows deep cascade chains.
    ok(
        &mut s,
        "CREATE TABLE tree (id INT PRIMARY KEY, parent_id INT REFERENCES tree(id) ON DELETE CASCADE)",
    )
    .await;

    // Build a chain: 1 -> 2 -> 3 -> ... -> 26
    // Inserting root first (NULL parent), then each subsequent child.
    ok(&mut s, "INSERT INTO tree VALUES (1, NULL)").await;
    for i in 2usize..=26 {
        ok(&mut s, &format!("INSERT INTO tree VALUES ({i}, {})", i - 1)).await;
    }

    // Deleting the root triggers 26 levels of cascade — exceeds MAX_TRIGGER_DEPTH=25.
    let code = err_code(&mut s, "DELETE FROM tree WHERE id = 1").await;
    assert_eq!(
        code, "54001",
        "cascade depth > 25 should yield SQLSTATE 54001 (stack_depth_limit_exceeded)"
    );
}
