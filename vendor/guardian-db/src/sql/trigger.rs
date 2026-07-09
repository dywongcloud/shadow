//! Triggers: `CREATE TRIGGER` / `DROP TRIGGER` DDL, `ALTER TABLE ...
//! ENABLE/DISABLE TRIGGER`, and the firing engine the DML executors call.
//!
//! Supported surface: `BEFORE`/`AFTER`/`INSTEAD OF` × `INSERT`/`UPDATE [OF
//! cols]`/`DELETE`/`TRUNCATE` × `FOR EACH ROW`/`FOR EACH STATEMENT`, `WHEN`
//! conditions on row triggers, `EXECUTE FUNCTION|PROCEDURE fn()` naming a
//! zero-argument `RETURNS trigger` PL/pgSQL function, `OR REPLACE`,
//! `CONSTRAINT TRIGGER` with `DEFERRABLE`/`INITIALLY DEFERRED|IMMEDIATE`,
//! and `REFERENCING OLD TABLE AS …`/`NEW TABLE AS …` for statement-level
//! transition tables (stored but not yet injected into the CTE scope for UDF
//! bodies — documented under `docs/postgres-compat.md`).
//!
//! Constraints:
//! * `INSTEAD OF` — view only, always `FOR EACH ROW`, no `WHEN` restriction.
//! * `TRUNCATE` event — always `FOR EACH STATEMENT`, never `INSTEAD OF`.
//! * `CONSTRAINT TRIGGER` — must be `AFTER FOR EACH ROW`; `DEFERRABLE` only
//!   allowed here.
//! * `REFERENCING` — `AFTER FOR EACH STATEMENT` only (PostgreSQL rule).
//!
//! Firing semantics (PostgreSQL): same-event triggers fire in alphabetical
//! name order; a BEFORE ROW trigger's returned `NEW` feeds the next trigger
//! in the chain and `RETURN NULL` suppresses the row; INSTEAD OF ROW triggers
//! route view DML through user-provided logic; AFTER ROW triggers observe the
//! final rows, including foreign-key cascade effects; statement-level triggers
//! fire exactly once per statement, even when zero rows are affected.

use crate::relational::FunctionDef;
use crate::relational::catalog::{
    QualifiedName, Table, TriggerDef, TriggerEventDef, TriggerLevel, TriggerTiming,
};
use crate::sql::dml::{row_tuple, table_schema_named};
use crate::sql::error::{Result, SqlError, unsupported};
use crate::sql::exec::{Exec, Frame};
use crate::sql::names::{ident_name, object_name_parts, split_schema_table};
use crate::sql::result::ExecResult;
use crate::sql::row::{FieldRef, RowSchema, RowSet};
use crate::sql::store::RowValues;
use crate::sql::udf::TriggerInvocation;
use sqlparser::ast::{
    CreateTrigger, DeferrableInitial, DropTrigger, Expr, Ident, TriggerEvent, TriggerObject,
    TriggerObjectKind, TriggerPeriod, TriggerReferencingType,
};
use std::collections::BTreeSet;

/// `pg_trigger.tgtype` bits (PostgreSQL's values).
pub(crate) const TGTYPE_ROW: i16 = 1;
pub(crate) const TGTYPE_BEFORE: i16 = 2;
pub(crate) const TGTYPE_INSERT: i16 = 4;
pub(crate) const TGTYPE_DELETE: i16 = 8;
pub(crate) const TGTYPE_UPDATE: i16 = 16;
pub(crate) const TGTYPE_TRUNCATE: i16 = 32;
pub(crate) const TGTYPE_INSTEAD: i16 = 64;

/// The PostgreSQL `pg_trigger.tgtype` bitmask for a stored trigger.
pub(crate) fn tgtype(trg: &TriggerDef) -> i16 {
    let mut bits = 0i16;
    if trg.level == TriggerLevel::Row {
        bits |= TGTYPE_ROW;
    }
    match trg.timing {
        TriggerTiming::Before => bits |= TGTYPE_BEFORE,
        TriggerTiming::After => {}
        TriggerTiming::InsteadOf => bits |= TGTYPE_INSTEAD,
    }
    for e in &trg.events {
        bits |= match e {
            TriggerEventDef::Insert => TGTYPE_INSERT,
            TriggerEventDef::Update { .. } => TGTYPE_UPDATE,
            TriggerEventDef::Delete => TGTYPE_DELETE,
            TriggerEventDef::Truncate => TGTYPE_TRUNCATE,
        };
    }
    bits
}

/// The operation a firing corresponds to (`TG_OP`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerOp {
    Insert,
    Update,
    Delete,
    Truncate,
}

impl TriggerOp {
    /// The `TG_OP` spelling.
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            TriggerOp::Insert => "INSERT",
            TriggerOp::Update => "UPDATE",
            TriggerOp::Delete => "DELETE",
            TriggerOp::Truncate => "TRUNCATE",
        }
    }
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

impl Exec {
    pub fn exec_create_trigger(&mut self, ct: &CreateTrigger) -> Result<ExecResult> {
        // Non-PostgreSQL forms always fail typed.
        if ct.temporary {
            return Err(unsupported("CREATE TEMPORARY TRIGGER"));
        }
        if ct.or_alter {
            return Err(unsupported("CREATE OR ALTER TRIGGER"));
        }

        let timing = match ct.period {
            Some(TriggerPeriod::Before) => TriggerTiming::Before,
            Some(TriggerPeriod::After) => TriggerTiming::After,
            Some(TriggerPeriod::InsteadOf) => TriggerTiming::InsteadOf,
            Some(TriggerPeriod::For) | None => {
                return Err(SqlError::Syntax(
                    "CREATE TRIGGER requires BEFORE, AFTER, or INSTEAD OF".into(),
                ));
            }
        };

        if ct.referenced_table_name.is_some() {
            return Err(unsupported("constraint-trigger FROM clause"));
        }

        // REFERENCING OLD TABLE / NEW TABLE: store aliases; only valid for
        // AFTER FOR EACH STATEMENT (PostgreSQL rule).
        let mut referencing_old: Option<String> = None;
        let mut referencing_new: Option<String> = None;
        for r in &ct.referencing {
            let alias = object_name_parts(&r.transition_relation_name)
                .into_iter()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            match r.refer_type {
                TriggerReferencingType::OldTable => {
                    if referencing_old.is_some() {
                        return Err(SqlError::Syntax(
                            "duplicate OLD TABLE referencing clause".into(),
                        ));
                    }
                    referencing_old = Some(alias);
                }
                TriggerReferencingType::NewTable => {
                    if referencing_new.is_some() {
                        return Err(SqlError::Syntax(
                            "duplicate NEW TABLE referencing clause".into(),
                        ));
                    }
                    referencing_new = Some(alias);
                }
            }
        }

        // DEFERRABLE — only allowed on CONSTRAINT TRIGGERs.
        let is_constraint = ct.is_constraint;
        let (deferrable, initially_deferred) = if let Some(chars) = &ct.characteristics {
            if !is_constraint {
                return Err(SqlError::Syntax(
                    "DEFERRABLE is only allowed for CONSTRAINT TRIGGER".into(),
                ));
            }
            let defer = chars.deferrable.unwrap_or(false);
            let init_defer = matches!(chars.initially, Some(DeferrableInitial::Deferred));
            (defer, init_defer)
        } else {
            (false, false)
        };

        if ct.statements.is_some() || ct.exec_body.is_none() {
            return Err(unsupported("trigger body without EXECUTE FUNCTION"));
        }
        let exec_body = ct.exec_body.as_ref().expect("checked above");
        if exec_body
            .func_desc
            .args
            .as_ref()
            .is_some_and(|a| !a.is_empty())
        {
            return Err(unsupported("trigger arguments (TG_ARGV)"));
        }

        // Target resolution. INSTEAD OF requires a view; BEFORE/AFTER require a table.
        let (schema, n) = split_schema_table(&ct.table_name);
        let q = self
            .catalog
            .resolve_table_name(schema.as_deref(), &n)
            .ok_or_else(|| SqlError::UndefinedTable(n.clone()))?;
        let on_view = self.catalog.get_view(&q).is_some();

        if on_view && timing != TriggerTiming::InsteadOf {
            return Err(SqlError::WrongObjectType(format!(
                "\"{}\" is a view — only INSTEAD OF triggers are supported on views",
                q.name
            )));
        }
        if !on_view && timing == TriggerTiming::InsteadOf {
            return Err(SqlError::WrongObjectType(format!(
                "\"{}\" is not a view — INSTEAD OF triggers require a view target",
                q.name
            )));
        }

        // Row/statement level; PostgreSQL defaults to STATEMENT when omitted.
        let level = match &ct.trigger_object {
            None => TriggerLevel::Statement,
            Some(TriggerObjectKind::For(o)) | Some(TriggerObjectKind::ForEach(o)) => match o {
                TriggerObject::Row => TriggerLevel::Row,
                TriggerObject::Statement => TriggerLevel::Statement,
            },
        };

        // Cross-field validation (PostgreSQL rules).
        if timing == TriggerTiming::InsteadOf && level != TriggerLevel::Row {
            return Err(SqlError::InvalidObjectDefinition(
                "INSTEAD OF triggers must be FOR EACH ROW".into(),
            ));
        }
        if is_constraint && timing != TriggerTiming::After {
            return Err(SqlError::InvalidObjectDefinition(
                "CONSTRAINT TRIGGER must be AFTER".into(),
            ));
        }
        if is_constraint && level != TriggerLevel::Row {
            return Err(SqlError::InvalidObjectDefinition(
                "CONSTRAINT TRIGGER must be FOR EACH ROW".into(),
            ));
        }
        if referencing_old.is_some() || referencing_new.is_some() {
            if timing != TriggerTiming::After {
                return Err(SqlError::InvalidObjectDefinition(
                    "REFERENCING is only valid for AFTER triggers".into(),
                ));
            }
            if level != TriggerLevel::Statement {
                return Err(SqlError::InvalidObjectDefinition(
                    "REFERENCING is only valid for FOR EACH STATEMENT triggers".into(),
                ));
            }
        }

        // Events (`INSERT OR UPDATE [OF cols] OR DELETE OR TRUNCATE`).
        let event_kind = |e: &TriggerEventDef| match e {
            TriggerEventDef::Insert => 0u8,
            TriggerEventDef::Update { .. } => 1,
            TriggerEventDef::Delete => 2,
            TriggerEventDef::Truncate => 3,
        };
        let mut events: Vec<TriggerEventDef> = Vec::new();
        for ev in &ct.events {
            let mapped = match ev {
                TriggerEvent::Insert => TriggerEventDef::Insert,
                TriggerEvent::Delete => TriggerEventDef::Delete,
                TriggerEvent::Truncate => {
                    // TRUNCATE requires FOR EACH STATEMENT and cannot be INSTEAD OF.
                    if level == TriggerLevel::Row {
                        return Err(SqlError::FeatureNotSupported(
                            "TRUNCATE FOR EACH ROW triggers are not supported".into(),
                        ));
                    }
                    if timing == TriggerTiming::InsteadOf {
                        return Err(SqlError::FeatureNotSupported(
                            "INSTEAD OF TRUNCATE triggers are not supported".into(),
                        ));
                    }
                    TriggerEventDef::Truncate
                }
                TriggerEvent::Update(cols) => {
                    let mut columns = Vec::new();
                    if !on_view {
                        let table = self.catalog.require_table(&q)?.clone();
                        for c in cols {
                            let cname = ident_name(c);
                            if table.column(&cname).is_none() {
                                return Err(SqlError::UndefinedColumn(cname));
                            }
                            columns.push(cname);
                        }
                    } else {
                        for c in cols {
                            columns.push(ident_name(c));
                        }
                    }
                    TriggerEventDef::Update { columns }
                }
            };
            if events.iter().any(|e| event_kind(e) == event_kind(&mapped)) {
                return Err(SqlError::Syntax(
                    "duplicate trigger events specified".into(),
                ));
            }
            events.push(mapped);
        }
        if events.is_empty() {
            return Err(SqlError::Syntax(
                "CREATE TRIGGER requires at least one event".into(),
            ));
        }

        // WHEN condition (row triggers only; stored as raw SQL text).
        let when_expr = match &ct.condition {
            None => None,
            Some(cond) => {
                if level == TriggerLevel::Statement {
                    return Err(unsupported("WHEN conditions on statement-level triggers"));
                }
                if on_view {
                    // For views we can't validate column refs without a Table.
                    Some(cond.to_string())
                } else {
                    let table = self.catalog.require_table(&q)?.clone();
                    let mut refs = WhenRefs::default();
                    validate_when_expr(cond, &table, &mut refs)?;
                    let has_insert = events.iter().any(|e| matches!(e, TriggerEventDef::Insert));
                    let has_delete = events.iter().any(|e| matches!(e, TriggerEventDef::Delete));
                    if refs.old && has_insert {
                        return Err(SqlError::InvalidObjectDefinition(
                            "INSERT trigger's WHEN condition cannot reference OLD values".into(),
                        ));
                    }
                    if refs.new && has_delete {
                        return Err(SqlError::InvalidObjectDefinition(
                            "DELETE trigger's WHEN condition cannot reference NEW values".into(),
                        ));
                    }
                    Some(cond.to_string())
                }
            }
        };

        // Trigger function: resolved once at DDL time.
        let (fschema, fname) = split_schema_table(&exec_body.func_desc.name);
        let def = self
            .catalog
            .find_function(fschema.as_deref(), &fname, 0)
            .ok_or_else(|| SqlError::UndefinedFunction(format!("{fname}()")))?;
        if !def.returns_trigger {
            return Err(SqlError::InvalidObjectDefinition(format!(
                "function {fname} must return type trigger"
            )));
        }
        let function_schema = def.schema.clone();
        let function_name = def.name.clone();

        // Trigger name: per-table/view namespace, never schema-qualified.
        let name_parts = object_name_parts(&ct.name);
        if name_parts.len() > 1 {
            return Err(SqlError::Syntax("trigger name cannot be qualified".into()));
        }
        let name = name_parts.into_iter().next().unwrap_or_default();
        if name.is_empty() {
            return Err(SqlError::Syntax("trigger name cannot be empty".into()));
        }

        let oid = if on_view {
            let view = self.catalog.get_view(&q).expect("resolved above");
            let existing_oid = view.trigger(&name).map(|t| t.oid);
            if existing_oid.is_some() && !ct.or_replace {
                return Err(SqlError::DuplicateObject(format!(
                    "trigger \"{name}\" for relation \"{}\"",
                    q.name
                )));
            }
            existing_oid.unwrap_or_else(|| self.catalog.allocate_oid())
        } else {
            let table = self.catalog.require_table(&q)?.clone();
            let existing_oid = table.trigger(&name).map(|t| t.oid);
            if existing_oid.is_some() && !ct.or_replace {
                return Err(SqlError::DuplicateObject(format!(
                    "trigger \"{name}\" for relation \"{}\"",
                    q.name
                )));
            }
            existing_oid.unwrap_or_else(|| self.catalog.allocate_oid())
        };

        let trg = TriggerDef {
            oid,
            name: name.clone(),
            timing,
            events,
            level,
            when_expr,
            function_schema,
            function_name,
            enabled: true,
            is_constraint,
            deferrable,
            initially_deferred,
            referencing_old,
            referencing_new,
            on_view,
        };

        if on_view {
            let view = self.catalog.get_view_mut(&q).expect("resolved above");
            match view.triggers.iter_mut().find(|t| t.name == name) {
                Some(slot) => *slot = trg,
                None => view.triggers.push(trg),
            }
        } else {
            let table = self.catalog.get_table_mut(&q).expect("resolved above");
            match table.triggers.iter_mut().find(|t| t.name == name) {
                Some(slot) => *slot = trg,
                None => table.triggers.push(trg),
            }
        }
        self.catalog_dirty = true;
        Ok(ExecResult::empty_command("CREATE TRIGGER"))
    }

    pub fn exec_drop_trigger(&mut self, dt: &DropTrigger) -> Result<ExecResult> {
        let Some(table_name) = &dt.table_name else {
            return Err(SqlError::Syntax("DROP TRIGGER requires ON <table>".into()));
        };
        let name_parts = object_name_parts(&dt.trigger_name);
        if name_parts.len() > 1 {
            return Err(SqlError::Syntax("trigger name cannot be qualified".into()));
        }
        let name = name_parts.into_iter().next().unwrap_or_default();
        let (schema, n) = split_schema_table(table_name);
        let q = self
            .catalog
            .resolve_table_name(schema.as_deref(), &n)
            .ok_or_else(|| SqlError::UndefinedTable(n.clone()))?;

        // `CASCADE`/`RESTRICT` are accepted and ignored: no dependents.
        let removed = if self.catalog.get_view(&q).is_some() {
            self.catalog
                .get_view_mut(&q)
                .map(|view| {
                    let before = view.triggers.len();
                    view.triggers.retain(|t| t.name != name);
                    view.triggers.len() != before
                })
                .unwrap_or(false)
        } else {
            self.catalog
                .get_table_mut(&q)
                .map(|table| {
                    let before = table.triggers.len();
                    table.triggers.retain(|t| t.name != name);
                    table.triggers.len() != before
                })
                .unwrap_or(false)
        };

        if !removed && !dt.if_exists {
            return Err(SqlError::UndefinedObject(format!(
                "trigger \"{name}\" for table \"{}\"",
                q.name
            )));
        }
        self.catalog_dirty = true;
        Ok(ExecResult::empty_command("DROP TRIGGER"))
    }

    /// `ALTER TABLE ... ENABLE/DISABLE TRIGGER [name | ALL | USER]`.
    pub(crate) fn exec_set_trigger_enabled(
        &mut self,
        q: &QualifiedName,
        name: &Ident,
        enabled: bool,
    ) -> Result<()> {
        let target = ident_name(name);
        // Try table first; views can also carry INSTEAD OF triggers.
        if let Some(table) = self.catalog.get_table_mut(q) {
            if target == "all" || target == "user" {
                for trg in &mut table.triggers {
                    trg.enabled = enabled;
                }
                return Ok(());
            }
            return match table.triggers.iter_mut().find(|t| t.name == target) {
                Some(trg) => {
                    trg.enabled = enabled;
                    Ok(())
                }
                None => Err(SqlError::UndefinedObject(format!(
                    "trigger \"{target}\" for table \"{}\"",
                    q.to_string_qualified()
                ))),
            };
        }
        if let Some(view) = self.catalog.get_view_mut(q) {
            if target == "all" || target == "user" {
                for trg in &mut view.triggers {
                    trg.enabled = enabled;
                }
                return Ok(());
            }
            return match view.triggers.iter_mut().find(|t| t.name == target) {
                Some(trg) => {
                    trg.enabled = enabled;
                    Ok(())
                }
                None => Err(SqlError::UndefinedObject(format!(
                    "trigger \"{target}\" for table \"{}\"",
                    q.to_string_qualified()
                ))),
            };
        }
        Err(SqlError::UndefinedTable(q.to_string_qualified()))
    }
}

/// Which of `NEW`/`OLD` a WHEN condition references.
#[derive(Default)]
struct WhenRefs {
    new: bool,
    old: bool,
}

/// DDL-time validation of a trigger `WHEN` condition.
fn validate_when_expr(expr: &Expr, table: &Table, refs: &mut WhenRefs) -> Result<()> {
    match expr {
        Expr::Identifier(ident) => {
            let name = ident_name(ident);
            if matches!(name.as_str(), "true" | "false" | "null") {
                return Ok(());
            }
            Err(SqlError::UndefinedColumn(name))
        }
        Expr::CompoundIdentifier(parts) => {
            let names: Vec<String> = parts.iter().map(ident_name).collect();
            match names.as_slice() {
                [q, col] if q == "new" || q == "old" => {
                    if table.column(col).is_none() {
                        return Err(SqlError::UndefinedColumn(format!("{q}.{col}")));
                    }
                    if q == "new" {
                        refs.new = true;
                    } else {
                        refs.old = true;
                    }
                    Ok(())
                }
                _ => Err(SqlError::UndefinedTable(
                    names.first().cloned().unwrap_or_default(),
                )),
            }
        }
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err(unsupported("subqueries in trigger WHEN conditions"))
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_when_expr(left, table, refs)?;
            validate_when_expr(right, table, refs)
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            validate_when_expr(left, table, refs)?;
            validate_when_expr(right, table, refs)
        }
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            validate_when_expr(a, table, refs)?;
            validate_when_expr(b, table, refs)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner)
        | Expr::Cast { expr: inner, .. } => validate_when_expr(inner, table, refs),
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            validate_when_expr(inner, table, refs)?;
            validate_when_expr(low, table, refs)?;
            validate_when_expr(high, table, refs)
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            validate_when_expr(inner, table, refs)?;
            for e in list {
                validate_when_expr(e, table, refs)?;
            }
            Ok(())
        }
        Expr::Like {
            expr: inner,
            pattern,
            ..
        }
        | Expr::ILike {
            expr: inner,
            pattern,
            ..
        } => {
            validate_when_expr(inner, table, refs)?;
            validate_when_expr(pattern, table, refs)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                validate_when_expr(o, table, refs)?;
            }
            for w in conditions {
                validate_when_expr(&w.condition, table, refs)?;
                validate_when_expr(&w.result, table, refs)?;
            }
            if let Some(e) = else_result {
                validate_when_expr(e, table, refs)?;
            }
            Ok(())
        }
        Expr::Function(func) => {
            if let sqlparser::ast::FunctionArguments::List(list) = &func.args {
                for arg in &list.args {
                    let e = match arg {
                        sqlparser::ast::FunctionArg::Named { arg, .. }
                        | sqlparser::ast::FunctionArg::ExprNamed { arg, .. }
                        | sqlparser::ast::FunctionArg::Unnamed(arg) => arg,
                    };
                    if let sqlparser::ast::FunctionArgExpr::Expr(e) = e {
                        validate_when_expr(e, table, refs)?;
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Deferred constraint trigger
// ---------------------------------------------------------------------------

/// A CONSTRAINT TRIGGER firing deferred to `COMMIT` because the constraint is
/// currently running in `DEFERRED` mode (see
/// [`crate::sql::exec::ConstraintModes`]). Queued on
/// [`crate::sql::exec::Exec::deferred_triggers`] during statement execution;
/// the engine splices these into `engine::Transaction::deferred_triggers` and
/// fires all of them at `COMMIT`.
pub(crate) struct DeferredTriggerFiring {
    pub trigger: TriggerDef,
    pub table: Table,
    pub op: TriggerOp,
    pub old_row: Option<RowValues>,
    pub new_row: Option<RowValues>,
}

// ---------------------------------------------------------------------------
// Firing engine
// ---------------------------------------------------------------------------

/// Build a [`RowSet`] from a slice of row maps for use as a REFERENCING
/// transition table CTE injected into `exec.cte` before calling a statement
/// trigger with `REFERENCING NEW TABLE AS alias` or `REFERENCING OLD TABLE AS
/// alias`. The alias becomes the table qualifier in the resulting schema so
/// the trigger body can reference `alias.col`.
fn build_rowset_for_table(table: &Table, alias: &str, rows: &[RowValues]) -> RowSet {
    let schema = RowSchema::new(
        table
            .columns
            .iter()
            .map(|c| FieldRef {
                table: Some(alias.to_string()),
                name: c.name.clone(),
                ty: c.ty.clone(),
            })
            .collect(),
    );
    let tuples = rows.iter().map(|r| row_tuple(table, r)).collect();
    RowSet {
        schema,
        rows: tuples,
    }
}

/// Does `e` match a firing of `op`?
fn event_matches(e: &TriggerEventDef, op: TriggerOp, set_cols: Option<&BTreeSet<String>>) -> bool {
    match (e, op) {
        (TriggerEventDef::Insert, TriggerOp::Insert) => true,
        (TriggerEventDef::Delete, TriggerOp::Delete) => true,
        (TriggerEventDef::Truncate, TriggerOp::Truncate) => true,
        (TriggerEventDef::Update { columns }, TriggerOp::Update) => {
            columns.is_empty() || set_cols.is_some_and(|s| columns.iter().any(|c| s.contains(c)))
        }
        _ => false,
    }
}

/// Enabled triggers on `table` matching (timing, level, op), in name order.
fn triggers_for(
    table: &Table,
    timing: TriggerTiming,
    level: TriggerLevel,
    op: TriggerOp,
    set_cols: Option<&BTreeSet<String>>,
) -> Vec<TriggerDef> {
    let mut out: Vec<TriggerDef> = table
        .triggers
        .iter()
        .filter(|t| t.enabled && t.timing == timing && t.level == level)
        .filter(|t| t.events.iter().any(|e| event_matches(e, op, set_cols)))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Coerce every column of a trigger-returned row to its declared type.
fn coerce_trigger_row(table: &Table, row: RowValues) -> Result<RowValues> {
    let mut out = RowValues::new();
    for (col, v) in row {
        let coerced = crate::sql::dml::coerce_to_col(v, table, &col)?;
        out.insert(col, coerced);
    }
    Ok(out)
}

impl Exec {
    /// BEFORE ... FOR EACH ROW chain for one candidate row.
    pub(crate) fn fire_before_row(
        &mut self,
        table: &Table,
        op: TriggerOp,
        old: Option<&RowValues>,
        new: Option<RowValues>,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<Option<RowValues>> {
        let triggers = triggers_for(
            table,
            TriggerTiming::Before,
            TriggerLevel::Row,
            op,
            set_cols,
        );
        let mut new_row = new;
        for trg in &triggers {
            if !self.trigger_when_passes(trg, table, old, new_row.as_ref())? {
                continue;
            }
            let def = self.resolve_trigger_function(trg)?;
            let result = crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table,
                    op: op.as_sql(),
                    timing: TriggerTiming::Before,
                    level: TriggerLevel::Row,
                    old: old.cloned(),
                    new: if op == TriggerOp::Delete {
                        None
                    } else {
                        new_row.clone()
                    },
                },
            )?;
            match op {
                TriggerOp::Delete => {
                    if result.is_none() {
                        return Ok(None);
                    }
                }
                TriggerOp::Insert | TriggerOp::Update => match result {
                    None => return Ok(None),
                    Some(row) => new_row = Some(coerce_trigger_row(table, row)?),
                },
                TriggerOp::Truncate => {}
            }
        }
        Ok(match op {
            TriggerOp::Delete => old.cloned(),
            TriggerOp::Insert | TriggerOp::Update => new_row,
            TriggerOp::Truncate => None,
        })
    }

    /// AFTER ... FOR EACH ROW.
    ///
    /// CONSTRAINT TRIGGERs that are `DEFERRABLE` and currently running in
    /// `DEFERRED` mode (inside an explicit transaction with matching
    /// `SET CONSTRAINTS` / `INITIALLY DEFERRED` state) are queued on
    /// [`Exec::deferred_triggers`] instead of firing immediately; the engine
    /// drains and fires them at `COMMIT`.
    pub(crate) fn fire_after_row(
        &mut self,
        table: &Table,
        op: TriggerOp,
        old: Option<&RowValues>,
        new: Option<&RowValues>,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<()> {
        let triggers = triggers_for(table, TriggerTiming::After, TriggerLevel::Row, op, set_cols);
        let table_q = crate::relational::catalog::QualifiedName {
            schema: table.schema.clone(),
            name: table.name.clone(),
        };
        for trg in &triggers {
            if !self.trigger_when_passes(trg, table, old, new)? {
                continue;
            }
            // CONSTRAINT TRIGGER: defer to COMMIT when the constraint is
            // currently running in DEFERRED mode and we are inside an explicit
            // transaction (constraint_modes.is_some()).
            if trg.is_constraint && trg.deferrable {
                let currently_deferred = if let Some(modes) = &self.constraint_modes {
                    if let Some(&v) = modes.named.get(&(table_q.clone(), trg.name.clone())) {
                        v
                    } else {
                        modes.all_deferred.unwrap_or(trg.initially_deferred)
                    }
                } else {
                    false // Autocommit: always check immediately
                };
                if currently_deferred {
                    self.deferred_triggers.push(DeferredTriggerFiring {
                        trigger: trg.clone(),
                        table: table.clone(),
                        op,
                        old_row: old.cloned(),
                        new_row: new.cloned(),
                    });
                    continue;
                }
            }
            let def = self.resolve_trigger_function(trg)?;
            crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table,
                    op: op.as_sql(),
                    timing: TriggerTiming::After,
                    level: TriggerLevel::Row,
                    old: old.cloned(),
                    new: new.cloned(),
                },
            )?;
        }
        Ok(())
    }

    /// FOR EACH STATEMENT triggers fired exactly once per statement.
    pub(crate) fn fire_statement_triggers(
        &mut self,
        table: &Table,
        timing: TriggerTiming,
        op: TriggerOp,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<()> {
        self.fire_statement_triggers_ex(table, timing, op, set_cols, &[], &[])
    }

    /// Like [`fire_statement_triggers`], but accepts optional transition-table
    /// rows for `REFERENCING NEW TABLE / OLD TABLE` clauses (PostgreSQL §
    /// statement-level triggers with transition tables).
    ///
    /// Each trigger whose definition names a `referencing_new` alias receives
    /// the supplied `new_rows` injected into `exec.cte` under that alias
    /// before the trigger body runs; similarly for `referencing_old`.  The
    /// CTE entry is removed after each trigger call so it does not leak into
    /// subsequent statements.  Other triggers (no REFERENCING clause) are
    /// unaffected.
    pub(crate) fn fire_statement_triggers_ex(
        &mut self,
        table: &Table,
        timing: TriggerTiming,
        op: TriggerOp,
        set_cols: Option<&BTreeSet<String>>,
        new_rows: &[RowValues],
        old_rows: &[RowValues],
    ) -> Result<()> {
        let triggers = triggers_for(table, timing, TriggerLevel::Statement, op, set_cols);
        for trg in &triggers {
            // Inject transition tables into exec.cte before the trigger fires.
            let new_alias = trg.referencing_new.clone();
            let old_alias = trg.referencing_old.clone();
            if let Some(alias) = &new_alias {
                let rowset = build_rowset_for_table(table, alias, new_rows);
                self.cte.insert(alias.clone(), rowset);
            }
            if let Some(alias) = &old_alias {
                let rowset = build_rowset_for_table(table, alias, old_rows);
                self.cte.insert(alias.clone(), rowset);
            }

            let def = self.resolve_trigger_function(trg)?;
            let result = crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table,
                    op: op.as_sql(),
                    timing,
                    level: TriggerLevel::Statement,
                    old: None,
                    new: None,
                },
            );

            // Remove transition-table CTEs regardless of whether the call
            // succeeded — they must not persist into subsequent statements.
            if let Some(alias) = &new_alias {
                self.cte.remove(alias);
            }
            if let Some(alias) = &old_alias {
                self.cte.remove(alias);
            }
            result?;
        }
        Ok(())
    }

    /// INSTEAD OF ... FOR EACH ROW chain for a DML on a view.
    ///
    /// Returns the (possibly modified) NEW row, or `None` when a trigger
    /// returns `NULL` (DML row suppression, same as BEFORE ROW).
    pub(crate) fn fire_instead_of_row(
        &mut self,
        view_q: &QualifiedName,
        op: TriggerOp,
        old: Option<&RowValues>,
        new: Option<RowValues>,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<Option<RowValues>> {
        let view = match self.catalog.get_view(view_q) {
            Some(v) => v.clone(),
            None => return Ok(new),
        };

        // Synthetic table for trigger invocation (view columns only).
        let table = synthetic_table_for_view(&view);

        let trigs: Vec<TriggerDef> = view
            .triggers
            .iter()
            .filter(|t| {
                t.enabled
                    && t.timing == TriggerTiming::InsteadOf
                    && t.level == TriggerLevel::Row
                    && t.events.iter().any(|e| event_matches(e, op, set_cols))
            })
            .cloned()
            .collect();

        let mut trigs = trigs;
        trigs.sort_by(|a, b| a.name.cmp(&b.name));

        let mut new_row = new;
        for trg in &trigs {
            let def = self.resolve_trigger_function(trg)?;
            let result = crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table: &table,
                    op: op.as_sql(),
                    timing: TriggerTiming::InsteadOf,
                    level: TriggerLevel::Row,
                    old: old.cloned(),
                    new: new_row.clone(),
                },
            )?;
            match op {
                TriggerOp::Delete => {
                    if result.is_none() {
                        // Trigger returned NULL: suppress this row.
                        return Ok(None);
                    }
                }
                TriggerOp::Insert | TriggerOp::Update => match result {
                    None => return Ok(None),
                    Some(row) => new_row = Some(row),
                },
                TriggerOp::Truncate => {}
            }
        }
        // For DELETE, return the old row as a sentinel so callers can
        // distinguish "trigger ran, not suppressed" (Some) from "suppressed" (None).
        if op == TriggerOp::Delete {
            Ok(old.cloned())
        } else {
            Ok(new_row)
        }
    }

    /// Fire every deferred CONSTRAINT TRIGGER firing in `firings` (collected
    /// across the transaction by the engine) against the committed state
    /// already loaded into `self.tables`. Called at `COMMIT` by the engine.
    pub(crate) fn fire_deferred(&mut self, firings: Vec<DeferredTriggerFiring>) -> Result<()> {
        for f in firings {
            let DeferredTriggerFiring {
                trigger,
                table,
                op,
                old_row,
                new_row,
            } = f;
            let def = self.resolve_trigger_function(&trigger)?;
            crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trigger.name,
                    table: &table,
                    op: op.as_sql(),
                    timing: TriggerTiming::After,
                    level: TriggerLevel::Row,
                    old: old_row,
                    new: new_row,
                },
            )?;
        }
        Ok(())
    }

    /// Resolve a trigger's stored (schema, name) to its function definition.
    pub(crate) fn resolve_trigger_function(&self, trg: &TriggerDef) -> Result<FunctionDef> {
        self.catalog
            .find_function(Some(&trg.function_schema), &trg.function_name, 0)
            .cloned()
            .ok_or_else(|| SqlError::UndefinedFunction(format!("{}()", trg.function_name)))
    }

    /// Evaluate a row trigger's `WHEN` condition against NEW/OLD frames.
    fn trigger_when_passes(
        &self,
        trg: &TriggerDef,
        table: &Table,
        old: Option<&RowValues>,
        new: Option<&RowValues>,
    ) -> Result<bool> {
        let Some(text) = &trg.when_expr else {
            return Ok(true);
        };
        let expr = crate::sql::parser::parse_expr(text)?;
        let old_schema = table_schema_named(table, "old");
        let new_schema = table_schema_named(table, "new");
        let old_tuple = old.map(|r| row_tuple(table, r));
        let new_tuple = new.map(|r| row_tuple(table, r));
        let mut frames: Vec<Frame> = Vec::new();
        if let Some(t) = &old_tuple {
            frames.push(Frame {
                schema: &old_schema,
                row: t,
            });
        }
        if let Some(t) = &new_tuple {
            frames.push(Frame {
                schema: &new_schema,
                row: t,
            });
        }
        Ok(self.eval(&expr, &frames)?.truthy() == Some(true))
    }
}

/// Build a synthetic `Table` for a view (column names only, all nullable text)
/// so that trigger firing infrastructure can work against views.
fn synthetic_table_for_view(view: &crate::relational::catalog::View) -> Table {
    use crate::relational::catalog::Column;
    use crate::relational::types::SqlType;
    let mut t = Table {
        oid: view.oid,
        schema: view.schema.clone(),
        name: view.name.clone(),
        columns: view
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| Column {
                name: col.clone(),
                ty: SqlType::Text,
                nullable: true,
                default: None,
                identity_sequence: None,
                ordinal: i,
            })
            .collect(),
        primary_key: None,
        uniques: Vec::new(),
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        storage_collection: format!("view:{}.{}", view.schema, view.name),
        rls_enabled: false,
        rls_forced: false,
        policies: Vec::new(),
        triggers: Vec::new(),
        column_map: std::collections::HashMap::new(),
    };
    t.rebuild_column_map();
    t
}
