//! Fleet-consistent state via guardian-db's native relational/SQL layer
//! (`guardian_db::sql` — in-process, zero network hop; NOT the pgwire wire
//! protocol, which is never enabled — see `Cargo.toml`'s guardian-db feature
//! comment). Every node opens the same named database ("hive") against the
//! same shared, already relay-patched `GuardianDB` instance `guardian::handle`
//! uses; guardian-db's own iroh-docs CRDT replication converges every node's
//! local copy of every table within seconds of a write anywhere in the fleet.
//!
//! Scope: `ProjectStore` and `BillingStore` — both confirmed node-local with
//! zero live cross-node merge (the Simpfi/drugs-wtf visibility bug and the
//! 5-way billing-state divergence bug). Both have RARE writes (a project
//! settings change, or the once-a-minute metering tick) and need
//! fleet-consistent reads — exactly what this layer buys. `MetricsStore` is
//! deliberately NOT here: its writes fire on every request (a hot path), and
//! migrating those onto CRDT-replicated storage would be a real performance
//! regression for no correctness gain — its fix is the fleet-fan-out-on-READ
//! in `metrics_get` instead (see admin.rs).
//!
//! Writes still funnel through the EXISTING single-writer discipline
//! (`admin_ingress` forwards every mutation to the control-plane leader;
//! `spawn_billing_meter_loop` metering ticks only fire on the elected
//! billing leader) — this layer adds fleet-wide REPLICATED READS on top of
//! that, not a new write-consistency model. guardian-db's own docs describe
//! its default mode as local-first / eventually-consistent CRDT (LWW) —
//! single-writer sidesteps that risk entirely by construction (there is
//! never a second concurrent writer to conflict with).
//!
//! Additive only: every existing store (`ProjectStore`, `BillingStore`) and
//! its on-disk/guardian-KV backup (`persist.rs`, `guardian::replicate`) is
//! completely unchanged and remains the source of truth for local reads and
//! for the process's own in-memory state. This module is a best-effort
//! fleet-replicated CACHE layered alongside it: a failure here never blocks
//! the mutation it mirrors (same "durability/consistency on top, never on
//! the critical path" convention as `guardian::replicate`).

use guardian_db::sql::{ExecResult, Session, SqlValue};

use crate::guardian::SqlDb;

/// Idempotent schema bring-up — safe to call on every boot (`CREATE TABLE IF
/// NOT EXISTS`), PLUS an unconditional `billing_accounts` column
/// reconciliation (`reconcile_billing_accounts_schema`) every single call —
/// see that function's doc comment for why `CREATE TABLE IF NOT EXISTS`
/// alone is not enough for that one table. Best-effort: GuardianDB may not
/// be ready yet (same init-race as every other guardian.rs caller); a
/// failure here just means the fleet fallback is unavailable until the next
/// successful call, never a boot failure.
pub(crate) async fn init_schema() {
    let Ok(db) = crate::guardian::sql_db().await else {
        tracing::debug!("relational: GuardianDB not ready yet; schema bring-up deferred");
        return;
    };
    let mut session = Session::new(db, "hive-init");
    for (table, ddl) in [
        (
            "project_teams",
            "CREATE TABLE IF NOT EXISTS project_teams (\
            project TEXT PRIMARY KEY, \
            team TEXT NOT NULL, \
            root_dir TEXT NOT NULL DEFAULT '', \
            updated_ms BIGINT NOT NULL\
        )",
        ),
        // Normalized (real columns, one row per fact) — replaces the old
        // single-JSON-blob-per-tenant `account_json`. See `upsert_billing`.
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
            stripe_session_id TEXT NOT NULL DEFAULT '', \
            created_ms BIGINT NOT NULL\
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
    ] {
        ensure_table_exists(&mut session, table, ddl).await;
    }
    // REMOVAL (not just deprecation): `billing_ledger_snapshot` /
    // `billing_invoices_snapshot` were the old per-tenant single-JSON-blob
    // tables, fully superseded by the real per-row `billing_ledger` /
    // `billing_invoices` + `billing_invoice_lines` tables above -- nothing
    // in the codebase writes OR reads them anymore on the live path (the
    // admin `billing_ledger`/`billing_invoices` handlers were migrated to
    // the new tables; see `admin.rs`). `DROP TABLE IF EXISTS` is idempotent
    // -- a genuine no-op on a node that already dropped these on a prior
    // boot, and a real drop on any node still carrying the legacy table
    // from before this change. Run unconditionally on every boot, same as
    // the DDL loop above, so this converges fleet-wide without a manual
    // operator step per node.
    for table in ["billing_ledger_snapshot", "billing_invoices_snapshot"] {
        if let Err(e) = exec(&mut session, &format!("DROP TABLE IF EXISTS {table}")).await {
            tracing::warn!(table, error = %e, "relational: drop of deprecated snapshot table failed (non-fatal, will retry next boot)");
        }
    }
    // `CREATE TABLE IF NOT EXISTS billing_accounts` above is a genuine no-op
    // against the OLD 3-column shape every real production node already has
    // (see `reconcile_billing_accounts_schema`'s doc comment for the full
    // finding) — reconcile it unconditionally, on every boot, before any
    // `upsert_billing`/`billing_snapshot` call is ever possible. This makes
    // correct schema a boot-time guarantee rather than an operator-remembered
    // manual step (previously this only ran inside the admin-triggered
    // `backfill_billing_normalize` endpoint).
    reconcile_billing_accounts_schema(&mut session).await;
}

/// Attempts to bring up a single table before giving up and logging loudly —
/// see `ensure_table_exists`'s doc comment for why a `CREATE TABLE IF NOT
/// EXISTS` reporting `Ok` is not sufficient proof the table actually exists.
const SCHEMA_BRINGUP_MAX_ATTEMPTS: u32 = 4;

/// Run `ddl` (a `CREATE TABLE IF NOT EXISTS ...` statement for `table`) and
/// then VERIFY the table actually exists afterward — retrying (re-running the
/// DDL, then re-verifying) if it doesn't, up to `SCHEMA_BRINGUP_MAX_ATTEMPTS`
/// times, before logging loudly and giving up.
///
/// LIVE-WITNESSED ROOT CAUSE this closes: on a genuinely fresh/empty
/// GuardianDB, `billing_ledger` silently failed to come into existence with
/// NO warning logged at all — not a swallowed `Err`, the `if let Err` branch
/// never fired, because `exec()` genuinely returned `Ok` for that statement.
/// Root-caused by reading `vendor/guardian-db/src/sql/engine.rs` +
/// `guardian_storage.rs`: guardian-db's SQL catalog is a SINGLE JSON document
/// (`GuardianRelationalStorage::{load_catalog,save_catalog}`, keyed
/// `__gdb_sql_catalog/catalog`) that every autocommit statement reads whole
/// (`Session::execute_inner`'s `None => self.load_catalog().await?` when not
/// inside an explicit transaction), mutates in memory, and writes back whole
/// (`if catalog_dirty { self.save_catalog(&new_catalog).await? }`) — with NO
/// compare-and-swap or lock around that read-modify-write pair. `init_schema`
/// runs its DDL loop on ONE `Session`, sequentially — but main.rs boots other
/// loops concurrently (`spawn_billing_meter_loop` / `spawn_relational_mirror_loop`)
/// that open their OWN, separate `Session`s (this module's `session()`
/// helper) and can issue their own autocommit DDL at the same time (e.g.
/// `upsert_billing`'s unconditional `reconcile_billing_accounts_schema` ->
/// `ALTER TABLE billing_accounts ADD COLUMN ...`, which is `catalog_dirty`
/// too). If that concurrent session's `load_catalog()` snapshots the catalog
/// just BEFORE this loop's `CREATE TABLE billing_ledger` persists, and its
/// own `save_catalog()` lands just AFTER, its stale-based whole-document
/// overwrite silently ERASES the just-created table — a lost-update race,
/// not a syntax error or a timeout. Every individual statement in BOTH
/// sessions genuinely returned `Ok`, so `exec()`'s `if let Err` never had
/// anything to catch and nothing was ever logged. `CREATE TABLE IF NOT
/// EXISTS` itself can't detect this (it only ever sees its OWN session's
/// view), so the only reliable defense is to check afterward, from a freshly
/// reloaded session view, whether the table is REALLY there — which is what
/// this function does.
async fn ensure_table_exists(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
    table: &str,
    ddl: &str,
) {
    for attempt in 1..=SCHEMA_BRINGUP_MAX_ATTEMPTS {
        if let Err(e) = exec(s, ddl).await {
            tracing::warn!(table, attempt, error = %e, "relational: schema bring-up CREATE TABLE statement failed");
        }
        // Verify against a freshly reloaded catalog view (every autocommit
        // statement reloads the catalog — see this fn's doc comment): a
        // trivial SELECT only succeeds if the table is genuinely present,
        // unlike trusting the CREATE TABLE call's own `Ok` result.
        match exec(s, &format!("SELECT 1 FROM {table} LIMIT 1")).await {
            Ok(_) => return,
            Err(e) if attempt == SCHEMA_BRINGUP_MAX_ATTEMPTS => {
                tracing::error!(
                    table,
                    attempts = SCHEMA_BRINGUP_MAX_ATTEMPTS,
                    error = %e,
                    "relational: schema bring-up could not verify this table exists after \
                     repeated CREATE TABLE + retry — writes against it (e.g. upsert_billing's \
                     single atomic transaction) will keep silently failing until this is fixed"
                );
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

/// One-time boot backfill: mirror THIS node's existing `ProjectStore` snapshot
/// into the relational table. Without this, a project created BEFORE this
/// migration shipped (e.g. `drugs-wtf`) would never appear in the mirror —
/// `set_project_team` only fires on the NEXT mutation, and most projects'
/// team tag is set once at creation and never touched again. Safe to run on
/// EVERY node unconditionally (unlike billing — see below): the upsert is
/// idempotent and every node's local row for a given project is, by
/// construction, a copy of the same project's data, so a multi-node backfill
/// race converges to the same correct value, never a conflict.
///
/// Billing is deliberately NOT backfilled here: only the CURRENTLY-elected
/// billing authority's local `BillingStore` is correct/live (every other
/// node's is stale-by-design, only ever bootstrapped from a peer snapshot at
/// boot) — backfilling from a non-authority node would overwrite the mirror
/// with wrong data. Billing self-heals instead via `spawn_billing_meter_loop`,
/// which mirrors every actively-metered tenant on every tick (default 60s)
/// from whichever node is actually authoritative at the time.
pub(crate) async fn backfill_projects(projects: Vec<(String, String, String)>) {
    for (project, team, root_dir) in projects {
        set_project_team(&project, &team, &root_dir).await;
    }
}

/// SQL single-quote escaping for a hand-built literal (doubles embedded `'`,
/// the SQL-standard escape). Every caller in this module passes
/// already-constrained identifiers (project/team slugs, tenant ids) or our
/// own `serde_json`-serialized JSON — never raw, unvalidated user SQL — so
/// this (not a full parameter-binding API, which `Session` does not expose
/// today) is a safe, standard, minimal-surface approach for this internal,
/// non-user-facing use.
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

/// A `Session` over a freshly re-synced local index. CRITICAL: guardian-db's
/// own `GuardianRelationalStorage::refresh` doc comment states plainly that
/// "the relational engine reads the DocumentStore's synchronous local index;
/// that index updates on local writes and on `load`/`sync`, but NOT
/// automatically when documents arrive from peers in the background" — so
/// without this call, a node whose `Database` handle was opened before a
/// remote peer's write replicated in would serve a permanently-stale read
/// (live-witnessed: fc-virginia's `/v1/gitops/projects` stayed empty for
/// drugs-wtf even after every other node's backfill had long since written
/// it, while fc-hongkong/fc-lax happened to read correctly). Cheap: this
/// re-scans the LOCAL already-replicated doc index, not a network round-trip
/// (the actual iroh-docs replication runs continuously in the background
/// regardless of this call).
async fn session(db: SqlDb) -> Session<guardian_db::sql::GuardianRelationalStorage> {
    match tokio::time::timeout(SQL_OP_TIMEOUT, db.storage().refresh()).await {
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "relational: index refresh failed (serving from the previous local index)")
        }
        Err(_) => tracing::warn!(
            timeout_secs = SQL_OP_TIMEOUT.as_secs(),
            "relational: index refresh timed out (serving from the previous local index) -- \
             a fresh node's first refresh on a corrupted/interrupted namespace can hang \
             indefinitely with no error at all; see run_readonly_query's own timeout for the \
             matching guard on the query side"
        ),
        Ok(Ok(())) => {}
    }
    Session::new(db, "hive")
}

/// Bound on a single guardian-db SQL operation (index refresh or a query
/// execute). Live-witnessed on a freshly-joined node (fc-sanjose-2): a
/// corrupted/interrupted first-open of the "hive" relational namespace made
/// `Session::execute`/`storage().refresh()` hang SILENTLY FOREVER -- near-zero
/// CPU, connection established, no error, no log line -- while every other
/// guardian-touching endpoint (KV reads, `/v1/nodes`) stayed instant. Unlike
/// the KV path's `guardian::handle()` (which has its own `GUARDIAN_INIT_TIMEOUT`
/// specifically because of this failure class), this SQL/relational path had
/// no bound at all. A timeout turns an indefinite hang into a real, fast
/// error a caller can log/retry/surface instead of an admin request (or a
/// background loop iteration) that never returns.
const SQL_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Execute one statement with `SQL_OP_TIMEOUT` bound, flattening the
/// timeout and the inner guardian-db error into a single `Result<_, String>`
/// so every existing call site (`?`, `if let Err`, `.ok()`) keeps working
/// unchanged.
async fn exec(
    s: &mut Session<guardian_db::sql::GuardianRelationalStorage>,
    sql: &str,
) -> Result<Vec<ExecResult>, String> {
    match tokio::time::timeout(SQL_OP_TIMEOUT, s.execute(sql)).await {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("guardian sql execute timed out after {SQL_OP_TIMEOUT:?} (namespace likely wedged from an interrupted first-open; restart or reset this node's guardian data)")),
    }
}

// ---------------------------------------------------------------------------
// Projects (the durable fix for the Simpfi/drugs-wtf visibility gap)
// ---------------------------------------------------------------------------

/// Upsert a project's team tag + root dir. Called from `ProjectStore::set_team`
/// (the mutation that actually changes ownership) so every node's replicated
/// copy converges regardless of which node the project was created/built on —
/// the durable fix for a project-with-no-deployment being invisible on any
/// node lacking its local `ProjectSettings` row (`drugs-wtf`'s exact bug:
/// zero deployments fleet-wide, so the peer_deployments-derived fallback in
/// `gitops_projects` can't help it).
pub(crate) async fn set_project_team(project: &str, team: &str, root_dir: &str) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    let query = format!(
        "INSERT INTO project_teams (project, team, root_dir, updated_ms) VALUES ({}, {}, {}, {}) \
         ON CONFLICT (project) DO UPDATE SET team = excluded.team, root_dir = excluded.root_dir, updated_ms = excluded.updated_ms",
        q(project),
        q(team),
        q(root_dir),
        hive_core::now_ms(),
    );
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(project, error = %e, "relational: set_project_team failed (non-fatal, fleet-fallback unavailable for this project until retried)");
    }
}

/// Forget a project's relational row (mirrors `ProjectStore::remove`).
pub(crate) async fn remove_project(project: &str) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let mut s = session(db).await;
    let query = format!("DELETE FROM project_teams WHERE project = {}", q(project));
    if let Err(e) = exec(&mut s, &query).await {
        tracing::debug!(project, error = %e, "relational: remove_project failed (non-fatal)");
    }
}

/// Every (project, team) pair this node's replica currently holds for `team`
/// — backs `gitops_projects`'s fleet-visibility fallback (and any future
/// "list every project for team X" surface) with a source that includes
/// projects with zero deployments anywhere, unlike the peer_deployments-only
/// fallback.
pub(crate) async fn projects_for_team(team: &str) -> Vec<String> {
    let Ok(db) = crate::guardian::sql_db().await else {
        return Vec::new();
    };
    let mut s = session(db).await;
    let query = format!(
        "SELECT project, team FROM project_teams WHERE team = {}",
        q(team)
    );
    let Ok(res) = exec(&mut s, &query).await else {
        return Vec::new();
    };
    all_text_pairs(&res).into_iter().map(|(p, _)| p).collect()
}

// ---------------------------------------------------------------------------
// Billing (fleet-consistent reads on top of the existing single-writer meter)
// ---------------------------------------------------------------------------

/// Upsert a tenant's billing account, ledger entries, finalized invoices
/// (+ their line items), and checkouts as REAL per-row tables — no more
/// pre-serialized JSON blobs. Called alongside every `BillingStore` mutation
/// on whichever node is CURRENTLY the billing authority (the elected
/// metering leader — see `admin::billing_authority_node`) so every other
/// node's local replica converges to the SAME account state within seconds,
/// instead of staying empty/stale until the next full-snapshot boot
/// bootstrap. Best-effort, same as `set_project_team` — a failure here never
/// blocks the real mutation; the existing HTTP proxy-to-leader fix
/// (`admin::proxy_billing_read`) remains the safety net if this mirror is
/// ever behind/unavailable.
///
/// ATOMICITY: the entire per-tenant write (account row + every ledger row +
/// every invoice row + every invoice-line row + every checkout row) is a
/// SINGLE `Session::execute` call over one `BEGIN; ...; COMMIT;` string.
/// guardian-db's engine buffers every statement inside an explicit
/// transaction into `Transaction::overlay` and applies it to storage
/// atomically only at `COMMIT` (see `vendor/guardian-db/src/sql/engine.rs`'s
/// `begin`/`commit`) — a mid-write error aborts the transaction (the failing
/// statement flips `txn.aborted`, every statement after it short-circuits
/// with `SqlError::InFailedTransaction`, and `execute`'s `?` propagates out
/// before `COMMIT` ever runs), so nothing partially applies. This closes the
/// previous gap where 3 separate autocommit statements could leave the
/// account row updated with stale/missing ledger or invoice rows on a
/// mid-write failure. Also `SQL_OP_TIMEOUT`-bounded via `exec()`, which the
/// old 3-autocommit-statement version bypassed entirely (a separate,
/// pre-existing gap fixed here too).
///
/// Draft invoices are filtered out defensively (`status != "paid"` is
/// skipped) even though callers should already only ever pass finalized
/// invoices (`BillingStore::finalized_invoices`) — a draft is transient,
/// computed on-the-fly by `build_invoice`/`current_invoice`, and must NEVER
/// be written as a row.
///
/// SCHEMA SAFETY NET: calls `reconcile_billing_accounts_schema` on every
/// invocation, before the transaction below, using this same `Session` —
/// not just relying on `init_schema()`'s one-shot boot-time call.
/// `init_schema()` runs exactly once, fire-and-forget, at boot
/// (`main.rs`'s `tokio::spawn`) with no retry; if `guardian::sql_db()` fails
/// (or wedges) on that single attempt, `billing_accounts` never gets its
/// `ALTER TABLE ADD COLUMN` reconciliation for this node's entire uptime,
/// while `spawn_billing_meter_loop` / `spawn_relational_mirror_loop` keep
/// calling this function on their own retry cadence regardless — silently
/// failing every write against the unreconciled table. Doing it here instead
/// means schema reconciliation inherits those loops' existing retry
/// semantics for free: the next time this runs after guardian recovers, the
/// schema gets fixed as a side effect. Memoized (see
/// `BILLING_ACCOUNTS_SCHEMA_RECONCILED`) so this is a cheap no-op on every
/// call after the first genuine success.
pub(crate) async fn upsert_billing(
    account: &crate::billing::BillingAccount,
    ledger: &[crate::billing::LedgerEntry],
    invoices: &[crate::billing::Invoice],
    checkouts: &[crate::billing::Checkout],
) {
    let Ok(db) = crate::guardian::sql_db().await else {
        return;
    };
    let now = hive_core::now_ms();
    let mut s = session(db).await;
    reconcile_billing_accounts_schema(&mut s).await;

    let mut sql = String::from("BEGIN;");
    sql.push_str(&build_account_sql(account, now));
    sql.push_str(&build_ledger_sql(ledger));
    sql.push_str(&build_invoices_sql(invoices));
    sql.push_str(&build_checkouts_sql(checkouts));
    sql.push_str("COMMIT;");

    if let Err(e) = exec(&mut s, &sql).await {
        tracing::debug!(
            tenant = %account.tenant,
            error = %e,
            "relational: upsert_billing transaction failed (non-fatal, HTTP proxy-to-leader remains the \
             fallback — and since this ran as ONE transaction, the failure rolled back atomically: no \
             partial account/ledger/invoice/checkout row state was left behind)"
        );
    }
}

/// SQL for the account row alone (`ON CONFLICT (tenant) DO UPDATE`, so it
/// converges whether or not a row already exists for this tenant — including
/// a pre-existing OLD-shape row predating the `billing_accounts` DDL rewrite,
/// since that rewrite only ever ADDS the new scalar columns onto the
/// existing table — see `backfill_billing_normalize` — rather than
/// dropping/recreating it). Factored out of `upsert_billing` so
/// `backfill_billing_normalize` can reuse the exact same statement shape.
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

/// SQL for every ledger row (`ON CONFLICT (id) DO UPDATE` — deterministic
/// entry ids, so a re-upsert/backfill converges instead of duplicating).
/// Factored out of `upsert_billing` — see `build_account_sql`.
fn build_ledger_sql(ledger: &[crate::billing::LedgerEntry]) -> String {
    let mut sql = String::new();
    for e in ledger {
        sql.push_str(&format!(
            "INSERT INTO billing_ledger (id, tenant, ts_ms, kind, amount_cents, balance_after_cents, note) \
             VALUES ({}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (id) DO UPDATE SET tenant = excluded.tenant, ts_ms = excluded.ts_ms, \
             kind = excluded.kind, amount_cents = excluded.amount_cents, \
             balance_after_cents = excluded.balance_after_cents, note = excluded.note;",
            q(&e.id),
            q(&e.tenant),
            e.ts_ms,
            q(&e.kind),
            e.amount_cents,
            e.balance_after_cents,
            q(&e.note),
        ));
    }
    sql
}

/// SQL for every FINALIZED invoice (+ its line items) — a draft
/// (`status != "paid"`) is always skipped, never persisted as a row (see
/// `upsert_billing`'s original doc comment on why: a draft is transient,
/// computed on-the-fly, and must never become a stored fact). Factored out
/// of `upsert_billing` — see `build_account_sql`.
fn build_invoices_sql(invoices: &[crate::billing::Invoice]) -> String {
    let mut sql = String::new();
    for inv in invoices {
        if inv.status != "paid" {
            continue;
        }
        sql.push_str(&format!(
            "INSERT INTO billing_invoices (id, tenant, number, plan, period_start_ms, period_end_ms, \
             subtotal_cents, total_cents, status, created_ms) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (id) DO UPDATE SET tenant = excluded.tenant, number = excluded.number, \
             plan = excluded.plan, period_start_ms = excluded.period_start_ms, \
             period_end_ms = excluded.period_end_ms, subtotal_cents = excluded.subtotal_cents, \
             total_cents = excluded.total_cents, status = excluded.status, created_ms = excluded.created_ms;",
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
        ));
        // Stable per-line id: parent invoice id + its zero-padded index in
        // `lines` (e.g. "in_abc123-000", "in_abc123-001") — deterministic
        // across ticks so a re-upsert of the same invoice converges to the
        // same rows instead of accumulating duplicates. Zero-padded to 3
        // digits (not just "-0", "-1") so `ORDER BY id` (relational.rs,
        // billing_snapshot's per-invoice lines query) sorts lines correctly
        // as strings — unpadded indices sort "…-10" before "…-2"
        // lexicographically once an invoice has 10+ lines. 3 digits covers
        // up to 999 lines, comfortably beyond any realistic invoice.
        for (idx, line) in inv.lines.iter().enumerate() {
            let line_id = format!("{}-{:03}", inv.id, idx);
            sql.push_str(&format!(
                "INSERT INTO billing_invoice_lines (id, invoice_id, description, amount_cents) \
                 VALUES ({}, {}, {}, {}) \
                 ON CONFLICT (id) DO UPDATE SET invoice_id = excluded.invoice_id, \
                 description = excluded.description, amount_cents = excluded.amount_cents;",
                q(&line_id),
                q(&inv.id),
                q(&line.description),
                line.amount_cents,
            ));
        }
    }
    sql
}

/// SQL for every checkout row (`ON CONFLICT (id) DO UPDATE`). Factored out
/// of `upsert_billing` — see `build_account_sql`.
fn build_checkouts_sql(checkouts: &[crate::billing::Checkout]) -> String {
    let mut sql = String::new();
    for co in checkouts {
        sql.push_str(&format!(
            "INSERT INTO billing_checkouts (id, tenant, kind, plan, amount_cents, stripe_session_id, created_ms) \
             VALUES ({}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (id) DO UPDATE SET tenant = excluded.tenant, kind = excluded.kind, \
             plan = excluded.plan, amount_cents = excluded.amount_cents, \
             stripe_session_id = excluded.stripe_session_id, created_ms = excluded.created_ms;",
            q(&co.id),
            q(&co.tenant),
            q(&co.kind),
            q(&co.plan),
            co.amount_cents,
            q(&co.stripe_session_id),
            co.created_ms,
        ));
    }
    sql
}

// ---------------------------------------------------------------------------
// One-time backfill: normalize pre-migration JSON-blob billing rows into the
// new per-row tables. NOT wired to run automatically — callable only via the
// admin-only `POST /v1/admin/billing/backfill` endpoint (admin.rs
// `billing_backfill_run`, `require_operator`-gated). Even then it must only
// be run against real production data after the separate operator-approval
// gate the migration rollout plan calls for — this function performs REAL
// writes the moment it is invoked; it does not itself ask for confirmation.
//
// IDEMPOTENT / SAFE TO RE-RUN: every tenant that verifies clean is recorded
// in `billing_backfill_state`; a later call skips any tenant already marked
// (unless `force`), so a retry after a transient failure only reprocesses
// tenants that did not finish — never re-applies one that already verified.
// ---------------------------------------------------------------------------

/// `billing_accounts`' new scalar columns, added via idempotent `ALTER TABLE
/// ... ADD COLUMN IF NOT EXISTS` by `reconcile_billing_accounts_schema`
/// below — never assumed present from `init_schema()`'s `CREATE TABLE IF NOT
/// EXISTS` alone.
///
/// CRITICAL, confirmed by direct inspection (this is the load-bearing finding
/// this whole reconciliation exists to handle): `billing_accounts` is the
/// SAME table name the OLD JSON-blob schema used (`tenant, account_json,
/// updated_ms`) — `git diff` on `init_schema()` shows the DDL rewrite changed
/// that `CREATE TABLE IF NOT EXISTS billing_accounts (...)` statement's
/// column list IN PLACE rather than renaming the table or adding a
/// differently-named replacement. The other four tables this same migration
/// introduced do NOT have this problem: `billing_ledger`/`billing_invoices`/
/// `billing_invoice_lines`/`billing_checkouts` are all genuinely new,
/// non-colliding table names (`billing_ledger`/`billing_invoices` sat next
/// to their old `billing_ledger_snapshot`/`billing_invoices_snapshot`
/// JSON-blob originals, now dropped by `init_schema`'s `DROP TABLE IF
/// EXISTS` cleanup; `billing_invoice_lines`/`billing_checkouts` are net-new
/// with no pre-migration table at all) — a
/// plain `CREATE TABLE IF NOT EXISTS` is sufficient for all four and none of
/// them need this ALTER-based treatment. `billing_accounts` is the ONLY
/// name collision in this migration. `CREATE TABLE IF NOT EXISTS` is a
/// genuine unconditional no-op when the table already exists — verified by
/// reading guardian-db's own `exec_create_table`
/// (`vendor/guardian-db/src/sql/ddl.rs`): `if self.catalog.has_table(&q) { if
/// ct.if_not_exists { return Ok(ExecResult::empty_command(..)) } ... }` — it
/// never reconciles columns against an existing table. So on any node whose
/// `billing_accounts` already existed pre-migration (every node in the real
/// fleet today, per this repo's own migration history), `init_schema()`'s
/// DDL loop ALONE would leave the table in its OLD 3-column shape forever;
/// `upsert_billing` / `billing_snapshot`'s new column-named SQL would fail
/// outright (column not found) against live production the moment this
/// binary ships, unless something adds the new columns first.
/// `reconcile_billing_accounts_schema` is that something — and
/// `init_schema()` now calls it unconditionally on every boot (not only from
/// the manually-triggered backfill endpoint), so correct schema is a
/// boot-time guarantee for every node, before any `upsert_billing` call is
/// ever possible.
///
/// Deliberately NOT NOT NULL (unlike the target `init_schema` DDL) since
/// these are being added onto a table that may already hold rows with no
/// value for them yet — `backfill_billing_normalize`'s per-tenant write
/// populates every existing row for real, but declaring NOT NULL on the
/// ALTER itself would risk a constraint failure on old rows depending on
/// exactly how the engine retroactively enforces it, which this function
/// deliberately does not assume either way.
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

/// Set once `reconcile_billing_accounts_schema` has completed with every
/// `ALTER TABLE` statement succeeding, from ANY call site in this process —
/// memoizes the reconciliation so repeat callers (chiefly `upsert_billing`,
/// which now calls this on every single write — see below) skip the
/// (idempotent but non-free) `ALTER TABLE` round-trip once it's known to
/// have already landed. Left UNSET on any failure so the very next call
/// retries for real: this is what closes the gap where `init_schema()`'s
/// one-shot boot-time call was the ONLY place reconciliation ever ran — if
/// `crate::guardian::sql_db()`/`handle()` fails (or wedges — see
/// `guardian.rs`'s "Live evidence (2026-07-06 onward)" doc comment) on that
/// single attempt, `init_schema()` never gets a second chance for that
/// node's entire uptime, while `spawn_billing_meter_loop` /
/// `spawn_relational_mirror_loop` keep calling `upsert_billing` on their own
/// retry cadence regardless. Relaxed ordering is sufficient: this is a
/// best-effort memoization racing independent loops, not a
/// correctness-critical lock — at worst a concurrent miss re-runs a handful
/// of idempotent ALTERs once more, never a wrong result.
static BILLING_ACCOUNTS_SCHEMA_RECONCILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Idempotent `billing_accounts` column reconciliation — issues `ALTER TABLE
/// ... ADD COLUMN IF NOT EXISTS` for every column in
/// `BILLING_ACCOUNTS_NEW_COLUMNS`. Called from THREE sites: `init_schema()`
/// on every boot (an early-opportunistic attempt — covers the common case
/// where guardian is already up by the time it runs), defensively from
/// `backfill_billing_normalize`, and — the actual safety net —
/// unconditionally from `upsert_billing` on every write, so correct schema
/// no longer depends on `init_schema()`'s single, unretried boot-time
/// attempt ever landing (see `BILLING_ACCOUNTS_SCHEMA_RECONCILED`'s doc
/// comment for the full failure mode this closes). Memoized via
/// `BILLING_ACCOUNTS_SCHEMA_RECONCILED`: a no-op past the first genuinely
/// successful call anywhere in this process, but still cheap and safe to
/// call unconditionally since `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` is
/// itself idempotent. See `BILLING_ACCOUNTS_NEW_COLUMNS`'s doc comment for
/// why this one table specifically needs ALTER-based reconciliation instead
/// of relying on plain `CREATE TABLE IF NOT EXISTS`.
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

/// One-time backfill entry point. Reads every existing row from the OLD
/// JSON-blob tables (`billing_accounts.account_json`,
/// `billing_ledger_snapshot.ledger_json`, `billing_invoices_snapshot.invoices_json`),
/// parses each blob back into `BillingAccount` / `Vec<LedgerEntry>` /
/// `Vec<Invoice>`, and writes normalized rows into the new tables using the
/// SAME atomic per-tenant `BEGIN...COMMIT` pattern the real writer uses — for
/// the common case (an old account row exists) this literally calls the
/// production `upsert_billing` writer itself. Verifies row-count parity per
/// tenant (1 account row + every ledger/invoice/invoice-line row actually
/// present) before marking that tenant migrated in `billing_backfill_state`;
/// a tenant that fails verification is left unmarked (any partial marker
/// cleared) so a later re-run retries exactly that tenant.
///
/// FRESHNESS GUARD (retry-vs-live-mirror race): a tenant left unmarked after
/// a partial/failed attempt gets reprocessed by a later call/retry. Between
/// that first attempt and the retry, the regular mirror loop
/// (`spawn_billing_meter_loop` / `upsert_billing`) may have already written
/// a NEWER `billing_accounts` row for this tenant from live, current data.
/// Blindly re-running `upsert_billing` with the frozen pre-migration blob on
/// the retry would silently overwrite that fresher live row with stale
/// content — a real (if self-healing within one more mirror tick)
/// data-freshness regression. So before writing a tenant's account row, this
/// compares the parsed old blob's `BillingAccount::updated_ms` against the
/// CURRENT `billing_accounts.updated_ms` for that tenant (if a row already
/// exists): the account row is only written when the blob's data is newer
/// or equal; if the live row is already strictly fresher, the account write
/// is skipped (a no-op, not a failure) while ledger/invoice rows — immutable,
/// append-only historical facts, never a point-in-time snapshot like the
/// account row — are still written/verified as normal. A tenant with no
/// existing row to compare against (the ordinary not-yet-migrated-at-all
/// case) always writes, preserving the existing idempotency/retry-safety.
///
/// Returns a JSON report. If NONE of the three old sources are queryable
/// (columns/tables already gone), returns `{"ran": false, "blocking_finding":
/// ...}` instead of attempting anything — this backfill must run BEFORE any
/// destructive drop of the old JSON-blob data; if that ordering was already
/// violated by the time this runs, fabricating a "successful" migration
/// against data that no longer exists would be strictly worse than surfacing
/// the gap loudly.
pub(crate) async fn backfill_billing_normalize(force: bool) -> Result<serde_json::Value, String> {
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("relational store unavailable: {e}"))?;
    let mut s = session(db).await;

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

    // ---- Step 1: is each OLD source still queryable AT ALL? ----
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

    // ---- Step 2: parse whatever IS available into per-tenant buckets ----
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
                // Old snapshot was written from `BillingStore::invoices()`
                // (see the pre-migration main.rs call site this diff
                // replaced), which INCLUDES the current in-progress draft —
                // filter to finalized only, matching `upsert_billing`'s own
                // "never persist a draft" rule so the new tables end up in
                // exactly the state they'd be in had they been written live.
                Ok(invs) => {
                    invoices_by_tenant.insert(
                        tenant,
                        invs.into_iter().filter(|i| i.status == "paid").collect(),
                    );
                }
                Err(e) => {
                    tracing::warn!(tenant, error = %e, "backfill: could not parse old invoices_json (row skipped, NOT counted as migrated)")
                }
            }
        }
    }

    // ---- Step 3: schema reconciliation is now a boot-time guarantee (see
    // `init_schema()` -> `reconcile_billing_accounts_schema`) -- call the
    // SAME idempotent function here too, defensively (cheap: `ALTER TABLE
    // ... ADD COLUMN IF NOT EXISTS` is a no-op once the columns already
    // exist), so this function never depends on ordering relative to boot
    // and the rest of it can assume the schema is already correct, focusing
    // purely on data migration.
    reconcile_billing_accounts_schema(&mut s).await;

    let already_migrated: std::collections::HashSet<String> = if force {
        std::collections::HashSet::new()
    } else {
        exec(&mut s, "SELECT tenant FROM billing_backfill_state")
            .await
            .ok()
            .map(|r| all_text_firsts(&r).into_iter().collect())
            .unwrap_or_default()
    };

    // Current `billing_accounts.updated_ms` per tenant, queried once in bulk
    // rather than per-tenant. `updated_ms` is a column that existed on this
    // table even pre-migration (see `BILLING_ACCOUNTS_NEW_COLUMNS` doc
    // comment), so this is safe to read for both a not-yet-migrated tenant
    // (old-schema row) and one the regular mirror loop already refreshed
    // with live, post-migration data. Used below as the freshness guard
    // against a retry clobbering a fresher live row with the frozen blob.
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

        // ---- Write: the SAME atomic per-tenant BEGIN...COMMIT pattern the
        // real writer uses — when an old account row exists (the normal
        // case), this literally calls the production `upsert_billing`
        // writer, checkouts empty (the old snapshot schema never captured
        // checkouts at all — see `billing-checkouts-persistence-gap`).
        //
        // FRESHNESS GUARD: if a `billing_accounts` row already exists for
        // this tenant (either its own not-yet-migrated old-schema row, or
        // one the regular mirror loop already refreshed with live data
        // between a prior failed/partial backfill attempt and this retry),
        // only overwrite it when the frozen blob's data is newer or equal.
        // A strictly fresher live row means the account write is skipped —
        // a no-op, never a failure — while ledger/invoice rows (immutable,
        // append-only historical facts, not a mutable point-in-time
        // snapshot like the account row) are still written/verified as
        // normal. No existing row to compare against -> always write,
        // exactly the prior idempotent-retry behavior.
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
            // Defensive edge case: ledger/invoice history with no parsed
            // account row for this tenant (should not happen in practice —
            // the old code always lazily created an account before any
            // ledger entry existed — but never silently drop real ledger/
            // invoice data just because the account row was missing/unparseable).
            write_ledger_and_invoices_only(&mut s, ledger, invoices).await
        } else {
            Ok(())
        };
        if let Err(e) = write_result {
            failed.push(serde_json::json!({ "tenant": tenant, "stage": "write", "error": e }));
            continue;
        }

        // ---- Verify: row-count parity before considering this tenant done ----
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
            // Never leave a stale marker for a tenant that just failed
            // verification -- a retry must reprocess it, not skip it.
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

/// Fallback write path for the defensive edge case in
/// `backfill_billing_normalize`: ledger/invoice history exists for a tenant
/// with no parsed old account row. Same atomic-transaction + upsert-SQL
/// building blocks as `upsert_billing`, just without the account statement.
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

// ---------------------------------------------------------------------------
// Teams / members / deployments (populated by main.rs's relational mirror
// loop — see spawn_relational_mirror_loop). VIEW-ONLY tables: nothing in the
// control plane reads them back for logic; they exist solely so the admin
// "view as PostgreSQL" browser shows real fleet data. Same best-effort,
// never-blocks-the-real-mutation convention as everything else here.
// ---------------------------------------------------------------------------

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

/// Full-snapshot sync of the `teams` + `team_members` tables. Caller gates on
/// being the control-plane leader (the single node where every admin mutation
/// lands, so its `TeamStore` is the authoritative copy) — same single-writer
/// discipline as `upsert_billing`. Upserts every current team/member, then
/// deletes rows whose team/member no longer exists in the snapshot.
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
            // warn, not debug: a failed upsert here is permanently invisible
            // otherwise (teams_hash still advances unconditionally in the
            // main.rs caller, so a failure never retries until the team
            // snapshot itself changes again) -- silently swallowing it at
            // debug! (filtered out under the fleet's RUST_LOG=info default
            // for this crate) previously let 3 of 5 real teams vanish from
            // the SQL mirror with zero observable signal.
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
    // Remove teams/members that vanished from the snapshot (team deleted,
    // member removed) so the mirror never shows ghosts forever.
    //
    // TOMBSTONE GUARD: only delete a row the authoritative leader has STOPPED
    // asserting (its updated_ms no longer advances), never one merely missing
    // from THIS node's local store. Every sync tick re-stamps every live
    // team's updated_ms=now, so a row the real leader still owns is always
    // fresh. Without this guard, a brief control-plane failover (pinned
    // leader restarting -> next node in the owner chain takes over) let the
    // stand-in leader's STALE local TeamStore drive this reconciliation and
    // DELETE every team it had never heard of -- live-witnessed: the mirror
    // lost 3 of 5 real teams fleet-wide during this session's rolling
    // restarts (the follower stores held 2/4 of the 5 teams; team mutations
    // only ever land on the pinned leader and followers never merge them).
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

/// Sync THIS node's own deployments into the `deployments` table. Every node
/// writes ONLY rows whose `node` column is itself and deletes only its own
/// stale rows — a single writer per row by construction, so a fleet-wide
/// multi-node sync converges without conflicts (a relocated deployment's row
/// moves to the new host on its next tick via the id-keyed upsert).
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
    // Delete only THIS node's rows for deployments it no longer hosts.
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

// ---------------------------------------------------------------------------
// Admin "view as PostgreSQL" browser — read-only introspection + query
// ---------------------------------------------------------------------------

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

/// The exact tables `init_schema()` creates. Hand-maintained rather than a
/// live `information_schema` introspection query — this module owns both the
/// schema and this list, they're already the single source of truth for each
/// other, and a fixed list can't be tricked into enumerating anything this
/// module didn't itself create.
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
                col("stripe_session_id", "text"),
                col("created_ms", "bigint"),
            ],
        },
        // Audit trail + idempotency marker for `backfill_billing_normalize`
        // (the one-time old-JSON-blob-to-normalized-rows migration) — one row
        // per tenant that has verified clean, so a re-run of the backfill
        // never reprocesses (or duplicates) a tenant already migrated.
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
    ]
}

/// SQL keywords that would mutate state or schema — rejected anywhere in a
/// query submitted through the read-only admin browser, not just as the
/// first token (defends against `SELECT 1; DROP TABLE x` multi-statement
/// injection and Postgres data-modifying CTEs like
/// `WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x`). Matched as a
/// whole token (split on any non-alphanumeric byte) so an ordinary column or
/// table name that merely CONTAINS one of these words as a substring (e.g. a
/// hypothetical `updated_by` column) is never a false-positive rejection.
const BLOCKED_SQL_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "TRUNCATE", "CREATE", "GRANT", "REVOKE",
    "REPLACE", "MERGE", "CALL", "EXEC", "EXECUTE", "COPY", "VACUUM",
];

/// `Err(reason)` if `sql` contains anything beyond a single read-only
/// statement. This is the ONLY gate between an admin's typed input and a live
/// `Session::execute` against real fleet-replicated tables — deliberately
/// conservative (reject-by-default on anything ambiguous) since a false
/// rejection just shows an error, while a false acceptance is a real data-loss
/// path.
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
    // A semicolon anywhere but trailing whitespace at the very end means a
    // second statement is present — reject rather than guess which one runs.
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
            // Numeric/Uuid/Date/Time/Timestamp(tz)/Json/Array/Bytea/etc: none of
            // this module's own tables use these types today; stringify via
            // accessor helpers where available, else Debug — always something
            // legible in the admin UI rather than a serialization error.
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

/// Run an admin-submitted, already-validated read-only query against the
/// relational mirror and shape the result as `{fields: [name...], rows:
/// [[value...]]}` for the admin "view as PostgreSQL" browser. Callers MUST
/// have already passed `sql` through `reject_unless_readonly` — this function
/// re-checks it anyway (cheap, and a missing call site must never become a
/// live mutation path).
pub(crate) async fn run_readonly_query(sql: &str) -> Result<serde_json::Value, String> {
    reject_unless_readonly(sql)?;
    let db = crate::guardian::sql_db()
        .await
        .map_err(|e| format!("relational store unavailable: {e}"))?;
    let mut s = session(db).await;
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
    // A read-only statement that legitimately returns zero result sets (rare,
    // but not itself evidence of anything unsafe having run) — an empty grid.
    Ok(
        serde_json::json!({ "fields": Vec::<String>::new(), "rows": Vec::<Vec<serde_json::Value>>::new() }),
    )
}

/// Build `{field: value, ...}` JSON objects from a query's result rows, keyed
/// by the query's OWN returned column names — reused by `billing_snapshot`'s
/// per-table reconstruction so each SELECT list is the single source of
/// truth for its own output shape (no hand-maintained positional mapping to
/// drift out of sync with a query edit).
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

/// Fleet-replicated billing read: `(account_json, ledger_json, invoices_json)`
/// reconstructed from the real per-row tables (`billing_accounts`,
/// `billing_ledger`, `billing_invoices` + `billing_invoice_lines`) into the
/// same JSON shapes `BillingAccount`/`LedgerEntry`/`Invoice` serialize to
/// (field names match exactly), so both existing callers
/// (`admin::billing_invoices`, `admin::billing_ledger`) keep working
/// unchanged. The whole function returns `None` only when this tenant has
/// never been mirrored to the relational layer at all — callers fall back to
/// the existing HTTP proxy-to-leader (`admin::proxy_billing_read`) in that
/// case, so a cold/lagging replica never surfaces a wrong answer, only a
/// slightly slower one.
///
/// Mirrored-ness is decided by the presence of a `billing_accounts` row, NOT
/// by whether the `billing_ledger`/`billing_invoices` queries themselves
/// returned any rows: `upsert_billing` writes the account row unconditionally
/// in the same transaction as the ledger/invoice rows, so a mirrored tenant
/// with a genuinely empty ledger or invoice history is a real, common case —
/// its `ledger_json`/`invoices_json` must come back `Some("[]")`, not `None`,
/// or callers' `if let Some((_, Some(_), _))` fast-path check falls through
/// to the (correct but slower) fallback for the common case of an
/// active-but-empty tenant, silently defeating the local-read optimization.
///
/// `invoices_json` reflects ONLY finalized invoices — the current
/// in-progress period's draft is never persisted here (see
/// `upsert_billing`'s doc comment) — `admin::billing_invoices` appends
/// `BillingStore::current_invoice` itself to restore that field for callers.
pub(crate) async fn billing_snapshot(
    tenant: &str,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let db = crate::guardian::sql_db().await.ok()?;
    let mut s = session(db).await;

    let acc_q = format!(
        "SELECT tenant, plan, status, included_cents, used_cents, balance_cents, \
         stripe_customer, stripe_subscription, period_start_ms, period_end_ms, updated_ms \
         FROM billing_accounts WHERE tenant = {}",
        q(tenant)
    );
    let account = exec(&mut s, &acc_q)
        .await
        .ok()
        .map(|r| rows_to_json_objects(&r))
        .and_then(|rows| rows.into_iter().next())
        .map(|v| v.to_string());

    // Not mirrored at all — never write a partial/empty answer here, let the
    // caller fall back to the HTTP proxy-to-leader (or in-memory read).
    account.as_ref()?;

    let ledger_q = format!(
        "SELECT id, tenant, ts_ms, kind, amount_cents, balance_after_cents, note \
         FROM billing_ledger WHERE tenant = {} ORDER BY ts_ms DESC",
        q(tenant)
    );
    let ledger_rows = exec(&mut s, &ledger_q)
        .await
        .ok()
        .map(|r| rows_to_json_objects(&r))
        .unwrap_or_default();
    let ledger = Some(serde_json::Value::Array(ledger_rows).to_string());

    let inv_q = format!(
        "SELECT id, number, plan, period_start_ms, period_end_ms, subtotal_cents, total_cents, status, created_ms \
         FROM billing_invoices WHERE tenant = {} ORDER BY period_start_ms DESC",
        q(tenant)
    );
    let invoice_rows = exec(&mut s, &inv_q)
        .await
        .ok()
        .map(|r| rows_to_json_objects(&r))
        .unwrap_or_default();
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
        let lines = exec(&mut s, &lines_q)
            .await
            .ok()
            .map(|r| rows_to_json_objects(&r))
            .unwrap_or_default();
        if let Some(obj) = inv.as_object_mut() {
            obj.insert(
                "tenant".into(),
                serde_json::Value::String(tenant.to_string()),
            );
            obj.insert("lines".into(), serde_json::Value::Array(lines));
        }
        invoices_out.push(inv);
    }
    let invoices = Some(serde_json::Value::Array(invoices_out).to_string());

    Some((account, ledger, invoices))
}
