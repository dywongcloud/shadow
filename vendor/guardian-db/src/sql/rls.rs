//! Row-Level Security (RLS) enforcement.
//!
//! The enforcement point is the boundary where [`LoadedTable`] rows become
//! visible to execution: [`Exec::init_rls`] runs once per statement (after
//! session variables are installed, before anything is evaluated) and computes,
//! for every loaded table with `rls_enabled`, the set of row ids the current
//! role may *not* see. Scans ([`crate::sql::select`]), `SELECT ... FOR UPDATE`
//! locking, and the UPDATE/DELETE target snapshots ([`crate::sql::dml`]) all
//! consult those sets, so joins, subqueries, CTEs and index scans inherit the
//! filtering without further plumbing. New-row validity (`INSERT` /
//! `UPDATE ... SET`) is enforced separately via [`Exec::rls_check_new_row`].
//!
//! Semantics mirror PostgreSQL:
//! * a row is visible/allowed iff **any** applicable PERMISSIVE policy passes
//!   **and** **all** applicable RESTRICTIVE policies pass;
//! * `rls_enabled` with no applicable policy is default-deny;
//! * `FOR ALL` matches every command; UPDATE filters old rows with `USING` and
//!   checks new rows with `WITH CHECK` (falling back to `USING`); INSERT uses
//!   `WITH CHECK`; DELETE and SELECT use `USING`;
//! * expressions evaluating to false **or NULL** deny;
//! * the role `service_role` (Supabase's `BYPASSRLS` equivalent) bypasses row
//!   security entirely; `postgres` and `guardian` (the engine's owner names)
//!   bypass it like PostgreSQL table owners — unless the table sets
//!   `FORCE ROW LEVEL SECURITY`, which revokes exactly that owner exemption
//!   (`BYPASSRLS` beats `FORCE`, as in PostgreSQL). Tables with
//!   `rls_enabled = false` are never filtered.
//!
//! [`LoadedTable`]: crate::sql::store::LoadedTable

use crate::relational::catalog::{PolicyCmd, QualifiedName, Table};
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::{Exec, Frame};
use crate::sql::store::RowValues;
use sqlparser::ast::{Expr, Statement, TableFactor};
use std::collections::{BTreeSet, HashMap};

/// Roles equivalent to PostgreSQL's `BYPASSRLS` attribute: they bypass row
/// security even under `FORCE ROW LEVEL SECURITY`. `service_role` mirrors
/// Supabase's service key.
const BYPASS_ROLES: &[&str] = &["service_role"];

/// The engine's owner/superuser role names. Like PostgreSQL table owners they
/// are not subject to row security, unless a table declares
/// `FORCE ROW LEVEL SECURITY` — which revokes exactly this exemption.
const OWNER_ROLES: &[&str] = &["postgres", "guardian"];

/// Does `role` bypass row security on tables *without*
/// `FORCE ROW LEVEL SECURITY`? (Owner roles and `BYPASSRLS` roles alike.)
pub fn role_bypasses_rls(role: &str) -> bool {
    BYPASS_ROLES.contains(&role) || OWNER_ROLES.contains(&role)
}

/// Does `role` bypass row security on `table`, honouring its
/// `FORCE ROW LEVEL SECURITY` flag? `BYPASSRLS`-equivalent roles always
/// bypass; owner roles only until the table is FORCEd.
pub fn role_bypasses_rls_on(role: &str, table: &Table) -> bool {
    BYPASS_ROLES.contains(&role) || (OWNER_ROLES.contains(&role) && !table.rls_forced)
}

/// Which expression of a policy applies: `USING` (visibility of existing rows)
/// or `WITH CHECK` (validity of new rows; falls back to `USING` when absent).
#[derive(Clone, Copy)]
enum Phase {
    Using,
    Check,
}

/// A policy applicable to the current role, with its expressions parsed once
/// per statement (the texts are validated at CREATE POLICY time, so a parse
/// failure here means the stored catalog was edited by hand).
struct CompiledPolicy {
    cmd: PolicyCmd,
    permissive: bool,
    using_expr: Option<Expr>,
    check_expr: Option<Expr>,
}

impl CompiledPolicy {
    fn expr_for(&self, phase: Phase) -> Option<&Expr> {
        match phase {
            Phase::Using => self.using_expr.as_ref(),
            // WITH CHECK defaults to the USING expression (PostgreSQL).
            Phase::Check => self.check_expr.as_ref().or(self.using_expr.as_ref()),
        }
    }
}

/// Per-statement row-security state, populated by [`Exec::init_rls`]. Empty
/// (the default) means "no enforcement" — bypass role or no RLS tables loaded.
#[derive(Default)]
pub struct RlsContext {
    /// Compiled, role-applicable policies per enforced table.
    policies: HashMap<QualifiedName, Vec<CompiledPolicy>>,
    /// Row ids invisible to read scans (SELECT-class `USING` filtering).
    select_hidden: HashMap<QualifiedName, BTreeSet<String>>,
    /// Row ids invisible to the UPDATE/DELETE target scan (that command's
    /// `USING` filtering — distinct from SELECT visibility).
    dml_hidden: HashMap<QualifiedName, BTreeSet<String>>,
}

impl Exec {
    /// Compute row-security state for this statement. Must run after session
    /// variables are copied into the context (policy expressions read them via
    /// `current_setting()` / `auth.uid()`) and before any evaluation.
    pub fn init_rls(&mut self, stmt: &Statement) -> Result<()> {
        if BYPASS_ROLES.contains(&self.username.as_str()) {
            return Ok(());
        }
        // Enforced tables among the ones this statement loaded, in stable order
        // (a policy subquery scanning another RLS table sees that table's
        // already-computed filtering when it sorts earlier). Owner roles skip
        // tables that have not FORCEd row security.
        let mut keys: Vec<QualifiedName> = self
            .tables
            .iter()
            .filter(|(_, l)| l.meta.rls_enabled && !role_bypasses_rls_on(&self.username, &l.meta))
            .map(|(q, _)| q.clone())
            .collect();
        keys.sort();
        if keys.is_empty() {
            return Ok(());
        }
        for q in &keys {
            let compiled = compile_policies(&self.tables[q].meta, &self.username)?;
            self.rls.policies.insert(q.clone(), compiled);
        }
        // The UPDATE/DELETE target table is additionally filtered by that
        // command's USING expressions (not SELECT's).
        let dml_target: Option<(QualifiedName, PolicyCmd)> = match stmt {
            Statement::Update(u) => self
                .rls_target(&u.table.relation)
                .map(|q| (q, PolicyCmd::Update)),
            Statement::Delete(d) => {
                let items = match &d.from {
                    sqlparser::ast::FromTable::WithFromKeyword(items)
                    | sqlparser::ast::FromTable::WithoutKeyword(items) => items,
                };
                items
                    .first()
                    .and_then(|twj| self.rls_target(&twj.relation))
                    .map(|q| (q, PolicyCmd::Delete))
            }
            _ => None,
        };

        for q in &keys {
            let (meta, rows): (Table, Vec<(String, RowValues)>) = {
                let loaded = &self.tables[q];
                (
                    loaded.meta.clone(),
                    loaded
                        .rows
                        .iter()
                        .map(|(rid, v)| (rid.clone(), v.clone()))
                        .collect(),
                )
            };
            let schema = crate::sql::dml::table_schema(&meta, &meta.name);
            let dml_cmd = dml_target
                .as_ref()
                .filter(|(tq, _)| tq == q)
                .map(|(_, cmd)| *cmd);
            let mut select_hidden = BTreeSet::new();
            let mut dml_hidden = BTreeSet::new();
            for (rid, values) in &rows {
                let tuple = crate::sql::dml::row_tuple(&meta, values);
                if !self.rls_row_passes(q, PolicyCmd::Select, Phase::Using, &schema, &tuple)? {
                    select_hidden.insert(rid.clone());
                }
                if let Some(cmd) = dml_cmd
                    && !self.rls_row_passes(q, cmd, Phase::Using, &schema, &tuple)?
                {
                    dml_hidden.insert(rid.clone());
                }
            }
            self.rls.select_hidden.insert(q.clone(), select_hidden);
            if dml_cmd.is_some() {
                self.rls.dml_hidden.insert(q.clone(), dml_hidden);
            }
        }
        Ok(())
    }

    /// Row ids of `q` hidden from read scans, if RLS filters this table.
    pub fn rls_select_hidden(&self, q: &QualifiedName) -> Option<&BTreeSet<String>> {
        self.rls.select_hidden.get(q).filter(|h| !h.is_empty())
    }

    /// Row ids of `q` hidden from the UPDATE/DELETE target scan.
    pub fn rls_dml_hidden(&self, q: &QualifiedName) -> Option<&BTreeSet<String>> {
        self.rls.dml_hidden.get(q).filter(|h| !h.is_empty())
    }

    /// Enforce the `WITH CHECK` phase for a new or updated row. Raises the
    /// PostgreSQL error (SQLSTATE 42501) when the row is not allowed.
    pub fn rls_check_new_row(
        &self,
        q: &QualifiedName,
        table: &Table,
        values: &RowValues,
        cmd: PolicyCmd,
    ) -> Result<()> {
        if !self.rls.policies.contains_key(q) {
            return Ok(()); // bypass role, or RLS not enabled on this table
        }
        let schema = crate::sql::dml::table_schema(table, &table.name);
        let tuple = crate::sql::dml::row_tuple(table, values);
        if self.rls_row_passes(q, cmd, Phase::Check, &schema, &tuple)? {
            Ok(())
        } else {
            Err(SqlError::InsufficientPrivilege(format!(
                "new row violates row-level security policy for table \"{}\"",
                table.name
            )))
        }
    }

    /// Does an existing row pass `cmd`'s `USING` expressions? Used by
    /// `INSERT ... ON CONFLICT DO UPDATE`, where the conflicting row must be
    /// updatable under UPDATE policies.
    pub fn rls_old_row_visible(
        &self,
        q: &QualifiedName,
        table: &Table,
        values: &RowValues,
        cmd: PolicyCmd,
    ) -> Result<bool> {
        if !self.rls.policies.contains_key(q) {
            return Ok(true);
        }
        let schema = crate::sql::dml::table_schema(table, &table.name);
        let tuple = crate::sql::dml::row_tuple(table, values);
        self.rls_row_passes(q, cmd, Phase::Using, &schema, &tuple)
    }

    /// PostgreSQL's combining rule for one row: OR over applicable PERMISSIVE
    /// policies AND over applicable RESTRICTIVE policies; non-TRUE (false or
    /// NULL) denies; no applicable permissive policy denies (default deny).
    fn rls_row_passes(
        &self,
        q: &QualifiedName,
        cmd: PolicyCmd,
        phase: Phase,
        schema: &crate::sql::row::RowSchema,
        tuple: &crate::sql::row::Tuple,
    ) -> Result<bool> {
        let Some(policies) = self.rls.policies.get(q) else {
            return Ok(true);
        };
        let mut permissive_ok = false;
        for p in policies {
            if p.cmd != PolicyCmd::All && p.cmd != cmd {
                continue;
            }
            let Some(expr) = p.expr_for(phase) else {
                continue;
            };
            let pass = if p.permissive && permissive_ok {
                true // already granted; skip re-evaluating further permissive exprs
            } else {
                self.eval(expr, &[Frame { schema, row: tuple }])?.truthy() == Some(true)
            };
            if p.permissive {
                permissive_ok |= pass;
            } else if !pass {
                return Ok(false);
            }
        }
        Ok(permissive_ok)
    }

    /// Resolve a DML target relation to its qualified name (best effort).
    fn rls_target(&self, relation: &TableFactor) -> Option<QualifiedName> {
        if let TableFactor::Table { name, .. } = relation {
            let (schema, n) = crate::sql::names::split_schema_table(name);
            self.catalog.resolve_table_name(schema.as_deref(), &n)
        } else {
            None
        }
    }
}

/// Filter `table`'s policies down to the ones applying to `role` (empty role
/// list = PUBLIC) and parse their stored expression texts once.
fn compile_policies(table: &Table, role: &str) -> Result<Vec<CompiledPolicy>> {
    let mut out = Vec::new();
    for p in &table.policies {
        if !p.roles.is_empty() && !p.roles.iter().any(|r| r == role) {
            continue;
        }
        out.push(CompiledPolicy {
            cmd: p.cmd,
            permissive: p.permissive,
            using_expr: parse_stored(p.using_expr.as_deref(), &p.name)?,
            check_expr: parse_stored(p.check_expr.as_deref(), &p.name)?,
        });
    }
    Ok(out)
}

fn parse_stored(text: Option<&str>, policy: &str) -> Result<Option<Expr>> {
    match text {
        None => Ok(None),
        Some(t) => crate::sql::parser::parse_expr(t).map(Some).map_err(|e| {
            SqlError::Internal(format!(
                "stored expression of policy \"{policy}\" failed to parse: {e}"
            ))
        }),
    }
}
