//! The `cube` extension: multidimensional cubes and points.
//!
//! The `cube` type lives in the core value model
//! ([`crate::relational::SqlType::Cube`] / [`SqlValue::Cube`]) with the exact
//! contrib/cube text forms: `'1'`, `'1,2'`, `'(1,2)'`, `'(1,2),(3,4)'` and
//! `'[(1),(2)]'` in; `(1, 2)` / `(1, 2),(3, 4)` out, corners preserved as
//! given, coincident corners printed as a point. This module provides the
//! constructors `cube(float8)`, `cube(float8,float8)`, `cube(float8[])`,
//! `cube(float8[],float8[])` and the functions `cube_dim`, `cube_ll_coord`,
//! `cube_ur_coord` (both normalize per dimension and return 0 outside the
//! defined dimensions, like contrib/cube), `cube_is_point`, `cube_distance`,
//! `cube_union`, `cube_inter` (which may produce an "inverted" cube for
//! disjoint inputs, exactly like PostgreSQL) and `cube_enlarge`. Operators,
//! routed by [`super::dispatch_operator`]: `@>`/`<@` containment, `&&`
//! overlap and `<->` euclidean distance — all padding missing dimensions
//! with 0 for mixed-dimension operands.
//!
//! Not implemented: the `cube(cube, float8[, float8])` dimension-appending
//! constructors, the `~>` coordinate-extraction and `<#>`/`<=>` (taxicab /
//! chebyshev) distance operators, and the zero-dimensional cube `'()'`
//! (input requires at least one coordinate here).

use super::{
    ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_cube, arg_f64, arg_i64, bad_arg,
    missing_arg, no_such,
};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};

pub static DEF: ExtensionDef = ExtensionDef {
    name: "cube",
    default_version: "1.5",
    comment: "data type for multidimensional cubes",
    requires: &[],
    functions: &[
        "cube",
        "cube_dim",
        "cube_ll_coord",
        "cube_ur_coord",
        "cube_is_point",
        "cube_distance",
        "cube_union",
        "cube_inter",
        "cube_enlarge",
    ],
    types: &["cube"],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// The two corners of a cube, as stored.
pub(super) struct Corners {
    pub ll: Vec<f64>,
    pub ur: Vec<f64>,
}

impl Corners {
    fn dim(&self) -> usize {
        self.ll.len()
    }

    /// Normalized lower bound of dimension `i` (0-based; 0 beyond `dim`).
    pub(super) fn min(&self, i: usize) -> f64 {
        coord(&self.ll, i).min(coord(&self.ur, i))
    }

    /// Normalized upper bound of dimension `i` (0-based; 0 beyond `dim`).
    fn max(&self, i: usize) -> f64 {
        coord(&self.ll, i).max(coord(&self.ur, i))
    }

    pub(super) fn value(self) -> SqlValue {
        SqlValue::Cube {
            ll: self.ll,
            ur: self.ur,
        }
    }
}

fn coord(v: &[f64], i: usize) -> f64 {
    v.get(i).copied().unwrap_or(0.0)
}

pub(super) fn point(coords: Vec<f64>) -> SqlValue {
    SqlValue::Cube {
        ll: coords.clone(),
        ur: coords,
    }
}

/// Scalar-function entry point. All functions are strict.
fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "cube" => construct(args),
        "cube_dim" => {
            let c = cube_arg(args, 0, name)?;
            Ok(SqlValue::Int4(c.dim() as i32))
        }
        "cube_ll_coord" | "cube_ur_coord" => {
            let c = cube_arg(args, 0, name)?;
            let n = arg_i64(args, 1, name)?;
            // 1-based; out-of-range coordinates are 0 (PG-verified).
            let out = if n < 1 || n > c.dim() as i64 {
                0.0
            } else if name == "cube_ll_coord" {
                c.min(n as usize - 1)
            } else {
                c.max(n as usize - 1)
            };
            Ok(SqlValue::Float8(out))
        }
        "cube_is_point" => {
            let c = cube_arg(args, 0, name)?;
            Ok(SqlValue::Bool(c.ll == c.ur))
        }
        "cube_distance" => {
            let a = cube_arg(args, 0, name)?;
            let b = cube_arg(args, 1, name)?;
            Ok(SqlValue::Float8(distance(&a, &b)))
        }
        "cube_union" => {
            let a = cube_arg(args, 0, name)?;
            let b = cube_arg(args, 1, name)?;
            let dim = a.dim().max(b.dim());
            let ll = (0..dim).map(|i| a.min(i).min(b.min(i))).collect();
            let ur = (0..dim).map(|i| a.max(i).max(b.max(i))).collect();
            Ok(Corners { ll, ur }.value())
        }
        "cube_inter" => {
            let a = cube_arg(args, 0, name)?;
            let b = cube_arg(args, 1, name)?;
            let dim = a.dim().max(b.dim());
            // Disjoint inputs produce an inverted (ll > ur) cube, exactly
            // like contrib/cube (PG-verified: '(2, 2),(1, 1)').
            let ll = (0..dim).map(|i| a.min(i).max(b.min(i))).collect();
            let ur = (0..dim).map(|i| a.max(i).min(b.max(i))).collect();
            Ok(Corners { ll, ur }.value())
        }
        "cube_enlarge" => {
            let c = cube_arg(args, 0, name)?;
            let r = arg_f64(args, 1, name)?;
            let n = arg_i64(args, 2, name)?.max(0) as usize;
            Ok(enlarge(&c, r, n).value())
        }
        _ => Err(no_such(name)),
    }
}

/// Operator entry point, routed by [`super::dispatch_operator`].
/// SQL NULL semantics: any NULL operand yields NULL.
pub fn operator(op: &str, left: &SqlValue, right: &SqlValue) -> Result<SqlValue> {
    if left.is_null() || right.is_null() {
        return Ok(SqlValue::Null);
    }
    let args = [left.clone(), right.clone()];
    let a = cube_arg(&args, 0, op)?;
    let b = cube_arg(&args, 1, op)?;
    let dim = a.dim().max(b.dim());
    match op {
        "@>" => Ok(SqlValue::Bool(contains(&a, &b, dim))),
        "<@" => Ok(SqlValue::Bool(contains(&b, &a, dim))),
        "&&" => Ok(SqlValue::Bool(
            (0..dim).all(|i| a.min(i) <= b.max(i) && b.min(i) <= a.max(i)),
        )),
        "<->" => Ok(SqlValue::Float8(distance(&a, &b))),
        _ => Err(no_such(op)),
    }
}

fn contains(a: &Corners, b: &Corners, dim: usize) -> bool {
    (0..dim).all(|i| a.min(i) <= b.min(i) && b.max(i) <= a.max(i))
}

/// Euclidean distance between two cubes: per dimension, the gap between the
/// closest faces (0 when the intervals overlap), missing dimensions read
/// as 0 (PG-verified: cube_distance('(0)','(3,4)') = 5).
pub(super) fn distance(a: &Corners, b: &Corners) -> f64 {
    let dim = a.dim().max(b.dim());
    (0..dim)
        .map(|i| {
            let gap = if a.max(i) < b.min(i) {
                b.min(i) - a.max(i)
            } else if b.max(i) < a.min(i) {
                a.min(i) - b.max(i)
            } else {
                0.0
            };
            gap * gap
        })
        .sum::<f64>()
        .sqrt()
}

/// contrib/cube's `cube_enlarge`: grow (or shrink, for negative `r`) every
/// defined dimension by `r`; when growing, add zero-initialized dimensions
/// up to `n`; a dimension shrunk past itself collapses to its midpoint
/// (PG-verified: cube_enlarge('(0,0),(1,1)',-2,2) = '(0.5, 0.5)').
pub(super) fn enlarge(c: &Corners, r: f64, n: usize) -> Corners {
    let dim = if r >= 0.0 { c.dim().max(n) } else { c.dim() };
    let mut ll = Vec::with_capacity(dim);
    let mut ur = Vec::with_capacity(dim);
    for i in 0..dim {
        let (mut lo, mut hi) = if i < c.dim() {
            (c.min(i), c.max(i))
        } else {
            (0.0, 0.0)
        };
        lo -= r;
        hi += r;
        if lo > hi {
            let mid = (lo + hi) / 2.0;
            lo = mid;
            hi = mid;
        }
        ll.push(lo);
        ur.push(hi);
    }
    Corners { ll, ur }
}

/// The `cube(...)` constructors: `cube(float8)`, `cube(float8, float8)`,
/// `cube(float8[])`, `cube(float8[], float8[])`.
fn construct(args: &[SqlValue]) -> Result<SqlValue> {
    let func = "cube";
    match (args.first(), args.get(1)) {
        (Some(SqlValue::Array(_)), None) => Ok(point(float_array(args, 0, func)?)),
        (Some(SqlValue::Array(_)), Some(_)) => {
            let ll = float_array(args, 0, func)?;
            let ur = float_array(args, 1, func)?;
            if ll.len() != ur.len() {
                // PG: "UR and LL arrays must be of same length" (2202E).
                return Err(SqlError::InvalidParameter(
                    "UR and LL arrays must be of same length".into(),
                ));
            }
            Ok(SqlValue::Cube { ll, ur })
        }
        (Some(_), None) => Ok(point(vec![arg_f64(args, 0, func)?])),
        (Some(_), Some(_)) => {
            let ll = arg_f64(args, 0, func)?;
            let ur = arg_f64(args, 1, func)?;
            Ok(SqlValue::Cube {
                ll: vec![ll],
                ur: vec![ur],
            })
        }
        (None, _) => Err(missing_arg(func, 0)),
    }
}

fn float_array(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<f64>> {
    match args.get(idx) {
        Some(arr @ SqlValue::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_f64()
                    .ok_or_else(|| bad_arg(func, idx, "float8[]", arr))
            })
            .collect(),
        Some(other) => Err(bad_arg(func, idx, "float8[]", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// Extract a cube argument (Cube, or text parsed as cube) at `idx` and view
/// it as [`Corners`].
fn cube_arg(args: &[SqlValue], idx: usize, func: &str) -> Result<Corners> {
    let (ll, ur) = arg_cube(args, idx, func)?;
    Ok(Corners { ll, ur })
}

#[cfg(test)]
mod tests {
    //! Expected values generated from PostgreSQL 16.13 with cube 1.5.
    use super::*;
    use crate::relational::SqlType;
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn invoke(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: Utc::now(),
            vars: &vars,
        };
        call(&ctx, name, args)
    }

    fn c(text: &str) -> SqlValue {
        SqlValue::from_text(text, &SqlType::Cube).unwrap()
    }

    fn f(n: f64) -> SqlValue {
        SqlValue::Float8(n)
    }

    fn i(n: i64) -> SqlValue {
        SqlValue::Int8(n)
    }

    fn farr(items: &[f64]) -> SqlValue {
        SqlValue::Array(items.iter().map(|x| SqlValue::Float8(*x)).collect())
    }

    fn out(v: Result<SqlValue>) -> String {
        v.unwrap().to_text().expect("non-null")
    }

    #[test]
    fn constructors_match_pg() {
        assert_eq!(out(invoke("cube", &[f(1.0)])), "(1)");
        // PG: cube(1.0, 3.0) => (1),(3)
        assert_eq!(out(invoke("cube", &[f(1.0), f(3.0)])), "(1),(3)");
        assert_eq!(out(invoke("cube", &[farr(&[1.0, 2.0])])), "(1, 2)");
        assert_eq!(
            out(invoke("cube", &[farr(&[1.0, 2.0]), farr(&[3.0, 4.0])])),
            "(1, 2),(3, 4)"
        );
        // PG: mismatched corner arrays => "UR and LL arrays must be of same length"
        assert!(invoke("cube", &[farr(&[1.0]), farr(&[1.0, 2.0])]).is_err());
    }

    #[test]
    fn accessors_normalize_and_zero_fill() {
        // PG: cube_dim('(1,2),(3,4)') => 2
        assert_eq!(out(invoke("cube_dim", &[c("(1,2),(3,4)")])), "2");
        assert_eq!(out(invoke("cube_ll_coord", &[c("(1,2),(3,4)"), i(1)])), "1");
        assert_eq!(out(invoke("cube_ur_coord", &[c("(1,2),(3,4)"), i(2)])), "4");
        // Out-of-range coordinate numbers are 0 (PG-verified, incl. 0 and -1).
        for n in [3, 0, -1] {
            assert_eq!(out(invoke("cube_ll_coord", &[c("(1,2)"), i(n)])), "0");
        }
        // Accessors normalize stored corners: PG cube_ll_coord('(3),(1)',1)=1.
        assert_eq!(out(invoke("cube_ll_coord", &[c("(3),(1)"), i(1)])), "1");
        assert_eq!(out(invoke("cube_ur_coord", &[c("(3),(1)"), i(1)])), "3");
    }

    #[test]
    fn is_point_matches_pg() {
        assert_eq!(out(invoke("cube_is_point", &[c("(1,2)")])), "t");
        assert_eq!(out(invoke("cube_is_point", &[c("(1,2),(3,4)")])), "f");
        assert_eq!(out(invoke("cube_is_point", &[c("(1,2),(1,2)")])), "t");
    }

    #[test]
    fn distance_matches_pg() {
        // PG: cube_distance('(0,0)','(3,4)') => 5
        assert_eq!(out(invoke("cube_distance", &[c("(0,0)"), c("(3,4)")])), "5");
        // PG: cube_distance('(0,0),(1,1)','(2,2),(3,3)') => 1.4142135623730951
        assert_eq!(
            out(invoke(
                "cube_distance",
                &[c("(0,0),(1,1)"), c("(2,2),(3,3)")]
            )),
            "1.4142135623730951"
        );
        // Overlapping cubes are at distance 0.
        assert_eq!(
            out(invoke(
                "cube_distance",
                &[c("(0,0),(2,2)"), c("(1,1),(3,3)")]
            )),
            "0"
        );
        // Mixed dimensions pad with 0: PG cube_distance('(0)','(3,4)') => 5.
        assert_eq!(out(invoke("cube_distance", &[c("(0)"), c("(3,4)")])), "5");
    }

    #[test]
    fn union_and_inter_match_pg() {
        assert_eq!(
            out(invoke("cube_union", &[c("(1,2)"), c("(3,4)")])),
            "(1, 2),(3, 4)"
        );
        assert_eq!(
            out(invoke("cube_union", &[c("(0,0),(1,1)"), c("(-1,5)")])),
            "(-1, 0),(1, 5)"
        );
        // Mixed dimensions pad with 0: PG cube_union('(1)','(2,3)') => (1, 0),(2, 3)
        assert_eq!(
            out(invoke("cube_union", &[c("(1)"), c("(2,3)")])),
            "(1, 0),(2, 3)"
        );
        assert_eq!(
            out(invoke("cube_inter", &[c("(0,0),(2,2)"), c("(1,1),(3,3)")])),
            "(1, 1),(2, 2)"
        );
        // Disjoint cubes: an inverted result, exactly like PG.
        assert_eq!(
            out(invoke("cube_inter", &[c("(0,0),(1,1)"), c("(2,2),(3,3)")])),
            "(2, 2),(1, 1)"
        );
        assert_eq!(
            out(invoke("cube_inter", &[c("(0)"), c("(1,2),(3,4)")])),
            "(1, 2),(0, 0)"
        );
    }

    #[test]
    fn enlarge_matches_pg() {
        assert_eq!(
            out(invoke("cube_enlarge", &[c("(1,2),(3,4)"), f(0.5), i(2)])),
            "(0.5, 1.5),(3.5, 4.5)"
        );
        // Extra dimensions start at 0 and are enlarged too.
        assert_eq!(
            out(invoke("cube_enlarge", &[c("(1,2),(3,4)"), f(0.5), i(4)])),
            "(0.5, 1.5, -0.5, -0.5),(3.5, 4.5, 0.5, 0.5)"
        );
        assert_eq!(
            out(invoke("cube_enlarge", &[c("(1)"), f(1.0), i(3)])),
            "(0, -1, -1),(2, 1, 1)"
        );
        // Shrinking past the midpoint collapses to it.
        assert_eq!(
            out(invoke("cube_enlarge", &[c("(0,0),(1,1)"), f(-2.0), i(2)])),
            "(0.5, 0.5)"
        );
        assert_eq!(
            out(invoke("cube_enlarge", &[c("(0,0),(4,4)"), f(-1.0), i(2)])),
            "(1, 1),(3, 3)"
        );
    }

    #[test]
    fn operators_match_pg() {
        let b = |v: Result<SqlValue>| v.unwrap().to_text().unwrap();
        assert_eq!(b(operator("@>", &c("(0,0),(3,3)"), &c("(1,1)"))), "t");
        assert_eq!(b(operator("@>", &c("(0,0),(3,3)"), &c("(1,1),(4,4)"))), "f");
        assert_eq!(b(operator("<@", &c("(1,1)"), &c("(0,0),(3,3)"))), "t");
        assert_eq!(b(operator("&&", &c("(0,0),(1,1)"), &c("(1,1),(2,2)"))), "t");
        assert_eq!(b(operator("&&", &c("(0,0),(1,1)"), &c("(2,2),(3,3)"))), "f");
        // Containment pads missing dimensions with 0 and normalizes corners.
        assert_eq!(b(operator("@>", &c("(0,0),(3,3)"), &c("(1)"))), "t");
        assert_eq!(b(operator("@>", &c("(3),(1)"), &c("(2)"))), "t");
        assert_eq!(b(operator("<->", &c("(0,0)"), &c("(3,4)"))), "5");
    }

    #[test]
    fn null_arguments_yield_null() {
        assert!(invoke("cube_dim", &[SqlValue::Null]).unwrap().is_null());
        assert!(invoke("cube", &[SqlValue::Null]).unwrap().is_null());
        assert!(
            operator("@>", &c("(1)"), &SqlValue::Null)
                .unwrap()
                .is_null()
        );
        assert!(
            operator("<->", &SqlValue::Null, &c("(1)"))
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn every_registered_function_is_routed() {
        for name in DEF.functions {
            let args = match *name {
                "cube" => vec![f(1.0)],
                "cube_ll_coord" | "cube_ur_coord" => vec![c("(1,2)"), i(1)],
                "cube_distance" | "cube_union" | "cube_inter" => vec![c("(1)"), c("(2)")],
                "cube_enlarge" => vec![c("(1)"), f(1.0), i(1)],
                _ => vec![c("(1,2)")],
            };
            assert!(invoke(name, &args).is_ok(), "{name} not routed");
        }
    }

    #[test]
    fn text_coercion_in_arguments() {
        // Text arguments coerce to cube like the ::cube cast.
        assert_eq!(
            out(invoke("cube_dim", &[SqlValue::Text("(1,2),(3,4)".into())])),
            "2"
        );
        assert!(invoke("cube_dim", &[SqlValue::Int4(3)]).is_err());
    }
}
