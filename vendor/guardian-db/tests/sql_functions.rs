#![cfg(feature = "sql")]
//! End-to-end tests for `CREATE FUNCTION` / `DROP FUNCTION`: `LANGUAGE SQL`
//! functions, the PL/pgSQL subset, `CREATE OR REPLACE`, signature conflicts,
//! recursion, and `pg_proc` introspection. See `docs/postgres-compat.md` for
//! the exact supported subset.

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

// ---------------------------------------------------------------------------
// LANGUAGE SQL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sql_language_basic_call_and_arg_binding() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION add(a int, b int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + $2 $$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT add(2, 3)").await.unwrap(), "5");
    assert_eq!(scalar(&mut s, "SELECT add(10, -4)").await.unwrap(), "6");
}

#[tokio::test]
async fn sql_language_multi_statement_body_returns_last_statement() {
    let db = db();
    let mut s = session(&db);
    ok(&mut s, "CREATE TABLE t (x int)").await;
    ok(
        &mut s,
        "CREATE FUNCTION seed(n int) RETURNS int LANGUAGE SQL AS $$ \
         INSERT INTO t(x) VALUES ($1); SELECT $1 + 1 $$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT seed(41)").await.unwrap(), "42");
    // The INSERT side effect actually ran.
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 1);
    assert_eq!(scalar(&mut s, "SELECT x FROM t").await.unwrap(), "41");
}

#[tokio::test]
async fn create_or_replace_changes_behavior() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + 1 $$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT f(1)").await.unwrap(), "2");
    ok(
        &mut s,
        "CREATE OR REPLACE FUNCTION f(a int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 * 10 $$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT f(1)").await.unwrap(), "10");
}

#[tokio::test]
async fn duplicate_signature_without_or_replace_is_42723() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 $$",
    )
    .await;
    assert_eq!(
        err_code(
            &mut s,
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + 1 $$"
        )
        .await,
        "42723"
    );
    // The original definition is untouched.
    assert_eq!(scalar(&mut s, "SELECT f(5)").await.unwrap(), "5");
}

#[tokio::test]
async fn drop_function_and_missing_function_shapes() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 $$",
    )
    .await;
    ok(&mut s, "DROP FUNCTION f(int)").await;
    // Calling it now is an undefined-function error, like any unknown name.
    assert_eq!(err_code(&mut s, "SELECT f(1)").await, "42883");
    // Dropping again without IF EXISTS is also 42883.
    assert_eq!(err_code(&mut s, "DROP FUNCTION f(int)").await, "42883");
    // IF EXISTS tolerates the missing signature.
    ok(&mut s, "DROP FUNCTION IF EXISTS f(int)").await;
}

// ---------------------------------------------------------------------------
// PL/pgSQL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plpgsql_declare_assign_if_elsif_else_return() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION classify(n int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  label text := 'unknown';
BEGIN
  IF n > 0 THEN
    label := 'positive';
  ELSIF n = 0 THEN
    label := 'zero';
  ELSE
    label := 'negative';
  END IF;
  RETURN label;
END;
$$",
    )
    .await;
    assert_eq!(
        scalar(&mut s, "SELECT classify(5)").await.unwrap(),
        "positive"
    );
    assert_eq!(scalar(&mut s, "SELECT classify(0)").await.unwrap(), "zero");
    assert_eq!(
        scalar(&mut s, "SELECT classify(-3)").await.unwrap(),
        "negative"
    );
}

#[tokio::test]
async fn plpgsql_raise_exception_aborts_with_message() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION guard(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  IF n < 0 THEN
    RAISE EXCEPTION 'n must not be negative, got %', n;
  END IF;
  RETURN n;
END;
$$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT guard(5)").await.unwrap(), "5");
    let msg = err_message(&mut s, "SELECT guard(-1)").await;
    assert!(
        msg.contains("n must not be negative, got -1"),
        "message was: {msg}"
    );
    assert_eq!(err_code(&mut s, "SELECT guard(-1)").await, "P0001");
}

#[tokio::test]
async fn plpgsql_dml_against_real_tables_has_observable_side_effects() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE TABLE counters (id int PRIMARY KEY, value int)",
    )
    .await;
    ok(&mut s, "INSERT INTO counters VALUES (1, 10)").await;
    ok(
        &mut s,
        "CREATE FUNCTION bump(target_id int, amount int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  UPDATE counters SET value = value + amount WHERE id = target_id;
  DELETE FROM counters WHERE id = target_id AND value > 1000000;
  INSERT INTO counters VALUES (2, amount);
  RETURN amount;
END;
$$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT bump(1, 5)").await.unwrap(), "5");
    // Side effects from the function body are visible to a later statement.
    assert_eq!(
        scalar(&mut s, "SELECT value FROM counters WHERE id = 1")
            .await
            .unwrap(),
        "15"
    );
    assert_eq!(
        scalar(&mut s, "SELECT value FROM counters WHERE id = 2")
            .await
            .unwrap(),
        "5"
    );
}

// ---------------------------------------------------------------------------
// Recursion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn self_recursive_udf_computes_correctly() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION fact(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  IF n <= 1 THEN
    RETURN 1;
  ELSE
    RETURN n * fact(n - 1);
  END IF;
END;
$$",
    )
    .await;
    assert_eq!(scalar(&mut s, "SELECT fact(5)").await.unwrap(), "120");
    assert_eq!(scalar(&mut s, "SELECT fact(0)").await.unwrap(), "1");
}

#[tokio::test]
async fn self_recursive_udf_depth_guard() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION spin(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN spin(n + 1);
END;
$$",
    )
    .await;
    assert_eq!(err_code(&mut s, "SELECT spin(0)").await, "54001");
}

// ---------------------------------------------------------------------------
// Out-of-subset PL/pgSQL rejected at CREATE FUNCTION time
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plpgsql_for_loop_rejected_at_create_time() {
    let db = db();
    let mut s = session(&db);
    let msg = err_message(
        &mut s,
        "CREATE FUNCTION loopy() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  FOR i IN 1..10 LOOP
    RAISE NOTICE 'x';
  END LOOP;
  RETURN 1;
END;
$$",
    )
    .await;
    assert!(msg.contains("FOR loop"), "message was: {msg}");
    // The broken function never entered the catalog.
    assert_eq!(err_code(&mut s, "SELECT loopy()").await, "42883");
}

#[tokio::test]
async fn plpgsql_missing_return_rejected_at_create_time() {
    let db = db();
    let mut s = session(&db);
    assert_eq!(
        err_code(
            &mut s,
            "CREATE FUNCTION broken(a int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  a := a + 1;
END;
$$"
        )
        .await,
        "42P13"
    );
}

// ---------------------------------------------------------------------------
// pg_proc introspection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_proc_reflects_created_functions() {
    let db = db();
    let mut s = session(&db);
    ok(
        &mut s,
        "CREATE FUNCTION add(a int, b int) RETURNS int LANGUAGE SQL IMMUTABLE AS $$ SELECT $1 + $2 $$",
    )
    .await;
    ok(
        &mut s,
        "CREATE FUNCTION classify(n int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN 'x';
END;
$$",
    )
    .await;
    let rows = match ok(
        &mut s,
        "SELECT proname, prolang, provolatile, pronargs, prosrc FROM pg_proc \
         WHERE proname = 'add'",
    )
    .await
    .pop()
    {
        Some(ExecResult::Rows { rows, .. }) => rows,
        other => panic!("expected rows: {other:?}"),
    };
    assert_eq!(rows.len(), 1, "expected exactly one add() row: {rows:?}");
    let row = &rows[0];
    assert_eq!(row[0].to_text().unwrap(), "add");
    assert_eq!(row[1].to_text().unwrap(), "sql");
    assert_eq!(row[2].to_text().unwrap(), "i");
    assert_eq!(row[3].to_text().unwrap(), "2");
    assert!(row[4].to_text().unwrap().contains("$1 + $2"));

    assert_eq!(
        count(
            &mut s,
            "SELECT count(*) FROM pg_proc WHERE proname = 'classify'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT prolang FROM pg_proc WHERE proname = 'classify'"
        )
        .await
        .unwrap(),
        "plpgsql"
    );
}

// ---------------------------------------------------------------------------
// Argument modes / return shapes out of scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn out_parameters_are_unsupported() {
    let db = db();
    let mut s = session(&db);
    assert_eq!(
        err_code(
            &mut s,
            "CREATE FUNCTION f(a int, OUT b int) LANGUAGE SQL AS $$ SELECT $1 $$"
        )
        .await,
        "0A000"
    );
}

#[tokio::test]
async fn returns_table_is_unsupported() {
    let db = db();
    let mut s = session(&db);
    assert_eq!(
        err_code(
            &mut s,
            "CREATE FUNCTION f() RETURNS TABLE(a int) LANGUAGE SQL AS $$ SELECT 1 $$"
        )
        .await,
        "0A000"
    );
}
