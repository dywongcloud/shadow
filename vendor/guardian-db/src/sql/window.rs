//! Window-function evaluation (`... OVER ...`).
//!
//! Window calls are computed by the SELECT pipeline after WHERE / GROUP BY /
//! HAVING and before DISTINCT / ORDER BY / LIMIT (PostgreSQL evaluation
//! order). Each distinct call is evaluated over the whole input — partitioned
//! and sorted per its window specification — and its per-row results are
//! stored in a map keyed by the call's SQL text; the evaluator resolves
//! window calls from that map.
//!
//! Supported subset (out-of-subset constructs fail typed, never silently):
//! the ranking functions, lag/lead, first/last/nth_value, and every regular
//! aggregate as a window aggregate (reusing the GROUP BY fold); ROWS and
//! RANGE frames with UNBOUNDED/CURRENT ROW bounds plus `N` offsets in ROWS
//! mode; named windows (`WINDOW w AS ...`) including refinement. GROUPS
//! frames and RANGE offset frames are rejected with `0A000`.

use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::{Exec, Frame};
use crate::sql::funcs;
use crate::sql::names::{function_dispatch_name, ident_name};
use crate::sql::row::{RowSchema, Tuple};
use crate::sql::select::{aggregate_arg, compare_sort, expr_has_window, walk_expr};
use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, NamedWindowDefinition,
    NamedWindowExpr, OrderBy, OrderByKind, Select, SelectItem, WindowFrame, WindowFrameBound,
    WindowFrameUnits, WindowSpec, WindowType,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// A frame bound with its offset (for `N PRECEDING/FOLLOWING`) evaluated.
#[derive(Clone, Copy)]
enum Bound {
    UnboundedPreceding,
    Preceding(i64),
    CurrentRow,
    Following(i64),
    UnboundedFollowing,
}

/// A resolved frame clause.
struct FrameSpec {
    /// ROWS mode (positional) vs RANGE mode (peer groups).
    rows: bool,
    start: Bound,
    end: Bound,
}

impl Exec {
    /// Compute per-row window values for every window call in the SELECT list
    /// and final ORDER BY. `rows` are the window input (filtered rows, or the
    /// surviving group representatives with `aggs_per_row` their aggregate
    /// maps). Returns `None` when the query has no window calls.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compute_window_maps(
        &self,
        select: &Select,
        order_by: Option<&OrderBy>,
        schema: &RowSchema,
        rows: &[Tuple],
        aggs_per_row: Option<&[HashMap<String, SqlValue>]>,
        outer: &[Frame],
    ) -> Result<Option<Vec<HashMap<String, SqlValue>>>> {
        let calls = collect_window_calls(select, order_by);
        if calls.is_empty() {
            return Ok(None);
        }
        let mut maps: Vec<HashMap<String, SqlValue>> = vec![HashMap::new(); rows.len()];
        for func in &calls {
            self.compute_window_call(func, select, schema, rows, aggs_per_row, outer, &mut maps)?;
        }
        Ok(Some(maps))
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_window_call(
        &self,
        func: &Function,
        select: &Select,
        schema: &RowSchema,
        rows: &[Tuple],
        aggs_per_row: Option<&[HashMap<String, SqlValue>]>,
        outer: &[Frame],
        maps: &mut [HashMap<String, SqlValue>],
    ) -> Result<()> {
        let key = func.to_string();
        let name = function_dispatch_name(&func.name);
        let is_agg = funcs::is_aggregate(&name);
        if !is_agg && !is_builtin_window_function(&name) {
            return Err(SqlError::WrongObjectType(format!(
                "OVER specified, but {name} is not a window function nor an aggregate function"
            )));
        }
        if func.null_treatment.is_some() {
            return Err(SqlError::FeatureNotSupported(
                "IGNORE NULLS/RESPECT NULLS on window functions is not supported".into(),
            ));
        }
        if !func.within_group.is_empty() {
            return Err(SqlError::FeatureNotSupported(
                "WITHIN GROUP on window functions is not supported".into(),
            ));
        }
        if let FunctionArguments::List(list) = &func.args {
            if matches!(
                list.duplicate_treatment,
                Some(sqlparser::ast::DuplicateTreatment::Distinct)
            ) {
                return Err(SqlError::FeatureNotSupported(
                    "DISTINCT is not implemented for window functions".into(),
                ));
            }
            if !list.clauses.is_empty() {
                return Err(SqlError::FeatureNotSupported(
                    "argument clauses inside a window function call are not supported".into(),
                ));
            }
        }
        if func.filter.is_some() && !is_agg {
            return Err(SqlError::FeatureNotSupported(
                "FILTER is not implemented for non-aggregate window functions".into(),
            ));
        }

        let spec = resolve_window_type(func.over.as_ref().unwrap(), &select.named_window)?;
        // Window calls cannot be nested inside a window call's arguments or
        // its window specification.
        let mut inner_exprs: Vec<&Expr> = Vec::new();
        if let FunctionArguments::List(list) = &func.args {
            for arg in &list.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } = arg
                {
                    inner_exprs.push(e);
                }
            }
        }
        inner_exprs.extend(spec.partition_by.iter());
        inner_exprs.extend(spec.order_by.iter().map(|o| &o.expr));
        if inner_exprs.into_iter().any(expr_has_window) {
            return Err(SqlError::WindowingError(
                "window function calls cannot be nested".into(),
            ));
        }

        // Argument expressions + arity (the aggregate path reuses the GROUP
        // BY argument extraction, so `count(*)` etc. work unchanged).
        let args: Vec<&Expr> = if is_agg {
            Vec::new()
        } else {
            plain_arg_exprs(func, &name)?
        };
        if !is_agg {
            let ok = match name.as_str() {
                "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
                    args.is_empty()
                }
                "ntile" | "first_value" | "last_value" => args.len() == 1,
                "lag" | "lead" => (1..=3).contains(&args.len()),
                "nth_value" => args.len() == 2,
                _ => true,
            };
            if !ok {
                return Err(SqlError::UndefinedFunction(format!(
                    "{name} with {} argument(s)",
                    args.len()
                )));
            }
        }
        let (agg_arg, agg_star) = if is_agg {
            let (_, a, s) = aggregate_arg(func)?;
            (a, s)
        } else {
            (None, false)
        };

        let frame = self.resolve_frame(spec.window_frame.as_ref(), outer)?;

        let eval_at = |expr: &Expr, i: usize| -> Result<SqlValue> {
            let mut frames: Vec<Frame> = outer
                .iter()
                .map(|f| Frame {
                    schema: f.schema,
                    row: f.row,
                })
                .collect();
            frames.push(Frame {
                schema,
                row: &rows[i],
            });
            self.eval_opt_agg(expr, &frames, aggs_per_row.map(|a| &a[i]))
        };

        // Partition the input rows (original order preserved within each).
        let mut part_index: HashMap<Vec<String>, usize> = HashMap::new();
        let mut partitions: Vec<Vec<usize>> = Vec::new();
        for i in 0..rows.len() {
            let pkey: Vec<String> = spec
                .partition_by
                .iter()
                .map(|e| eval_at(e, i).map(|v| v.index_key()))
                .collect::<Result<_>>()?;
            match part_index.get(&pkey) {
                Some(&pi) => partitions[pi].push(i),
                None => {
                    part_index.insert(pkey, partitions.len());
                    partitions.push(vec![i]);
                }
            }
        }

        // Window ORDER BY keys per row, with PostgreSQL NULL-ordering
        // defaults (ASC → NULLS LAST, DESC → NULLS FIRST).
        let directions: Vec<(bool, bool)> = spec
            .order_by
            .iter()
            .map(|o| {
                let asc = o.options.asc.unwrap_or(true);
                (asc, o.options.nulls_first.unwrap_or(!asc))
            })
            .collect();
        let mut order_keys: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
        for i in 0..rows.len() {
            let mut keys = Vec::with_capacity(spec.order_by.len());
            for o in &spec.order_by {
                keys.push(eval_at(&o.expr, i)?);
            }
            order_keys.push(keys);
        }
        let cmp = |a: usize, b: usize| -> Ordering {
            for (k, (asc, nf)) in directions.iter().enumerate() {
                let ord = compare_sort(&order_keys[a][k], &order_keys[b][k], *asc, *nf);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        };

        for part in &mut partitions {
            part.sort_by(|&a, &b| cmp(a, b));
            let n = part.len();
            // Peer-group bounds per position: rows tied under the window
            // ORDER BY are peers (all rows, when there is no ORDER BY).
            let mut g_start = vec![0usize; n];
            let mut g_end = vec![0usize; n];
            let mut group_ord = vec![0usize; n];
            let mut i = 0usize;
            let mut g = 0usize;
            while i < n {
                let mut j = i;
                while j + 1 < n && cmp(part[j + 1], part[i]) == Ordering::Equal {
                    j += 1;
                }
                for k in i..=j {
                    g_start[k] = i;
                    g_end[k] = j;
                    group_ord[k] = g;
                }
                g += 1;
                i = j + 1;
            }

            // ntile assigns buckets to the whole partition at once.
            let ntile_buckets: Option<Vec<SqlValue>> = if name == "ntile" {
                let v = eval_at(args[0], part[0])?;
                if v.is_null() {
                    Some(vec![SqlValue::Null; n])
                } else {
                    let b = v.as_i64().ok_or_else(|| SqlError::CannotCoerce {
                        from: v.type_of().name(),
                        to: "integer".into(),
                    })?;
                    if b <= 0 {
                        return Err(SqlError::InvalidParameter(
                            "argument of ntile must be greater than zero".into(),
                        ));
                    }
                    let b = b as usize;
                    let base = n / b;
                    let rem = n % b;
                    let mut out = Vec::with_capacity(n);
                    'fill: for k in 0..b {
                        let size = base + usize::from(k < rem);
                        for _ in 0..size {
                            if out.len() == n {
                                break 'fill;
                            }
                            out.push(SqlValue::Int4(k as i32 + 1));
                        }
                    }
                    Some(out)
                }
            } else {
                None
            };

            for (pos, &ri) in part.iter().enumerate() {
                let value = match name.as_str() {
                    // Ranking functions and lag/lead ignore the frame clause.
                    "row_number" => SqlValue::Int8(pos as i64 + 1),
                    "rank" => SqlValue::Int8(g_start[pos] as i64 + 1),
                    "dense_rank" => SqlValue::Int8(group_ord[pos] as i64 + 1),
                    "percent_rank" => {
                        if n <= 1 {
                            SqlValue::Float8(0.0)
                        } else {
                            SqlValue::Float8(g_start[pos] as f64 / (n - 1) as f64)
                        }
                    }
                    "cume_dist" => SqlValue::Float8((g_end[pos] + 1) as f64 / n as f64),
                    "ntile" => ntile_buckets.as_ref().unwrap()[pos].clone(),
                    "lag" | "lead" => {
                        let off = match args.get(1) {
                            None => Some(1i64),
                            Some(e) => {
                                let v = eval_at(e, ri)?;
                                if v.is_null() {
                                    None
                                } else {
                                    Some(v.as_i64().ok_or_else(|| SqlError::CannotCoerce {
                                        from: v.type_of().name(),
                                        to: "integer".into(),
                                    })?)
                                }
                            }
                        };
                        match off {
                            None => SqlValue::Null,
                            Some(off) => {
                                let target = if name == "lead" {
                                    pos as i64 + off
                                } else {
                                    pos as i64 - off
                                };
                                if target >= 0 && (target as usize) < n {
                                    eval_at(args[0], part[target as usize])?
                                } else {
                                    match args.get(2) {
                                        Some(d) => eval_at(d, ri)?,
                                        None => SqlValue::Null,
                                    }
                                }
                            }
                        }
                    }
                    // Frame-honouring value functions (the PostgreSQL
                    // last_value-needs-an-explicit-frame behaviour follows
                    // from the default frame ending at the peer group).
                    "first_value" | "last_value" | "nth_value" => {
                        match frame_bounds(&frame, pos, n, g_start[pos], g_end[pos]) {
                            None => SqlValue::Null,
                            Some((fs, fe)) => match name.as_str() {
                                "first_value" => eval_at(args[0], part[fs])?,
                                "last_value" => eval_at(args[0], part[fe])?,
                                _ => {
                                    let nv = eval_at(args[1], ri)?;
                                    if nv.is_null() {
                                        SqlValue::Null
                                    } else {
                                        let k =
                                            nv.as_i64().ok_or_else(|| SqlError::CannotCoerce {
                                                from: nv.type_of().name(),
                                                to: "integer".into(),
                                            })?;
                                        if k <= 0 {
                                            return Err(SqlError::InvalidParameter(
                                                "argument of nth_value must be greater than zero"
                                                    .into(),
                                            ));
                                        }
                                        let target = fs as i64 + k - 1;
                                        if target <= fe as i64 {
                                            eval_at(args[0], part[target as usize])?
                                        } else {
                                            SqlValue::Null
                                        }
                                    }
                                }
                            },
                        }
                    }
                    // Window aggregate over the frame, reusing the GROUP BY
                    // gather/fold logic (FILTER included).
                    _ => {
                        let mut values: Vec<SqlValue> = Vec::new();
                        let mut count_all = 0usize;
                        if let Some((fs, fe)) =
                            frame_bounds(&frame, pos, n, g_start[pos], g_end[pos])
                        {
                            for &fr in &part[fs..=fe] {
                                if let Some(filter) = &func.filter
                                    && eval_at(filter, fr)?.truthy() != Some(true)
                                {
                                    continue;
                                }
                                count_all += 1;
                                if agg_star {
                                    continue;
                                }
                                let v = eval_at(agg_arg.as_ref().unwrap(), fr)?;
                                if !v.is_null() {
                                    values.push(v);
                                }
                            }
                        }
                        self.fold_aggregate(&name, func, agg_star, values, count_all)?
                    }
                };
                maps[ri].insert(key.clone(), value);
            }
        }
        Ok(())
    }

    /// Resolve a frame clause: evaluate ROWS offsets (constants) and reject
    /// out-of-subset or invalid bound combinations.
    fn resolve_frame(&self, frame: Option<&WindowFrame>, outer: &[Frame]) -> Result<FrameSpec> {
        let Some(frame) = frame else {
            // Default frame: RANGE UNBOUNDED PRECEDING .. CURRENT ROW — for
            // aggregates this includes all peers of the current row.
            return Ok(FrameSpec {
                rows: false,
                start: Bound::UnboundedPreceding,
                end: Bound::CurrentRow,
            });
        };
        let rows = match frame.units {
            WindowFrameUnits::Rows => true,
            WindowFrameUnits::Range => false,
            WindowFrameUnits::Groups => {
                return Err(SqlError::FeatureNotSupported(
                    "GROUPS mode in window frames is not supported".into(),
                ));
            }
        };
        let offset = |e: &Expr, which: &str| -> Result<i64> {
            if !rows {
                return Err(SqlError::FeatureNotSupported(
                    "RANGE with offset PRECEDING/FOLLOWING is not supported in window frames"
                        .into(),
                ));
            }
            let v = self.eval(e, outer)?;
            let n = v.as_i64().ok_or_else(|| {
                SqlError::InvalidParameter(format!("frame {which} offset must not be null"))
            })?;
            if n < 0 {
                return Err(SqlError::InvalidParameter(format!(
                    "frame {which} offset must not be negative"
                )));
            }
            Ok(n)
        };
        let start = match &frame.start_bound {
            WindowFrameBound::CurrentRow => Bound::CurrentRow,
            WindowFrameBound::Preceding(None) => Bound::UnboundedPreceding,
            WindowFrameBound::Preceding(Some(e)) => Bound::Preceding(offset(e, "starting")?),
            WindowFrameBound::Following(Some(e)) => Bound::Following(offset(e, "starting")?),
            WindowFrameBound::Following(None) => {
                return Err(SqlError::WindowingError(
                    "frame start cannot be UNBOUNDED FOLLOWING".into(),
                ));
            }
        };
        let end = match &frame.end_bound {
            None | Some(WindowFrameBound::CurrentRow) => Bound::CurrentRow,
            Some(WindowFrameBound::Following(None)) => Bound::UnboundedFollowing,
            Some(WindowFrameBound::Following(Some(e))) => Bound::Following(offset(e, "ending")?),
            Some(WindowFrameBound::Preceding(Some(e))) => Bound::Preceding(offset(e, "ending")?),
            Some(WindowFrameBound::Preceding(None)) => {
                return Err(SqlError::WindowingError(
                    "frame end cannot be UNBOUNDED PRECEDING".into(),
                ));
            }
        };
        match (&start, &end) {
            (Bound::CurrentRow, Bound::Preceding(_)) => {
                return Err(SqlError::WindowingError(
                    "frame starting from current row cannot have preceding rows".into(),
                ));
            }
            (Bound::Following(_), Bound::Preceding(_))
            | (Bound::Following(_), Bound::CurrentRow) => {
                return Err(SqlError::WindowingError(
                    "frame starting from following row cannot have preceding rows".into(),
                ));
            }
            _ => {}
        }
        Ok(FrameSpec { rows, start, end })
    }
}

/// Inclusive frame positions within the sorted partition for the row at
/// `pos`, or `None` when the frame is empty. `g_start`/`g_end` are the
/// current row's peer-group bounds (used by RANGE mode).
fn frame_bounds(
    spec: &FrameSpec,
    pos: usize,
    n: usize,
    g_start: usize,
    g_end: usize,
) -> Option<(usize, usize)> {
    let p = pos as i64;
    let last = n as i64 - 1;
    let (s, e) = if spec.rows {
        let s = match spec.start {
            Bound::UnboundedPreceding => 0,
            Bound::Preceding(k) => p - k,
            Bound::CurrentRow => p,
            Bound::Following(k) => p + k,
            // Rejected in resolve_frame.
            Bound::UnboundedFollowing => unreachable!(),
        };
        let e = match spec.end {
            Bound::CurrentRow => p,
            Bound::UnboundedFollowing => last,
            Bound::Following(k) => p + k,
            Bound::Preceding(k) => p - k,
            Bound::UnboundedPreceding => unreachable!(),
        };
        (s, e)
    } else {
        // RANGE mode: only UNBOUNDED / CURRENT ROW bounds reach here; CURRENT
        // ROW extends to the current row's peer group.
        let s = match spec.start {
            Bound::UnboundedPreceding => 0,
            Bound::CurrentRow => g_start as i64,
            _ => unreachable!(),
        };
        let e = match spec.end {
            Bound::CurrentRow => g_end as i64,
            Bound::UnboundedFollowing => last,
            _ => unreachable!(),
        };
        (s, e)
    };
    if e < s || e < 0 || s > last {
        return None;
    }
    Some((s.max(0) as usize, e.min(last) as usize))
}

/// Every distinct window call (by SQL text) in the SELECT list and final
/// ORDER BY — the only clauses where window functions are allowed.
fn collect_window_calls(select: &Select, order_by: Option<&OrderBy>) -> Vec<Function> {
    let mut out: Vec<Function> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push_from = |e: &Expr| {
        walk_expr(e, &mut |inner| {
            if let Expr::Function(f) = inner
                && f.over.is_some()
                && seen.insert(f.to_string())
            {
                out.push(f.clone());
            }
        });
    };
    for item in &select.projection {
        if let SelectItem::UnnamedExpr(e)
        | SelectItem::ExprWithAlias { expr: e, .. }
        | SelectItem::ExprWithAliases { expr: e, .. } = item
        {
            push_from(e);
        }
    }
    if let Some(OrderBy {
        kind: OrderByKind::Expressions(exprs),
        ..
    }) = order_by
    {
        for ob in exprs {
            push_from(&ob.expr);
        }
    }
    out
}

fn is_builtin_window_function(name: &str) -> bool {
    matches!(
        name,
        "row_number"
            | "rank"
            | "dense_rank"
            | "percent_rank"
            | "cume_dist"
            | "ntile"
            | "lag"
            | "lead"
            | "first_value"
            | "last_value"
            | "nth_value"
    )
}

/// Positional argument expressions of a non-aggregate window function.
fn plain_arg_exprs<'a>(func: &'a Function, name: &str) -> Result<Vec<&'a Expr>> {
    match &func.args {
        FunctionArguments::None => Ok(Vec::new()),
        FunctionArguments::Subquery(_) => Err(SqlError::FeatureNotSupported(
            "subquery argument to a window function is not supported".into(),
        )),
        FunctionArguments::List(list) => {
            let mut out = Vec::with_capacity(list.args.len());
            for arg in &list.args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    } => out.push(e),
                    _ => {
                        return Err(SqlError::UndefinedFunction(format!("{name}(*)")));
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Resolve `OVER (...)` / `OVER w` to a concrete window specification,
/// applying the named-window inheritance rules for refinements.
fn resolve_window_type(over: &WindowType, named: &[NamedWindowDefinition]) -> Result<WindowSpec> {
    match over {
        WindowType::WindowSpec(spec) => resolve_spec(spec, named, named.len()),
        WindowType::NamedWindow(ident) => {
            let name = ident_name(ident);
            let (idx, expr) = lookup_named(&name, named, named.len())?;
            resolve_named_expr(expr, named, idx)
        }
    }
}

/// Look up a named window among the definitions before `upto` (a WINDOW
/// definition may only reference earlier definitions; `OVER` may reference
/// any, in which case `upto` is the full length).
fn lookup_named<'a>(
    name: &str,
    named: &'a [NamedWindowDefinition],
    upto: usize,
) -> Result<(usize, &'a NamedWindowExpr)> {
    named[..upto.min(named.len())]
        .iter()
        .enumerate()
        .find(|(_, d)| ident_name(&d.0) == name)
        .map(|(i, d)| (i, &d.1))
        .ok_or_else(|| SqlError::UndefinedObject(format!("window \"{name}\"")))
}

fn resolve_named_expr(
    expr: &NamedWindowExpr,
    named: &[NamedWindowDefinition],
    upto: usize,
) -> Result<WindowSpec> {
    match expr {
        NamedWindowExpr::NamedWindow(ident) => {
            let name = ident_name(ident);
            let (idx, inner) = lookup_named(&name, named, upto)?;
            resolve_named_expr(inner, named, idx)
        }
        NamedWindowExpr::WindowSpec(spec) => resolve_spec(spec, named, upto),
    }
}

/// Apply PostgreSQL's named-window refinement rules for a spec of the form
/// `(base_name [ORDER BY ...] [frame])`.
fn resolve_spec(
    spec: &WindowSpec,
    named: &[NamedWindowDefinition],
    upto: usize,
) -> Result<WindowSpec> {
    let Some(base_ident) = &spec.window_name else {
        return Ok(spec.clone());
    };
    let base_name = ident_name(base_ident);
    let (idx, base_expr) = lookup_named(&base_name, named, upto)?;
    let base = resolve_named_expr(base_expr, named, idx)?;
    if base.window_frame.is_some() {
        return Err(SqlError::WindowingError(format!(
            "cannot copy window \"{base_name}\" because it has a frame clause"
        )));
    }
    if !spec.partition_by.is_empty() {
        return Err(SqlError::WindowingError(format!(
            "cannot override PARTITION BY clause of window \"{base_name}\""
        )));
    }
    if !spec.order_by.is_empty() && !base.order_by.is_empty() {
        return Err(SqlError::WindowingError(format!(
            "cannot override ORDER BY clause of window \"{base_name}\""
        )));
    }
    Ok(WindowSpec {
        window_name: None,
        partition_by: base.partition_by,
        order_by: if spec.order_by.is_empty() {
            base.order_by
        } else {
            spec.order_by.clone()
        },
        window_frame: spec.window_frame.clone(),
    })
}
