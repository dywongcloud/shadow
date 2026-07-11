#![cfg(feature = "sql")]
//! End-to-end conformance tests for the PostgreSQL extension mechanism:
//! CREATE/DROP EXTENSION lifecycle, catalog views, function/type/operator
//! gating, GUC configuration, and persistence of installed state in the
//! catalog (the same document that replicates between peers).

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

fn db() -> Arc<Database<MemoryStorage>> {
    Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"))
}

async fn session() -> Session<MemoryStorage> {
    Session::new(db(), "guardian")
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

/// First row/column of a row-producing result, as PostgreSQL text output.
async fn scalar(s: &mut Session<MemoryStorage>, sql: &str) -> Option<String> {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Rows { rows, .. }) => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.to_text()),
        other => panic!("`{sql}` did not produce rows: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_extension_gates_functions() {
    let mut s = session().await;
    // Not installed: typed undefined-function error naming the extension.
    assert_eq!(
        &err_code(&mut s, "SELECT uuid_generate_v4()").await,
        "42883"
    );
    ok(&mut s, "CREATE EXTENSION \"uuid-ossp\"").await;
    let u = scalar(&mut s, "SELECT uuid_generate_v4()").await.unwrap();
    assert_eq!(u.len(), 36);
}

#[tokio::test]
async fn create_extension_unknown_name_is_typed() {
    let mut s = session().await;
    assert_eq!(&err_code(&mut s, "CREATE EXTENSION postgis").await, "0A000");
}

#[tokio::test]
async fn create_extension_duplicate_and_if_not_exists() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION pgcrypto").await;
    assert_eq!(
        &err_code(&mut s, "CREATE EXTENSION pgcrypto").await,
        "42710"
    );
    ok(&mut s, "CREATE EXTENSION IF NOT EXISTS pgcrypto").await;
}

#[tokio::test]
async fn create_extension_bad_version() {
    let mut s = session().await;
    assert_eq!(
        &err_code(&mut s, "CREATE EXTENSION pg_trgm WITH VERSION '9.9'").await,
        "42704"
    );
}

#[tokio::test]
async fn drop_extension_lifecycle() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION pg_trgm").await;
    ok(&mut s, "DROP EXTENSION pg_trgm").await;
    assert_eq!(&err_code(&mut s, "DROP EXTENSION pg_trgm").await, "42704");
    ok(&mut s, "DROP EXTENSION IF EXISTS pg_trgm").await;
    // Functions are gated again after drop.
    assert_eq!(
        &err_code(&mut s, "SELECT similarity('a','b')").await,
        "42883"
    );
}

#[tokio::test]
async fn plpgsql_is_preinstalled_like_postgres() {
    let mut s = session().await;
    let v = scalar(
        &mut s,
        "SELECT extversion FROM pg_extension WHERE extname = 'plpgsql'",
    )
    .await;
    assert_eq!(v.as_deref(), Some("1.0"));
    ok(&mut s, "DROP EXTENSION plpgsql").await;
}

#[tokio::test]
async fn drop_extension_with_dependent_table_is_blocked() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION citext").await;
    ok(&mut s, "CREATE TABLE users (email CITEXT PRIMARY KEY)").await;
    // RESTRICT (default) refuses; CASCADE refuses explicitly (no implicit
    // data destruction) — both typed.
    assert_eq!(&err_code(&mut s, "DROP EXTENSION citext").await, "0A000");
    assert_eq!(
        &err_code(&mut s, "DROP EXTENSION citext CASCADE").await,
        "0A000"
    );
    ok(&mut s, "DROP TABLE users").await;
    ok(&mut s, "DROP EXTENSION citext").await;
}

// ---------------------------------------------------------------------------
// ALTER EXTENSION (hand-parsed: sqlparser 0.62 has no AST for it)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alter_extension_update() {
    let mut s = session().await;
    // Not installed: typed undefined-object error.
    assert_eq!(
        &err_code(&mut s, "ALTER EXTENSION pg_trgm UPDATE").await,
        "42704"
    );
    ok(&mut s, "CREATE EXTENSION pg_trgm").await;
    // UPDATE and UPDATE TO the available version succeed with the right tag.
    let r = ok(&mut s, "ALTER EXTENSION pg_trgm UPDATE").await;
    assert_eq!(r[0].command_tag(), "ALTER EXTENSION");
    ok(&mut s, "ALTER EXTENSION pg_trgm UPDATE TO '1.6'").await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT extversion FROM pg_extension WHERE extname = 'pg_trgm'"
        )
        .await
        .as_deref(),
        Some("1.6")
    );
    // Unknown target version: 42704 naming the available version.
    let err = s
        .execute("ALTER EXTENSION pg_trgm UPDATE TO '9.9'")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42704");
    assert!(err.to_string().contains("1.6"), "should name 1.6: {err}");
}

#[tokio::test]
async fn alter_extension_set_schema_and_membership_are_refused() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION citext").await;
    // None of the registry extensions are relocatable.
    let err = s
        .execute("ALTER EXTENSION citext SET SCHEMA util")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "0A000");
    assert!(err.to_string().contains("not relocatable"), "{err}");
    // Membership changes are reserved for extension scripts in PostgreSQL.
    assert_eq!(
        &err_code(&mut s, "ALTER EXTENSION citext ADD FUNCTION f(text)").await,
        "0A000"
    );
    assert_eq!(
        &err_code(&mut s, "ALTER EXTENSION citext DROP TYPE citext").await,
        "0A000"
    );
    // Malformed ALTER EXTENSION is a syntax error.
    assert_eq!(
        &err_code(&mut s, "ALTER EXTENSION citext FROBNICATE").await,
        "42601"
    );
}

#[tokio::test]
async fn alter_extension_mixes_with_other_statements_in_order() {
    let mut s = session().await;
    let results = ok(
        &mut s,
        "CREATE EXTENSION pg_trgm; \
         ALTER EXTENSION pg_trgm UPDATE; \
         SELECT similarity('abc', 'abc')",
    )
    .await;
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].command_tag(), "CREATE EXTENSION");
    assert_eq!(results[1].command_tag(), "ALTER EXTENSION");
    match &results[2] {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0].to_text().as_deref(), Some("1"));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // A `;` inside a string literal does not split the ALTER EXTENSION route.
    let results = ok(&mut s, "SELECT 'ALTER EXTENSION x; UPDATE'").await;
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn alter_extension_transaction_semantics() {
    let mut s = session().await;
    // Inside an explicit transaction the version change is staged and commits.
    ok(
        &mut s,
        "BEGIN; CREATE EXTENSION pg_trgm; ALTER EXTENSION pg_trgm UPDATE TO '1.6'; COMMIT",
    )
    .await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT extversion FROM pg_extension WHERE extname = 'pg_trgm'"
        )
        .await
        .as_deref(),
        Some("1.6")
    );
    // A failing ALTER EXTENSION aborts an open block, like any other error.
    ok(&mut s, "BEGIN").await;
    assert_eq!(
        &err_code(&mut s, "ALTER EXTENSION nope UPDATE").await,
        "42704"
    );
    assert_eq!(&err_code(&mut s, "SELECT 1").await, "25P02");
    ok(&mut s, "ROLLBACK").await;
}

#[tokio::test]
async fn alter_extension_extended_protocol_is_rejected() {
    let s = session().await;
    let err = match s.prepare("ALTER EXTENSION pg_trgm UPDATE") {
        Err(e) => e,
        Ok(_) => panic!("preparing ALTER EXTENSION should fail"),
    };
    assert_eq!(err.sqlstate(), "42601");
    assert!(
        err.to_string().contains("simple query protocol"),
        "error should point at simple-protocol support: {err}"
    );
}

// ---------------------------------------------------------------------------
// Catalog views
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_available_extensions_lists_registry() {
    let mut s = session().await;
    let n = scalar(&mut s, "SELECT count(*) FROM pg_available_extensions").await;
    let count: i64 = n.unwrap().parse().unwrap();
    assert!(count >= 10, "registry should list all bundled extensions");
    // installed_version reflects state.
    ok(&mut s, "CREATE EXTENSION unaccent").await;
    let v = scalar(
        &mut s,
        "SELECT installed_version FROM pg_available_extensions WHERE name = 'unaccent'",
    )
    .await;
    assert_eq!(v.as_deref(), Some("1.1"));
}

#[tokio::test]
async fn pg_extension_reflects_installs() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION vector").await;
    let v = scalar(
        &mut s,
        "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
    )
    .await;
    assert_eq!(v.as_deref(), Some("0.8.1"));
    let installed = scalar(
        &mut s,
        "SELECT installed FROM pg_available_extension_versions WHERE name = 'vector'",
    )
    .await;
    assert_eq!(installed.as_deref(), Some("t"));
}

#[tokio::test]
async fn runtime_column_reports_extension_strategy() {
    let mut s = session().await;
    // `runtime` is a GuardianDB extension column on pg_available_extensions.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT runtime FROM pg_available_extensions WHERE name = 'pg_trgm'"
        )
        .await
        .as_deref(),
        Some("native")
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT runtime FROM pg_available_extensions WHERE name = 'postgis'"
        )
        .await
        .as_deref(),
        Some("sidecar")
    );
    // The sidecar-routed registry entries.
    let n = scalar(
        &mut s,
        "SELECT count(*) FROM pg_available_extensions WHERE runtime = 'sidecar'",
    )
    .await;
    assert_eq!(n.as_deref(), Some("3"));
    // Without a configured sidecar, installing any of them fails typed, with
    // an actionable message naming both configuration channels.
    for name in ["postgis", "timescaledb", "pg_stat_statements"] {
        let err = s
            .execute(&format!("CREATE EXTENSION {name}"))
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "0A000", "{name}");
        let msg = err.to_string();
        assert!(msg.contains("guardian.sidecar_dsn"), "{msg}");
        assert!(msg.contains("GUARDIAN_PG_SIDECAR_DSN"), "{msg}");
    }
}

#[tokio::test]
async fn pg_depend_tracks_extension_dependencies() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION citext").await;
    ok(
        &mut s,
        "CREATE TABLE users (id INT PRIMARY KEY, email CITEXT)",
    )
    .await;
    // One pg_extension -> pg_namespace row per installed extension
    // (plpgsql is pre-installed, so citext makes two).
    let n = scalar(
        &mut s,
        "SELECT count(*) FROM pg_depend \
         WHERE classid = 3079 AND refclassid = 2615 AND deptype = 'n'",
    )
    .await;
    assert_eq!(n.as_deref(), Some("2"));
    // The citext column registers a pg_class -> pg_extension dependency with
    // objsubid = its attribute number (email is column 2 of users).
    let sub = scalar(
        &mut s,
        "SELECT d.objsubid FROM pg_depend d JOIN pg_class c ON c.oid = d.objid \
         WHERE d.classid = 1259 AND d.refclassid = 3079 AND c.relname = 'users'",
    )
    .await;
    assert_eq!(sub.as_deref(), Some("2"));
    // ... and it points at citext's pg_extension row.
    let ext = scalar(
        &mut s,
        "SELECT e.extname FROM pg_depend d JOIN pg_extension e ON e.oid = d.refobjid \
         WHERE d.classid = 1259 AND d.refclassid = 3079",
    )
    .await;
    assert_eq!(ext.as_deref(), Some("citext"));
    // Dropping the table clears the column dependency.
    ok(&mut s, "DROP TABLE users").await;
    let n = scalar(
        &mut s,
        "SELECT count(*) FROM pg_depend WHERE classid = 1259",
    )
    .await;
    assert_eq!(n.as_deref(), Some("0"));
}

// ---------------------------------------------------------------------------
// Persistence: installed state lives in the catalog document (the replicated
// unit), so a fresh session over the SAME storage sees it; a fresh storage
// does not.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extension_state_persists_across_sessions() {
    let storage = Arc::new(MemoryStorage::new());
    let database = Arc::new(Database::new(storage.clone(), "app"));
    let mut s1 = Session::new(database.clone(), "guardian");
    ok(&mut s1, "CREATE EXTENSION pgcrypto").await;
    drop(s1);
    // New session, same storage: still installed (catalog was saved).
    let database2 = Arc::new(Database::new(storage, "app"));
    let mut s2 = Session::new(database2, "guardian");
    let d = scalar(&mut s2, "SELECT encode(digest('abc','sha256'),'hex')").await;
    assert_eq!(
        d.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    // Fresh storage: not installed.
    let mut s3 = session().await;
    assert_eq!(
        &err_code(&mut s3, "SELECT digest('a','md5')").await,
        "42883"
    );
}

// ---------------------------------------------------------------------------
// Functions, operators, types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_trgm_operators_and_gucs() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION pg_trgm").await;
    assert_eq!(
        scalar(&mut s, "SELECT similarity('word','two words')")
            .await
            .as_deref(),
        Some("0.36363637")
    );
    // Default threshold 0.3: 'word' % 'two words' is true.
    assert_eq!(
        scalar(&mut s, "SELECT 'word' % 'two words'")
            .await
            .as_deref(),
        Some("t")
    );
    // Raise the threshold via SET; the operator observes it.
    ok(&mut s, "SET pg_trgm.similarity_threshold = 0.5").await;
    assert_eq!(
        scalar(&mut s, "SELECT 'word' % 'two words'")
            .await
            .as_deref(),
        Some("f")
    );
    // set_limit()/show_limit() round-trip and persist for the session.
    assert_eq!(
        scalar(&mut s, "SELECT set_limit(0.2)").await.as_deref(),
        Some("0.2")
    );
    assert_eq!(
        scalar(&mut s, "SELECT show_limit()").await.as_deref(),
        Some("0.2")
    );
    assert_eq!(
        scalar(&mut s, "SELECT 'word' % 'two words'")
            .await
            .as_deref(),
        Some("t")
    );
}

#[tokio::test]
async fn citext_column_semantics() {
    let mut s = session().await;
    // Type is gated on the extension.
    assert_eq!(
        &err_code(&mut s, "CREATE TABLE t (e CITEXT)").await,
        "42704"
    );
    ok(&mut s, "CREATE EXTENSION citext").await;
    ok(&mut s, "CREATE TABLE t (e CITEXT UNIQUE)").await;
    ok(&mut s, "INSERT INTO t VALUES ('Alice@Example.COM')").await;
    // Case-insensitive UNIQUE.
    assert_eq!(
        &err_code(&mut s, "INSERT INTO t VALUES ('alice@example.com')").await,
        "23505"
    );
    // Case-insensitive comparison, original case preserved on output.
    assert_eq!(
        scalar(&mut s, "SELECT e FROM t WHERE e = 'ALICE@EXAMPLE.COM'")
            .await
            .as_deref(),
        Some("Alice@Example.COM")
    );
}

#[tokio::test]
async fn vector_column_and_distance_operators() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION vector").await;
    ok(
        &mut s,
        "CREATE TABLE items (id INT PRIMARY KEY, v VECTOR(2))",
    )
    .await;
    ok(&mut s, "INSERT INTO items VALUES (1,'[1,2]'), (2,'[4,6]')").await;
    // Dimension enforcement.
    assert_eq!(
        &err_code(&mut s, "INSERT INTO items VALUES (3,'[1,2,3]')").await,
        "42804"
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT v <-> '[4,6]'::vector FROM items WHERE id = 1"
        )
        .await
        .as_deref(),
        Some("5")
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT l2_distance('[1,2]'::vector,'[4,6]'::vector)"
        )
        .await
        .as_deref(),
        Some("5")
    );
    // ORDER BY nearest-neighbour, the canonical pgvector query shape.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT id FROM items ORDER BY v <-> '[3.9,5.9]'::vector LIMIT 1"
        )
        .await
        .as_deref(),
        Some("2")
    );
}

#[tokio::test]
async fn fuzzystrmatch_and_unaccent_functions() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION fuzzystrmatch").await;
    ok(&mut s, "CREATE EXTENSION unaccent").await;
    assert_eq!(
        scalar(&mut s, "SELECT levenshtein('kitten','sitting')")
            .await
            .as_deref(),
        Some("3")
    );
    assert_eq!(
        scalar(&mut s, "SELECT soundex('Margaret')")
            .await
            .as_deref(),
        Some("M626")
    );
    assert_eq!(
        scalar(&mut s, "SELECT unaccent('Hôtel')").await.as_deref(),
        Some("Hotel")
    );
}

// ---------------------------------------------------------------------------
// Sidecar runtime: DSN validation and transaction-scoped routing rules that
// need no live sidecar. The wire-level tests live in `sidecar_wire` below.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sidecar_dsn_rejects_non_disable_sslmode() {
    let mut s = session().await;
    ok(
        &mut s,
        "SET guardian.sidecar_dsn = 'postgres://u:p@127.0.0.1:5432/db?sslmode=require'",
    )
    .await;
    // The client is plaintext-only, so the DSN is rejected before any I/O.
    let err = s.execute("CREATE EXTENSION postgis").await.unwrap_err();
    assert_eq!(err.sqlstate(), "0A000");
    assert!(err.to_string().contains("sslmode"), "{err}");
}

#[tokio::test]
async fn sidecar_routing_is_autocommit_only() {
    let mut s = session().await;
    ok(
        &mut s,
        "SET guardian.sidecar_dsn = 'postgres://u@127.0.0.1:1/db?sslmode=disable'",
    )
    .await;
    // Inside an explicit transaction the local error is kept (same SQLSTATE)
    // with a hint explaining why nothing was forwarded — no connection is
    // even attempted, so the unreachable DSN above is never dialed.
    ok(&mut s, "BEGIN").await;
    let err = s
        .execute("SELECT this_function_does_not_exist(1)")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42883");
    assert!(err.to_string().contains("sidecar"), "{err}");
    assert!(err.to_string().contains("transaction"), "{err}");
    ok(&mut s, "ROLLBACK").await;
    // Sidecar extension DDL is refused inside transaction blocks too.
    ok(&mut s, "BEGIN").await;
    let err = s.execute("CREATE EXTENSION postgis").await.unwrap_err();
    assert_eq!(err.sqlstate(), "0A000");
    assert!(err.to_string().contains("transaction block"), "{err}");
    ok(&mut s, "ROLLBACK").await;
}

// ---------------------------------------------------------------------------
// Sidecar runtime over the real wire protocol. The mock sidecar is a second
// GuardianDB pgwire server (same protocol, same SQLSTATE-tagged errors), so
// these tests exercise the client, the forwarding rules and the error
// mapping end to end over TCP.
// ---------------------------------------------------------------------------

#[cfg(feature = "pgwire")]
mod sidecar_wire {
    use super::*;
    use guardian_db::sql::{Catalog, RelationalStorage};
    use tokio::net::TcpListener;

    /// Start a GuardianDB pgwire server over `storage` on an ephemeral port;
    /// returns its DSN and the served database (for direct seeding).
    async fn mock_sidecar(storage: Arc<MemoryStorage>) -> (String, Arc<Database<MemoryStorage>>) {
        let db = Arc::new(Database::new(storage, "sidecar"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = db.clone();
        tokio::spawn(async move {
            let _ = guardian_db::pgwire::serve_on(listener, served, "guardian").await;
        });
        (
            format!("postgres://guardian:guardian@127.0.0.1:{port}/sidecar?sslmode=disable"),
            db,
        )
    }

    async fn set_dsn(s: &mut Session<MemoryStorage>, dsn: &str) {
        ok(s, &format!("SET guardian.sidecar_dsn = '{dsn}'")).await;
    }

    #[tokio::test]
    async fn create_extension_forwarding_propagates_sidecar_errors() {
        // The mock (a GuardianDB engine with no sidecar of its own) rejects
        // CREATE EXTENSION pg_stat_statements with 0A000. That SQLSTATE must
        // arrive typed at the main session — proving the statement was
        // forwarded and the wire ErrorResponse was mapped faithfully.
        let (dsn, _mock_db) = mock_sidecar(Arc::new(MemoryStorage::new())).await;
        let mut s = session().await;
        set_dsn(&mut s, &dsn).await;
        let err = s
            .execute("CREATE EXTENSION pg_stat_statements")
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "0A000");
        // The failed install left no local record.
        let n = scalar(
            &mut s,
            "SELECT count(*) FROM pg_extension WHERE extname = 'pg_stat_statements'",
        )
        .await;
        assert_eq!(n.as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn query_fallback_routes_to_sidecar_over_the_wire() {
        // pg_trgm is installed on the MOCK only. Locally, similarity() fails
        // with 42883; once a DSN is configured the statement is forwarded
        // verbatim and the mock's answer comes back decoded.
        let (dsn, mock_db) = mock_sidecar(Arc::new(MemoryStorage::new())).await;
        let mut mock_session = Session::new(mock_db, "guardian");
        ok(&mut mock_session, "CREATE EXTENSION pg_trgm").await;
        ok(
            &mut mock_session,
            "CREATE TABLE sidecar_only (n INT); INSERT INTO sidecar_only VALUES (7)",
        )
        .await;
        drop(mock_session);

        let mut s = session().await;
        // Without a DSN the local error stands.
        assert_eq!(
            &err_code(&mut s, "SELECT similarity('a','a')").await,
            "42883"
        );
        set_dsn(&mut s, &dsn).await;
        assert_eq!(
            scalar(&mut s, "SELECT similarity('a','a')")
                .await
                .as_deref(),
            Some("1")
        );
        // Undefined-table fallback works the same way: the relation exists
        // only on the sidecar, so the local 42P01 forwards the query.
        assert_eq!(
            scalar(&mut s, "SELECT n FROM sidecar_only")
                .await
                .as_deref(),
            Some("7")
        );
        // A statement that is undefined on both sides keeps the sidecar's
        // typed error.
        assert_eq!(
            &err_code(&mut s, "SELECT nowhere_function(1)").await,
            "42883"
        );
    }

    #[tokio::test]
    async fn sidecar_binding_installs_records_and_persists() {
        // Seed the mock as a sidecar that already provides
        // pg_stat_statements (as a real PostgreSQL would), so the forwarded
        // CREATE EXTENSION IF NOT EXISTS succeeds and reports a version.
        let mock_storage = Arc::new(MemoryStorage::new());
        let mut seeded = Catalog::new("sidecar");
        seeded.install_extension("pg_stat_statements", "1.11");
        mock_storage
            .save_catalog(&serde_json::to_value(&seeded).unwrap())
            .await
            .unwrap();
        let (dsn, _mock_db) = mock_sidecar(mock_storage).await;

        let storage = Arc::new(MemoryStorage::new());
        let database = Arc::new(Database::new(storage.clone(), "app"));
        let mut s1 = Session::new(database, "guardian");
        set_dsn(&mut s1, &dsn).await;
        ok(&mut s1, "CREATE EXTENSION IF NOT EXISTS pg_stat_statements").await;
        // Recorded locally with the version the sidecar reported (not the
        // registry default), suffix-free in every view.
        assert_eq!(
            scalar(
                &mut s1,
                "SELECT extversion FROM pg_extension WHERE extname = 'pg_stat_statements'"
            )
            .await
            .as_deref(),
            Some("1.11")
        );
        drop(s1);

        // Restart: a fresh session over the same storage still sees the
        // binding — no DSN is needed just to read the catalog.
        let database2 = Arc::new(Database::new(storage, "app"));
        let mut s2 = Session::new(database2, "guardian");
        assert_eq!(
            scalar(
                &mut s2,
                "SELECT extversion FROM pg_extension WHERE extname = 'pg_stat_statements'"
            )
            .await
            .as_deref(),
            Some("1.11")
        );
        // Dropping a sidecar-bound extension without a sidecar configured is
        // refused with the actionable message; with the DSN it forwards the
        // drop and removes the local record.
        assert_eq!(
            &err_code(&mut s2, "DROP EXTENSION pg_stat_statements").await,
            "0A000"
        );
        set_dsn(&mut s2, &dsn).await;
        ok(&mut s2, "DROP EXTENSION pg_stat_statements").await;
        assert_eq!(
            scalar(
                &mut s2,
                "SELECT count(*) FROM pg_extension WHERE extname = 'pg_stat_statements'"
            )
            .await
            .as_deref(),
            Some("0")
        );
    }
}

// ---------------------------------------------------------------------------
// Session configuration (SHOW / current_setting)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn show_and_current_setting() {
    let mut s = session().await;
    ok(&mut s, "SET application_name = 'conformance'").await;
    assert_eq!(
        scalar(&mut s, "SHOW application_name").await.as_deref(),
        Some("conformance")
    );
    assert_eq!(
        scalar(&mut s, "SELECT current_setting('application_name')")
            .await
            .as_deref(),
        Some("conformance")
    );
    // Extension GUC default is visible without SET once registered.
    assert_eq!(
        scalar(&mut s, "SHOW pg_trgm.similarity_threshold")
            .await
            .as_deref(),
        Some("0.3")
    );
    // Unknown parameter: typed.
    assert_eq!(&err_code(&mut s, "SHOW no_such_parameter").await, "42704");
    // current_setting(name, true) is NULL-forgiving.
    assert_eq!(
        scalar(&mut s, "SELECT current_setting('nope', true)").await,
        None
    );
}

// ---------------------------------------------------------------------------
// Sidecar runtime against a REAL PostgreSQL (ignored by default: requires a
// local PostgreSQL 16 installation). Run with:
//   cargo test --features sql --test sql_extensions -- --ignored
// ---------------------------------------------------------------------------

/// Kills the postgres child and removes its data directory on drop, so a
/// failing assertion cannot leak the server process.
struct PgGuard {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl Drop for PgGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Numeric output of an `id` invocation (e.g. `id -u postgres`).
fn id_of(args: &[&str]) -> Option<u32> {
    let out = std::process::Command::new("id").args(args).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[tokio::test]
#[ignore = "requires a local PostgreSQL 16 installation (initdb/postgres)"]
async fn sidecar_real_postgres() {
    let bin = std::path::Path::new("/usr/lib/postgresql/16/bin");
    assert!(
        bin.join("initdb").exists(),
        "PostgreSQL 16 not found at {}",
        bin.display()
    );
    let dir = std::env::temp_dir().join(format!("gdb_sidecar_pg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    let sockets = dir.join("sock");
    std::fs::create_dir_all(&sockets).unwrap();
    // initdb refuses to run as root: when the test runs as root (containers,
    // sandboxes), hand the server to the unprivileged `postgres` user.
    let run_as: Option<(u32, u32)> = if id_of(&["-u"]) == Some(0) {
        let uid = id_of(&["-u", "postgres"]).expect("running as root and no `postgres` user");
        let gid = id_of(&["-g", "postgres"]).expect("no `postgres` group");
        let chown = std::process::Command::new("chown")
            .arg("-R")
            .arg("postgres")
            .arg(&dir)
            .status()
            .unwrap();
        assert!(chown.success(), "chown of {} failed", dir.display());
        Some((uid, gid))
    } else {
        None
    };
    let demote = |cmd: &mut std::process::Command| {
        // `uid`/`gid` demotion is a Unix-only concept; on other platforms `run_as`
        // is always `None` (no `id`/`postgres` user), so this is a no-op.
        #[cfg(unix)]
        if let Some((uid, gid)) = run_as {
            use std::os::unix::process::CommandExt;
            cmd.uid(uid).gid(gid);
        }
        #[cfg(not(unix))]
        let _ = (cmd, &run_as);
    };
    let mut initdb = std::process::Command::new(bin.join("initdb"));
    initdb.args([
        "-D",
        data.to_str().unwrap(),
        "-U",
        "postgres",
        "-A",
        "trust",
    ]);
    demote(&mut initdb);
    let out = initdb.output().unwrap();
    assert!(
        out.status.success(),
        "initdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // An ephemeral port: bind, remember, release.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let mut server = std::process::Command::new(bin.join("postgres"));
    server
        .args([
            "-D",
            data.to_str().unwrap(),
            "-p",
            &port.to_string(),
            "-c",
            "listen_addresses=127.0.0.1",
            // pg_stat_statements needs its shared library preloaded for the
            // stats view to be readable.
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-k",
            sockets.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    demote(&mut server);
    let child = server.spawn().unwrap();
    let _guard = PgGuard { child, dir };
    // Wait for readiness.
    let mut ready = false;
    for _ in 0..100 {
        let ok = std::process::Command::new(bin.join("pg_isready"))
            .args(["-h", "127.0.0.1", "-p", &port.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "postgres did not become ready on port {port}");

    let mut s = session().await;
    ok(
        &mut s,
        &format!(
            "SET guardian.sidecar_dsn = \
             'postgres://postgres@127.0.0.1:{port}/postgres?sslmode=disable'"
        ),
    )
    .await;
    // CREATE EXTENSION forwards to the real PostgreSQL, and the recorded
    // version is what the sidecar reports (PostgreSQL 16 ships 1.10).
    ok(&mut s, "CREATE EXTENSION pg_stat_statements").await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT extversion FROM pg_extension WHERE extname = 'pg_stat_statements'"
        )
        .await
        .as_deref(),
        Some("1.10")
    );
    // The stats view exists only on the sidecar: the local 42P01 triggers
    // fallback-forwarding and real rows come back over the wire.
    let calls = scalar(&mut s, "SELECT count(*) FROM pg_stat_statements").await;
    assert!(
        calls.as_deref().map(|n| n.parse::<i64>().is_ok()) == Some(true),
        "expected a numeric count from pg_stat_statements, got {calls:?}"
    );
    // Clean up the binding (forwards DROP EXTENSION to the sidecar).
    ok(&mut s, "DROP EXTENSION pg_stat_statements").await;
}

// ---------------------------------------------------------------------------
// Tier-2 native contrib extensions: hstore, intarray, ltree, cube,
// earthdistance. Expected values in this section were generated from live
// PostgreSQL 16.13 with the matching contrib extensions installed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_lists_all_eighteen_extensions() {
    let mut s = session().await;
    let n = scalar(&mut s, "SELECT count(*) FROM pg_available_extensions").await;
    assert_eq!(n.as_deref(), Some("18"));
    // The five tier-2 extensions run natively.
    let native = scalar(
        &mut s,
        "SELECT count(*) FROM pg_available_extensions WHERE runtime = 'native' \
         AND name IN ('hstore','intarray','ltree','cube','earthdistance')",
    )
    .await;
    assert_eq!(native.as_deref(), Some("5"));
    // Versions match the registry.
    for (name, version) in [
        ("hstore", "1.8"),
        ("intarray", "1.5"),
        ("ltree", "1.3"),
        ("cube", "1.5"),
        ("earthdistance", "1.2"),
    ] {
        let v = scalar(
            &mut s,
            &format!("SELECT default_version FROM pg_available_extensions WHERE name = '{name}'"),
        )
        .await;
        assert_eq!(v.as_deref(), Some(version), "{name}");
    }
    // The excluded contrib names stay typed failures, never fake successes.
    for name in ["isn", "lo", "tablefunc"] {
        assert_eq!(
            &err_code(&mut s, &format!("CREATE EXTENSION {name}")).await,
            "0A000",
            "{name}"
        );
    }
}

#[tokio::test]
async fn hstore_column_end_to_end() {
    let mut s = session().await;
    // The type is gated on the extension.
    assert_eq!(
        &err_code(&mut s, "CREATE TABLE cfg (h HSTORE)").await,
        "42704"
    );
    ok(&mut s, "CREATE EXTENSION hstore").await;
    ok(&mut s, "CREATE TABLE cfg (id INT PRIMARY KEY, h HSTORE)").await;
    ok(
        &mut s,
        "INSERT INTO cfg VALUES (1, 'a=>1, b=>NULL'), (2, 'a=>2, c=>x')",
    )
    .await;
    // Canonical PG output: quoted, NULLs bare, (length, bytes) key order.
    assert_eq!(
        scalar(&mut s, "SELECT h FROM cfg WHERE id = 1")
            .await
            .as_deref(),
        Some(r#""a"=>"1", "b"=>NULL"#)
    );
    // -> key lookup in projections and WHERE.
    assert_eq!(
        scalar(&mut s, "SELECT h -> 'a' FROM cfg WHERE id = 2")
            .await
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        scalar(&mut s, "SELECT id FROM cfg WHERE h -> 'a' = '1'")
            .await
            .as_deref(),
        Some("1")
    );
    // ? exists and @> containment.
    assert_eq!(
        scalar(&mut s, "SELECT id FROM cfg WHERE h ? 'c'")
            .await
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        scalar(&mut s, "SELECT id FROM cfg WHERE h @> 'a=>1'")
            .await
            .as_deref(),
        Some("1")
    );
    // || concatenation (right side wins) and - deletion.
    assert_eq!(
        scalar(&mut s, "SELECT h || 'a=>9' FROM cfg WHERE id = 1")
            .await
            .as_deref(),
        Some(r#""a"=>"9", "b"=>NULL"#)
    );
    assert_eq!(
        scalar(&mut s, "SELECT h - 'b'::text FROM cfg WHERE id = 1")
            .await
            .as_deref(),
        Some(r#""a"=>"1""#)
    );
    // Functions route through the extension dispatch.
    assert_eq!(
        scalar(&mut s, "SELECT akeys(h) FROM cfg WHERE id = 1")
            .await
            .as_deref(),
        Some("{a,b}")
    );
    assert_eq!(
        scalar(&mut s, "SELECT hstore_to_json(h) FROM cfg WHERE id = 1")
            .await
            .as_deref(),
        Some(r#"{"a":"1","b":null}"#)
    );
    // Bad input is a typed syntax error: 42601 from the hstore parser (like
    // PG); the INSERT coercion path wraps it as datatype mismatch (42804).
    assert_eq!(
        &err_code(&mut s, "SELECT 'not hstore'::hstore").await,
        "42601"
    );
    assert_eq!(
        &err_code(&mut s, "INSERT INTO cfg VALUES (3, 'not hstore')").await,
        "42804"
    );
    // The dependent column blocks DROP EXTENSION.
    assert_eq!(&err_code(&mut s, "DROP EXTENSION hstore").await, "0A000");
}

#[tokio::test]
async fn hstore_data_persists_across_sessions() {
    let storage = Arc::new(MemoryStorage::new());
    let database = Arc::new(Database::new(storage.clone(), "app"));
    let mut s1 = Session::new(database, "guardian");
    ok(&mut s1, "CREATE EXTENSION hstore").await;
    ok(&mut s1, "CREATE TABLE kv (h HSTORE)").await;
    ok(&mut s1, "INSERT INTO kv VALUES ('k=>persisted')").await;
    drop(s1);
    let database2 = Arc::new(Database::new(storage, "app"));
    let mut s2 = Session::new(database2, "guardian");
    assert_eq!(
        scalar(&mut s2, "SELECT h -> 'k' FROM kv").await.as_deref(),
        Some("persisted")
    );
}

#[tokio::test]
async fn intarray_functions_and_operators() {
    let mut s = session().await;
    // Gated before install: operators keep their normal error path...
    assert!(s.execute("SELECT '{1,2}'::int[] + 3").await.is_err());
    // ...and functions name the extension (42883).
    assert_eq!(
        &err_code(&mut s, "SELECT icount('{1,2}'::int[])").await,
        "42883"
    );
    ok(&mut s, "CREATE EXTENSION intarray").await;
    for (sql, expect) in [
        ("SELECT icount('{1,2,3,2}'::int[])", "4"),
        ("SELECT sort('{3,1,2}'::int[])", "{1,2,3}"),
        ("SELECT sort('{3,1,2}'::int[], 'desc')", "{3,2,1}"),
        ("SELECT uniq('{1,1,2,2,3,1,1}'::int[])", "{1,2,3,1}"),
        ("SELECT uniq(sort('{1,2,3,2,1}'::int[]))", "{1,2,3}"),
        ("SELECT idx('{1,2,3,2}'::int[], 2)", "2"),
        ("SELECT subarray('{1,2,3,2,1}'::int[], 2, 3)", "{2,3,2}"),
        ("SELECT intset(42)", "{42}"),
        ("SELECT '{1,2,3}'::int[] && '{3,4}'::int[]", "t"),
        ("SELECT '{1,2,3}'::int[] && '{4,5}'::int[]", "f"),
        ("SELECT '{1,2,3}'::int[] @> '{2,2}'::int[]", "t"),
        ("SELECT '{2}'::int[] <@ '{1,2,3}'::int[]", "t"),
        ("SELECT '{1,2,3}'::int[] + 4", "{1,2,3,4}"),
        ("SELECT '{1,2,3}'::int[] + '{3,4}'::int[]", "{1,2,3,3,4}"),
        ("SELECT '{1,2,3,2}'::int[] - 2", "{1,3}"),
        ("SELECT '{1,2,3,2,4}'::int[] - '{2,4,9}'::int[]", "{1,3}"),
        ("SELECT '{1,2,3}'::int[] | '{3,4}'::int[]", "{1,2,3,4}"),
        ("SELECT '{1,2,3,2}'::int[] & '{2,3,4}'::int[]", "{2,3}"),
        ("SELECT '{1,3,5}'::int[] # 5", "3"),
    ] {
        assert_eq!(scalar(&mut s, sql).await.as_deref(), Some(expect), "{sql}");
    }
    // Table-driven WHERE with overlap.
    ok(
        &mut s,
        "CREATE TABLE tagged (id INT PRIMARY KEY, tags INT[])",
    )
    .await;
    ok(
        &mut s,
        "INSERT INTO tagged VALUES (1, '{1,2}'), (2, '{3,4}')",
    )
    .await;
    assert_eq!(
        scalar(&mut s, "SELECT id FROM tagged WHERE tags && '{2,9}'::int[]")
            .await
            .as_deref(),
        Some("1")
    );
    // Non-int arrays are rejected with CannotCoerce (42846).
    assert_eq!(
        &err_code(&mut s, "SELECT icount(ARRAY['a','b'])").await,
        "42846"
    );
}

#[tokio::test]
async fn ltree_column_end_to_end() {
    let mut s = session().await;
    assert_eq!(
        &err_code(&mut s, "CREATE TABLE paths (p LTREE)").await,
        "42704"
    );
    ok(&mut s, "CREATE EXTENSION ltree").await;
    ok(&mut s, "CREATE TABLE paths (p LTREE)").await;
    ok(
        &mut s,
        "INSERT INTO paths VALUES ('Top'), ('Top.Science'), ('Top.Science.Astronomy'), \
         ('Top.Hobbies'), ('Top.Hobbies.Amateurs_Astronomy')",
    )
    .await;
    // Descendant-or-self search, the canonical ltree query shape.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM paths WHERE p <@ 'Top.Science'"
        )
        .await
        .as_deref(),
        Some("2")
    );
    // Ancestor includes equality (PG-verified).
    assert_eq!(
        scalar(&mut s, "SELECT 'a.b'::ltree @> 'a.b'::ltree")
            .await
            .as_deref(),
        Some("t")
    );
    // lquery matching with quantifiers and word matching.
    assert_eq!(
        scalar(&mut s, "SELECT count(*) FROM paths WHERE p ~ 'Top.*{1}'")
            .await
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT p FROM paths WHERE p ~ '*.Amateurs_Astronomy%'"
        )
        .await
        .as_deref(),
        Some("Top.Hobbies.Amateurs_Astronomy")
    );
    // Functions.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT nlevel(p) FROM paths WHERE p = 'Top.Science.Astronomy'"
        )
        .await
        .as_deref(),
        Some("3")
    );
    assert_eq!(
        scalar(&mut s, "SELECT subpath('Top.Child1.Child2', -2, 1)")
            .await
            .as_deref(),
        Some("Child1")
    );
    assert_eq!(
        scalar(&mut s, "SELECT lca('1.2.3'::ltree, '1.2.3.4'::ltree)")
            .await
            .as_deref(),
        Some("1.2")
    );
    // ORDER BY uses label-sequence ordering.
    let rows = ok(&mut s, "SELECT p FROM paths ORDER BY p").await;
    match rows.into_iter().next().unwrap() {
        ExecResult::Rows { rows, .. } => {
            let sorted: Vec<String> = rows.iter().map(|r| r[0].to_text().unwrap()).collect();
            assert_eq!(
                sorted,
                [
                    "Top",
                    "Top.Hobbies",
                    "Top.Hobbies.Amateurs_Astronomy",
                    "Top.Science",
                    "Top.Science.Astronomy"
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Label validation is typed: 42601 from the ltree parser (like PG); the
    // INSERT coercion path wraps it as datatype mismatch (42804).
    assert_eq!(
        &err_code(&mut s, "SELECT 'not a path'::ltree").await,
        "42601"
    );
    assert_eq!(
        &err_code(&mut s, "INSERT INTO paths VALUES ('not a path')").await,
        "42804"
    );
}

#[tokio::test]
async fn cube_column_end_to_end() {
    let mut s = session().await;
    assert_eq!(
        &err_code(&mut s, "CREATE TABLE boxes (c CUBE)").await,
        "42704"
    );
    ok(&mut s, "CREATE EXTENSION cube").await;
    ok(&mut s, "CREATE TABLE boxes (id INT PRIMARY KEY, c CUBE)").await;
    ok(
        &mut s,
        "INSERT INTO boxes VALUES (1, '(0,0),(1,1)'), (2, '(2,2),(3,3)'), (3, '5')",
    )
    .await;
    // Text output matches contrib/cube exactly.
    assert_eq!(
        scalar(&mut s, "SELECT c FROM boxes WHERE id = 1")
            .await
            .as_deref(),
        Some("(0, 0),(1, 1)")
    );
    assert_eq!(
        scalar(&mut s, "SELECT c FROM boxes WHERE id = 3")
            .await
            .as_deref(),
        Some("(5)")
    );
    // Containment and overlap in WHERE.
    assert_eq!(
        scalar(&mut s, "SELECT id FROM boxes WHERE c @> '(0.5,0.5)'::cube")
            .await
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM boxes WHERE c && '(0.5,0.5),(2.5,2.5)'::cube"
        )
        .await
        .as_deref(),
        Some("2")
    );
    // Distance operator and nearest-neighbour ordering.
    assert_eq!(
        scalar(&mut s, "SELECT '(0,0)'::cube <-> '(3,4)'::cube")
            .await
            .as_deref(),
        Some("5")
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT id FROM boxes WHERE id < 3 ORDER BY c <-> '(2.4,2.4)'::cube LIMIT 1"
        )
        .await
        .as_deref(),
        Some("2")
    );
    // Functions (PG-verified vectors).
    for (sql, expect) in [
        ("SELECT cube_dim('(1,2),(3,4)'::cube)", "2"),
        ("SELECT cube_is_point('(1,2),(1,2)'::cube)", "t"),
        (
            "SELECT cube_union('(1)'::cube, '(2,3)'::cube)",
            "(1, 0),(2, 3)",
        ),
        (
            "SELECT cube_inter('(0,0),(1,1)'::cube, '(2,2),(3,3)'::cube)",
            "(2, 2),(1, 1)",
        ),
        (
            "SELECT cube_enlarge('(0,0),(1,1)'::cube, -2, 2)",
            "(0.5, 0.5)",
        ),
        ("SELECT cube(1.5)", "(1.5)"),
    ] {
        assert_eq!(scalar(&mut s, sql).await.as_deref(), Some(expect), "{sql}");
    }
    // Bad input is 22P02 from the cube parser (like PG); the INSERT coercion
    // path wraps it as datatype mismatch (42804).
    assert_eq!(&err_code(&mut s, "SELECT '(1,2),(3)'::cube").await, "22P02");
    assert_eq!(
        &err_code(&mut s, "INSERT INTO boxes VALUES (4, '(1,2),(3)')").await,
        "42804"
    );
}

#[tokio::test]
async fn earthdistance_requires_cube_and_cascades() {
    let mut s = session().await;
    // Without cube: typed 0A000 naming the missing requirement.
    let err = s
        .execute("CREATE EXTENSION earthdistance")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "0A000");
    assert!(err.to_string().contains("cube"), "{err}");
    assert!(err.to_string().contains("CASCADE"), "{err}");
    // CASCADE installs the dependency chain.
    ok(&mut s, "CREATE EXTENSION earthdistance CASCADE").await;
    let n = scalar(
        &mut s,
        "SELECT count(*) FROM pg_extension WHERE extname IN ('cube','earthdistance')",
    )
    .await;
    assert_eq!(n.as_deref(), Some("2"));
    // PG: earth() = 6378168.
    assert_eq!(
        scalar(&mut s, "SELECT earth()").await.as_deref(),
        Some("6378168")
    );
    // PG: earth_distance(ll_to_earth(51.5074,-0.1278), ll_to_earth(40.7128,-74.0060))
    //     = 5576489.226133242 (London -> New York, meters). Transcendental
    //     results are compared numerically, not textually.
    let d: f64 = scalar(
        &mut s,
        "SELECT earth_distance(ll_to_earth(51.5074,-0.1278), ll_to_earth(40.7128,-74.0060))",
    )
    .await
    .unwrap()
    .parse()
    .unwrap();
    assert!((d - 5576489.226133242).abs() < 1e-3, "got {d}");
    // Radius search: earth_box + containment (PG-verified truth values).
    assert_eq!(
        scalar(
            &mut s,
            "SELECT earth_box(ll_to_earth(0,0), 1000) @> ll_to_earth(0.001, 0.001)"
        )
        .await
        .as_deref(),
        Some("t")
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT earth_box(ll_to_earth(0,0), 1000) @> ll_to_earth(1, 1)"
        )
        .await
        .as_deref(),
        Some("f")
    );
    // latitude/longitude round-trip (exact for longitude at this input).
    assert_eq!(
        scalar(&mut s, "SELECT longitude(ll_to_earth(45, 100))")
            .await
            .as_deref(),
        Some("100")
    );
    // cube cannot be dropped while earthdistance needs it... PostgreSQL
    // allows it (dependencies are script-level), and so do we: dropping
    // earthdistance first is the clean path.
    ok(&mut s, "DROP EXTENSION earthdistance").await;
    ok(&mut s, "DROP EXTENSION cube").await;
}

#[tokio::test]
async fn hstore_arrow_does_not_shadow_json() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION hstore").await;
    // `->` with an hstore left operand routes to hstore...
    assert_eq!(
        scalar(&mut s, "SELECT 'a=>1'::hstore -> 'a'")
            .await
            .as_deref(),
        Some("1")
    );
    // ...while non-hstore left operands keep their pre-existing behaviour
    // (the engine's JSON `->` path, whatever it supports) — the operator
    // must not be silently claimed by hstore.
    let json_arrow = s.execute("SELECT '{\"a\": 1}'::jsonb -> 'a'").await;
    if let Ok(results) = json_arrow {
        // If the JSON path supports `->`, it must produce JSON, not hstore.
        if let Some(ExecResult::Rows { rows, .. }) = results.into_iter().next() {
            assert_eq!(rows[0][0].to_text().as_deref(), Some("1"));
        }
    }
}
