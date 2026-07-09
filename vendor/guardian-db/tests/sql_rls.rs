#![cfg(feature = "sql")]
//! End-to-end tests for Row-Level Security: ENABLE/DISABLE, CREATE/DROP
//! POLICY, PostgreSQL combining semantics (permissive OR / restrictive AND,
//! default deny), per-command policies, role targeting, `auth.uid()` claims,
//! the `pg_policies` view, bypass roles, and catalog persistence.

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

fn db() -> Arc<Database<MemoryStorage>> {
    Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"))
}

fn session(db: &Arc<Database<MemoryStorage>>, role: &str) -> Session<MemoryStorage> {
    Session::new(db.clone(), role)
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

/// First row/column of a row-producing result, as text.
async fn scalar(s: &mut Session<MemoryStorage>, sql: &str) -> Option<String> {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Rows { rows, .. }) => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.to_text()),
        other => panic!("`{sql}` did not produce rows: {other:?}"),
    }
}

/// The command tag of the last result (e.g. `UPDATE 2`).
async fn tag(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    ok(s, sql).await.pop().unwrap().command_tag()
}

async fn count(s: &mut Session<MemoryStorage>, sql: &str) -> i64 {
    scalar(s, sql).await.unwrap().parse().unwrap()
}

/// `docs(id, owner, val)` with three rows, RLS enabled, no policies yet.
async fn setup(db: &Arc<Database<MemoryStorage>>) {
    let mut s = session(db, "guardian");
    ok(
        &mut s,
        "CREATE TABLE docs (id int PRIMARY KEY, owner text, val int)",
    )
    .await;
    ok(
        &mut s,
        "INSERT INTO docs VALUES (1, 'alice', 1), (2, 'bob', 2), (3, 'alice', 3)",
    )
    .await;
    ok(&mut s, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY").await;
}

// ---------------------------------------------------------------------------
// Enable / disable, default deny, bypass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rls_enabled_with_no_policies_is_default_deny() {
    let db = db();
    setup(&db).await;
    let mut alice = session(&db, "alice");
    assert_eq!(count(&mut alice, "SELECT count(*) FROM docs").await, 0);
    // Writes are denied too: no policy allows anything.
    assert_eq!(
        err_code(&mut alice, "INSERT INTO docs VALUES (4, 'alice', 4)").await,
        "42501"
    );
    assert_eq!(tag(&mut alice, "UPDATE docs SET val = 9").await, "UPDATE 0");
    assert_eq!(tag(&mut alice, "DELETE FROM docs").await, "DELETE 0");
}

#[tokio::test]
async fn disable_row_level_security_restores_access() {
    let db = db();
    setup(&db).await;
    let mut alice = session(&db, "alice");
    assert_eq!(count(&mut alice, "SELECT count(*) FROM docs").await, 0);
    let mut owner = session(&db, "guardian");
    ok(&mut owner, "ALTER TABLE docs DISABLE ROW LEVEL SECURITY").await;
    assert_eq!(count(&mut alice, "SELECT count(*) FROM docs").await, 3);
}

#[tokio::test]
async fn bypass_roles_see_everything() {
    let db = db();
    setup(&db).await;
    for role in ["service_role", "postgres", "guardian"] {
        let mut s = session(&db, role);
        assert_eq!(
            count(&mut s, "SELECT count(*) FROM docs").await,
            3,
            "role {role} must bypass RLS"
        );
    }
}

// ---------------------------------------------------------------------------
// FORCE ROW LEVEL SECURITY
// ---------------------------------------------------------------------------

#[tokio::test]
async fn force_rls_subjects_owner_roles_and_no_force_restores() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_own ON docs FOR SELECT USING (owner = current_user)",
    )
    .await;
    // Without FORCE, the owner roles bypass row security entirely.
    assert_eq!(count(&mut owner, "SELECT count(*) FROM docs").await, 3);
    ok(&mut owner, "ALTER TABLE docs FORCE ROW LEVEL SECURITY").await;
    // With ENABLE + FORCE they are subject to policies: no row is owned by
    // 'guardian' or 'postgres'.
    assert_eq!(count(&mut owner, "SELECT count(*) FROM docs").await, 0);
    let mut pg = session(&db, "postgres");
    assert_eq!(count(&mut pg, "SELECT count(*) FROM docs").await, 0);
    // Writes are policed too: only a SELECT policy exists, so INSERT denies.
    assert_eq!(
        err_code(&mut owner, "INSERT INTO docs VALUES (7, 'guardian', 7)").await,
        "42501"
    );
    // NO FORCE restores the owner exemption.
    ok(&mut owner, "ALTER TABLE docs NO FORCE ROW LEVEL SECURITY").await;
    assert_eq!(count(&mut owner, "SELECT count(*) FROM docs").await, 3);
}

#[tokio::test]
async fn force_rls_without_enable_has_no_effect() {
    let db = db();
    let mut s = session(&db, "guardian");
    ok(&mut s, "CREATE TABLE t (id int PRIMARY KEY)").await;
    ok(&mut s, "INSERT INTO t VALUES (1)").await;
    // FORCE only matters while row security is enabled (PostgreSQL).
    ok(&mut s, "ALTER TABLE t FORCE ROW LEVEL SECURITY").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM t").await, 1);
    let mut bob = session(&db, "bob");
    assert_eq!(count(&mut bob, "SELECT count(*) FROM t").await, 1);
}

#[tokio::test]
async fn service_role_still_bypasses_under_force() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(&mut owner, "ALTER TABLE docs FORCE ROW LEVEL SECURITY").await;
    // BYPASSRLS beats FORCE: service_role sees everything with no policies…
    let mut service = session(&db, "service_role");
    assert_eq!(count(&mut service, "SELECT count(*) FROM docs").await, 3);
    // …while the owner roles fall into default-deny.
    assert_eq!(count(&mut owner, "SELECT count(*) FROM docs").await, 0);
}

#[tokio::test]
async fn force_rls_flag_is_introspectable() {
    let db = db();
    setup(&db).await;
    let mut s = session(&db, "guardian");
    let probe = "SELECT relforcerowsecurity::text FROM pg_class WHERE relname = 'docs'";
    assert_eq!(scalar(&mut s, probe).await, Some("f".into()));
    ok(&mut s, "ALTER TABLE docs FORCE ROW LEVEL SECURITY").await;
    assert_eq!(scalar(&mut s, probe).await, Some("t".into()));
    ok(&mut s, "ALTER TABLE docs NO FORCE ROW LEVEL SECURITY").await;
    assert_eq!(scalar(&mut s, probe).await, Some("f".into()));
}

// ---------------------------------------------------------------------------
// Combining semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn permissive_policies_or_together() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_own ON docs FOR SELECT USING (owner = current_user)",
    )
    .await;
    ok(
        &mut owner,
        "CREATE POLICY p_big ON docs FOR SELECT USING (val >= 3)",
    )
    .await;
    // bob matches p_own for row 2 and p_big for row 3.
    let mut bob = session(&db, "bob");
    assert_eq!(count(&mut bob, "SELECT count(*) FROM docs").await, 2);
    assert_eq!(
        scalar(&mut bob, "SELECT string_agg(id::text, ',') FROM docs").await,
        Some("2,3".into())
    );
}

#[tokio::test]
async fn restrictive_policies_and_together() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_all ON docs FOR SELECT USING (true)",
    )
    .await;
    ok(
        &mut owner,
        "CREATE POLICY p_cap ON docs AS RESTRICTIVE FOR SELECT USING (val < 3)",
    )
    .await;
    let mut bob = session(&db, "bob");
    assert_eq!(count(&mut bob, "SELECT count(*) FROM docs").await, 2);
}

#[tokio::test]
async fn restrictive_alone_grants_nothing() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    // A restrictive policy without any permissive one still denies everything
    // (PostgreSQL: at least one permissive policy must pass).
    ok(
        &mut owner,
        "CREATE POLICY p_cap ON docs AS RESTRICTIVE FOR SELECT USING (true)",
    )
    .await;
    let mut bob = session(&db, "bob");
    assert_eq!(count(&mut bob, "SELECT count(*) FROM docs").await, 0);
}

#[tokio::test]
async fn null_policy_result_denies() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    // `owner = NULL` is NULL for every row: non-TRUE means invisible.
    ok(
        &mut owner,
        "CREATE POLICY p_null ON docs FOR SELECT USING (owner = NULL)",
    )
    .await;
    let mut bob = session(&db, "bob");
    assert_eq!(count(&mut bob, "SELECT count(*) FROM docs").await, 0);
}

// ---------------------------------------------------------------------------
// Per-command policies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn select_only_policy_does_not_grant_writes() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_read ON docs FOR SELECT USING (true)",
    )
    .await;
    let mut alice = session(&db, "alice");
    assert_eq!(count(&mut alice, "SELECT count(*) FROM docs").await, 3);
    // No UPDATE/DELETE policy: target rows are invisible to those commands.
    assert_eq!(tag(&mut alice, "UPDATE docs SET val = 9").await, "UPDATE 0");
    assert_eq!(tag(&mut alice, "DELETE FROM docs").await, "DELETE 0");
    // No INSERT policy: WITH CHECK denies.
    let err = alice
        .execute("INSERT INTO docs VALUES (4, 'alice', 4)")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42501");
    assert!(
        err.to_string()
            .contains("new row violates row-level security policy for table \"docs\""),
        "unexpected message: {err}"
    );
}

#[tokio::test]
async fn insert_with_check_enforced() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_ins ON docs FOR INSERT WITH CHECK (owner = current_user)",
    )
    .await;
    let mut alice = session(&db, "alice");
    assert_eq!(
        tag(&mut alice, "INSERT INTO docs VALUES (4, 'alice', 4)").await,
        "INSERT 0 1"
    );
    assert_eq!(
        err_code(&mut alice, "INSERT INTO docs VALUES (5, 'bob', 5)").await,
        "42501"
    );
    // FOR INSERT policies must not carry USING (PostgreSQL).
    assert_eq!(
        err_code(
            &mut owner,
            "CREATE POLICY bad ON docs FOR INSERT USING (true)"
        )
        .await,
        "42601"
    );
    // WITH CHECK is meaningless for SELECT/DELETE (PostgreSQL).
    assert_eq!(
        err_code(
            &mut owner,
            "CREATE POLICY bad ON docs FOR SELECT WITH CHECK (true)"
        )
        .await,
        "42601"
    );
}

#[tokio::test]
async fn update_splits_using_and_with_check() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_upd ON docs FOR UPDATE USING (owner = current_user) \
         WITH CHECK (val < 10)",
    )
    .await;
    let mut alice = session(&db, "alice");
    // Own row, new value passes WITH CHECK.
    assert_eq!(
        tag(&mut alice, "UPDATE docs SET val = 5 WHERE id = 1").await,
        "UPDATE 1"
    );
    // Own row, new value violates WITH CHECK: PostgreSQL error, 42501.
    assert_eq!(
        err_code(&mut alice, "UPDATE docs SET val = 50 WHERE id = 1").await,
        "42501"
    );
    // Someone else's row is filtered by USING: silently zero rows.
    assert_eq!(
        tag(&mut alice, "UPDATE docs SET val = 5 WHERE id = 2").await,
        "UPDATE 0"
    );
    // The failed update did not go through.
    let mut check = session(&db, "guardian");
    assert_eq!(
        scalar(&mut check, "SELECT val FROM docs WHERE id = 1").await,
        Some("5".into())
    );
}

#[tokio::test]
async fn update_with_check_falls_back_to_using() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_upd ON docs FOR UPDATE USING (owner = current_user)",
    )
    .await;
    let mut alice = session(&db, "alice");
    // Handing the row to someone else violates the (fallback) check.
    assert_eq!(
        err_code(&mut alice, "UPDATE docs SET owner = 'bob' WHERE id = 1").await,
        "42501"
    );
    assert_eq!(
        tag(&mut alice, "UPDATE docs SET val = 7 WHERE id = 1").await,
        "UPDATE 1"
    );
}

#[tokio::test]
async fn delete_filters_by_using() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_del ON docs FOR DELETE USING (owner = current_user)",
    )
    .await;
    let mut alice = session(&db, "alice");
    assert_eq!(tag(&mut alice, "DELETE FROM docs").await, "DELETE 2");
    let mut check = session(&db, "guardian");
    assert_eq!(
        scalar(&mut check, "SELECT owner FROM docs").await,
        Some("bob".into())
    );
}

#[tokio::test]
async fn on_conflict_do_update_requires_update_visibility() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_ins ON docs FOR INSERT WITH CHECK (true)",
    )
    .await;
    ok(
        &mut owner,
        "CREATE POLICY p_upd ON docs FOR UPDATE USING (owner = current_user)",
    )
    .await;
    let mut alice = session(&db, "alice");
    // Conflicting row 2 belongs to bob: not updatable under UPDATE USING.
    assert_eq!(
        err_code(
            &mut alice,
            "INSERT INTO docs VALUES (2, 'alice', 9) \
             ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val"
        )
        .await,
        "42501"
    );
    // Conflicting row 1 belongs to alice: allowed.
    assert_eq!(
        tag(
            &mut alice,
            "INSERT INTO docs VALUES (1, 'alice', 9) \
             ON CONFLICT (id) DO UPDATE SET val = EXCLUDED.val"
        )
        .await,
        "INSERT 0 1"
    );
}

// ---------------------------------------------------------------------------
// Role targeting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn to_role_list_limits_applicability() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_auth ON docs FOR SELECT TO authenticated USING (true)",
    )
    .await;
    let mut anon = session(&db, "anon");
    assert_eq!(count(&mut anon, "SELECT count(*) FROM docs").await, 0);
    let mut authed = session(&db, "authenticated");
    assert_eq!(count(&mut authed, "SELECT count(*) FROM docs").await, 3);
    // TO PUBLIC (and no TO clause) applies to every role.
    ok(
        &mut owner,
        "CREATE POLICY p_pub ON docs FOR SELECT TO public USING (val = 1)",
    )
    .await;
    assert_eq!(count(&mut anon, "SELECT count(*) FROM docs").await, 1);
}

// ---------------------------------------------------------------------------
// Scans inherit filtering (joins, subqueries, aggregates)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn joins_and_subqueries_inherit_visibility() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_own ON docs FOR SELECT USING (owner = current_user)",
    )
    .await;
    let mut alice = session(&db, "alice");
    assert_eq!(
        count(
            &mut alice,
            "SELECT count(*) FROM docs d JOIN docs e ON d.id = e.id"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &mut alice,
            "SELECT count(*) FROM (SELECT * FROM docs) AS sub"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &mut alice,
            "WITH visible AS (SELECT * FROM docs) SELECT count(*) FROM visible"
        )
        .await,
        2
    );
    // An indexed point lookup on a hidden row is filtered too.
    assert_eq!(
        count(&mut alice, "SELECT count(*) FROM docs WHERE id = 2").await,
        0
    );
}

// ---------------------------------------------------------------------------
// auth.uid() and session claims
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_uid_policy_with_set_claims() {
    let db = db();
    let uid_a = "0b9fbc1e-6a34-4bff-8df5-6b9f7c4e3d21";
    let uid_b = "7f3a1d52-9c1b-4e8e-b0a4-2c5d9e8f7a61";
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE TABLE notes (id int PRIMARY KEY, user_id text, body text)",
    )
    .await;
    ok(
        &mut owner,
        &format!("INSERT INTO notes VALUES (1, '{uid_a}', 'a'), (2, '{uid_b}', 'b')"),
    )
    .await;
    ok(&mut owner, "ALTER TABLE notes ENABLE ROW LEVEL SECURITY").await;
    ok(
        &mut owner,
        "CREATE POLICY p_own ON notes FOR SELECT TO authenticated \
         USING (user_id = auth.uid()::text)",
    )
    .await;

    let mut user = session(&db, "authenticated");
    // Without claims, auth.uid() is NULL: nothing is visible.
    assert_eq!(count(&mut user, "SELECT count(*) FROM notes").await, 0);
    ok(
        &mut user,
        &format!(
            "SET request.jwt.claims = \
             '{{\"sub\": \"{uid_a}\", \"role\": \"authenticated\"}}'"
        ),
    )
    .await;
    assert_eq!(
        scalar(&mut user, "SELECT auth.uid()::text").await,
        Some(uid_a.into())
    );
    assert_eq!(
        scalar(&mut user, "SELECT auth.role()").await,
        Some("authenticated".into())
    );
    let jwt = scalar(&mut user, "SELECT auth.jwt()").await.unwrap();
    assert!(jwt.contains(uid_a), "auth.jwt() must carry sub: {jwt}");
    assert_eq!(count(&mut user, "SELECT count(*) FROM notes").await, 1);
    assert_eq!(
        scalar(&mut user, "SELECT body FROM notes").await,
        Some("a".into())
    );
    // The per-claim variable form (PostgREST v9 style) works too.
    let mut user_b = session(&db, "authenticated");
    ok(
        &mut user_b,
        &format!("SELECT set_config('request.jwt.claim.sub', '{uid_b}', false)"),
    )
    .await;
    assert_eq!(
        scalar(&mut user_b, "SELECT body FROM notes").await,
        Some("b".into())
    );
}

// ---------------------------------------------------------------------------
// DDL surface: pg_policies, duplicate / missing policies, DROP POLICY
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_policies_reflects_catalog() {
    let db = db();
    setup(&db).await;
    let mut s = session(&db, "guardian");
    ok(
        &mut s,
        "CREATE POLICY p1 ON docs AS RESTRICTIVE FOR UPDATE TO alice, bob \
         USING (owner = current_user) WITH CHECK (val < 10)",
    )
    .await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT schemaname || '.' || tablename || '/' || policyname \
             FROM pg_policies"
        )
        .await,
        Some("public.docs/p1".into())
    );
    assert_eq!(
        scalar(&mut s, "SELECT permissive FROM pg_policies").await,
        Some("RESTRICTIVE".into())
    );
    assert_eq!(
        scalar(&mut s, "SELECT cmd FROM pg_policies").await,
        Some("UPDATE".into())
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT array_to_string(roles, ',') FROM pg_policies"
        )
        .await,
        Some("alice,bob".into())
    );
    assert_eq!(
        scalar(&mut s, "SELECT qual FROM pg_policies").await,
        Some("owner = current_user".into())
    );
    assert_eq!(
        scalar(&mut s, "SELECT with_check FROM pg_policies").await,
        Some("val < 10".into())
    );
    // pg_tables / pg_class expose the RLS flag.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT rowsecurity::text FROM pg_tables WHERE tablename = 'docs'"
        )
        .await,
        Some("t".into())
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT relrowsecurity::text FROM pg_class WHERE relname = 'docs'"
        )
        .await,
        Some("t".into())
    );
}

#[tokio::test]
async fn duplicate_and_missing_policpolicies_are_typed() {
    let db = db();
    setup(&db).await;
    let mut s = session(&db, "guardian");
    ok(&mut s, "CREATE POLICY p1 ON docs USING (true)").await;
    assert_eq!(
        err_code(&mut s, "CREATE POLICY p1 ON docs USING (false)").await,
        "42710"
    );
    assert_eq!(err_code(&mut s, "DROP POLICY nope ON docs").await, "42704");
    ok(&mut s, "DROP POLICY IF EXISTS nope ON docs").await;
    assert_eq!(
        err_code(&mut s, "CREATE POLICY p2 ON missing_table USING (true)").await,
        "42P01"
    );
}

#[tokio::test]
async fn drop_policy_returns_to_default_deny() {
    let db = db();
    setup(&db).await;
    let mut owner = session(&db, "guardian");
    ok(
        &mut owner,
        "CREATE POLICY p_read ON docs FOR SELECT USING (true)",
    )
    .await;
    let mut bob = session(&db, "bob");
    assert_eq!(count(&mut bob, "SELECT count(*) FROM docs").await, 3);
    ok(&mut owner, "DROP POLICY p_read ON docs").await;
    assert_eq!(count(&mut bob, "SELECT count(*) FROM docs").await, 0);
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policies_persist_across_sessions_and_databases() {
    let storage = Arc::new(MemoryStorage::new());
    let database = Arc::new(Database::new(storage.clone(), "app"));
    {
        let mut s = Session::new(database.clone(), "guardian");
        ok(
            &mut s,
            "CREATE TABLE docs (id int PRIMARY KEY, owner text, val int)",
        )
        .await;
        ok(
            &mut s,
            "INSERT INTO docs VALUES (1, 'alice', 1), (2, 'bob', 2)",
        )
        .await;
        ok(&mut s, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY").await;
        ok(
            &mut s,
            "CREATE POLICY p_own ON docs FOR SELECT USING (owner = current_user)",
        )
        .await;
    }
    // A brand-new Database over the SAME storage re-reads the catalog: the
    // flag and policy survive (this is the document that replicates).
    let database2 = Arc::new(Database::new(storage, "app"));
    let mut alice = Session::new(database2.clone(), "alice");
    assert_eq!(count(&mut alice, "SELECT count(*) FROM docs").await, 1);
    let mut owner = Session::new(database2, "guardian");
    assert_eq!(count(&mut owner, "SELECT count(*) FROM docs").await, 2);
    assert_eq!(
        scalar(&mut owner, "SELECT policyname FROM pg_policies").await,
        Some("p_own".into())
    );
}
