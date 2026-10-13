//! Fleet-replicated read mirror of project ownership, billing, teams, deployments and shadw-drive
//! metadata in guardian-db's in-process SQL layer (`guardian_db::sql`, never pgwire). Writes still
//! funnel through the existing single writers (control-plane leader, billing authority) and the
//! in-memory stores stay the source of truth, so a failure here never blocks the mutation it
//! mirrors. `teams`, `team_members` and `deployments` are view-only (admin SQL browser): nothing
//! reads them back.

use guardian_db::sql::{ExecResult, Session, SqlValue};
use serde::{Deserialize, Serialize};

use crate::guardian::SqlDb;

/// Idempotent schema bring-up, safe on every boot and the ONLY path that creates the catalog's
/// tables: waits for the first index walk, creates and verifies each table, then reconciles the
/// columns added after first release. Best-effort: a failure only leaves the fleet read fallback
/// unavailable until the next successful call, never a boot failure.
pub(crate) async fn init_schema() {
    let deadline = std::time::Instant::now() + index_build_timeout();
    let db = loop {
        match crate::guardian::sql_db().await {
            Ok(db) => break db,
            Err(e) if std::time::Instant::now() < deadline => {
                tracing::debug!(error = %e, "relational: GuardianDB not ready yet; schema bring-up retrying");
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "relational: schema bring-up SKIPPED -- GuardianDB never opened within its bound");
                return;
            }
        }
    };
    spawn_index_refresher(db.clone());
    let bound = index_build_timeout();
    if !wait_index_ready(bound).await {
        tracing::error!(
            bound_secs = bound.as_secs(),
            "relational: schema bring-up SKIPPED -- the index never built within its bound, \
             and CREATE TABLE against an unbuilt catalog would overwrite the real catalog"
        );
        return;
    }
    let index_keys = db.storage().index_len();
    let mut session = Session::new(db, "hive-init");
    for (table, ddl) in [
        (
            "project_teams",
            "CREATE TABLE IF NOT EXISTS project_teams (\
            project TEXT PRIMARY KEY, \
            team TEXT NOT NULL, \
            root_dir TEXT NOT NULL DEFAULT '', \
            updated_ms BIGINT NOT NULL, \
            deleted_ms BIGINT NOT NULL DEFAULT 0\
        )",
        ),
        (
            "billing_accounts",
            "CREATE TABLE IF NOT EXISTS billing_accounts (\
            tenant TEXT PRIMARY KEY, \
            plan TEXT NOT NULL, \
            status TEXT NOT NULL, \
            included_cents BIGINT NOT NULL, \
            used_cents BIGINT NOT NULL, \
            balance_cents BIGINT NOT NULL, \
            stripe_customer TEXT NOT NULL DEFAULT '', \
            stripe_subscription TEXT NOT NULL DEFAULT '', \
            period_start_ms BIGINT NOT NULL, \
            period_end_ms BIGINT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        (
            "billing_ledger",
            "CREATE TABLE IF NOT EXISTS billing_ledger (\
            id TEXT PRIMARY KEY, \
            tenant TEXT NOT NULL, \
            ts_ms BIGINT NOT NULL, \
            kind TEXT NOT NULL, \
            amount_cents BIGINT NOT NULL, \
            balance_after_cents BIGINT NOT NULL, \
            note TEXT NOT NULL DEFAULT ''\
        )",
        ),
        (
            "billing_invoices",
            "CREATE TABLE IF NOT EXISTS billing_invoices (\
            id TEXT PRIMARY KEY, \
            tenant TEXT NOT NULL, \
            number TEXT NOT NULL, \
            plan TEXT NOT NULL, \
            period_start_ms BIGINT NOT NULL, \
            period_end_ms BIGINT NOT NULL, \
            subtotal_cents BIGINT NOT NULL, \
            total_cents BIGINT NOT NULL, \
            status TEXT NOT NULL, \
            created_ms BIGINT NOT NULL\
        )",
        ),
        (
            "billing_invoice_lines",
            "CREATE TABLE IF NOT EXISTS billing_invoice_lines (\
            id TEXT PRIMARY KEY, \
            invoice_id TEXT NOT NULL, \
            description TEXT NOT NULL, \
            amount_cents BIGINT NOT NULL\
        )",
        ),
        (
            "billing_checkouts",
            "CREATE TABLE IF NOT EXISTS billing_checkouts (\
            id TEXT PRIMARY KEY, \
            tenant TEXT NOT NULL, \
            kind TEXT NOT NULL, \
            plan TEXT NOT NULL, \
            amount_cents BIGINT NOT NULL, \
            sku TEXT NOT NULL DEFAULT '', \
            target TEXT NOT NULL DEFAULT '', \
            stripe_session_id TEXT NOT NULL DEFAULT '', \
            created_ms BIGINT NOT NULL\
        )",
        ),
        (
            "billing_ledger_snapshot",
            "CREATE TABLE IF NOT EXISTS billing_ledger_snapshot (\
            tenant TEXT PRIMARY KEY, \
            ledger_json TEXT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        (
            "billing_invoices_snapshot",
            "CREATE TABLE IF NOT EXISTS billing_invoices_snapshot (\
            tenant TEXT PRIMARY KEY, \
            invoices_json TEXT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        (
            "teams",
            "CREATE TABLE IF NOT EXISTS teams (\
            slug TEXT PRIMARY KEY, \
            name TEXT NOT NULL, \
            plan TEXT NOT NULL, \
            member_count BIGINT NOT NULL, \
            sso_enabled BIGINT NOT NULL, \
            created_ms BIGINT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        (
            "team_members",
            "CREATE TABLE IF NOT EXISTS team_members (\
            id TEXT PRIMARY KEY, \
            team TEXT NOT NULL, \
            email TEXT NOT NULL, \
            name TEXT NOT NULL DEFAULT '', \
            role TEXT NOT NULL, \
            added_ms BIGINT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        (
            "deployments",
            "CREATE TABLE IF NOT EXISTS deployments (\
            id TEXT PRIMARY KEY, \
            project TEXT NOT NULL, \
            team TEXT NOT NULL, \
            node TEXT NOT NULL, \
            alias TEXT NOT NULL DEFAULT '', \
            kind TEXT NOT NULL DEFAULT '', \
            target TEXT NOT NULL DEFAULT '', \
            state TEXT NOT NULL DEFAULT '', \
            production BIGINT NOT NULL, \
            created_at_ms BIGINT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        (
            "drive_nodes",
            "CREATE TABLE IF NOT EXISTS drive_nodes (\
            id TEXT PRIMARY KEY, \
            project TEXT NOT NULL, \
            parent_id TEXT NOT NULL, \
            name TEXT NOT NULL, \
            kind TEXT NOT NULL, \
            size_bytes BIGINT NOT NULL DEFAULT 0, \
            mime TEXT NOT NULL DEFAULT '', \
            content_hash TEXT NOT NULL DEFAULT '', \
            owner_user TEXT NOT NULL DEFAULT '', \
            owner_node TEXT NOT NULL DEFAULT '', \
            mtime BIGINT NOT NULL, \
            ctime BIGINT NOT NULL, \
            deleted_at BIGINT NOT NULL DEFAULT 0\
        )",
        ),
        (
            "drive_shares",
            "CREATE TABLE IF NOT EXISTS drive_shares (\
            id TEXT PRIMARY KEY, \
            node_id TEXT NOT NULL, \
            token_hash TEXT NOT NULL, \
            scope TEXT NOT NULL, \
            expires_at BIGINT NOT NULL, \
            created_ms BIGINT NOT NULL\
        )",
        ),
        (
            "drive_tokens",
            "CREATE TABLE IF NOT EXISTS drive_tokens (\
            project TEXT PRIMARY KEY, \
            token_hash TEXT NOT NULL, \
            created_ms BIGINT NOT NULL\
        )",
        ),
    ] {
        ensure_table_exists(&mut session, table, ddl).await;
    }
    reconcile_project_teams_schema(&mut session).await;
    reconcile_billing_accounts_schema(&mut session).await;
    reconcile_billing_checkouts_schema(&mut session).await;
    reconcile_billing_invoices_schema(&mut session).await;
    let failed = SCHEMA_BRINGUP_FAILURES.load(std::sync::atomic::Ordering::Relaxed);
    if failed == 0 {
        tracing::info!(
            keys = index_keys,
            "relational: schema bring-up complete (every table verified present in the catalog)"
        );
    } else {
        tracing::error!(
            keys = index_keys,
            failed,
            "relational: schema bring-up finished with tables that could not be verified"
        );
    }
}

/// Tables `ensure_table_exists` gave up on this process (for the bring-up
/// summary; never reset — a later successful retry logs its own line).
static SCHEMA_BRINGUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

const SCHEMA_BRINGUP_MAX_ATTEMPTS: u32 = 4;

/// Runs `ddl`, then VERIFIES the table is in the catalog, re-running up to
/// `SCHEMA_BRINGUP_MAX_ATTEMPTS` times. The SQL catalog is ONE JSON document that every autocommit
/// statement loads and rewrites whole with no compare-and-swap, so a concurrent session's catalog
/// write can silently erase a just-created table while every statement still returns `Ok`.
async fn ensure_table_exists(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
    table: &str,
    ddl: &str,
) {
    if !index_ready() {
        warn_index_not_ready(table);
        return;
    }
    for attempt in 1..=SCHEMA_BRINGUP_MAX_ATTEMPTS {
        if let Err(e) = exec(s, ddl).await {
            tracing::warn!(table, attempt, error = %e, "relational: schema bring-up CREATE TABLE statement failed");
        }
        let verify = format!(
            "SELECT table_name FROM information_schema.tables WHERE table_name = '{table}'"
        );
        let present = exec(s, &verify).await.map(|results| {
            results
                .iter()
                .any(|r| matches!(r, ExecResult::Rows { rows, .. } if !rows.is_empty()))
        });
        match present {
            Ok(true) => return,
            Ok(false) if attempt == SCHEMA_BRINGUP_MAX_ATTEMPTS => {
                tracing::error!(
                    table,
                    attempts = SCHEMA_BRINGUP_MAX_ATTEMPTS,
                    "relational: schema bring-up could not verify this table exists after \
                     repeated CREATE TABLE + retry (absent from information_schema.tables)"
                );
                SCHEMA_BRINGUP_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(false) => {
                tracing::warn!(
                    table,
                    attempt,
                    "relational: table absent from the catalog right after CREATE TABLE IF NOT \
                     EXISTS reported success (a concurrent session's catalog read-modify-write \
                     race — see ensure_table_exists's doc comment); retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(150 * attempt as u64)).await;
            }
            Err(e) if attempt == SCHEMA_BRINGUP_MAX_ATTEMPTS => {
                tracing::error!(
                    table,
                    attempts = SCHEMA_BRINGUP_MAX_ATTEMPTS,
                    error = %e,
                    "relational: schema bring-up could not verify this table exists after \
                     repeated CREATE TABLE + retry — writes against it (e.g. upsert_billing's \
                     single atomic transaction) will keep silently failing until this is fixed"
                );
                SCHEMA_BRINGUP_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(
                    table,
                    attempt,
                    error = %e,
                    "relational: table missing immediately after CREATE TABLE IF NOT EXISTS \
                     reported success (likely lost to a concurrent session's catalog \
                     read-modify-write race — see ensure_table_exists's doc comment); retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(150 * attempt as u64)).await;
            }
        }
    }
}

static PROJECT_TEAMS_SCHEMA_RECONCILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Adds the `deleted_ms` tombstone column to a `project_teams` table that predates deletion
/// generations (`CREATE TABLE IF NOT EXISTS` never alters an existing table); memoized.
async fn reconcile_project_teams_schema(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
) {
    if PROJECT_TEAMS_SCHEMA_RECONCILED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if let Err(e) = exec(
        s,
        "ALTER TABLE project_teams ADD COLUMN IF NOT EXISTS deleted_ms BIGINT DEFAULT 0",
    )
    .await
    {
        tracing::warn!(error = %e, "relational: ALTER TABLE project_teams add deleted_ms failed");
        return;
    }
    if let Err(e) = exec(
        s,
        "UPDATE project_teams SET deleted_ms = 0 WHERE deleted_ms IS NULL",
    )
    .await
    {
        tracing::warn!(error = %e, "relational: project_teams deleted_ms backfill failed");
        return;
    }
    PROJECT_TEAMS_SCHEMA_RECONCILED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// One-time boot backfill of THIS node's `ProjectStore` rows into the mirror; safe on every node
/// (idempotent upserts of the same data). Rows the replica already holds identically are skipped:
/// LWW compares document timestamps, so re-asserting a stale live row would resurrect it over a
/// newer tombstone. Billing is NOT backfilled here: only the billing authority's store is live.
pub(crate) async fn backfill_projects(projects: Vec<(String, String, String, u64)>) {
    if projects.is_empty() {
        return;
    }
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    reconcile_project_teams_schema(&mut s).await;
    for (project, team, root_dir, updated_ms) in projects {
        let probe = format!(
            "SELECT team, root_dir, updated_ms, deleted_ms FROM project_teams WHERE project = {}",
            q(&project)
        );
        if let Ok(res) = exec(&mut s, &probe).await {
            let num = |row: &[SqlValue], i: usize| match row.get(i) {
                Some(SqlValue::Int8(n)) => *n as u64,
                Some(SqlValue::Int4(n)) => *n as u64,
                Some(SqlValue::Int2(n)) => *n as u64,
                Some(SqlValue::Text(t)) => t.parse().unwrap_or(0),
                _ => 0,
            };
            let unchanged = res.iter().any(|r| {
                if let ExecResult::Rows { rows, .. } = r {
                    rows.iter().any(|row| {
                        let rt = match row.first() {
                            Some(SqlValue::Text(t)) => t.as_str(),
                            _ => "",
                        };
                        let rr = match row.get(1) {
                            Some(SqlValue::Text(t)) => t.as_str(),
                            _ => "",
                        };
                        rt == team
                            && rr == root_dir
                            && num(row, 2) == updated_ms
                            && num(row, 3) == 0
                    })
                } else {
                    false
                }
            });
            if unchanged {
                continue;
            }
        }
        let query = project_team_upsert_sql(&project, &team, &root_dir, updated_ms);
        if let Err(e) = exec(&mut s, &query).await {
            tracing::debug!(project, error = %e, "relational: project backfill failed (non-fatal, periodic leader reconciliation will retry)");
        }
    }
}

/// SQL single-quote escape for hand-built literals; callers pass only constrained identifiers or
/// our own serialized JSON, never raw user SQL (`Session` exposes no parameter binding).
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// (TEXT, BIGINT) row pairs — e.g. `SELECT slug, updated_ms FROM teams`.
fn text_num_pairs(res: &[ExecResult]) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for r in res {
        if let ExecResult::Rows { rows, .. } = r {
            for row in rows {
                if let Some(SqlValue::Text(a)) = row.first() {
                    let n = match row.get(1) {
                        Some(SqlValue::Int8(n)) => *n as u64,
                        Some(SqlValue::Int4(n)) => *n as u64,
                        Some(SqlValue::Int2(n)) => *n as u64,
                        Some(SqlValue::Text(s)) => s.parse().unwrap_or(0),
                        _ => 0,
                    };
                    out.push((a.clone(), n));
                }
            }
        }
    }
    out
}

fn all_text_pairs(res: &[ExecResult]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for r in res {
        if let ExecResult::Rows { rows, .. } = r {
            for row in rows {
                if let (Some(SqlValue::Text(a)), Some(SqlValue::Text(b))) =
                    (row.first(), row.get(1))
                {
                    out.push((a.clone(), b.clone()));
                }
            }
        }
    }
    out
}

/// True once one full walk of the `hive` namespace has completed in this process. Before that the
/// index is EMPTY, not stale: every table reads as absent and a `CREATE TABLE IF NOT EXISTS` would
/// overwrite the real catalog, so every catalog-writing path refuses until it flips. The ONE
/// background walker is `spawn_index_refresher`; sessions never walk.
static INDEX_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INDEX_REFRESHER_SPAWNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Wall-clock ms of the last COMPLETED walk (0 = never).
static INDEX_LAST_OK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INDEX_NOT_READY_WARNED_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static INDEX_READY_NOTIFY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();

fn index_ready_notify() -> &'static tokio::sync::Notify {
    INDEX_READY_NOTIFY.get_or_init(tokio::sync::Notify::new)
}

/// `true` once one full index walk has completed in this process.
pub(crate) fn index_ready() -> bool {
    INDEX_READY.load(std::sync::atomic::Ordering::Acquire)
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn env_secs(name: &str, default: u64) -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default),
    )
}

/// Bound on ONE full walk. Sized to the store (a multi-GB guardian namespace
/// walked once), never to a request: the walk is off every request path.
fn index_build_timeout() -> std::time::Duration {
    env_secs("HIVE_RELATIONAL_INDEX_BUILD_SECS", 900)
}

/// Budget for the DETACHED post-build row-value warm-up (`HIVE_RELATIONAL_WARM_SECS`, default 600):
/// an optimization only, since rows materialize lazily on read, and it must never block the index
/// refresher's next tick.
fn warm_values_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var("HIVE_RELATIONAL_WARM_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n: &u64| *n > 0)
            .unwrap_or(600)
    )
}

/// Cadence of the background re-walk once the index is ready (`HIVE_RELATIONAL_REFRESH_SECS`,
/// default 120): bounds the staleness of a REMOTE write the live sync missed; local writes and
/// live-synced inserts reach the index immediately.
fn index_refresh_interval() -> std::time::Duration {
    env_secs("HIVE_RELATIONAL_REFRESH_SECS", 120)
}

/// Rate-limited (30 s) WARN for a caller that found the index unbuilt.
fn warn_index_not_ready(what: &str) {
    use std::sync::atomic::Ordering;
    let now = wall_ms();
    let last = INDEX_NOT_READY_WARNED_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 30_000
        && INDEX_NOT_READY_WARNED_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            what,
            "relational: index not built yet -- this session sees an EMPTY catalog (every \
             table reads as missing) until the first full walk completes; see \
             spawn_index_refresher"
        );
    }
}

/// Wait up to `bound` for the first walk to complete; `true` if it did.
async fn wait_index_ready(bound: std::time::Duration) -> bool {
    if index_ready() {
        return true;
    }
    let _ = tokio::time::timeout(bound, index_ready_notify().notified()).await;
    index_ready()
}

/// The one background walker for this process. Idempotent: the first caller
/// starts it, later calls return at once.
fn spawn_index_refresher(db: SqlDb) {
    use std::sync::atomic::Ordering;
    if INDEX_REFRESHER_SPAWNED.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::bulkhead::spawn(async move {
        let bound = index_build_timeout();
        let interval = index_refresh_interval();
        let mut failures: u32 = 0;
        loop {
            let started = std::time::Instant::now();
            match tokio::time::timeout(bound, db.storage().refresh()).await {
                Ok(Ok(())) => {
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let keys = db.storage().index_len();
                    INDEX_LAST_OK_MS.store(wall_ms(), Ordering::Release);
                    failures = 0;
                    if !INDEX_READY.swap(true, Ordering::AcqRel) {
                        tracing::info!(
                            elapsed_ms,
                            keys,
                            "relational: index built -- sessions now read the real catalog"
                        );
                        index_ready_notify().notify_waiters();
                        let db2 = db.clone();
                        crate::bulkhead::spawn(async move {
                            let warm_started = std::time::Instant::now();
                            match tokio::time::timeout(
                                warm_values_timeout(),
                                db2.storage().warm_values(),
                            )
                            .await
                            {
                                Ok(Ok(documents)) => tracing::info!(
                                    elapsed_ms = warm_started.elapsed().as_millis() as u64,
                                    documents,
                                    "relational: row values warmed -- cold scans no longer pay a per-row fetch"
                                ),
                                Ok(Err(e)) => tracing::warn!(
                                    error = %e,
                                    elapsed_ms = warm_started.elapsed().as_millis() as u64,
                                    "relational: row value warm-up failed; rows materialize lazily on read instead"
                                ),
                                Err(_) => tracing::warn!(
                                    bound_secs = warm_values_timeout().as_secs(),
                                    "relational: row value warm-up exceeded its budget; rows materialize lazily on read instead"
                                ),
                            }
                        });
                    } else {
                        tracing::debug!(elapsed_ms, keys, "relational: index re-walked");
                    }
                }
                Ok(Err(e)) => {
                    failures += 1;
                    tracing::warn!(
                        error = %e,
                        failures,
                        ready = index_ready(),
                        "relational: index walk failed; the previous index stays in service"
                    );
                }
                Err(_) => {
                    failures += 1;
                    tracing::warn!(
                        bound_secs = bound.as_secs(),
                        failures,
                        ready = index_ready(),
                        "relational: index walk exceeded its bound (HIVE_RELATIONAL_INDEX_BUILD_SECS); \
                         the previous index -- or, before the first build, NONE -- stays in service"
                    );
                }
            }
            let nap = if index_ready() {
                interval
            } else {
                std::time::Duration::from_secs(5)
            };
            tokio::time::sleep(nap).await;
        }
    });
}

/// A `Session` over the local index: starts the background walker if nothing has, waits up to
/// `SQL_OP_TIMEOUT` for the first walk, then proceeds either way (a not-yet-ready session reads an
/// empty catalog; catalog writers gate on `index_ready()`).
async fn session(db: SqlDb) -> Session<guardian_db::sql::GuardianRelationalStorage> {
    spawn_index_refresher(db.clone());
    if !wait_index_ready(SQL_OP_TIMEOUT).await {
        warn_index_not_ready("session");
    }
    Session::new(db, "hive")
}

/// Bound on one guardian-db SQL operation (index refresh or statement): an interrupted first open
/// once left `Session::execute` hanging silently forever.
const SQL_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The ONE chokepoint for every SQL execution: bounded by `SQL_OP_TIMEOUT`, errors flattened to
/// `String`, and run under `block_in_place` because the engine reloads every row of each table per
/// statement and would pin a serving tokio worker.
async fn exec(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
    sql: &str,
) -> Result<Vec<ExecResult>, String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(move || {
        handle.block_on(async {
            match tokio::time::timeout(SQL_OP_TIMEOUT, s.execute(sql)).await {
                Ok(Ok(res)) => Ok(res),
                Ok(Err(e)) => Err(format!("{e}")),
                Err(_) => Err(format!("guardian sql execute timed out after {SQL_OP_TIMEOUT:?} (namespace likely wedged from an interrupted first-open; restart or reset this node's guardian data)")),
            }
        })
    })
}

fn project_team_upsert_sql(project: &str, team: &str, root_dir: &str, updated_ms: u64) -> String {
    format!(
        "BEGIN; \
         UPDATE project_teams SET team = {}, root_dir = {}, updated_ms = {}, deleted_ms = 0 \
         WHERE project = {} AND deleted_ms = 0 AND updated_ms <= {}; \
         UPDATE project_teams SET team = {}, root_dir = {}, updated_ms = {}, deleted_ms = 0 \
         WHERE project = {} AND deleted_ms > 0 AND updated_ms < {}; \
         INSERT INTO project_teams (project, team, root_dir, updated_ms, deleted_ms) \
         VALUES ({}, {}, {}, {}, 0) \
         ON CONFLICT (project) DO NOTHING; \
         COMMIT;",
        q(team),
        q(root_dir),
        updated_ms,
        q(project),
        updated_ms,
        q(team),
        q(root_dir),
        updated_ms,
        q(project),
        updated_ms,
        q(project),
        q(team),
        q(root_dir),
        updated_ms,
    )
}

/// Upserts a project's team tag at the `ProjectStore` row's own causal `updated_ms`, never the
/// async execution time: a delayed pre-delete write would otherwise look newer than the deletion
/// and resurrect a stale row.
pub(crate) async fn set_project_team(project: &str, team: &str, root_dir: &str, updated_ms: u64) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    reconcile_project_teams_schema(&mut s).await;
    let query = project_team_upsert_sql(project, team, root_dir, updated_ms);
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(project, error = %e, "relational: set_project_team failed (non-fatal, fleet-fallback unavailable for this project until retried)");
    }
}

/// Records a project tombstone and keeps the row: after a hard DELETE a delayed pre-delete backfill
/// would INSERT stale ownership again. A causally newer recreate clears it via
/// `project_team_upsert_sql`; the write is skipped when a tombstone at or past `deleted_ms` already
/// exists (every node re-asserts every tombstone periodically).
pub(crate) async fn remove_project(project: &str, deleted_ms: u64) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    reconcile_project_teams_schema(&mut s).await;
    let probe = format!(
        "SELECT deleted_ms FROM project_teams WHERE project = {}",
        q(project)
    );
    if let Ok(res) = exec(&mut s, &probe).await {
        let cur = all_text_firsts(&res)
            .first()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if cur >= deleted_ms {
            return;
        }
    }
    let query = format!(
        "BEGIN; \
         UPDATE project_teams SET team = '', root_dir = '', updated_ms = {}, deleted_ms = {} \
         WHERE project = {} AND deleted_ms = 0 AND updated_ms <= {}; \
         INSERT INTO project_teams (project, team, root_dir, updated_ms, deleted_ms) \
         VALUES ({}, '', '', {}, {}) ON CONFLICT (project) DO NOTHING; \
         COMMIT;",
        deleted_ms,
        deleted_ms,
        q(project),
        deleted_ms,
        q(project),
        deleted_ms,
        deleted_ms,
    );
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(project, error = %e, "relational: remove_project failed (non-fatal)");
    }
}

/// Moves a project's mirror identity in ONE transaction: upserts the new name and TOMBSTONES the
/// old one (a hard DELETE would let a delayed pre-rename backfill resurrect it). A derived
/// read-only projection: failure is logged, never fatal.
pub(crate) async fn rename_project(from: &str, to: &str, updated_ms: u64) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    reconcile_project_teams_schema(&mut s).await;
    let probe = format!(
        "SELECT team, root_dir FROM project_teams WHERE project = {}",
        q(from)
    );
    let (team, root_dir) = match exec(&mut s, &probe).await {
        Ok(res) => {
            let cols = all_text_firsts(&res);
            (
                cols.first().cloned().unwrap_or_default(),
                cols.get(1).cloned().unwrap_or_default(),
            )
        }
        Err(_) => (String::new(), String::new()),
    };
    let query = format!(
        "BEGIN; \
         UPDATE project_teams SET deleted_ms = {updated_ms} WHERE project = {qf} AND deleted_ms = 0; \
         INSERT INTO project_teams (project, team, root_dir, updated_ms, deleted_ms) \
         VALUES ({qt}, {qteam}, {qroot}, {updated_ms}, 0) ON CONFLICT (project) DO NOTHING; \
         COMMIT;",
        qf = q(from),
        qt = q(to),
        qteam = q(&team),
        qroot = q(&root_dir),
    );
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(from, to, error = %e, "relational: rename_project failed (non-fatal)");
    }
}

/// Every project this node's replica holds for `team`, including projects with no deployment
/// anywhere (backs `gitops_projects`' fleet-visibility fallback).
pub(crate) async fn projects_for_team(team: &str) -> Vec<String> {
    let Ok(db) = crate::guardian::sql_db().await else {
        return Vec::new();
    };
    let mut s = session(db).await;
    reconcile_project_teams_schema(&mut s).await;
    let query = format!(
        "SELECT project, team FROM project_teams WHERE team = {} AND deleted_ms = 0",
        q(team)
    );
    let Ok(res) = exec(&mut s, &query).await else {
        return Vec::new();
    };
    all_text_pairs(&res).into_iter().map(|(p, _)| p).collect()
}

/// The owning team from a freshly refreshed replica: `Ok(None)` = no live row; `Err` = freshness
/// could not be established (index unbuilt, or last walk older than two refresh intervals), so
/// callers use their lower-authority fallback instead of a stale index row.
pub(crate) async fn team_for_project(project: &str) -> Result<Option<String>, String> {
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("guardian relational database unavailable: {e}"))?;
    spawn_index_refresher(db.clone());
    if !wait_index_ready(SQL_OP_TIMEOUT).await {
        return Err("relational index not built yet".to_string());
    }
    let age_ms =
        wall_ms().saturating_sub(INDEX_LAST_OK_MS.load(std::sync::atomic::Ordering::Acquire));
    let max_age_ms = index_refresh_interval().as_millis() as u64 * 2;
    if age_ms > max_age_ms {
        return Err(format!(
            "relational index is stale: last completed walk {age_ms} ms ago (limit {max_age_ms} ms)"
        ));
    }
    let mut s = Session::new(db, "hive");
    reconcile_project_teams_schema(&mut s).await;
    let query = format!(
        "SELECT project, team FROM project_teams WHERE project = {} AND deleted_ms = 0",
        q(project)
    );
    let res = exec(&mut s, &query).await?;
    Ok(all_text_pairs(&res)
        .into_iter()
        .map(|(_, t)| t)
        .find(|t| !t.trim().is_empty()))
}

/// EVERY (project, team) pair in this node's replica: the tenancy reconciler's repair source
/// (unscoped; one row per project).
pub(crate) async fn all_project_teams() -> Vec<(String, String)> {
    let Ok(db) = crate::guardian::sql_db().await else {
        return Vec::new();
    };
    let mut s = session(db).await;
    reconcile_project_teams_schema(&mut s).await;
    let Ok(res) = exec(
        &mut s,
        "SELECT project, team FROM project_teams WHERE deleted_ms = 0",
    )
    .await
    else {
        return Vec::new();
    };
    all_text_pairs(&res)
}

/// Mirrors ONE tenant's billing rows: [`upsert_billing_many`] with a batch of one.
pub(crate) async fn upsert_billing(
    account: &crate::billing::BillingAccount,
    ledger: &[crate::billing::LedgerEntry],
    invoices: &[crate::billing::Invoice],
    checkouts: &[crate::billing::Checkout],
) {
    upsert_billing_many(std::slice::from_ref(&(
        account, ledger, invoices, checkouts,
    )))
    .await
}

/// One tenant's billing rows, as the batch writer takes them.
pub(crate) type BillingRows<'a> = (
    &'a crate::billing::BillingAccount,
    &'a [crate::billing::LedgerEntry],
    &'a [crate::billing::Invoice],
    &'a [crate::billing::Checkout],
);

/// Mirrors tenants' billing account, ledger, FINALIZED invoices (+ lines) and checkouts as per-row
/// tables, on the billing authority. Each tenant is ONE `BEGIN; ...; COMMIT;` execute (no partial
/// rows; one tenant's failure never rolls back another's) and the batch shares ONE session and
/// schema reconciliation. Best-effort (`admin::proxy_billing_read` is the safety net); the
/// `billing_*` column reconciliation runs on every call (memoized), so schema never rests on
/// `init_schema`'s one boot-time attempt.
pub(crate) async fn upsert_billing_many(batch: &[BillingRows<'_>]) {
    if batch.is_empty() {
        return;
    }
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let now = hive_core::now_ms();
    let mut s = session(db).await;
    reconcile_billing_accounts_schema(&mut s).await;
    reconcile_billing_checkouts_schema(&mut s).await;
    reconcile_billing_invoices_schema(&mut s).await;

    for (account, ledger, invoices, checkouts) in batch {
        let started = std::time::Instant::now();
        let mut sql = String::from("BEGIN;");
        sql.push_str(&build_account_sql(account, now));
        sql.push_str(&build_ledger_sql(ledger));
        sql.push_str(&build_invoices_sql(invoices));
        sql.push_str(&build_checkouts_sql(checkouts));
        sql.push_str("COMMIT;");
        let statements = sql.matches(';').count();
        let ledger_rows = ledger.len();

        if let Err(e) = exec(&mut s, &sql).await {
            tracing::debug!(
                tenant = %account.tenant,
                error = %e,
                "relational: upsert_billing transaction failed (non-fatal, HTTP proxy-to-leader remains the \
                 fallback — and since this ran as ONE transaction, the failure rolled back atomically: no \
                 partial account/ledger/invoice/checkout row state was left behind)"
            );
        } else {
            let ms = started.elapsed().as_millis() as u64;
            if ms >= 500 {
                tracing::info!(
                    tenant = %account.tenant,
                    ms,
                    statements,
                    ledger_rows,
                    "relational: billing mirror write"
                );
            }
        }
    }
}

/// SQL for the account row alone (`ON CONFLICT (tenant) DO UPDATE`, so it converges with a
/// pre-existing old-shape row); shared with `backfill_billing_normalize`.
fn build_account_sql(account: &crate::billing::BillingAccount, now: u64) -> String {
    format!(
        "INSERT INTO billing_accounts (tenant, plan, status, included_cents, used_cents, balance_cents, \
         stripe_customer, stripe_subscription, period_start_ms, period_end_ms, updated_ms) \
         VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}) \
         ON CONFLICT (tenant) DO UPDATE SET plan = excluded.plan, status = excluded.status, \
         included_cents = excluded.included_cents, used_cents = excluded.used_cents, \
         balance_cents = excluded.balance_cents, stripe_customer = excluded.stripe_customer, \
         stripe_subscription = excluded.stripe_subscription, period_start_ms = excluded.period_start_ms, \
         period_end_ms = excluded.period_end_ms, updated_ms = excluded.updated_ms;",
        q(&account.tenant),
        q(&account.plan),
        q(&account.status),
        account.included_cents,
        account.used_cents,
        account.balance_cents,
        q(&account.stripe_customer),
        q(&account.stripe_subscription),
        account.period_start_ms,
        account.period_end_ms,
        now,
    )
}

/// Rows per INSERT: the guardian engine reloads the WHOLE target table once per statement, so one
/// row per statement made an N-row ledger cost N table loads; chunking also keeps a huge ledger
/// from becoming one unbounded statement.
const ROWS_PER_INSERT: usize = 500;

/// Chunks `rows` into multi-row INSERTs of [`ROWS_PER_INSERT`]: `prefix` ends in `VALUES `,
/// `on_conflict` in `;`; no rows, no SQL.
fn multi_row_insert(prefix: &str, rows: &[String], on_conflict: &str) -> String {
    let mut sql = String::with_capacity(rows.iter().map(|r| r.len() + 1).sum::<usize>() + 256);
    for chunk in rows.chunks(ROWS_PER_INSERT) {
        sql.push_str(prefix);
        sql.push_str(&chunk.join(","));
        sql.push(' ');
        sql.push_str(on_conflict);
    }
    sql
}

/// SQL for every ledger row (`ON CONFLICT (id) DO UPDATE`: entry ids are deterministic, so
/// re-upserts converge).
fn build_ledger_sql(ledger: &[crate::billing::LedgerEntry]) -> String {
    let rows: Vec<String> = ledger
        .iter()
        .map(|e| {
            format!(
                "({}, {}, {}, {}, {}, {}, {})",
                q(&e.id),
                q(&e.tenant),
                e.ts_ms,
                q(&e.kind),
                e.amount_cents,
                e.balance_after_cents,
                q(&e.note),
            )
        })
        .collect();
    multi_row_insert(
        "INSERT INTO billing_ledger (id, tenant, ts_ms, kind, amount_cents, balance_after_cents, note) VALUES ",
        &rows,
        "ON CONFLICT (id) DO UPDATE SET tenant = excluded.tenant, ts_ms = excluded.ts_ms, \
         kind = excluded.kind, amount_cents = excluded.amount_cents, \
         balance_after_cents = excluded.balance_after_cents, note = excluded.note;",
    )
}

/// Row count of `billing_ledger` for `tenant` in `[period_start_ms, period_end_ms)`: the durability
/// gate, since pruning waits until the mirror has caught up to a checkpoint's `entry_count`.
pub(crate) async fn count_billing_ledger_rows(
    tenant: &str,
    period_start_ms: u64,
    period_end_ms: u64,
) -> Result<u64, String> {
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("guardian sql unavailable: {e}"))?;
    let mut s = session(db).await;
    let query = format!(
        "SELECT COUNT(*) FROM billing_ledger WHERE tenant = {} AND ts_ms >= {} AND ts_ms < {}",
        q(tenant),
        period_start_ms,
        period_end_ms,
    );
    let res = exec(&mut s, &query).await?;
    for r in &res {
        if let ExecResult::Rows { rows, .. } = r {
            if let Some(row) = rows.first() {
                let n = match row.first() {
                    Some(SqlValue::Int8(n)) => *n as u64,
                    Some(SqlValue::Int4(n)) => *n as u64,
                    _ => 0,
                };
                return Ok(n);
            }
        }
    }
    Ok(0)
}

/// Deletes `billing_ledger` rows for `tenant` in `[period_start_ms, period_end_ms)`, only once the
/// covering `LedgerCheckpoint` is durable. Its own autocommit DELETE, never part of the upsert
/// transaction (a prune failure must not roll back an account write); idempotent.
pub(crate) async fn prune_billing_ledger(
    tenant: &str,
    period_start_ms: u64,
    period_end_ms: u64,
) -> Result<(), String> {
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("guardian sql unavailable: {e}"))?;
    let mut s = session(db).await;
    let sql = format!(
        "DELETE FROM billing_ledger WHERE tenant = {} AND ts_ms >= {} AND ts_ms < {}",
        q(tenant),
        period_start_ms,
        period_end_ms,
    );
    exec(&mut s, &sql).await?;
    Ok(())
}

/// An invoice in this status is final; any other (a draft) is transient and never persisted.
const FINALIZED_INVOICE_STATUS: &str = "paid";

/// Deterministic per-line id (a re-upsert converges), zero-padded so `ORDER BY id` keeps line order.
fn invoice_line_id(invoice_id: &str, index: usize) -> String {
    format!("{invoice_id}-{index:03}")
}

/// SQL for every FINALIZED invoice and its lines; a draft is never persisted as a row.
fn build_invoices_sql(invoices: &[crate::billing::Invoice]) -> String {
    let mut invoice_rows: Vec<String> = Vec::new();
    let mut line_rows: Vec<String> = Vec::new();
    for inv in invoices {
        if inv.status != FINALIZED_INVOICE_STATUS {
            continue;
        }
        let (commitment, entry_count, pruned) = match inv.ledger_checkpoint.as_ref() {
            Some(cp) => (cp.commitment.clone(), cp.entry_count, cp.pruned),
            None => (String::new(), 0u64, false),
        };
        invoice_rows.push(format!(
            "({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            q(&inv.id),
            q(&inv.tenant),
            q(&inv.number),
            q(&inv.plan),
            inv.period_start_ms,
            inv.period_end_ms,
            inv.subtotal_cents,
            inv.total_cents,
            q(&inv.status),
            inv.created_ms,
            q(&commitment),
            entry_count,
            if pruned { 1 } else { 0 },
        ));
        for (idx, line) in inv.lines.iter().enumerate() {
            let line_id = invoice_line_id(&inv.id, idx);
            line_rows.push(format!(
                "({}, {}, {}, {})",
                q(&line_id),
                q(&inv.id),
                q(&line.description),
                line.amount_cents,
            ));
        }
    }
    let mut sql = multi_row_insert(
        "INSERT INTO billing_invoices (id, tenant, number, plan, period_start_ms, period_end_ms, \
         subtotal_cents, total_cents, status, created_ms, \
         ledger_commitment, ledger_entry_count, ledger_pruned) VALUES ",
        &invoice_rows,
        "ON CONFLICT (id) DO UPDATE SET tenant = excluded.tenant, number = excluded.number, \
         plan = excluded.plan, period_start_ms = excluded.period_start_ms, \
         period_end_ms = excluded.period_end_ms, subtotal_cents = excluded.subtotal_cents, \
         total_cents = excluded.total_cents, status = excluded.status, created_ms = excluded.created_ms, \
         ledger_commitment = excluded.ledger_commitment, \
         ledger_entry_count = excluded.ledger_entry_count, \
         ledger_pruned = excluded.ledger_pruned;",
    );
    sql.push_str(&multi_row_insert(
        "INSERT INTO billing_invoice_lines (id, invoice_id, description, amount_cents) VALUES ",
        &line_rows,
        "ON CONFLICT (id) DO UPDATE SET invoice_id = excluded.invoice_id, \
         description = excluded.description, amount_cents = excluded.amount_cents;",
    ));
    sql
}

/// SQL for every checkout row (`ON CONFLICT (id) DO UPDATE`). Factored out
/// of `upsert_billing` — see `build_account_sql`.
fn build_checkouts_sql(checkouts: &[crate::billing::Checkout]) -> String {
    let rows: Vec<String> = checkouts
        .iter()
        .map(|co| {
            format!(
                "({}, {}, {}, {}, {}, {}, {}, {}, {})",
                q(&co.id),
                q(&co.tenant),
                q(&co.kind),
                q(&co.plan),
                co.amount_cents,
                q(&co.sku),
                q(&co.target),
                q(&co.stripe_session_id),
                co.created_ms,
            )
        })
        .collect();
    multi_row_insert(
        "INSERT INTO billing_checkouts (id, tenant, kind, plan, amount_cents, sku, target, \
         stripe_session_id, created_ms) VALUES ",
        &rows,
        "ON CONFLICT (id) DO UPDATE SET tenant = excluded.tenant, kind = excluded.kind, \
         plan = excluded.plan, amount_cents = excluded.amount_cents, sku = excluded.sku, \
         target = excluded.target, stripe_session_id = excluded.stripe_session_id, \
         created_ms = excluded.created_ms;",
    )
}

/// `billing_accounts` columns added by `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`: the table name
/// predates the per-row schema, so `CREATE TABLE IF NOT EXISTS` is a no-op on the old 3-column
/// shape every existing node holds. Deliberately nullable, since the table may already hold old
/// rows.
const BILLING_ACCOUNTS_NEW_COLUMNS: &[(&str, &str)] = &[
    ("plan", "TEXT"),
    ("status", "TEXT"),
    ("included_cents", "BIGINT"),
    ("used_cents", "BIGINT"),
    ("balance_cents", "BIGINT"),
    ("stripe_customer", "TEXT"),
    ("stripe_subscription", "TEXT"),
    ("period_start_ms", "BIGINT"),
    ("period_end_ms", "BIGINT"),
];

/// Set once every `ALTER TABLE` of `reconcile_billing_accounts_schema` has succeeded in this
/// process; left UNSET on any failure so the next call retries. Relaxed ordering suffices: a lost
/// race only re-runs idempotent ALTERs.
static BILLING_ACCOUNTS_SCHEMA_RECONCILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Idempotent `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` for each `BILLING_ACCOUNTS_NEW_COLUMNS`
/// entry, memoized. Called from `init_schema`, the backfill and every billing upsert, so correct
/// schema never rests on one boot-time attempt.
async fn reconcile_billing_accounts_schema(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
) {
    if BILLING_ACCOUNTS_SCHEMA_RECONCILED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut all_ok = true;
    for (col, ty) in BILLING_ACCOUNTS_NEW_COLUMNS {
        let ddl = format!("ALTER TABLE billing_accounts ADD COLUMN IF NOT EXISTS {col} {ty}");
        if let Err(e) = exec(s, &ddl).await {
            all_ok = false;
            tracing::warn!(column = col, error = %e, "relational: ALTER TABLE billing_accounts add column failed");
        }
    }
    if all_ok {
        BILLING_ACCOUNTS_SCHEMA_RECONCILED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `billing_checkouts` columns added after the table first shipped, ALTER-reconciled because
/// `CREATE TABLE IF NOT EXISTS` never alters an existing table.
const BILLING_CHECKOUTS_NEW_COLUMNS: &[(&str, &str)] = &[("sku", "TEXT"), ("target", "TEXT")];

static BILLING_CHECKOUTS_SCHEMA_RECONCILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

async fn reconcile_billing_checkouts_schema(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
) {
    if BILLING_CHECKOUTS_SCHEMA_RECONCILED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut all_ok = true;
    for (col, ty) in BILLING_CHECKOUTS_NEW_COLUMNS {
        let ddl = format!(
            "ALTER TABLE billing_checkouts ADD COLUMN IF NOT EXISTS {col} {ty} NOT NULL DEFAULT ''"
        );
        if let Err(e) = exec(s, &ddl).await {
            all_ok = false;
            tracing::warn!(column = col, error = %e, "relational: ALTER TABLE billing_checkouts add column failed");
        }
    }
    if all_ok {
        BILLING_CHECKOUTS_SCHEMA_RECONCILED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `billing_invoices` ledger-checkpoint columns, ALTER-reconciled like the other post-launch
/// columns.
const BILLING_INVOICES_NEW_COLUMNS: &[(&str, &str, &str)] = &[
    ("ledger_commitment", "TEXT", "''"),
    ("ledger_entry_count", "BIGINT", "0"),
    ("ledger_pruned", "BIGINT", "0"),
];

/// See `BILLING_ACCOUNTS_SCHEMA_RECONCILED` — same memoization discipline.
static BILLING_INVOICES_SCHEMA_RECONCILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// See `reconcile_billing_checkouts_schema` — identical shape, called from
/// the same sites (`init_schema()`, unconditionally from `upsert_billing_many`).
async fn reconcile_billing_invoices_schema(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
) {
    if BILLING_INVOICES_SCHEMA_RECONCILED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut all_ok = true;
    for (col, ty, default) in BILLING_INVOICES_NEW_COLUMNS {
        let ddl = format!(
            "ALTER TABLE billing_invoices ADD COLUMN IF NOT EXISTS {col} {ty} NOT NULL DEFAULT {default}"
        );
        if let Err(e) = exec(s, &ddl).await {
            all_ok = false;
            tracing::warn!(column = col, error = %e, "relational: ALTER TABLE billing_invoices add column failed");
        }
    }
    if all_ok {
        BILLING_INVOICES_SCHEMA_RECONCILED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// One-time OPERATOR backfill (`POST /v1/admin/billing/backfill`; never automatic, writes for
/// real): rewrites each tenant's legacy JSON-blob rows through the production per-tenant
/// transaction, verifies row-count parity, then records it in `billing_backfill_state` (re-runs
/// skip verified tenants unless `force`). An account row is overwritten only by a blob not older
/// than the live row; with no legacy source queryable it returns `{"ran": false, ...}` rather than
/// fabricate a migration. The legacy `*_snapshot` tables are never dropped before it has run
/// fleet-wide.
pub(crate) async fn backfill_billing_normalize(force: bool) -> Result<serde_json::Value, String> {
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("relational store unavailable: {e}"))?;
    let mut s = session(db).await;
    if !index_ready() {
        return Err(
            "relational index not built yet; refusing to run DDL against an empty catalog"
                .to_string(),
        );
    }

    if let Err(e) = exec(
        &mut s,
        "CREATE TABLE IF NOT EXISTS billing_backfill_state (\
            tenant TEXT PRIMARY KEY, \
            accounts_migrated BIGINT NOT NULL, \
            ledger_rows_migrated BIGINT NOT NULL, \
            invoices_migrated BIGINT NOT NULL, \
            invoice_lines_migrated BIGINT NOT NULL, \
            migrated_ms BIGINT NOT NULL\
        )",
    )
    .await
    {
        return Err(format!(
            "could not bring up billing_backfill_state marker table: {e}"
        ));
    }

    let old_accounts_res = exec(&mut s, "SELECT tenant, account_json FROM billing_accounts").await;
    let old_ledger_res = exec(
        &mut s,
        "SELECT tenant, ledger_json FROM billing_ledger_snapshot",
    )
    .await;
    let old_invoices_res = exec(
        &mut s,
        "SELECT tenant, invoices_json FROM billing_invoices_snapshot",
    )
    .await;

    let accounts_old_queryable = old_accounts_res.is_ok();
    let ledger_old_queryable = old_ledger_res.is_ok();
    let invoices_old_queryable = old_invoices_res.is_ok();

    if !accounts_old_queryable && !ledger_old_queryable && !invoices_old_queryable {
        return Ok(serde_json::json!({
            "ran": false,
            "blocking_finding": "None of billing_accounts.account_json / billing_ledger_snapshot.ledger_json / \
                billing_invoices_snapshot.invoices_json are queryable on this node right now -- the pre-migration \
                JSON-blob data this backfill depends on appears to already be unreachable (columns/tables dropped, \
                or this replica never had them). This backfill MUST run before any destructive removal of the old \
                columns/tables; if this fired, that ordering was violated somewhere and needs investigation before \
                any further billing schema work proceeds -- do NOT assume this means there is nothing left to \
                migrate.",
            "accounts_old_queryable": false,
            "ledger_old_queryable": false,
            "invoices_old_queryable": false,
        }));
    }

    let mut accounts_by_tenant: std::collections::HashMap<String, crate::billing::BillingAccount> =
        std::collections::HashMap::new();
    if let Ok(res) = &old_accounts_res {
        for (tenant, blob) in all_text_pairs(res) {
            match serde_json::from_str::<crate::billing::BillingAccount>(&blob) {
                Ok(acc) => {
                    accounts_by_tenant.insert(tenant, acc);
                }
                Err(e) => {
                    tracing::warn!(tenant, error = %e, "backfill: could not parse old account_json (row skipped, NOT counted as migrated)")
                }
            }
        }
    }
    let mut ledger_by_tenant: std::collections::HashMap<String, Vec<crate::billing::LedgerEntry>> =
        std::collections::HashMap::new();
    if let Ok(res) = &old_ledger_res {
        for (tenant, blob) in all_text_pairs(res) {
            match serde_json::from_str::<Vec<crate::billing::LedgerEntry>>(&blob) {
                Ok(entries) => {
                    ledger_by_tenant.insert(tenant, entries);
                }
                Err(e) => {
                    tracing::warn!(tenant, error = %e, "backfill: could not parse old ledger_json (row skipped, NOT counted as migrated)")
                }
            }
        }
    }
    let mut invoices_by_tenant: std::collections::HashMap<String, Vec<crate::billing::Invoice>> =
        std::collections::HashMap::new();
    if let Ok(res) = &old_invoices_res {
        for (tenant, blob) in all_text_pairs(res) {
            match serde_json::from_str::<Vec<crate::billing::Invoice>>(&blob) {
                Ok(invs) => {
                    invoices_by_tenant.insert(
                        tenant,
                        invs.into_iter()
                            .filter(|i| i.status == FINALIZED_INVOICE_STATUS)
                            .collect(),
                    );
                }
                Err(e) => {
                    tracing::warn!(tenant, error = %e, "backfill: could not parse old invoices_json (row skipped, NOT counted as migrated)")
                }
            }
        }
    }

    reconcile_billing_accounts_schema(&mut s).await;
    reconcile_billing_checkouts_schema(&mut s).await;

    let already_migrated: std::collections::HashSet<String> = if force {
        std::collections::HashSet::new()
    } else {
        exec(&mut s, "SELECT tenant FROM billing_backfill_state")
            .await
            .ok()
            .map(|r| all_text_firsts(&r).into_iter().collect())
            .unwrap_or_default()
    };

    let current_account_updated_ms: std::collections::HashMap<String, u64> =
        if accounts_old_queryable {
            exec(&mut s, "SELECT tenant, updated_ms FROM billing_accounts")
                .await
                .ok()
                .map(|r| text_num_pairs(&r).into_iter().collect())
                .unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };

    let mut all_tenants: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all_tenants.extend(accounts_by_tenant.keys().cloned());
    all_tenants.extend(ledger_by_tenant.keys().cloned());
    all_tenants.extend(invoices_by_tenant.keys().cloned());
    let tenants_total = all_tenants.len();

    let mut migrated = Vec::new();
    let mut skipped_already_migrated = Vec::new();
    let mut failed = Vec::new();

    for tenant in all_tenants {
        if already_migrated.contains(&tenant) {
            skipped_already_migrated.push(tenant);
            continue;
        }
        let acc = accounts_by_tenant.get(&tenant);
        let empty_ledger = Vec::new();
        let ledger = ledger_by_tenant.get(&tenant).unwrap_or(&empty_ledger);
        let empty_invoices = Vec::new();
        let invoices = invoices_by_tenant.get(&tenant).unwrap_or(&empty_invoices);

        let write_result: Result<(), String> = if let Some(acc) = acc {
            let live_is_fresher = current_account_updated_ms
                .get(&tenant)
                .is_some_and(|&live_updated_ms| live_updated_ms > acc.updated_ms);
            if live_is_fresher {
                tracing::info!(
                    tenant = %tenant,
                    blob_updated_ms = acc.updated_ms,
                    "backfill: live billing_accounts row is already fresher than the frozen pre-migration \
                     snapshot; skipping account overwrite (ledger/invoice rows still written)"
                );
                write_ledger_and_invoices_only(&mut s, ledger, invoices).await
            } else {
                upsert_billing(acc, ledger, invoices, &[]).await;
                Ok(())
            }
        } else if !ledger.is_empty() || !invoices.is_empty() {
            write_ledger_and_invoices_only(&mut s, ledger, invoices).await
        } else {
            Ok(())
        };
        if let Err(e) = write_result {
            failed.push(serde_json::json!({ "tenant": tenant, "stage": "write", "error": e }));
            continue;
        }

        let mut mismatches = Vec::new();
        if acc.is_some() {
            let has_row = exec(
                &mut s,
                &format!(
                    "SELECT tenant FROM billing_accounts WHERE tenant = {} AND plan IS NOT NULL",
                    q(&tenant)
                ),
            )
            .await
            .ok()
            .map(|r| !all_text_firsts(&r).is_empty())
            .unwrap_or(false);
            if !has_row {
                mismatches.push("billing_accounts row missing or not yet populated".to_string());
            }
        }
        let ledger_count = exec(
            &mut s,
            &format!(
                "SELECT id FROM billing_ledger WHERE tenant = {}",
                q(&tenant)
            ),
        )
        .await
        .ok()
        .map(|r| all_text_firsts(&r).len())
        .unwrap_or(0);
        if ledger_count != ledger.len() {
            mismatches.push(format!(
                "billing_ledger row count {ledger_count} != expected {}",
                ledger.len()
            ));
        }
        let invoice_count = exec(
            &mut s,
            &format!(
                "SELECT id FROM billing_invoices WHERE tenant = {}",
                q(&tenant)
            ),
        )
        .await
        .ok()
        .map(|r| all_text_firsts(&r).len())
        .unwrap_or(0);
        if invoice_count != invoices.len() {
            mismatches.push(format!(
                "billing_invoices row count {invoice_count} != expected {}",
                invoices.len()
            ));
        }
        let expected_lines: usize = invoices.iter().map(|i| i.lines.len()).sum();
        let mut actual_lines = 0usize;
        for inv in invoices.iter() {
            actual_lines += exec(
                &mut s,
                &format!(
                    "SELECT id FROM billing_invoice_lines WHERE invoice_id = {}",
                    q(&inv.id)
                ),
            )
            .await
            .ok()
            .map(|r| all_text_firsts(&r).len())
            .unwrap_or(0);
        }
        if actual_lines != expected_lines {
            mismatches.push(format!(
                "billing_invoice_lines row count {actual_lines} != expected {expected_lines}"
            ));
        }

        if mismatches.is_empty() {
            let now = hive_core::now_ms();
            let marker = format!(
                "INSERT INTO billing_backfill_state (tenant, accounts_migrated, ledger_rows_migrated, invoices_migrated, invoice_lines_migrated, migrated_ms) \
                 VALUES ({}, {}, {}, {}, {}, {}) \
                 ON CONFLICT (tenant) DO UPDATE SET accounts_migrated = excluded.accounts_migrated, \
                 ledger_rows_migrated = excluded.ledger_rows_migrated, invoices_migrated = excluded.invoices_migrated, \
                 invoice_lines_migrated = excluded.invoice_lines_migrated, migrated_ms = excluded.migrated_ms",
                q(&tenant),
                if acc.is_some() { 1 } else { 0 },
                ledger.len(),
                invoices.len(),
                expected_lines,
                now,
            );
            if let Err(e) = exec(&mut s, &marker).await {
                failed.push(
                    serde_json::json!({ "tenant": tenant, "stage": "mark-migrated", "error": e }),
                );
            } else {
                migrated.push(tenant);
            }
        } else {
            let _ = exec(
                &mut s,
                &format!(
                    "DELETE FROM billing_backfill_state WHERE tenant = {}",
                    q(&tenant)
                ),
            )
            .await;
            failed.push(serde_json::json!({ "tenant": tenant, "stage": "verify", "mismatches": mismatches }));
        }
    }

    Ok(serde_json::json!({
        "ran": true,
        "accounts_old_queryable": accounts_old_queryable,
        "ledger_old_queryable": ledger_old_queryable,
        "invoices_old_queryable": invoices_old_queryable,
        "tenants_total": tenants_total,
        "tenants_migrated": migrated,
        "tenants_skipped_already_migrated": skipped_already_migrated,
        "tenants_failed": failed,
    }))
}

/// `upsert_billing` minus the account statement, for a tenant with ledger/invoice history but no
/// parsable old account row (real history is never dropped).
async fn write_ledger_and_invoices_only(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
    ledger: &[crate::billing::LedgerEntry],
    invoices: &[crate::billing::Invoice],
) -> Result<(), String> {
    let mut sql = String::from("BEGIN;");
    sql.push_str(&build_ledger_sql(ledger));
    sql.push_str(&build_invoices_sql(invoices));
    sql.push_str("COMMIT;");
    exec(s, &sql).await.map(|_| ())
}

/// Every value of the first column across all result sets (used to diff the
/// mirror's current rows against a fresh snapshot for stale-row deletion).
fn all_text_firsts(res: &[ExecResult]) -> Vec<String> {
    let mut out = Vec::new();
    for r in res {
        if let ExecResult::Rows { rows, .. } = r {
            for row in rows {
                if let Some(SqlValue::Text(s)) = row.first() {
                    out.push(s.clone());
                }
            }
        }
    }
    out
}

/// Full-snapshot sync of `teams` + `team_members`; the caller gates on being the control-plane
/// leader (single writer). A row missing from the snapshot is deleted only once its `updated_ms`
/// stopped advancing (`HIVE_RELATIONAL_TOMBSTONE_SECS`, default 600): a stand-in leader's stale
/// `TeamStore` must never wipe real teams.
pub(crate) async fn sync_teams(teams: &[crate::teams::Team]) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let now = hive_core::now_ms();
    let mut s = session(db).await;
    for t in teams {
        let query = format!(
            "INSERT INTO teams (slug, name, plan, member_count, sso_enabled, created_ms, updated_ms) \
             VALUES ({}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (slug) DO UPDATE SET name = excluded.name, plan = excluded.plan, \
             member_count = excluded.member_count, sso_enabled = excluded.sso_enabled, \
             created_ms = excluded.created_ms, updated_ms = excluded.updated_ms",
            q(&t.slug),
            q(&t.name),
            q(&t.plan),
            t.members.len(),
            if t.sso_enabled { 1 } else { 0 },
            t.created_ms,
            now,
        );
        if let Err(e) = exec(&mut s, &query).await {
            tracing::warn!(team = %t.slug, error = %e, "relational: sync_teams upsert failed (row will be missing from the mirror until the next team-data change)");
        }
        for m in &t.members {
            let id = format!("{}/{}", t.slug, m.email.to_lowercase());
            let role = serde_json::to_string(&m.role)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            let mq = format!(
                "INSERT INTO team_members (id, team, email, name, role, added_ms, updated_ms) \
                 VALUES ({}, {}, {}, {}, {}, {}, {}) \
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name, role = excluded.role, \
                 added_ms = excluded.added_ms, updated_ms = excluded.updated_ms",
                q(&id),
                q(&t.slug),
                q(&m.email),
                q(&m.name),
                q(&role),
                m.added_ms,
                now,
            );
            if let Err(e) = exec(&mut s, &mq).await {
                tracing::warn!(member = %id, error = %e, "relational: sync_teams member upsert failed (row will be missing from the mirror until the next team-data change)");
            }
        }
    }
    let tombstone_ms = std::env::var("HIVE_RELATIONAL_TOMBSTONE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600)
        * 1000;
    let live_teams: std::collections::HashSet<String> =
        teams.iter().map(|t| t.slug.clone()).collect();
    let live_members: std::collections::HashSet<String> = teams
        .iter()
        .flat_map(|t| {
            t.members
                .iter()
                .map(move |m| format!("{}/{}", t.slug, m.email.to_lowercase()))
        })
        .collect();
    let stale = |updated: u64| now.saturating_sub(updated) > tombstone_ms;
    if let Ok(res) = exec(&mut s, "SELECT slug, updated_ms FROM teams").await {
        for (slug, updated) in text_num_pairs(&res) {
            if !live_teams.contains(&slug) && stale(updated) {
                let _ = exec(
                    &mut s,
                    &format!("DELETE FROM teams WHERE slug = {}", q(&slug)),
                )
                .await;
            }
        }
    }
    if let Ok(res) = exec(&mut s, "SELECT id, updated_ms FROM team_members").await {
        for (id, updated) in text_num_pairs(&res) {
            if !live_members.contains(&id) && stale(updated) {
                let _ = exec(
                    &mut s,
                    &format!("DELETE FROM team_members WHERE id = {}", q(&id)),
                )
                .await;
            }
        }
    }
}

/// Syncs THIS node's deployments into `deployments`: each node writes and deletes only rows whose
/// `node` is itself (one writer per row), so a fleet-wide sync converges without conflicts.
pub(crate) async fn sync_deployments(node: &str, deps: &[fluid_core::DeploymentInfo]) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let now = hive_core::now_ms();
    let mut s = session(db).await;
    for d in deps {
        let state = serde_json::to_string(&d.state)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let team = if d.tenant.is_empty() {
            "personal"
        } else {
            d.tenant.as_str()
        };
        let query = format!(
            "INSERT INTO deployments (id, project, team, node, alias, kind, target, state, production, created_at_ms, updated_ms) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (id) DO UPDATE SET project = excluded.project, team = excluded.team, \
             node = excluded.node, alias = excluded.alias, kind = excluded.kind, target = excluded.target, \
             state = excluded.state, production = excluded.production, \
             created_at_ms = excluded.created_at_ms, updated_ms = excluded.updated_ms",
            q(&d.id.to_string()),
            q(&d.project),
            q(team),
            q(node),
            q(&d.alias),
            q(&d.kind),
            q(&d.target),
            q(&state),
            if d.production { 1 } else { 0 },
            d.created_at_ms,
            now,
        );
        if let Err(e) = exec(&mut s, &query).await {
            tracing::debug!(deployment = %d.id, error = %e, "relational: sync_deployments upsert failed (non-fatal)");
        }
    }
    let live: std::collections::HashSet<String> = deps.iter().map(|d| d.id.to_string()).collect();
    let sel = format!("SELECT id FROM deployments WHERE node = {}", q(node));
    if let Ok(res) = exec(&mut s, &sel).await {
        for id in all_text_firsts(&res) {
            if !live.contains(&id) {
                let _ = exec(
                    &mut s,
                    &format!("DELETE FROM deployments WHERE id = {}", q(&id)),
                )
                .await;
            }
        }
    }
}

/// A `drive_nodes` row (one-level CRUD here; multi-segment path walking lives in `drive_api`).
/// `kind` is `"file"` or `"dir"`; `parent_id` is `""` for a top-level entry; `deleted_at` is a
/// tombstone (0 = live: a hard delete racing a rename is undefined under LWW); `owner_node` is the
/// node holding the bytes, which a request landing elsewhere proxies to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DriveNode {
    pub id: String,
    pub project: String,
    pub parent_id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub owner_user: String,
    #[serde(default)]
    pub owner_node: String,
    pub mtime: i64,
    pub ctime: i64,
    #[serde(default)]
    pub deleted_at: i64,
}

const DRIVE_NODE_COLUMNS: &str = "id, project, parent_id, name, kind, size_bytes, mime, \
     content_hash, owner_user, owner_node, mtime, ctime, deleted_at";

fn drive_node_from_row(v: serde_json::Value) -> Option<DriveNode> {
    serde_json::from_value(v).ok()
}

/// The live (non-tombstoned) child named `name` directly under `parent_id`.
pub(crate) async fn drive_resolve(project: &str, parent_id: &str, name: &str) -> Option<DriveNode> {
    let db = crate::guardian::sql_db().await.ok()?;
    let mut s = session(db).await;
    let query = format!(
        "SELECT {DRIVE_NODE_COLUMNS} FROM drive_nodes WHERE project = {} AND parent_id = {} AND name = {} AND deleted_at = 0",
        q(project),
        q(parent_id),
        q(name),
    );
    let res = exec(&mut s, &query).await.ok()?;
    rows_to_json_objects(&res)
        .into_iter()
        .find_map(drive_node_from_row)
}

/// A node by id regardless of tombstone state, so a caller holding an id (a share grant) can report
/// a deleted target as gone.
pub(crate) async fn drive_get(id: &str) -> Option<DriveNode> {
    let db = crate::guardian::sql_db().await.ok()?;
    let mut s = session(db).await;
    let query = format!(
        "SELECT {DRIVE_NODE_COLUMNS} FROM drive_nodes WHERE id = {}",
        q(id)
    );
    let res = exec(&mut s, &query).await.ok()?;
    rows_to_json_objects(&res)
        .into_iter()
        .find_map(drive_node_from_row)
}

/// Live children of `parent_id`: directories first, each group lexically ordered.
pub(crate) async fn drive_list_children(project: &str, parent_id: &str) -> Vec<DriveNode> {
    let Ok(db) = crate::guardian::sql_db().await else {
        return Vec::new();
    };
    let mut s = session(db).await;
    let query = format!(
        "SELECT {DRIVE_NODE_COLUMNS} FROM drive_nodes WHERE project = {} AND parent_id = {} AND deleted_at = 0 ORDER BY kind, name",
        q(project),
        q(parent_id),
    );
    let Ok(res) = exec(&mut s, &query).await else {
        return Vec::new();
    };
    rows_to_json_objects(&res)
        .into_iter()
        .filter_map(drive_node_from_row)
        .collect()
}

pub(crate) enum DriveWriteError {
    /// The target name exists and is a directory (file write) or a file
    /// (mkdir) — the two kinds can never occupy the same (parent, name) slot.
    WrongKind,
}

/// Upserts a file at `(project, parent_id, name)`: updates the live row in place (id preserved, so
/// share links survive an overwrite) or inserts one; last-write-wins.
pub(crate) async fn drive_put_file(
    project: &str,
    parent_id: &str,
    name: &str,
    size_bytes: u64,
    mime: &str,
    content_hash: &str,
    owner_user: &str,
    owner_node: &str,
) -> Result<DriveNode, DriveWriteError> {
    let now = hive_core::now_ms() as i64;
    let existing = drive_resolve(project, parent_id, name).await;
    if let Some(row) = &existing {
        if row.kind != "file" {
            return Err(DriveWriteError::WrongKind);
        }
    }
    let Ok(db) = crate::guardian::sql_db().await else {
        return Ok(DriveNode {
            id: existing
                .map(|e| e.id)
                .unwrap_or_else(|| format!("dnode_{}", uuid::Uuid::new_v4().simple())),
            project: project.into(),
            parent_id: parent_id.into(),
            name: name.into(),
            kind: "file".into(),
            size_bytes: size_bytes as i64,
            mime: mime.into(),
            content_hash: content_hash.into(),
            owner_user: owner_user.into(),
            owner_node: owner_node.into(),
            mtime: now,
            ctime: now,
            deleted_at: 0,
        });
    };
    let mut s = session(db).await;
    let id = existing
        .as_ref()
        .map(|e| e.id.clone())
        .unwrap_or_else(|| format!("dnode_{}", uuid::Uuid::new_v4().simple()));
    let ctime = existing.as_ref().map(|e| e.ctime).unwrap_or(now);
    let query = format!(
        "INSERT INTO drive_nodes ({DRIVE_NODE_COLUMNS}) VALUES ({}, {}, {}, {}, 'file', {}, {}, {}, {}, {}, {}, {}, 0) \
         ON CONFLICT (id) DO UPDATE SET size_bytes = excluded.size_bytes, mime = excluded.mime, \
         content_hash = excluded.content_hash, owner_user = excluded.owner_user, owner_node = excluded.owner_node, \
         mtime = excluded.mtime, deleted_at = 0",
        q(&id),
        q(project),
        q(parent_id),
        q(name),
        size_bytes,
        q(mime),
        q(content_hash),
        q(owner_user),
        q(owner_node),
        now,
        ctime,
    );
    let node = DriveNode {
        id,
        project: project.into(),
        parent_id: parent_id.into(),
        name: name.into(),
        kind: "file".into(),
        size_bytes: size_bytes as i64,
        mime: mime.into(),
        content_hash: content_hash.into(),
        owner_user: owner_user.into(),
        owner_node: owner_node.into(),
        mtime: now,
        ctime,
        deleted_at: 0,
    };
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(project, name, error = %e, "relational: drive_put_file failed (non-fatal)");
    }
    Ok(node)
}

/// Idempotent directory create: returns the live dir at the slot, errors
/// [`DriveWriteError::WrongKind`] if a file occupies it.
pub(crate) async fn drive_mkdir(
    project: &str,
    parent_id: &str,
    name: &str,
    owner_user: &str,
) -> Result<DriveNode, DriveWriteError> {
    if let Some(existing) = drive_resolve(project, parent_id, name).await {
        return if existing.kind == "dir" {
            Ok(existing)
        } else {
            Err(DriveWriteError::WrongKind)
        };
    }
    let now = hive_core::now_ms() as i64;
    let id = format!("dnode_{}", uuid::Uuid::new_v4().simple());
    let node = DriveNode {
        id: id.clone(),
        project: project.into(),
        parent_id: parent_id.into(),
        name: name.into(),
        kind: "dir".into(),
        size_bytes: 0,
        mime: String::new(),
        content_hash: String::new(),
        owner_user: owner_user.into(),
        owner_node: String::new(),
        mtime: now,
        ctime: now,
        deleted_at: 0,
    };
    if let Ok(db) = crate::guardian::sql_db().await {
        let mut s = session(db).await;
        let query = format!(
            "INSERT INTO drive_nodes ({DRIVE_NODE_COLUMNS}) VALUES ({}, {}, {}, {}, 'dir', 0, '', '', {}, '', {}, {}, 0)",
            q(&id),
            q(project),
            q(parent_id),
            q(name),
            q(owner_user),
            now,
            now,
        );
        if let Err(e) = exec(&mut s, &query).await {
            tracing::debug!(project, name, error = %e, "relational: drive_mkdir failed (non-fatal)");
        }
    }
    Ok(node)
}

/// Tombstones a file or directory; the API layer decides whether a directory must be empty first.
pub(crate) async fn drive_tombstone(id: &str) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    let query = format!(
        "UPDATE drive_nodes SET deleted_at = {} WHERE id = {}",
        hive_core::now_ms(),
        q(id),
    );
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(id, error = %e, "relational: drive_tombstone failed (non-fatal)");
    }
}

/// Re-parent/rename a node in place (id, and any share link against it,
/// unchanged).
pub(crate) async fn drive_move(id: &str, new_parent_id: &str, new_name: &str) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    let query = format!(
        "UPDATE drive_nodes SET parent_id = {}, name = {}, mtime = {} WHERE id = {}",
        q(new_parent_id),
        q(new_name),
        hive_core::now_ms(),
        q(id),
    );
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(id, error = %e, "relational: drive_move failed (non-fatal)");
    }
}

/// Sum of `size_bytes` over a project's live files (the quota read), summed in Rust rather than SQL
/// `SUM()` so it does not depend on the engine's aggregate numeric type.
pub(crate) async fn drive_bytes_used(project: &str) -> u64 {
    let Ok(db) = crate::guardian::sql_db().await else {
        return 0;
    };
    let mut s = session(db).await;
    let query = format!(
        "SELECT size_bytes FROM drive_nodes WHERE project = {} AND kind = 'file' AND deleted_at = 0",
        q(project),
    );
    let Ok(res) = exec(&mut s, &query).await else {
        return 0;
    };
    let mut total: u64 = 0;
    for r in &res {
        if let ExecResult::Rows { rows, .. } = r {
            for row in rows {
                let n = match row.first() {
                    Some(SqlValue::Int8(n)) => *n as u64,
                    Some(SqlValue::Int4(n)) => *n as u64,
                    _ => 0,
                };
                total += n;
            }
        }
    }
    total
}

/// A resolved `drive_shares` row; `scope` is stored but not projected (the only consumer,
/// `drive_api::share_get`, always reads).
pub(crate) struct DriveShare {
    pub node_id: String,
    pub expires_at: i64,
}

/// Mints a share grant against `node_id`, keyed by the SHA-256 of the capability token (hash, never
/// store: a share link is as sensitive as an API key).
pub(crate) async fn drive_share_mint(
    node_id: &str,
    token_hash: &str,
    scope: &str,
    expires_at: i64,
) -> String {
    let id = format!("dshare_{}", uuid::Uuid::new_v4().simple());
    if let Ok(db) = crate::guardian::sql_db().await {
        let mut s = session(db).await;
        let query = format!(
            "INSERT INTO drive_shares (id, node_id, token_hash, scope, expires_at, created_ms) VALUES ({}, {}, {}, {}, {}, {})",
            q(&id),
            q(node_id),
            q(token_hash),
            q(scope),
            expires_at,
            hive_core::now_ms(),
        );
        if let Err(e) = exec(&mut s, &query).await {
            tracing::debug!(node_id, error = %e, "relational: drive_share_mint failed (non-fatal)");
        }
    }
    id
}

/// Resolves a token hash to its grant regardless of expiry: `drive_api` alone decides expired vs
/// revoked vs valid.
pub(crate) async fn drive_share_resolve(token_hash: &str) -> Option<DriveShare> {
    let db = crate::guardian::sql_db().await.ok()?;
    let mut s = session(db).await;
    let query = format!(
        "SELECT node_id, expires_at FROM drive_shares WHERE token_hash = {}",
        q(token_hash),
    );
    let res = exec(&mut s, &query).await.ok()?;
    for r in &res {
        if let ExecResult::Rows { rows, .. } = r {
            if let Some(row) = rows.first() {
                let node_id = match row.first() {
                    Some(SqlValue::Text(s)) => s.clone(),
                    _ => continue,
                };
                let expires_at = match row.get(1) {
                    Some(SqlValue::Int8(n)) => *n,
                    Some(SqlValue::Int4(n)) => *n as i64,
                    _ => 0,
                };
                return Some(DriveShare {
                    node_id,
                    expires_at,
                });
            }
        }
    }
    None
}

/// Set (mint or rotate) the one WebDAV Basic-auth credential for `project`.
pub(crate) async fn drive_token_set(project: &str, token_hash: &str) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    let query = format!(
        "INSERT INTO drive_tokens (project, token_hash, created_ms) VALUES ({}, {}, {}) \
         ON CONFLICT (project) DO UPDATE SET token_hash = excluded.token_hash, created_ms = excluded.created_ms",
        q(project),
        q(token_hash),
        hive_core::now_ms(),
    );
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(project, error = %e, "relational: drive_token_set failed (non-fatal)");
    }
}

/// Constant-time verify a presented WebDAV Basic-auth token hash against
/// `project`'s stored one.
pub(crate) async fn drive_token_verify(project: &str, token_hash: &str) -> bool {
    let Ok(db) = crate::guardian::sql_db().await else {
        return false;
    };
    let mut s = session(db).await;
    let query = format!(
        "SELECT token_hash FROM drive_tokens WHERE project = {}",
        q(project),
    );
    let Ok(res) = exec(&mut s, &query).await else {
        return false;
    };
    for r in &res {
        if let ExecResult::Rows { rows, .. } = r {
            if let Some(SqlValue::Text(stored)) = rows.first().and_then(|row| row.first()) {
                let a = stored.as_bytes();
                let b = token_hash.as_bytes();
                if a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Column shape for `known_tables()` — plain data, not tied to `guardian_db`'s
/// own `SqlType` so the admin JSON response stays simple/stable.
pub(crate) struct SqlColumnInfo {
    pub name: &'static str,
    pub ty: &'static str,
}

pub(crate) struct SqlTableInfo {
    pub name: &'static str,
    pub columns: Vec<SqlColumnInfo>,
}

/// The exact tables `init_schema` creates, hand-maintained so the admin browser can never enumerate
/// anything this module did not create.
pub(crate) fn known_tables() -> Vec<SqlTableInfo> {
    let col = |name: &'static str, ty: &'static str| SqlColumnInfo { name, ty };
    vec![
        SqlTableInfo {
            name: "project_teams",
            columns: vec![
                col("project", "text"),
                col("team", "text"),
                col("root_dir", "text"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_accounts",
            columns: vec![
                col("tenant", "text"),
                col("plan", "text"),
                col("status", "text"),
                col("included_cents", "bigint"),
                col("used_cents", "bigint"),
                col("balance_cents", "bigint"),
                col("stripe_customer", "text"),
                col("stripe_subscription", "text"),
                col("period_start_ms", "bigint"),
                col("period_end_ms", "bigint"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_ledger",
            columns: vec![
                col("id", "text"),
                col("tenant", "text"),
                col("ts_ms", "bigint"),
                col("kind", "text"),
                col("amount_cents", "bigint"),
                col("balance_after_cents", "bigint"),
                col("note", "text"),
            ],
        },
        SqlTableInfo {
            name: "billing_invoices",
            columns: vec![
                col("id", "text"),
                col("tenant", "text"),
                col("number", "text"),
                col("plan", "text"),
                col("period_start_ms", "bigint"),
                col("period_end_ms", "bigint"),
                col("subtotal_cents", "bigint"),
                col("total_cents", "bigint"),
                col("status", "text"),
                col("created_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_invoice_lines",
            columns: vec![
                col("id", "text"),
                col("invoice_id", "text"),
                col("description", "text"),
                col("amount_cents", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_checkouts",
            columns: vec![
                col("id", "text"),
                col("tenant", "text"),
                col("kind", "text"),
                col("plan", "text"),
                col("amount_cents", "bigint"),
                col("sku", "text"),
                col("target", "text"),
                col("stripe_session_id", "text"),
                col("created_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_ledger_snapshot",
            columns: vec![
                col("tenant", "text"),
                col("ledger_json", "text"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_invoices_snapshot",
            columns: vec![
                col("tenant", "text"),
                col("invoices_json", "text"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "billing_backfill_state",
            columns: vec![
                col("tenant", "text"),
                col("accounts_migrated", "bigint"),
                col("ledger_rows_migrated", "bigint"),
                col("invoices_migrated", "bigint"),
                col("invoice_lines_migrated", "bigint"),
                col("migrated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "teams",
            columns: vec![
                col("slug", "text"),
                col("name", "text"),
                col("plan", "text"),
                col("member_count", "bigint"),
                col("sso_enabled", "bigint"),
                col("created_ms", "bigint"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "team_members",
            columns: vec![
                col("id", "text"),
                col("team", "text"),
                col("email", "text"),
                col("name", "text"),
                col("role", "text"),
                col("added_ms", "bigint"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "deployments",
            columns: vec![
                col("id", "text"),
                col("project", "text"),
                col("team", "text"),
                col("node", "text"),
                col("alias", "text"),
                col("kind", "text"),
                col("target", "text"),
                col("state", "text"),
                col("production", "bigint"),
                col("created_at_ms", "bigint"),
                col("updated_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "drive_nodes",
            columns: vec![
                col("id", "text"),
                col("project", "text"),
                col("parent_id", "text"),
                col("name", "text"),
                col("kind", "text"),
                col("size_bytes", "bigint"),
                col("mime", "text"),
                col("content_hash", "text"),
                col("owner_user", "text"),
                col("owner_node", "text"),
                col("mtime", "bigint"),
                col("ctime", "bigint"),
                col("deleted_at", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "drive_shares",
            columns: vec![
                col("id", "text"),
                col("node_id", "text"),
                col("token_hash", "text"),
                col("scope", "text"),
                col("expires_at", "bigint"),
                col("created_ms", "bigint"),
            ],
        },
        SqlTableInfo {
            name: "drive_tokens",
            columns: vec![
                col("project", "text"),
                col("token_hash", "text"),
                col("created_ms", "bigint"),
            ],
        },
    ]
}

/// Keywords rejected ANYWHERE in an admin query (whole-token match, so a column like `updated_by`
/// passes): blocks `SELECT 1; DROP ...` and data-modifying CTEs.
const BLOCKED_SQL_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "TRUNCATE", "CREATE", "GRANT", "REVOKE",
    "REPLACE", "MERGE", "CALL", "EXEC", "EXECUTE", "COPY", "VACUUM",
];

/// `Err(reason)` unless `sql` is a single read-only statement: the ONLY gate between typed admin
/// input and a live `Session::execute`, reject-by-default (a false accept is a data-loss path, a
/// false reject only an error).
fn reject_unless_readonly(sql: &str) -> Result<(), String> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err("empty query".into());
    }
    let upper = trimmed.to_uppercase();
    let first_token = upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .find(|s| !s.is_empty())
        .unwrap_or("");
    if first_token != "SELECT" && first_token != "WITH" {
        return Err("only SELECT (or a read-only WITH ... SELECT) is allowed here".into());
    }
    for tok in upper.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if BLOCKED_SQL_KEYWORDS.contains(&tok) {
            return Err(format!("'{tok}' is not allowed in this read-only query"));
        }
    }
    let body = trimmed.trim_end_matches(';').trim_end();
    if body.contains(';') {
        return Err("only a single statement is allowed".into());
    }
    Ok(())
}

fn sql_value_to_json(v: &SqlValue) -> serde_json::Value {
    match v {
        SqlValue::Null => serde_json::Value::Null,
        SqlValue::Bool(b) => serde_json::Value::Bool(*b),
        SqlValue::Int2(n) => serde_json::json!(n),
        SqlValue::Int4(n) => serde_json::json!(n),
        SqlValue::Int8(n) => serde_json::json!(n),
        SqlValue::Float4(n) => serde_json::json!(n),
        SqlValue::Float8(n) => serde_json::json!(n),
        SqlValue::Text(s) => serde_json::Value::String(s.clone()),
        other => {
            if let Some(s) = other.as_str() {
                serde_json::Value::String(s.to_string())
            } else if let Some(n) = other.as_f64() {
                serde_json::json!(n)
            } else if let Some(b) = other.as_bool() {
                serde_json::Value::Bool(b)
            } else {
                serde_json::Value::String(format!("{other:?}"))
            }
        }
    }
}

/// Runs an admin-submitted read-only query and shapes `{fields, rows}` for the "view as PostgreSQL"
/// browser; re-checks `reject_unless_readonly` itself, so a missing call site can never become a
/// mutation path.
pub(crate) async fn run_readonly_query(sql: &str) -> Result<serde_json::Value, String> {
    reject_unless_readonly(sql)?;
    let sql = sql.to_string();
    let started = std::time::Instant::now();
    let out = crate::bulkhead::spawn(async move { run_readonly_query_inner(&sql).await })
        .await
        .map_err(|e| format!("relational query task failed: {e}"))?;
    if out.is_err() {
        tracing::warn!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "admin relational query failed"
        );
    }
    out
}

/// The body of [`run_readonly_query`], run on the bulkhead runtime.
async fn run_readonly_query_inner(sql: &str) -> Result<serde_json::Value, String> {
    reject_unless_readonly(sql)?;
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("relational store unavailable: {e}"))?;
    let mut s = session(db).await;
    if !index_ready() {
        return Err(
            "relational index not built yet (the first full walk of the namespace has not \
             completed in this process); every table would read as missing -- retry shortly"
                .to_string(),
        );
    }
    let results = exec(&mut s, sql).await?;
    for r in &results {
        if let ExecResult::Rows { fields, rows } = r {
            let field_names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
            let json_rows: Vec<Vec<serde_json::Value>> = rows
                .iter()
                .map(|row| row.iter().map(sql_value_to_json).collect())
                .collect();
            return Ok(serde_json::json!({ "fields": field_names, "rows": json_rows }));
        }
    }
    Ok(
        serde_json::json!({ "fields": Vec::<String>::new(), "rows": Vec::<Vec<serde_json::Value>>::new() }),
    )
}

/// `{column: value}` objects keyed by the query's own column names, so each SELECT list is the
/// single source of its output shape.
fn rows_to_json_objects(res: &[ExecResult]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for r in res {
        if let ExecResult::Rows { fields, rows } = r {
            for row in rows {
                let mut obj = serde_json::Map::new();
                for (f, v) in fields.iter().zip(row.iter()) {
                    obj.insert(f.name.clone(), sql_value_to_json(v));
                }
                out.push(serde_json::Value::Object(obj));
            }
        }
    }
    out
}

/// Budget for ONE user-facing billing mirror read (`HIVE_BILLING_SQL_READ_MS`, default 1500 ms):
/// far below `SQL_OP_TIMEOUT` because this is a request path; a miss serves the authority's or the
/// in-process billing state instead.
fn billing_read_budget() -> std::time::Duration {
    static T: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("HIVE_BILLING_SQL_READ_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_millis(1500))
    })
}

/// Rate-limited (30 s) WARN for a billing mirror read that missed its budget.
fn warn_billing_read_slow(budget: std::time::Duration) {
    use std::sync::atomic::Ordering;
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = wall_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 30_000
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            budget_ms = budget.as_millis() as u64,
            "relational: billing mirror read missed its budget -- serving the in-process \
             billing state instead (the guardian namespace may be wedged; see SQL_OP_TIMEOUT)"
        );
    }
}

/// Outcome of `billing_snapshot`; the cases are not interchangeable: `NotMirrored` is a real
/// absence, `Unavailable` means "could not tell", and a failed read must never be answered as `[]`
/// for a tenant that has history.
pub(crate) enum BillingSnapshot {
    /// The tenant IS mirrored here; each part is `None` when that part's read failed or missed the
    /// budget (an invoice whose lines cannot be read is withheld, never served with zero lines).
    Mirrored {
        ledger: Option<String>,
        invoices: Option<String>,
    },
    /// No `billing_accounts` row for this tenant on this node.
    NotMirrored,
    /// The DB layer is down, or did not answer inside `billing_read_budget()`.
    Unavailable,
}

/// Fleet-replicated billing read, rebuilt from the per-row tables into the JSON shapes the billing
/// types serialize to, bounded by [`billing_read_budget`] (a miss is `Unavailable`; callers fall
/// back to `admin::proxy_billing_read`, then the in-process store). Mirrored iff the tenant's
/// `billing_accounts` row exists, so an empty ledger is `Some("[]")`; `invoices` are FINALIZED only
/// (`admin::billing_invoices` appends the draft).
pub(crate) async fn billing_snapshot(tenant: &str) -> BillingSnapshot {
    let budget = billing_read_budget();
    match tokio::time::timeout(budget, billing_snapshot_uncapped(tenant)).await {
        Ok(v) => v,
        Err(_) => {
            warn_billing_read_slow(budget);
            BillingSnapshot::Unavailable
        }
    }
}

async fn billing_snapshot_uncapped(tenant: &str) -> BillingSnapshot {
    let Ok(db) = crate::guardian::sql_db().await else {
        return BillingSnapshot::Unavailable;
    };
    if !index_ready() {
        return BillingSnapshot::Unavailable;
    }
    let mut s = session(db).await;

    let acc_q = format!(
        "SELECT tenant, plan, status, included_cents, used_cents, balance_cents, \
         stripe_customer, stripe_subscription, period_start_ms, period_end_ms, updated_ms \
         FROM billing_accounts WHERE tenant = {}",
        q(tenant)
    );
    let account = match exec(&mut s, &acc_q).await {
        Ok(r) => rows_to_json_objects(&r)
            .into_iter()
            .next()
            .map(|v| v.to_string()),
        Err(_) => return BillingSnapshot::Unavailable,
    };
    if account.is_none() {
        return BillingSnapshot::NotMirrored;
    }

    let ledger_q = format!(
        "SELECT id, tenant, ts_ms, kind, amount_cents, balance_after_cents, note \
         FROM billing_ledger WHERE tenant = {} ORDER BY ts_ms DESC",
        q(tenant)
    );
    let ledger = match exec(&mut s, &ledger_q).await {
        Ok(r) => Some(serde_json::Value::Array(rows_to_json_objects(&r)).to_string()),
        Err(_) => None,
    };

    let inv_q = format!(
        "SELECT id, number, plan, period_start_ms, period_end_ms, subtotal_cents, total_cents, status, created_ms, \
         ledger_commitment, ledger_entry_count, ledger_pruned \
         FROM billing_invoices WHERE tenant = {} ORDER BY period_start_ms DESC",
        q(tenant)
    );
    let invoice_rows = match exec(&mut s, &inv_q).await {
        Ok(r) => rows_to_json_objects(&r),
        Err(_) => return BillingSnapshot::Mirrored {
            ledger,
            invoices: None,
        },
    };
    let mut invoices_out = Vec::with_capacity(invoice_rows.len());
    for mut inv in invoice_rows {
        let id = inv
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let lines_q = format!(
            "SELECT description, amount_cents FROM billing_invoice_lines WHERE invoice_id = {} ORDER BY id",
            q(&id)
        );
        match exec(&mut s, &lines_q).await {
            Ok(r) => {
                let lines = rows_to_json_objects(&r);
                if let Some(obj) = inv.as_object_mut() {
                    obj.insert(
                        "tenant".into(),
                        serde_json::Value::String(tenant.to_string()),
                    );
                    obj.insert("lines".into(), serde_json::Value::Array(lines));
                }
            }
            Err(_) => return BillingSnapshot::Mirrored {
                ledger,
                invoices: None,
            },
        }
        invoices_out.push(inv);
    }
    BillingSnapshot::Mirrored {
        ledger,
        invoices: Some(serde_json::Value::Array(invoices_out).to_string()),
    }
}
