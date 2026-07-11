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
/// NOT EXISTS`). Best-effort: GuardianDB may not be ready yet (same init-race
/// as every other guardian.rs caller); a failure here just means the fleet
/// fallback is unavailable until the next successful call, never a boot
/// failure.
pub(crate) async fn init_schema() {
    let Ok(db) = crate::guardian::sql_db().await else {
        tracing::debug!("relational: GuardianDB not ready yet; schema bring-up deferred");
        return;
    };
    let mut session = Session::new(db, "hive-init");
    for ddl in [
        "CREATE TABLE IF NOT EXISTS project_teams (\
            project TEXT PRIMARY KEY, \
            team TEXT NOT NULL, \
            root_dir TEXT NOT NULL DEFAULT '', \
            updated_ms BIGINT NOT NULL\
        )",
        "CREATE TABLE IF NOT EXISTS billing_accounts (\
            tenant TEXT PRIMARY KEY, \
            account_json TEXT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        "CREATE TABLE IF NOT EXISTS billing_ledger_snapshot (\
            tenant TEXT PRIMARY KEY, \
            ledger_json TEXT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        "CREATE TABLE IF NOT EXISTS billing_invoices_snapshot (\
            tenant TEXT PRIMARY KEY, \
            invoices_json TEXT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
    ] {
        if let Err(e) = session.execute(ddl).await {
            tracing::warn!(error = %e, ddl, "relational: schema bring-up statement failed");
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

fn first_text(res: &[ExecResult]) -> Option<String> {
    for r in res {
        if let ExecResult::Rows { rows, .. } = r {
            if let Some(row) = rows.first() {
                if let Some(SqlValue::Text(s)) = row.first() {
                    return Some(s.clone());
                }
            }
        }
    }
    None
}

fn all_text_pairs(res: &[ExecResult]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for r in res {
        if let ExecResult::Rows { rows, .. } = r {
            for row in rows {
                if let (Some(SqlValue::Text(a)), Some(SqlValue::Text(b))) = (row.first(), row.get(1)) {
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
    if let Err(e) = db.storage().refresh().await {
        tracing::debug!(error = %e, "relational: index refresh failed (serving from the previous local index)");
    }
    Session::new(db, "hive")
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
    let Ok(db) = crate::guardian::sql_db().await else { return };
    let mut s = session(db).await;
    let query = format!(
        "INSERT INTO project_teams (project, team, root_dir, updated_ms) VALUES ({}, {}, {}, {}) \
         ON CONFLICT (project) DO UPDATE SET team = excluded.team, root_dir = excluded.root_dir, updated_ms = excluded.updated_ms",
        q(project),
        q(team),
        q(root_dir),
        hive_core::now_ms(),
    );
    if let Err(e) = s.execute(&query).await {
        tracing::debug!(project, error = %e, "relational: set_project_team failed (non-fatal, fleet-fallback unavailable for this project until retried)");
    }
}

/// Forget a project's relational row (mirrors `ProjectStore::remove`).
pub(crate) async fn remove_project(project: &str) {
    let Ok(db) = crate::guardian::sql_db().await else { return };
    let mut s = session(db).await;
    let query = format!("DELETE FROM project_teams WHERE project = {}", q(project));
    if let Err(e) = s.execute(&query).await {
        tracing::debug!(project, error = %e, "relational: remove_project failed (non-fatal)");
    }
}

/// Every (project, team) pair this node's replica currently holds for `team`
/// — backs `gitops_projects`'s fleet-visibility fallback (and any future
/// "list every project for team X" surface) with a source that includes
/// projects with zero deployments anywhere, unlike the peer_deployments-only
/// fallback.
pub(crate) async fn projects_for_team(team: &str) -> Vec<String> {
    let Ok(db) = crate::guardian::sql_db().await else { return Vec::new() };
    let mut s = session(db).await;
    let query = format!("SELECT project, team FROM project_teams WHERE team = {}", q(team));
    let Ok(res) = s.execute(&query).await else { return Vec::new() };
    all_text_pairs(&res).into_iter().map(|(p, _)| p).collect()
}

// ---------------------------------------------------------------------------
// Billing (fleet-consistent reads on top of the existing single-writer meter)
// ---------------------------------------------------------------------------

/// Upsert a tenant's serialized billing account/ledger/invoices. Called
/// alongside every `BillingStore` mutation on whichever node is CURRENTLY the
/// billing authority (the elected metering leader — see
/// `admin::billing_authority_node`) so every other node's local replica
/// converges to the SAME account state within seconds, instead of staying
/// empty/stale until the next full-snapshot boot bootstrap. Best-effort, same
/// as `set_project_team` — a failure here never blocks the real mutation; the
/// existing HTTP proxy-to-leader fix (`admin::proxy_billing_read`) remains
/// the safety net if this table is ever behind/unavailable.
pub(crate) async fn upsert_billing(tenant: &str, account_json: &str, ledger_json: &str, invoices_json: &str) {
    let Ok(db) = crate::guardian::sql_db().await else { return };
    let now = hive_core::now_ms();
    let mut s = session(db).await;
    let queries = [
        format!(
            "INSERT INTO billing_accounts (tenant, account_json, updated_ms) VALUES ({}, {}, {}) \
             ON CONFLICT (tenant) DO UPDATE SET account_json = excluded.account_json, updated_ms = excluded.updated_ms",
            q(tenant), q(account_json), now
        ),
        format!(
            "INSERT INTO billing_ledger_snapshot (tenant, ledger_json, updated_ms) VALUES ({}, {}, {}) \
             ON CONFLICT (tenant) DO UPDATE SET ledger_json = excluded.ledger_json, updated_ms = excluded.updated_ms",
            q(tenant), q(ledger_json), now
        ),
        format!(
            "INSERT INTO billing_invoices_snapshot (tenant, invoices_json, updated_ms) VALUES ({}, {}, {}) \
             ON CONFLICT (tenant) DO UPDATE SET invoices_json = excluded.invoices_json, updated_ms = excluded.updated_ms",
            q(tenant), q(invoices_json), now
        ),
    ];
    for query in queries {
        if let Err(e) = s.execute(&query).await {
            tracing::debug!(tenant, error = %e, "relational: upsert_billing failed (non-fatal, HTTP proxy-to-leader remains the fallback)");
        }
    }
}

/// Fleet-replicated billing read: `(account_json, ledger_json, invoices_json)`
/// from THIS node's own local replica. `None` for any field never written
/// here yet (not yet replicated, or genuinely no billing history) — callers
/// fall back to the existing HTTP proxy-to-leader (`admin::proxy_billing_read`)
/// when this returns `None`, so a cold/lagging replica never surfaces a wrong
/// answer, only a slightly slower one.
pub(crate) async fn billing_snapshot(tenant: &str) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let db = crate::guardian::sql_db().await.ok()?;
    let mut s = session(db).await;
    let acc_q = format!("SELECT account_json FROM billing_accounts WHERE tenant = {}", q(tenant));
    let ledger_q = format!("SELECT ledger_json FROM billing_ledger_snapshot WHERE tenant = {}", q(tenant));
    let inv_q = format!("SELECT invoices_json FROM billing_invoices_snapshot WHERE tenant = {}", q(tenant));
    let account = s.execute(&acc_q).await.ok().and_then(|r| first_text(&r));
    let ledger = s.execute(&ledger_q).await.ok().and_then(|r| first_text(&r));
    let invoices = s.execute(&inv_q).await.ok().and_then(|r| first_text(&r));
    if account.is_none() && ledger.is_none() && invoices.is_none() {
        return None;
    }
    Some((account, ledger, invoices))
}
