//! The `ltree` extension: hierarchical tree-like label paths.
//!
//! The `ltree` type lives in the core value model
//! ([`crate::relational::SqlType::Ltree`] / [`SqlValue::Ltree`]): input
//! validation (labels of alphanumerics, `_` and `-` — hyphens as of
//! PostgreSQL 16 — each up to 255 characters, dot-separated, the empty path
//! being the valid zero-level path) and the label-wise ordering both live
//! there. This module provides `nlevel`, `subltree`, `subpath` (with the
//! documented negative offset/length forms), `index` (0-based, with the
//! optional offset argument), `text2ltree`, `ltree2text` and the two-argument
//! `lca`, plus the operators `@>`/`<@` (ancestor/descendant **or equal**,
//! like PostgreSQL) and `~` — an lquery matcher implementing the documented
//! lquery language: labels with the `@` (case-insensitive), `*` (prefix) and
//! `%` (underscore-separated word) modifiers, `|` alternation, `!` level
//! negation, and `{n}`/`{n,}`/`{,m}`/`{n,m}` quantifiers on both `*` and
//! non-star items.
//!
//! Not implemented: the `lquery`/`ltxtquery` named types (write the pattern
//! as a plain string literal — `path ~ 'a.*'` — instead of casting to
//! `::lquery`), the `@@` full-text-style ltxtquery operator, `?`/`?@>` array
//! operators, and the multi-argument/array form of `lca`. All semantics
//! below were verified against PostgreSQL 16.13 (ltree 1.2; version 1.3 adds
//! only planner support functions).

use super::{
    ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_i64, arg_text, bad_arg, missing_arg,
    no_such,
};
use crate::relational::value::ltree_labels;
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};

pub static DEF: ExtensionDef = ExtensionDef {
    name: "ltree",
    default_version: "1.3",
    comment: "data type for hierarchical tree-like structures",
    requires: &[],
    functions: &[
        "nlevel",
        "subltree",
        "subpath",
        "index",
        "text2ltree",
        "ltree2text",
        "lca",
    ],
    types: &["ltree"],
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
        "nlevel" => {
            let path = arg_ltree(args, 0, name)?;
            Ok(SqlValue::Int4(ltree_labels(&path).len() as i32))
        }
        "subltree" => {
            let path = arg_ltree(args, 0, name)?;
            let labels = ltree_labels(&path);
            let start = arg_i64(args, 1, name)?;
            let end = arg_i64(args, 2, name)?;
            Ok(SqlValue::Ltree(inner_subltree(&labels, start, end)?))
        }
        "subpath" => {
            let path = arg_ltree(args, 0, name)?;
            let labels = ltree_labels(&path);
            let n = labels.len() as i64;
            let offset = arg_i64(args, 1, name)?;
            let len = match args.get(2) {
                Some(_) => Some(arg_i64(args, 2, name)?),
                None => None,
            };
            // contrib/ltree's subpath: negative offsets count from the end,
            // negative lengths leave labels off the end (both PG-verified).
            // The end-relative adjustment is applied a second time when the
            // offset reaches past the front, quirky but faithful
            // (PG-verified: subpath('a.b.c', -5) = 'b.c').
            let mut start = offset;
            if start < 0 {
                start += n;
            }
            if start < 0 {
                start += n;
            }
            let end = match len {
                Some(l) if l < 0 => n + l,
                Some(0) => start,
                Some(l) => start + l,
                None => n,
            };
            Ok(SqlValue::Ltree(inner_subltree(&labels, start, end)?))
        }
        "index" => {
            let a = arg_ltree(args, 0, name)?;
            let b = arg_ltree(args, 1, name)?;
            let offset = match args.get(2) {
                Some(_) => arg_i64(args, 2, name)?,
                None => 0,
            };
            let hay = ltree_labels(&a);
            let needle = ltree_labels(&b);
            let n = hay.len() as i64;
            // A negative offset starts the search -offset labels from the
            // end (PG-verified: index('0.1.2.3.5.4.5.6.8.5.6.8','5.6',-4)=9).
            let start = if offset < 0 {
                (n + offset).max(0)
            } else {
                offset
            } as usize;
            // The empty path is found nowhere (PG-verified: -1).
            let found = if needle.is_empty() || needle.len() > hay.len() {
                None
            } else {
                (start..hay.len() - needle.len() + 1)
                    .find(|&i| hay[i..i + needle.len()] == needle[..])
            };
            Ok(SqlValue::Int4(found.map(|i| i as i32).unwrap_or(-1)))
        }
        "text2ltree" => {
            let t = arg_text(args, 0, name)?;
            SqlValue::from_text(&t, &SqlType::Ltree)
        }
        "ltree2text" => {
            let path = arg_ltree(args, 0, name)?;
            Ok(SqlValue::Text(path))
        }
        "lca" => {
            let a = arg_ltree(args, 0, name)?;
            let b = arg_ltree(args, 1, name)?;
            let (la, lb) = (ltree_labels(&a), ltree_labels(&b));
            let shared = la.iter().zip(&lb).take_while(|(x, y)| x == y).count();
            // The result is an *ancestor*: at most one level above the
            // shorter path (PG-verified: lca('1.2.3','1.2.3') = '1.2').
            let levels = shared.min(la.len().min(lb.len()).saturating_sub(1));
            Ok(SqlValue::Ltree(la[..levels].join(".")))
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
    match op {
        // Ancestor/descendant *or equal*, like PostgreSQL ('a.b' @> 'a.b').
        "@>" => {
            let a = arg_ltree(&args, 0, op)?;
            let b = arg_ltree(&args, 1, op)?;
            Ok(SqlValue::Bool(is_prefix(&a, &b)))
        }
        "<@" => {
            let a = arg_ltree(&args, 0, op)?;
            let b = arg_ltree(&args, 1, op)?;
            Ok(SqlValue::Bool(is_prefix(&b, &a)))
        }
        // `ltree ~ lquery` and `lquery ~ ltree`: the ltree side is the path.
        "~" => {
            let (path, query) = match (left, right) {
                (SqlValue::Ltree(p), q) => (p.clone(), q),
                (q, SqlValue::Ltree(p)) => (p.clone(), q),
                _ => return Err(bad_arg(op, 0, "ltree", left)),
            };
            let q = query
                .to_text()
                .ok_or_else(|| bad_arg(op, 1, "lquery", query))?;
            let levels = parse_lquery(&q)?;
            let labels = ltree_labels(&path);
            Ok(SqlValue::Bool(lquery_match(&levels, &labels, 0, 0)))
        }
        _ => Err(no_such(op)),
    }
}

fn is_prefix(a: &str, b: &str) -> bool {
    let (la, lb) = (ltree_labels(a), ltree_labels(b));
    la.len() <= lb.len() && la == lb[..la.len()]
}

/// contrib/ltree's `inner_subltree`: `start`/`end` are 0-based label
/// positions, `end` exclusive and clamped to the path length; out-of-range
/// or inverted positions raise "invalid positions" (`22023`), including
/// `start >= nlevel` (PG-verified: subltree('a.b.c',0,9) = 'a.b.c',
/// subltree('a.b.c',2,1) errors, subpath('a.b.c',5) errors).
fn inner_subltree(labels: &[&str], start: i64, end: i64) -> Result<String> {
    if start < 0 || end < 0 || start >= labels.len() as i64 || start > end {
        return Err(SqlError::InvalidParameter("invalid positions".into()));
    }
    let end = end.min(labels.len() as i64);
    Ok(labels[start as usize..end as usize].join("."))
}

/// Extract an ltree argument (Ltree, or text validated as ltree) at `idx`.
fn arg_ltree(args: &[SqlValue], idx: usize, func: &str) -> Result<String> {
    match args.get(idx) {
        Some(SqlValue::Ltree(p)) => Ok(p.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::Ltree)? {
                SqlValue::Ltree(p) => Ok(p),
                _ => unreachable!("from_text(ltree) yields Ltree"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "ltree", other)),
        None => Err(missing_arg(func, idx)),
    }
}

// ---------------------------------------------------------------------------
// lquery
// ---------------------------------------------------------------------------

/// One dot-separated lquery level.
struct Level {
    /// `!` — the level matches labels that do NOT match the variants
    /// (negation covers the whole alternation: `!b|x` is "neither b nor x",
    /// PG-verified).
    not: bool,
    /// `*` — matches any labels; `variants` is empty.
    star: bool,
    /// Quantifier bounds; `{1,1}` for plain items, `{0,∞}` for a bare `*`.
    min: usize,
    max: usize,
    variants: Vec<Variant>,
}

/// One `|`-alternation branch of a level.
struct Variant {
    label: String,
    /// `@` — case-insensitive.
    ci: bool,
    /// `*` — prefix match.
    prefix: bool,
    /// `%` — match underscore-separated words.
    word: bool,
}

fn lquery_error(query: &str) -> SqlError {
    SqlError::Syntax(format!("lquery syntax error in \"{query}\""))
}

/// Parse the documented lquery subset. Errors are `42601` like PostgreSQL.
fn parse_lquery(query: &str) -> Result<Vec<Level>> {
    let q = query.trim();
    if q.is_empty() {
        // PG: ''::lquery is "lquery syntax error: unexpected end of input".
        return Err(lquery_error(query));
    }
    q.split('.').map(|item| parse_level(item, query)).collect()
}

fn parse_level(item: &str, query: &str) -> Result<Level> {
    let err = || lquery_error(query);
    let mut rest = item;
    let not = rest.starts_with('!');
    if not {
        rest = &rest[1..];
    }
    // Optional trailing quantifier {n}, {n,}, {,m}, {n,m}.
    let (mut min, mut max) = (1usize, 1usize);
    let mut explicit_quant = false;
    if let Some(open) = rest.rfind('{') {
        let inner = rest[open..].strip_prefix('{').unwrap();
        let inner = inner.strip_suffix('}').ok_or_else(err)?;
        let parse_bound = |s: &str, default: usize| -> Result<usize> {
            if s.is_empty() {
                Ok(default)
            } else {
                s.parse().map_err(|_| err())
            }
        };
        (min, max) = match inner.split_once(',') {
            None => {
                let n = parse_bound(inner, 0)?;
                (n, n)
            }
            Some((lo, hi)) => (parse_bound(lo, 0)?, parse_bound(hi, usize::MAX)?),
        };
        if min > max {
            return Err(err());
        }
        explicit_quant = true;
        rest = &rest[..open];
    }
    if rest == "*" {
        if !explicit_quant {
            (min, max) = (0, usize::MAX);
        }
        return Ok(Level {
            not,
            star: true,
            min,
            max,
            variants: Vec::new(),
        });
    }
    if rest.is_empty() {
        return Err(err());
    }
    let mut variants = Vec::new();
    for branch in rest.split('|') {
        let mut label = branch;
        let (mut ci, mut prefix, mut word) = (false, false, false);
        while let Some(last) = label.chars().last() {
            match last {
                '@' => ci = true,
                '*' => prefix = true,
                '%' => word = true,
                _ => break,
            }
            label = &label[..label.len() - last.len_utf8()];
        }
        let valid = !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(err());
        }
        variants.push(Variant {
            label: label.to_string(),
            ci,
            prefix,
            word,
        });
    }
    Ok(Level {
        not,
        star: false,
        min,
        max,
        variants,
    })
}

/// Backtracking matcher: does `labels[li..]` satisfy `levels[qi..]`?
fn lquery_match(levels: &[Level], labels: &[&str], qi: usize, li: usize) -> bool {
    let Some(level) = levels.get(qi) else {
        return li == labels.len();
    };
    let remaining = labels.len() - li;
    if level.min > remaining {
        return false;
    }
    let cap = level.max.min(remaining);
    // Consume `count` labels with this level (all of which must match it,
    // checked incrementally), then match the rest of the query.
    let mut count = 0;
    loop {
        if count >= level.min && lquery_match(levels, labels, qi + 1, li + count) {
            return true;
        }
        if count == cap || !level_matches(level, labels[li + count]) {
            return false;
        }
        count += 1;
    }
}

fn level_matches(level: &Level, label: &str) -> bool {
    if level.star {
        return true;
    }
    let hit = level.variants.iter().any(|v| variant_matches(v, label));
    hit != level.not
}

fn variant_matches(v: &Variant, label: &str) -> bool {
    let (pat, lab) = if v.ci {
        (v.label.to_lowercase(), label.to_lowercase())
    } else {
        (v.label.clone(), label.to_string())
    };
    if v.word {
        return word_match(&pat, &lab, v.prefix);
    }
    if v.prefix {
        return lab.starts_with(&pat);
    }
    lab == pat
}

/// The `%` modifier: the pattern's underscore-separated words must match a
/// contiguous run of the label's words, each exactly (or by prefix when
/// combined with `*`). PG-verified: 'foo_bar%' matches 'foo_bar_baz' and
/// 'a_foo_bar' but not 'foo_barbaz'; 'foo_bar%*' matches 'foo1_bar2_baz'.
fn word_match(pattern: &str, label: &str, prefix: bool) -> bool {
    let pwords: Vec<&str> = pattern.split('_').collect();
    let lwords: Vec<&str> = label.split('_').collect();
    if pwords.len() > lwords.len() {
        return false;
    }
    (0..=lwords.len() - pwords.len()).any(|start| {
        pwords.iter().zip(&lwords[start..]).all(
            |(p, l)| {
                if prefix { l.starts_with(p) } else { l == p }
            },
        )
    })
}

#[cfg(test)]
mod tests {
    //! Expected values generated from PostgreSQL 16.13 with ltree 1.2.
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

    fn l(path: &str) -> SqlValue {
        SqlValue::Ltree(path.into())
    }

    fn t(s: &str) -> SqlValue {
        SqlValue::Text(s.into())
    }

    fn i(n: i64) -> SqlValue {
        SqlValue::Int8(n)
    }

    fn out(v: Result<SqlValue>) -> String {
        v.unwrap().to_text().expect("non-null")
    }

    fn matches(path: &str, query: &str) -> bool {
        match operator("~", &l(path), &t(query)).unwrap() {
            SqlValue::Bool(b) => b,
            other => panic!("expected bool, got {other:?}"),
        }
    }

    #[test]
    fn nlevel_counts_labels() {
        assert_eq!(out(invoke("nlevel", &[l("Top.Science.Astronomy")])), "3");
        assert_eq!(out(invoke("nlevel", &[l("")])), "0");
    }

    #[test]
    fn subltree_matches_pg() {
        // PG: subltree('Top.Child1.Child2',1,2) => Child1
        assert_eq!(
            out(invoke("subltree", &[l("Top.Child1.Child2"), i(1), i(2)])),
            "Child1"
        );
        // PG: subltree('a.b.c',0,9) => a.b.c (end clamps)
        assert_eq!(out(invoke("subltree", &[l("a.b.c"), i(0), i(9)])), "a.b.c");
        // PG: subltree('a.b.c',1,1) => '' (empty, no error)
        assert_eq!(out(invoke("subltree", &[l("a.b.c"), i(1), i(1)])), "");
        // PG: subltree('a.b.c',2,1) => 22023 invalid positions
        assert!(matches!(
            invoke("subltree", &[l("a.b.c"), i(2), i(1)]),
            Err(SqlError::InvalidParameter(_))
        ));
    }

    #[test]
    fn subpath_matches_pg() {
        let p = || l("Top.Child1.Child2");
        assert_eq!(out(invoke("subpath", &[p(), i(0), i(2)])), "Top.Child1");
        assert_eq!(out(invoke("subpath", &[p(), i(1)])), "Child1.Child2");
        assert_eq!(out(invoke("subpath", &[p(), i(-2), i(1)])), "Child1");
        assert_eq!(out(invoke("subpath", &[p(), i(-1)])), "Child2");
        // PG: subpath('a.b.c.d',1,-1) => b.c (negative len trims the end)
        assert_eq!(out(invoke("subpath", &[l("a.b.c.d"), i(1), i(-1)])), "b.c");
        // PG: subpath('a.b.c',-5) => b.c (the end-relative wrap applies twice)
        assert_eq!(out(invoke("subpath", &[l("a.b.c"), i(-5)])), "b.c");
        // PG: subpath('a.b.c',1,0) => '' (empty result, no error)
        assert_eq!(out(invoke("subpath", &[l("a.b.c"), i(1), i(0)])), "");
        // PG: subpath('a.b.c',5) => 22023 invalid positions
        assert!(matches!(
            invoke("subpath", &[l("a.b.c"), i(5)]),
            Err(SqlError::InvalidParameter(_))
        ));
    }

    #[test]
    fn index_matches_pg() {
        let hay = || l("0.1.2.3.5.4.5.6.8.5.6.8");
        // All PG-verified: index() is 0-based; -1 when absent; negative
        // offsets count from the end.
        assert_eq!(out(invoke("index", &[hay(), l("5.6")])), "6");
        assert_eq!(out(invoke("index", &[hay(), l("5.6"), i(6)])), "6");
        assert_eq!(out(invoke("index", &[hay(), l("5.6"), i(-4)])), "9");
        assert_eq!(out(invoke("index", &[l("a.b.c"), l("x")])), "-1");
        // PG: index('a.b','') => -1 (the empty path is found nowhere)
        assert_eq!(out(invoke("index", &[l("a.b"), l("")])), "-1");
    }

    #[test]
    fn text_conversions_round_trip() {
        assert_eq!(out(invoke("text2ltree", &[t("a.b")])), "a.b");
        assert_eq!(out(invoke("ltree2text", &[l("a.b")])), "a.b");
        assert!(invoke("text2ltree", &[t("a b")]).is_err());
    }

    #[test]
    fn lca_matches_pg() {
        // PG: lca('1.2.2.3','1.2.3.4.5.6') => 1.2
        assert_eq!(out(invoke("lca", &[l("1.2.2.3"), l("1.2.3.4.5.6")])), "1.2");
        // PG: lca('1.2.3','1.2.3') => 1.2 (an ancestor is strictly above)
        assert_eq!(out(invoke("lca", &[l("1.2.3"), l("1.2.3")])), "1.2");
        assert_eq!(out(invoke("lca", &[l("1.2.3"), l("1.2.3.4")])), "1.2");
        // PG: lca('a','b') => '' (the empty root path, not NULL)
        let root = invoke("lca", &[l("a"), l("b")]).unwrap();
        assert!(!root.is_null());
        assert_eq!(root.to_text().unwrap(), "");
    }

    #[test]
    fn ancestor_operators_include_equality() {
        let b = |v: Result<SqlValue>| v.unwrap().to_text().unwrap();
        // PG truth table.
        assert_eq!(b(operator("@>", &l("a.b"), &l("a.b.c"))), "t");
        assert_eq!(b(operator("@>", &l("a.b"), &l("a.b"))), "t");
        assert_eq!(b(operator("@>", &l("a.b.c"), &l("a.b"))), "f");
        assert_eq!(b(operator("<@", &l("a.b.c"), &l("a.b"))), "t");
        assert_eq!(b(operator("@>", &l(""), &l("a"))), "t");
    }

    #[test]
    fn lquery_basics_match_pg() {
        // Every row of this table was verified against PostgreSQL 16.13.
        for (path, query, expect) in [
            ("a.b.c", "a.b.c", true),
            ("a.b.c", "a.*", true),
            ("a.b.c", "*.c", true),
            ("a.b.c", "*.b.*", true),
            ("a.b.c", "a.*{1}.c", true),
            ("a.b.c", "a.*{2}.c", false),
            ("a.b.b.c", "a.*{1,2}.c", true),
            ("a.c", "a.*{0,1}.c", true),
            ("a.b.c", "a.*{,1}.c", true),
            ("a.b.b.b.c", "a.*{2,}.c", true),
            ("a.b.c.d", "a.*{1,}.d", true),
            ("a.d", "a.*{1,}.d", false),
            ("a.b.c", "*{3}", true),
            ("a.b.c", "*{4}", false),
            ("a.b.c", "*", true),
            ("", "*", true),
            ("A.B", "a.b", false), // case-sensitive by default
        ] {
            assert_eq!(matches(path, query), expect, "{path} ~ {query}");
        }
    }

    #[test]
    fn lquery_alternation_and_negation_match_pg() {
        for (path, query, expect) in [
            ("a.b.c", "a.b|x.c", true),
            ("a.x.c", "a.b|x.c", true),
            ("a.y.c", "a.!b.c", true),
            ("a.b.c", "a.!b.c", false),
            // ! negates the whole alternation (PG-verified: y matches,
            // both b and x do not).
            ("a.b.c", "a.!b|x.c", false),
            ("a.y.c", "a.!b|x.c", true),
            ("a.x.c", "a.!b|x.c", false),
            // A one-level query never matches a three-level path.
            ("a.b.c", "!b", false),
            ("b", "!b", false),
            ("c", "!b", true),
            // Non-star quantifiers.
            ("a.b.c", "a{2}.c", false),
            ("a.a.c", "a{2}.c", true),
            ("a.a.b.c", "a{1,2}.b.c", true),
            ("x.y", "!x.*", false),
            ("y.x", "!x.*", true),
            // Zero quantifier on a plain item (PG-verified).
            ("a", "a{0}", false),
            ("", "a{0}", true),
        ] {
            assert_eq!(matches(path, query), expect, "{path} ~ {query}");
        }
    }

    #[test]
    fn lquery_modifiers_match_pg() {
        for (path, query, expect) in [
            ("a.B.c", "a.b@.c", true),
            ("a.beta.c", "a.b*.c", true),
            ("a.b.c", "a.beta*.c", false),
            ("a.BETA.c", "a.b*@.c", true),
            ("ab.c", "a*@.c", true),
            // % word matching (see word_match).
            ("foo_bar_baz", "foo_bar%", true),
            ("foo_barbaz", "foo_bar%", false),
            ("foo_bar", "foo_bar%", true),
            ("foo", "foo_bar%", false),
            ("foo_bar_baz", "bar%", true),
            ("a_foo_b", "foo%", true),
            ("afoo_b", "foo%", false),
            ("xx_foo", "foo%", true),
            ("foo_bar", "fo%", false),
            ("a-b", "a%", false), // '-' does not separate words
            ("foo1_bar2_baz", "foo_bar%*", true),
            ("foo1_br2_baz", "foo_bar%*", false),
            ("xx_foo1", "foo%*", true),
            ("FOO_bar", "foo%@", true),
        ] {
            assert_eq!(matches(path, query), expect, "{path} ~ {query}");
        }
    }

    #[test]
    fn lquery_syntax_errors() {
        for bad in ["", "%", "a..b", "a.{2}", "a.b|", "a.*{2,1}", "a.*{x}"] {
            assert!(
                operator("~", &l("a"), &t(bad)).is_err(),
                "{bad:?} should be an lquery syntax error"
            );
        }
    }

    #[test]
    fn operator_accepts_query_on_either_side() {
        assert_eq!(
            operator("~", &t("a.*"), &l("a.b"))
                .unwrap()
                .to_text()
                .unwrap(),
            "t"
        );
    }

    #[test]
    fn null_arguments_yield_null() {
        assert!(invoke("nlevel", &[SqlValue::Null]).unwrap().is_null());
        assert!(invoke("lca", &[l("a"), SqlValue::Null]).unwrap().is_null());
        assert!(operator("~", &SqlValue::Null, &t("a")).unwrap().is_null());
        assert!(operator("@>", &l("a"), &SqlValue::Null).unwrap().is_null());
    }

    #[test]
    fn every_registered_function_is_routed() {
        for name in DEF.functions {
            let args = match *name {
                "subltree" => vec![l("a.b.c"), i(0), i(1)],
                "subpath" => vec![l("a.b.c"), i(0), i(1)],
                "index" | "lca" => vec![l("a.b"), l("b")],
                "text2ltree" => vec![t("a.b")],
                _ => vec![l("a.b")],
            };
            assert!(invoke(name, &args).is_ok(), "{name} not routed");
        }
    }
}
