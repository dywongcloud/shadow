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
        "CREATE TABLE IF NOT EXISTS teams (\
            slug TEXT PRIMARY KEY, \
            name TEXT NOT NULL, \
            plan TEXT NOT NULL, \
            member_count BIGINT NOT NULL, \
            sso_enabled BIGINT NOT NULL, \
            created_ms BIGINT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
        "CREATE TABLE IF NOT EXISTS team_members (\
            id TEXT PRIMARY KEY, \
            team TEXT NOT NULL, \
            email TEXT NOT NULL, \
            name TEXT NOT NULL DEFAULT '', \
            role TEXT NOT NULL, \
            added_ms BIGINT NOT NULL, \
            updated_ms BIGINT NOT NULL\
        )",
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
    let Ok(db) = crate::guardian::sql_db().await else { return };
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
        if let Err(e) = s.execute(&query).await {
            tracing::debug!(team = %t.slug, error = %e, "relational: sync_teams upsert failed (non-fatal)");
        }
        for m in &t.members {
            let id = format!("{}/{}", t.slug, m.email.to_lowercase());
            let role = serde_json::to_string(&m.role).unwrap_or_default().trim_matches('"').to_string();
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
            if let Err(e) = s.execute(&mq).await {
                tracing::debug!(member = %id, error = %e, "relational: sync_teams member upsert failed (non-fatal)");
            }
        }
    }
    // Remove teams/members that vanished from the snapshot (team deleted,
    // member removed) so the mirror never shows ghosts forever.
    let live_teams: std::collections::HashSet<String> = teams.iter().map(|t| t.slug.clone()).collect();
    let live_members: std::collections::HashSet<String> =
        teams.iter().flat_map(|t| t.members.iter().map(move |m| format!("{}/{}", t.slug, m.email.to_lowercase()))).collect();
    if let Ok(res) = s.execute("SELECT slug FROM teams").await {
        for slug in all_text_firsts(&res) {
            if !live_teams.contains(&slug) {
                let _ = s.execute(&format!("DELETE FROM teams WHERE slug = {}", q(&slug))).await;
            }
        }
    }
    if let Ok(res) = s.execute("SELECT id FROM team_members").await {
        for id in all_text_firsts(&res) {
            if !live_members.contains(&id) {
                let _ = s.execute(&format!("DELETE FROM team_members WHERE id = {}", q(&id))).await;
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
    let Ok(db) = crate::guardian::sql_db().await else { return };
    let now = hive_core::now_ms();
    let mut s = session(db).await;
    for d in deps {
        let state = serde_json::to_string(&d.state).unwrap_or_default().trim_matches('"').to_string();
        let team = if d.tenant.is_empty() { "personal" } else { d.tenant.as_str() };
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
        if let Err(e) = s.execute(&query).await {
            tracing::debug!(deployment = %d.id, error = %e, "relational: sync_deployments upsert failed (non-fatal)");
        }
    }
    // Delete only THIS node's rows for deployments it no longer hosts.
    let live: std::collections::HashSet<String> = deps.iter().map(|d| d.id.to_string()).collect();
    let sel = format!("SELECT id FROM deployments WHERE node = {}", q(node));
    if let Ok(res) = s.execute(&sel).await {
        for id in all_text_firsts(&res) {
            if !live.contains(&id) {
                let _ = s.execute(&format!("DELETE FROM deployments WHERE id = {}", q(&id))).await;
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
            columns: vec![col("project", "text"), col("team", "text"), col("root_dir", "text"), col("updated_ms", "bigint")],
        },
        SqlTableInfo {
            name: "billing_accounts",
            columns: vec![col("tenant", "text"), col("account_json", "text"), col("updated_ms", "bigint")],
        },
        SqlTableInfo {
            name: "billing_ledger_snapshot",
            columns: vec![col("tenant", "text"), col("ledger_json", "text"), col("updated_ms", "bigint")],
        },
        SqlTableInfo {
            name: "billing_invoices_snapshot",
            columns: vec![col("tenant", "text"), col("invoices_json", "text"), col("updated_ms", "bigint")],
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
const BLOCKED_SQL_KEYWORDS: &[&str] =
    &["INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "TRUNCATE", "CREATE", "GRANT", "REVOKE", "REPLACE", "MERGE", "CALL", "EXEC", "EXECUTE", "COPY", "VACUUM"];

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
    let first_token = upper.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').find(|s| !s.is_empty()).unwrap_or("");
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
    let db = crate::guardian::sql_db().await.map_err(|e| format!("relational store unavailable: {e}"))?;
    let mut s = session(db).await;
    let results = s.execute(sql).await.map_err(|e| format!("{e}"))?;
    for r in &results {
        if let ExecResult::Rows { fields, rows } = r {
            let field_names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
            let json_rows: Vec<Vec<serde_json::Value>> = rows.iter().map(|row| row.iter().map(sql_value_to_json).collect()).collect();
            return Ok(serde_json::json!({ "fields": field_names, "rows": json_rows }));
        }
    }
    // A read-only statement that legitimately returns zero result sets (rare,
    // but not itself evidence of anything unsafe having run) — an empty grid.
    Ok(serde_json::json!({ "fields": Vec::<String>::new(), "rows": Vec::<Vec<serde_json::Value>>::new() }))
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
