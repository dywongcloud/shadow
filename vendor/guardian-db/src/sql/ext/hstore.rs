//! The `hstore` extension: a key/value store in a single column.
//!
//! The `hstore` type itself lives in the core value model
//! ([`crate::relational::SqlType::HStore`] / [`SqlValue::HStore`]), including
//! the PostgreSQL text form (`'a=>1, b=>NULL'` in, `'"a"=>"1", "b"=>NULL'`
//! out, pairs ordered by key length then bytes exactly like contrib/hstore).
//! This module provides the scalar functions — `hstore(text,text)`,
//! `hstore(text[],text[])`, `akeys`, `avals`, `hstore_to_json`,
//! `hstore_to_jsonb`, `hstore_to_matrix`, `exist`, `defined`, `delete` (all
//! three overloads), `slice`, `hs_concat` — and the operators `->` (text and
//! text[] keys), `||`, `?`, `-` (text/text[]/hstore) and `@>`/`<@`, routed
//! here by [`super::dispatch_operator`].
//!
//! Set-returning members (`each`, `skeys`/`svals` as setof) need SRF
//! machinery the engine does not have; `akeys`/`avals` return the same data
//! as arrays. The `?&`/`?|` operators and the `#=`/record functions are not
//! implemented. `hstore_to_json` renders through the engine's compact JSON
//! output (PostgreSQL prints `{"a": "1"}` with spaces); the JSON *content*
//! is identical.
//!
//! Duplicate input keys keep the first occurrence, and `hstore(k, NULL)`
//! yields `"k"=>NULL` rather than SQL NULL — both verified against
//! PostgreSQL 16.13 / hstore 1.8.

use super::{ExtCtx, ExtensionDef, RuntimeStrategy, arg_text, bad_arg, missing_arg, no_such};
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use serde_json::Value as Json;
use std::collections::BTreeMap;

type Pairs = BTreeMap<String, Option<String>>;

pub static DEF: ExtensionDef = ExtensionDef {
    name: "hstore",
    default_version: "1.8",
    comment: "data type for storing sets of (key, value) pairs",
    requires: &[],
    functions: &[
        "hstore",
        "akeys",
        "avals",
        "hstore_to_json",
        "hstore_to_jsonb",
        "hstore_to_matrix",
        "exist",
        "defined",
        "delete",
        "slice",
        "hs_concat",
    ],
    types: &["hstore"],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    // `hstore(k, NULL)` is *not* strict: PG yields `"k"=>NULL`. Everything
    // else is strict (NULL in, NULL out).
    if name == "hstore" {
        return construct(args);
    }
    if super::any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "akeys" => {
            let h = arg_hstore(args, 0, name)?;
            Ok(SqlValue::Array(
                sorted_pairs(&h)
                    .into_iter()
                    .map(|(k, _)| SqlValue::Text(k.clone()))
                    .collect(),
            ))
        }
        "avals" => {
            let h = arg_hstore(args, 0, name)?;
            Ok(SqlValue::Array(
                sorted_pairs(&h)
                    .into_iter()
                    .map(|(_, v)| v.clone().map_or(SqlValue::Null, SqlValue::Text))
                    .collect(),
            ))
        }
        "hstore_to_json" | "hstore_to_jsonb" => {
            let h = arg_hstore(args, 0, name)?;
            Ok(SqlValue::Json(to_json(&h)))
        }
        "hstore_to_matrix" => {
            let h = arg_hstore(args, 0, name)?;
            Ok(SqlValue::Array(
                sorted_pairs(&h)
                    .into_iter()
                    .map(|(k, v)| {
                        SqlValue::Array(vec![
                            SqlValue::Text(k.clone()),
                            v.clone().map_or(SqlValue::Null, SqlValue::Text),
                        ])
                    })
                    .collect(),
            ))
        }
        "exist" => {
            let h = arg_hstore(args, 0, name)?;
            let k = arg_text(args, 1, name)?;
            Ok(SqlValue::Bool(h.contains_key(&k)))
        }
        "defined" => {
            let h = arg_hstore(args, 0, name)?;
            let k = arg_text(args, 1, name)?;
            Ok(SqlValue::Bool(matches!(h.get(&k), Some(Some(_)))))
        }
        "delete" => {
            let h = arg_hstore(args, 0, name)?;
            delete(h, args.get(1), name)
        }
        "slice" => {
            let h = arg_hstore(args, 0, name)?;
            let keys = arg_text_array(args, 1, name)?;
            let mut out = Pairs::new();
            for k in keys.into_iter().flatten() {
                if let Some(v) = h.get(&k) {
                    out.insert(k, v.clone());
                }
            }
            Ok(SqlValue::HStore(out))
        }
        "hs_concat" => {
            let a = arg_hstore(args, 0, name)?;
            let b = arg_hstore(args, 1, name)?;
            Ok(SqlValue::HStore(concat(a, b)))
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
    let h = arg_hstore(&args, 0, op)?;
    match op {
        // `h -> text` is the value (or NULL); `h -> text[]` is a text[] of
        // values in the requested key order, NULL where absent (PG-verified).
        "->" => match right {
            SqlValue::Array(_) => {
                let keys = arg_text_array(&args, 1, op)?;
                Ok(SqlValue::Array(
                    keys.into_iter()
                        .map(|k| {
                            k.and_then(|k| h.get(&k).cloned().flatten())
                                .map_or(SqlValue::Null, SqlValue::Text)
                        })
                        .collect(),
                ))
            }
            _ => {
                let k = arg_text(&args, 1, op)?;
                Ok(h.get(&k)
                    .cloned()
                    .flatten()
                    .map_or(SqlValue::Null, SqlValue::Text))
            }
        },
        "||" => {
            let b = arg_hstore(&args, 1, op)?;
            Ok(SqlValue::HStore(concat(h, b)))
        }
        "?" => {
            let k = arg_text(&args, 1, op)?;
            Ok(SqlValue::Bool(h.contains_key(&k)))
        }
        "-" => delete(h, args.get(1), op),
        "@>" => {
            let b = arg_hstore(&args, 1, op)?;
            Ok(SqlValue::Bool(contains(&h, &b)))
        }
        "<@" => {
            let b = arg_hstore(&args, 1, op)?;
            Ok(SqlValue::Bool(contains(&b, &h)))
        }
        _ => Err(no_such(op)),
    }
}

/// The `hstore(...)` constructors: `hstore(text, text)` (value may be NULL)
/// and `hstore(text[], text[])` (a NULL values array means all-NULL values,
/// like PG).
fn construct(args: &[SqlValue]) -> Result<SqlValue> {
    let func = "hstore";
    match args.first() {
        None | Some(SqlValue::Null) => Ok(SqlValue::Null),
        Some(SqlValue::Array(keys)) => {
            let keys: Vec<Option<String>> = keys.iter().map(text_of).collect();
            let values: Vec<Option<String>> = match args.get(1) {
                Some(SqlValue::Null) | None => vec![None; keys.len()],
                Some(SqlValue::Array(vals)) => {
                    if vals.len() != keys.len() {
                        return Err(SqlError::InvalidParameter(
                            "arrays must have same bounds".into(),
                        ));
                    }
                    vals.iter().map(text_of).collect()
                }
                Some(other) => return Err(bad_arg(func, 1, "text[]", other)),
            };
            let mut map = Pairs::new();
            for (k, v) in keys.into_iter().zip(values) {
                let k = k.ok_or_else(|| {
                    SqlError::InvalidParameter("null value not allowed for hstore key".into())
                })?;
                map.entry(k).or_insert(v);
            }
            Ok(SqlValue::HStore(map))
        }
        Some(_) => {
            let k = arg_text(args, 0, func)?;
            let v = match args.get(1) {
                Some(SqlValue::Null) | None => None,
                Some(_) => Some(arg_text(args, 1, func)?),
            };
            Ok(SqlValue::HStore(Pairs::from([(k, v)])))
        }
    }
}

/// The three `delete` overloads, keyed on the second argument's type:
/// text (one key), text[] (several keys), hstore (exact pairs).
fn delete(mut h: Pairs, target: Option<&SqlValue>, func: &str) -> Result<SqlValue> {
    match target {
        Some(SqlValue::Text(k)) | Some(SqlValue::Citext(k)) => {
            h.remove(k);
        }
        Some(SqlValue::Array(keys)) => {
            for k in keys.iter().filter_map(text_of) {
                h.remove(&k);
            }
        }
        Some(SqlValue::HStore(b)) => {
            h.retain(|k, v| b.get(k) != Some(v));
        }
        Some(other) => return Err(bad_arg(func, 1, "text, text[] or hstore", other)),
        None => return Err(missing_arg(func, 1)),
    }
    Ok(SqlValue::HStore(h))
}

/// `a || b`: right operand wins on duplicate keys (PG-verified:
/// `'a=>1, b=>2' || 'b=>3, c=>4'` is `"a"=>"1", "b"=>"3", "c"=>"4"`).
fn concat(a: Pairs, b: Pairs) -> Pairs {
    let mut out = a;
    out.extend(b);
    out
}

/// `a @> b`: every pair of `b` occurs in `a` with the same value
/// (hstore NULLs match hstore NULLs; PG-verified).
fn contains(a: &Pairs, b: &Pairs) -> bool {
    b.iter().all(|(k, v)| a.get(k) == Some(v))
}

/// Pairs in contrib/hstore's internal order — key length first, then bytes —
/// which is the order `akeys`/`avals`/`hstore_to_matrix` and the text output
/// use (PG-verified: `akeys('zz=>1, b=>2, aa=>3, a=>4')` is `{a,b,aa,zz}`).
fn sorted_pairs(map: &Pairs) -> Vec<(&String, &Option<String>)> {
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    pairs
}

/// JSON object with every value a JSON string (hstore NULL -> JSON null),
/// exactly what `hstore_to_json`/`hstore_to_jsonb` produce.
fn to_json(map: &Pairs) -> Json {
    Json::Object(
        sorted_pairs(map)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone().map_or(Json::Null, Json::String)))
            .collect(),
    )
}

fn text_of(v: &SqlValue) -> Option<String> {
    match v {
        SqlValue::Null => None,
        other => other.to_text(),
    }
}

/// Extract an hstore argument (HStore, or text parsed as hstore) at `idx`.
fn arg_hstore(args: &[SqlValue], idx: usize, func: &str) -> Result<Pairs> {
    match args.get(idx) {
        Some(SqlValue::HStore(map)) => Ok(map.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::HStore)? {
                SqlValue::HStore(map) => Ok(map),
                _ => unreachable!("from_text(hstore) yields HStore"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "hstore", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// Extract a text[] argument at `idx`; SQL NULL elements come back as `None`.
fn arg_text_array(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<Option<String>>> {
    match args.get(idx) {
        Some(SqlValue::Array(items)) => Ok(items.iter().map(text_of).collect()),
        Some(other) => Err(bad_arg(func, idx, "text[]", other)),
        None => Err(missing_arg(func, idx)),
    }
}

#[cfg(test)]
mod tests {
    //! Expected values generated from PostgreSQL 16.13 with hstore 1.8.
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

    fn h(text: &str) -> SqlValue {
        SqlValue::from_text(text, &SqlType::HStore).unwrap()
    }

    fn t(s: &str) -> SqlValue {
        SqlValue::Text(s.into())
    }

    fn arr(items: &[&str]) -> SqlValue {
        SqlValue::Array(items.iter().map(|s| t(s)).collect())
    }

    fn text_out(v: SqlValue) -> String {
        v.to_text().expect("non-null")
    }

    #[test]
    fn constructors_match_pg() {
        // PG: hstore('k','v') => "k"=>"v"; hstore('k',NULL) => "k"=>NULL
        assert_eq!(
            text_out(invoke("hstore", &[t("k"), t("v")]).unwrap()),
            r#""k"=>"v""#
        );
        assert_eq!(
            text_out(invoke("hstore", &[t("k"), SqlValue::Null]).unwrap()),
            r#""k"=>NULL"#
        );
        // PG: hstore(ARRAY['a','b'], ARRAY['1',NULL]) => "a"=>"1", "b"=>NULL
        let keys = arr(&["a", "b"]);
        let vals = SqlValue::Array(vec![t("1"), SqlValue::Null]);
        assert_eq!(
            text_out(invoke("hstore", &[keys, vals]).unwrap()),
            r#""a"=>"1", "b"=>NULL"#
        );
        // Mismatched array lengths error like PG (arrays must have same bounds).
        assert!(invoke("hstore", &[arr(&["a", "b"]), arr(&["1"])]).is_err());
    }

    #[test]
    fn akeys_avals_use_internal_order() {
        // PG: akeys('zz=>1, b=>2, aa=>3, a=>4') => {a,b,aa,zz}
        //     avals(...)                        => {4,2,3,1}
        let hs = h("zz=>1, b=>2, aa=>3, a=>4");
        assert_eq!(
            text_out(invoke("akeys", std::slice::from_ref(&hs)).unwrap()),
            "{a,b,aa,zz}"
        );
        assert_eq!(text_out(invoke("avals", &[hs]).unwrap()), "{4,2,3,1}");
    }

    #[test]
    fn json_and_matrix_forms() {
        // PG: hstore_to_json('a=>1, b=>NULL, c=>"x y"') =>
        //     {"a": "1", "b": null, "c": "x y"}  (content identical; GuardianDB
        //     prints JSON compactly, without PG's spaces)
        let out = invoke("hstore_to_json", &[h(r#"a=>1, b=>NULL, c=>"x y""#)]).unwrap();
        match out {
            SqlValue::Json(j) => {
                assert_eq!(j, serde_json::json!({"a": "1", "b": null, "c": "x y"}));
            }
            other => panic!("expected json, got {other:?}"),
        }
        // Numbers stay strings: PG hstore_to_json('n=>1.5') => {"n": "1.5"}
        let out = invoke("hstore_to_jsonb", &[h("n=>1.5")]).unwrap();
        assert_eq!(text_out(out), r#"{"n":"1.5"}"#);
        // PG: hstore_to_matrix('a=>1, b=>2') => {{a,1},{b,2}}
        assert_eq!(
            text_out(invoke("hstore_to_matrix", &[h("a=>1, b=>2")]).unwrap()),
            "{{a,1},{b,2}}"
        );
    }

    #[test]
    fn exist_and_defined_match_pg() {
        // PG: exist('a=>1','a') t; exist('a=>1','b') f
        assert_eq!(
            invoke("exist", &[h("a=>1"), t("a")]).unwrap().to_text(),
            Some("t".into())
        );
        assert_eq!(
            invoke("exist", &[h("a=>1"), t("b")]).unwrap().to_text(),
            Some("f".into())
        );
        // PG: defined('a=>NULL','a') f; defined('a=>1','a') t
        assert_eq!(
            invoke("defined", &[h("a=>NULL"), t("a")])
                .unwrap()
                .to_text(),
            Some("f".into())
        );
        assert_eq!(
            invoke("defined", &[h("a=>1"), t("a")]).unwrap().to_text(),
            Some("t".into())
        );
        // Key matching is case-sensitive: PG 'Key=>Val' ? 'key' => f
        assert_eq!(
            operator("?", &h("Key=>Val"), &t("key")).unwrap().to_text(),
            Some("f".into())
        );
    }

    #[test]
    fn delete_overloads_match_pg() {
        // PG: delete('a=>1, b=>2','a') => "b"=>"2"
        assert_eq!(
            text_out(invoke("delete", &[h("a=>1, b=>2"), t("a")]).unwrap()),
            r#""b"=>"2""#
        );
        // PG: delete('a=>1, b=>2, c=>3', ARRAY['a','c']) => "b"=>"2"
        assert_eq!(
            text_out(invoke("delete", &[h("a=>1, b=>2, c=>3"), arr(&["a", "c"])]).unwrap()),
            r#""b"=>"2""#
        );
        // PG: delete('a=>1, b=>2', 'a=>1, b=>99'::hstore) => "b"=>"2"
        //     (pairs must match key AND value)
        assert_eq!(
            text_out(invoke("delete", &[h("a=>1, b=>2"), h("a=>1, b=>99")]).unwrap()),
            r#""b"=>"2""#
        );
    }

    #[test]
    fn slice_ignores_missing_keys() {
        // PG: slice('a=>1, b=>2, c=>3', ARRAY['b','c','x']) => "b"=>"2", "c"=>"3"
        assert_eq!(
            text_out(invoke("slice", &[h("a=>1, b=>2, c=>3"), arr(&["b", "c", "x"])]).unwrap()),
            r#""b"=>"2", "c"=>"3""#
        );
    }

    #[test]
    fn operators_match_pg() {
        // PG: 'a=>1, b=>2'::hstore -> 'b' => 2
        assert_eq!(
            operator("->", &h("a=>1, b=>2"), &t("b")).unwrap().to_text(),
            Some("2".into())
        );
        assert!(operator("->", &h("a=>1"), &t("zzz")).unwrap().is_null());
        // hstore NULL value surfaces as SQL NULL: PG ('a=>NULL' -> 'a') IS NULL
        assert!(operator("->", &h("a=>NULL"), &t("a")).unwrap().is_null());
        // PG: 'a=>1, b=>2, c=>3' -> ARRAY['c','x','a'] => {3,NULL,1}
        assert_eq!(
            text_out(operator("->", &h("a=>1, b=>2, c=>3"), &arr(&["c", "x", "a"])).unwrap()),
            "{3,NULL,1}"
        );
        // PG: 'a=>1, b=>2' || 'b=>3, c=>4' => "a"=>"1", "b"=>"3", "c"=>"4"
        assert_eq!(
            text_out(operator("||", &h("a=>1, b=>2"), &h("b=>3, c=>4")).unwrap()),
            r#""a"=>"1", "b"=>"3", "c"=>"4""#
        );
        // PG: 'a=>1, b=>2' - 'a'::text => "b"=>"2"
        assert_eq!(
            text_out(operator("-", &h("a=>1, b=>2"), &t("a")).unwrap()),
            r#""b"=>"2""#
        );
        // Containment (PG-verified truth table).
        for (a, b, expect) in [
            ("a=>1, b=>2", "a=>1", true),
            ("a=>1, b=>2", "a=>2", false),
            ("a=>NULL", "a=>NULL", true),
        ] {
            assert_eq!(
                operator("@>", &h(a), &h(b)).unwrap().to_text(),
                Some(if expect { "t" } else { "f" }.into()),
                "{a} @> {b}"
            );
        }
        assert_eq!(
            operator("<@", &h("a=>1"), &h("a=>1, b=>2"))
                .unwrap()
                .to_text(),
            Some("t".into())
        );
    }

    #[test]
    fn null_operands_yield_null() {
        assert!(operator("->", &SqlValue::Null, &t("a")).unwrap().is_null());
        assert!(
            operator("||", &h("a=>1"), &SqlValue::Null)
                .unwrap()
                .is_null()
        );
        for name in ["akeys", "avals", "exist", "delete", "slice"] {
            assert!(
                invoke(name, &[SqlValue::Null, t("x")]).unwrap().is_null(),
                "{name} must be strict"
            );
        }
        // The hstore(text, NULL) constructor is the deliberate exception.
        assert!(
            !invoke("hstore", &[t("k"), SqlValue::Null])
                .unwrap()
                .is_null()
        );
        assert!(
            invoke("hstore", &[SqlValue::Null, t("v")])
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn every_registered_function_is_routed() {
        for name in DEF.functions {
            let args = match *name {
                "delete" | "exist" | "defined" => vec![h("a=>1"), t("a")],
                "slice" => vec![h("a=>1"), arr(&["a"])],
                "hs_concat" => vec![h("a=>1"), h("b=>2")],
                "hstore" => vec![t("a"), t("b")],
                _ => vec![h("a=>1")],
            };
            assert!(invoke(name, &args).is_ok(), "{name} not routed");
        }
    }

    #[test]
    fn wrong_argument_types_error() {
        assert!(invoke("akeys", &[SqlValue::Int4(1)]).is_err());
        assert!(invoke("slice", &[h("a=>1"), t("not-an-array")]).is_err());
        assert!(operator("?", &SqlValue::Int4(3), &t("a")).is_err());
    }
}
