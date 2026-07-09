//! The `intarray` extension: functions and operators for integer arrays.
//!
//! No new types: everything operates on the core `int[]`
//! ([`SqlValue::Array`] of integer values). Functions: `icount`, `sort`
//! (with an ASC/DESC direction), `sort_asc`, `sort_desc`, `uniq` (adjacent
//! duplicates only, like contrib/intarray), `idx` (1-based position, 0 when
//! absent), `subarray` (1-based start, negative start counts from the end,
//! negative length leaves elements off the end), and `intset`. Operators,
//! routed by [`super::dispatch_operator`]: `&&` (overlap), `@>`/`<@`
//! (containment, set semantics), `+` (append / concatenate), `-` (remove all
//! occurrences), `|` (sorted union), `&` (sorted intersection), and the
//! binary `#` (alias of `idx`).
//!
//! Not implemented: the `query_int` type with its `@@`/`~~` match operators
//! (a separate query language), and the prefix `#` count operator (the
//! parser treats `#` as a prefix expression only in PostgreSQL itself — use
//! `icount`). Arrays must contain integers and no NULLs; both are rejected
//! with typed errors, matching contrib/intarray's
//! "array must not contain nulls".

use super::{
    ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_i64, bad_arg, missing_arg, no_such,
};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};

pub static DEF: ExtensionDef = ExtensionDef {
    name: "intarray",
    default_version: "1.5",
    comment: "functions, operators, and index support for 1-D arrays of integers",
    requires: &[],
    functions: &[
        "icount",
        "sort",
        "sort_asc",
        "sort_desc",
        "uniq",
        "idx",
        "subarray",
        "intset",
    ],
    types: &[],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// Scalar-function entry point. All functions are strict.
fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "icount" => {
            let a = arg_int_array(args, 0, name)?;
            Ok(SqlValue::Int4(a.len() as i32))
        }
        "sort" => {
            let mut a = arg_int_array(args, 0, name)?;
            let asc = match args.get(1) {
                None => true,
                Some(dir) => match dir.as_str().map(str::to_ascii_lowercase).as_deref() {
                    Some("asc") => true,
                    Some("desc") => false,
                    _ => {
                        return Err(SqlError::InvalidParameter(
                            "second parameter must be \"ASC\" or \"DESC\"".into(),
                        ));
                    }
                },
            };
            a.sort_unstable();
            if !asc {
                a.reverse();
            }
            Ok(int_array(a))
        }
        "sort_asc" => {
            let mut a = arg_int_array(args, 0, name)?;
            a.sort_unstable();
            Ok(int_array(a))
        }
        "sort_desc" => {
            let mut a = arg_int_array(args, 0, name)?;
            a.sort_unstable_by(|x, y| y.cmp(x));
            Ok(int_array(a))
        }
        "uniq" => {
            let mut a = arg_int_array(args, 0, name)?;
            a.dedup(); // adjacent duplicates only, like contrib/intarray
            Ok(int_array(a))
        }
        "idx" => {
            let a = arg_int_array(args, 0, name)?;
            let item = arg_i64(args, 1, name)? as i32;
            Ok(SqlValue::Int4(idx(&a, item)))
        }
        "subarray" => {
            let a = arg_int_array(args, 0, name)?;
            let start = arg_i64(args, 1, name)?;
            let len = match args.get(2) {
                Some(_) => Some(arg_i64(args, 2, name)?),
                None => None,
            };
            Ok(int_array(subarray(&a, start, len)))
        }
        "intset" => {
            let n = arg_i64(args, 0, name)? as i32;
            Ok(int_array(vec![n]))
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
    let a = arg_int_array(&args, 0, op)?;
    // `+` and `-` accept an integer right operand; the rest need an array.
    let rhs_int = || arg_i64(&args, 1, op).map(|n| n as i32);
    let rhs_arr = || arg_int_array(&args, 1, op);
    match op {
        "+" => {
            let mut out = a;
            match right {
                SqlValue::Array(_) => out.extend(rhs_arr()?),
                _ => out.push(rhs_int()?),
            }
            Ok(int_array(out))
        }
        "-" => {
            let remove: Vec<i32> = match right {
                SqlValue::Array(_) => rhs_arr()?,
                _ => vec![rhs_int()?],
            };
            Ok(int_array(
                a.into_iter().filter(|x| !remove.contains(x)).collect(),
            ))
        }
        "&&" => {
            let b = rhs_arr()?;
            Ok(SqlValue::Bool(a.iter().any(|x| b.contains(x))))
        }
        "@>" => {
            let b = rhs_arr()?;
            Ok(SqlValue::Bool(b.iter().all(|x| a.contains(x))))
        }
        "<@" => {
            let b = rhs_arr()?;
            Ok(SqlValue::Bool(a.iter().all(|x| b.contains(x))))
        }
        "|" => {
            let mut out = a;
            match right {
                SqlValue::Array(_) => out.extend(rhs_arr()?),
                _ => out.push(rhs_int()?),
            }
            out.sort_unstable();
            out.dedup();
            Ok(int_array(out))
        }
        "&" => {
            let b = rhs_arr()?;
            let mut out: Vec<i32> = a.into_iter().filter(|x| b.contains(x)).collect();
            out.sort_unstable();
            out.dedup();
            Ok(int_array(out))
        }
        "#" => {
            let item = rhs_int()?;
            Ok(SqlValue::Int4(idx(&a, item)))
        }
        _ => Err(no_such(op)),
    }
}

/// 1-based position of the first occurrence of `item` (0 when absent).
fn idx(a: &[i32], item: i32) -> i32 {
    a.iter()
        .position(|x| *x == item)
        .map(|p| p as i32 + 1)
        .unwrap_or(0)
}

/// contrib/intarray's `subarray`: `start` is 1-based (0 behaves like 1), a
/// negative `start` counts from the end, and a negative `len` leaves that
/// many elements off the end. Out-of-range slices come back empty.
/// (PG-verified: `subarray('{1,2,3,2,1}', -2, 1)` is `{2}`,
/// `subarray('{1,2,3,2,1}', 2, -1)` is `{2,3,2}`.)
fn subarray(a: &[i32], start: i64, len: Option<i64>) -> Vec<i32> {
    let n = a.len() as i64;
    let begin = if start < 0 {
        n + start
    } else {
        (start - 1).max(0)
    };
    let end = match len {
        None => n,
        Some(l) if l < 0 => n + l,
        Some(l) => begin + l,
    };
    let begin = begin.clamp(0, n) as usize;
    let end = end.clamp(0, n) as usize;
    if begin >= end {
        return Vec::new();
    }
    a[begin..end].to_vec()
}

fn int_array(v: Vec<i32>) -> SqlValue {
    SqlValue::Array(v.into_iter().map(SqlValue::Int4).collect())
}

/// Extract an int[] argument: every element must be an integer (non-integer
/// arrays are `CannotCoerce`, NULL elements are rejected like
/// contrib/intarray's "array must not contain nulls").
fn arg_int_array(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<i32>> {
    match args.get(idx) {
        Some(arr @ SqlValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    SqlValue::Null => {
                        return Err(SqlError::InvalidParameter(
                            "array must not contain nulls".into(),
                        ));
                    }
                    SqlValue::Int2(_) | SqlValue::Int4(_) | SqlValue::Int8(_) => {
                        out.push(item.as_i64().expect("integer") as i32);
                    }
                    _ => return Err(bad_arg(func, idx, "integer[]", arr)),
                }
            }
            Ok(out)
        }
        Some(other) => Err(bad_arg(func, idx, "integer[]", other)),
        None => Err(missing_arg(func, idx)),
    }
}

#[cfg(test)]
mod tests {
    //! Expected values generated from PostgreSQL 16.13 with intarray 1.5.
    use super::*;
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

    fn ia(items: &[i32]) -> SqlValue {
        int_array(items.to_vec())
    }

    fn i(n: i64) -> SqlValue {
        SqlValue::Int8(n)
    }

    fn out(v: Result<SqlValue>) -> String {
        v.unwrap().to_text().expect("non-null")
    }

    #[test]
    fn icount_and_intset() {
        assert_eq!(out(invoke("icount", &[ia(&[1, 2, 3, 2])])), "4");
        assert_eq!(out(invoke("icount", &[ia(&[])])), "0");
        assert_eq!(out(invoke("intset", &[i(42)])), "{42}");
    }

    #[test]
    fn sort_directions_match_pg() {
        assert_eq!(out(invoke("sort", &[ia(&[3, 1, 2])])), "{1,2,3}");
        // PG: sort('{3,1,2}', 'desc') => {3,2,1}; direction is case-insensitive.
        let desc = SqlValue::Text("desc".into());
        assert_eq!(out(invoke("sort", &[ia(&[3, 1, 2]), desc])), "{3,2,1}");
        let upper = SqlValue::Text("ASC".into());
        assert_eq!(out(invoke("sort", &[ia(&[2, 1]), upper])), "{1,2}");
        assert_eq!(out(invoke("sort_asc", &[ia(&[3, 1, 2, 2])])), "{1,2,2,3}");
        assert_eq!(out(invoke("sort_desc", &[ia(&[3, 1, 2])])), "{3,2,1}");
        // PG: sort with a bad direction => 22023.
        let bogus = SqlValue::Text("bogus".into());
        assert!(matches!(
            invoke("sort", &[ia(&[1]), bogus]),
            Err(SqlError::InvalidParameter(_))
        ));
    }

    #[test]
    fn uniq_removes_adjacent_duplicates_only() {
        // PG: uniq('{1,1,2,2,3,1,1}') => {1,2,3,1}
        assert_eq!(
            out(invoke("uniq", &[ia(&[1, 1, 2, 2, 3, 1, 1])])),
            "{1,2,3,1}"
        );
        assert_eq!(out(invoke("uniq", &[ia(&[])])), "{}");
    }

    #[test]
    fn idx_is_one_based() {
        // PG: idx('{1,2,3,2}', 2) => 2; idx('{1,2,3}', 9) => 0
        assert_eq!(out(invoke("idx", &[ia(&[1, 2, 3, 2]), i(2)])), "2");
        assert_eq!(out(invoke("idx", &[ia(&[1, 2, 3]), i(9)])), "0");
    }

    #[test]
    fn subarray_matches_pg() {
        let a = || ia(&[1, 2, 3, 2, 1]);
        // All PG-verified.
        assert_eq!(out(invoke("subarray", &[a(), i(2), i(3)])), "{2,3,2}");
        assert_eq!(out(invoke("subarray", &[a(), i(2)])), "{2,3,2,1}");
        assert_eq!(out(invoke("subarray", &[a(), i(-2), i(1)])), "{2}");
        assert_eq!(out(invoke("subarray", &[a(), i(0), i(2)])), "{1,2}");
        assert_eq!(out(invoke("subarray", &[a(), i(2), i(-1)])), "{2,3,2}");
        assert_eq!(out(invoke("subarray", &[ia(&[1, 2, 3]), i(5), i(2)])), "{}");
    }

    #[test]
    fn operators_match_pg() {
        let b = |v| SqlValue::Bool(v);
        assert_eq!(
            operator("&&", &ia(&[1, 2, 3]), &ia(&[3, 4]))
                .unwrap()
                .sql_eq(&b(true)),
            Some(true)
        );
        assert_eq!(
            operator("&&", &ia(&[1, 2, 3]), &ia(&[4, 5]))
                .unwrap()
                .sql_eq(&b(false)),
            Some(true)
        );
        // Containment is set-wise: {1,2,3} @> {2,2,2} => t (PG-verified).
        assert_eq!(
            operator("@>", &ia(&[1, 2, 3]), &ia(&[2, 2, 2]))
                .unwrap()
                .sql_eq(&b(true)),
            Some(true)
        );
        assert_eq!(
            operator("@>", &ia(&[1, 2, 3]), &ia(&[2, 4]))
                .unwrap()
                .sql_eq(&b(false)),
            Some(true)
        );
        assert_eq!(
            operator("<@", &ia(&[2]), &ia(&[1, 2, 3]))
                .unwrap()
                .sql_eq(&b(true)),
            Some(true)
        );
        assert_eq!(
            operator("<@", &ia(&[]), &ia(&[1]))
                .unwrap()
                .sql_eq(&b(true)),
            Some(true)
        );
        assert_eq!(
            operator("@>", &ia(&[1, 2]), &ia(&[]))
                .unwrap()
                .sql_eq(&b(true)),
            Some(true)
        );
        // PG: '{1,2,3}' + 4 => {1,2,3,4}; '{1,2,3}' + '{3,4}' => {1,2,3,3,4}
        assert_eq!(
            operator("+", &ia(&[1, 2, 3]), &i(4))
                .unwrap()
                .to_text()
                .unwrap(),
            "{1,2,3,4}"
        );
        assert_eq!(
            operator("+", &ia(&[1, 2, 3]), &ia(&[3, 4]))
                .unwrap()
                .to_text()
                .unwrap(),
            "{1,2,3,3,4}"
        );
        // PG: '{1,2,3,2}' - 2 => {1,3}; '{1,2,3,2,4}' - '{2,4,9}' => {1,3}
        assert_eq!(
            operator("-", &ia(&[1, 2, 3, 2]), &i(2))
                .unwrap()
                .to_text()
                .unwrap(),
            "{1,3}"
        );
        assert_eq!(
            operator("-", &ia(&[1, 2, 3, 2, 4]), &ia(&[2, 4, 9]))
                .unwrap()
                .to_text()
                .unwrap(),
            "{1,3}"
        );
        // PG: '{1,2,3}' | 4 => {1,2,3,4}; '{1,2,3}' | '{3,4}' => {1,2,3,4}
        assert_eq!(
            operator("|", &ia(&[1, 2, 3]), &i(4))
                .unwrap()
                .to_text()
                .unwrap(),
            "{1,2,3,4}"
        );
        assert_eq!(
            operator("|", &ia(&[1, 2, 3]), &ia(&[3, 4]))
                .unwrap()
                .to_text()
                .unwrap(),
            "{1,2,3,4}"
        );
        // PG: '{1,2,3,2}' & '{2,3,4}' => {2,3}
        assert_eq!(
            operator("&", &ia(&[1, 2, 3, 2]), &ia(&[2, 3, 4]))
                .unwrap()
                .to_text()
                .unwrap(),
            "{2,3}"
        );
        // PG: '{1,3,5}' # 5 => 3
        assert_eq!(
            operator("#", &ia(&[1, 3, 5]), &i(5))
                .unwrap()
                .to_text()
                .unwrap(),
            "3"
        );
    }

    #[test]
    fn non_int_arrays_are_rejected() {
        let texts = SqlValue::Array(vec![SqlValue::Text("a".into())]);
        assert!(matches!(
            invoke("icount", std::slice::from_ref(&texts)),
            Err(SqlError::CannotCoerce { .. })
        ));
        assert!(operator("&&", &texts, &ia(&[1])).is_err());
        // NULL elements: typed error like contrib/intarray.
        let with_null = SqlValue::Array(vec![SqlValue::Int4(1), SqlValue::Null]);
        assert!(matches!(
            invoke("uniq", &[with_null]),
            Err(SqlError::InvalidParameter(_))
        ));
    }

    #[test]
    fn null_arguments_yield_null() {
        assert!(invoke("icount", &[SqlValue::Null]).unwrap().is_null());
        assert!(
            invoke("idx", &[ia(&[1]), SqlValue::Null])
                .unwrap()
                .is_null()
        );
        assert!(operator("+", &SqlValue::Null, &i(1)).unwrap().is_null());
        assert!(
            operator("@>", &ia(&[1]), &SqlValue::Null)
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn every_registered_function_is_routed() {
        for name in DEF.functions {
            let args = match *name {
                "idx" | "subarray" => vec![ia(&[1, 2]), i(1)],
                "intset" => vec![i(1)],
                _ => vec![ia(&[2, 1])],
            };
            assert!(invoke(name, &args).is_ok(), "{name} not routed");
        }
    }
}
