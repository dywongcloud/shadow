//! The `citext` extension: case-insensitive text.
//!
//! Unlike PostgreSQL, where citext is defined entirely by the extension's SQL
//! script, GuardianDB bakes the type into the core relational layer
//! ([`crate::relational::SqlType::Citext`] / [`crate::relational::SqlValue::Citext`]):
//! storage encoding, casts, case-insensitive comparison, and index-key
//! derivation all live there, because the value model cannot depend on the
//! SQL engine's extension registry. What this module contributes is the
//! extension *definition* — so `CREATE EXTENSION citext` gates DDL use of the
//! type (see [`super::check_type_usable`]) — plus the helper functions
//! PostgreSQL's citext exposes: `citextin` (text-input / cast helper) and the
//! comparison support functions `citext_eq` and `citext_cmp`.
//!
//! Case folding uses [`str::to_lowercase`] — the full Unicode lowercase
//! mapping, exactly matching the value-layer semantics in
//! `crate::relational::value`. This is *not* full Unicode case folding:
//! `'ß'` does not fold to `"ss"`, so `'straße'` and `'strasse'` compare
//! unequal even though a casefold-based collation would equate them.

use super::{ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_text, no_such};
use crate::relational::SqlValue;
use crate::sql::error::Result;
use std::cmp::Ordering;

pub static DEF: ExtensionDef = ExtensionDef {
    name: "citext",
    default_version: "1.6",
    comment: "data type for case-insensitive character strings",
    requires: &[],
    functions: &["citext_eq", "citext_cmp", "citextin"],
    types: &["citext"],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// The case-insensitive ordering shared by `citext_eq` / `citext_cmp`: the
/// same `to_lowercase` folding [`SqlValue::compare`] applies to citext
/// operands, so function results always agree with the `=` operator.
fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    // All citext functions are strict: any NULL argument yields NULL.
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "citextin" => Ok(SqlValue::Citext(arg_text(args, 0, name)?)),
        "citext_eq" => {
            let a = arg_text(args, 0, name)?;
            let b = arg_text(args, 1, name)?;
            Ok(SqlValue::Bool(cmp_ci(&a, &b) == Ordering::Equal))
        }
        "citext_cmp" => {
            let a = arg_text(args, 0, name)?;
            let b = arg_text(args, 1, name)?;
            Ok(SqlValue::Int4(match cmp_ci(&a, &b) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }))
        }
        _ => Err(no_such(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relational::SqlType;
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn call_fn(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: Utc::now(),
            vars: &vars,
        };
        call(&ctx, name, args)
    }

    fn text(s: &str) -> SqlValue {
        SqlValue::Text(s.into())
    }

    fn eq(a: &str, b: &str) -> bool {
        match call_fn("citext_eq", &[text(a), text(b)]).unwrap() {
            SqlValue::Bool(v) => v,
            other => panic!("citext_eq returned {other:?}, expected Bool"),
        }
    }

    fn cmp(a: &str, b: &str) -> i32 {
        match call_fn("citext_cmp", &[text(a), text(b)]).unwrap() {
            SqlValue::Int4(v) => v,
            other => panic!("citext_cmp returned {other:?}, expected Int4"),
        }
    }

    #[test]
    fn citextin_returns_citext_preserving_case() {
        let out = call_fn("citextin", &[text("MixedCase")]).unwrap();
        assert!(matches!(&out, SqlValue::Citext(s) if s == "MixedCase"));
        assert_eq!(out.to_text().unwrap(), "MixedCase");
    }

    #[test]
    fn citext_eq_is_case_insensitive() {
        assert!(eq("Hello", "HELLO"));
        assert!(!eq("a", "b"));
        // Citext operands are accepted interchangeably with text.
        let out = call_fn(
            "citext_eq",
            &[SqlValue::Citext("Hello".into()), text("hELLo")],
        )
        .unwrap();
        assert!(matches!(out, SqlValue::Bool(true)));
    }

    #[test]
    fn citext_cmp_orders_case_insensitively() {
        assert_eq!(cmp("apple", "Banana"), -1);
        assert_eq!(cmp("b", "A"), 1);
        assert_eq!(cmp("HELLO", "hello"), 0);
    }

    #[test]
    fn folding_is_to_lowercase_not_full_casefold() {
        // Full Unicode lowercase mapping reaches beyond ASCII...
        assert!(eq("ÄPFEL", "äpfel"));
        assert!(eq("STRASSE", "strasse"));
        // ...but it is not full case folding: 'ß' does not fold to "ss", so
        // these stay unequal (consistent with SqlValue::compare in value.rs).
        assert!(!eq("straße", "strasse"));
        assert!(!eq("STRASSE", "straße"));
        assert_ne!(cmp("straße", "strasse"), 0);
    }

    #[test]
    fn null_arguments_yield_null() {
        for name in ["citextin", "citext_eq", "citext_cmp"] {
            let out = call_fn(name, &[SqlValue::Null, text("x")]).unwrap();
            assert!(out.is_null(), "{name} must be strict");
        }
        let out = call_fn("citext_eq", &[text("x"), SqlValue::Null]).unwrap();
        assert!(out.is_null());
    }

    #[test]
    fn unknown_function_name_errors() {
        assert!(call_fn("citext_bogus", &[text("a")]).is_err());
    }

    #[test]
    fn def_routes_every_declared_function() {
        for name in DEF.functions {
            assert!(
                call_fn(name, &[text("a"), text("b")]).is_ok(),
                "{name} declared in DEF.functions but not routed"
            );
        }
    }

    // ------------------------------------------------------------------
    // Core value-layer semantics this extension relies on (value.rs/types.rs).
    // ------------------------------------------------------------------

    #[test]
    fn core_compare_citext_vs_text_is_case_insensitive() {
        let ci = SqlValue::Citext("Alice".into());
        let t = SqlValue::Text("ALICE".into());
        assert_eq!(ci.compare(&t), Some(Ordering::Equal));
        assert_eq!(t.compare(&ci), Some(Ordering::Equal));
    }

    #[test]
    fn core_index_key_folds_case() {
        assert_eq!(
            SqlValue::Citext("ALICE".into()).index_key(),
            SqlValue::Citext("alice".into()).index_key()
        );
        assert_ne!(
            SqlValue::Citext("alice".into()).index_key(),
            SqlValue::Citext("bob".into()).index_key()
        );
    }

    #[test]
    fn core_from_text_yields_citext() {
        let v = SqlValue::from_text("X", &SqlType::Citext).unwrap();
        assert!(matches!(&v, SqlValue::Citext(s) if s == "X"));
    }

    #[test]
    fn core_cast_text_citext_text_round_trips() {
        let start = SqlValue::Text("CaseKeeper".into());
        let ci = start.cast(&SqlType::Citext).unwrap();
        assert!(matches!(&ci, SqlValue::Citext(s) if s == "CaseKeeper"));
        let back = ci.cast(&SqlType::Text).unwrap();
        assert!(matches!(&back, SqlValue::Text(s) if s == "CaseKeeper"));
    }
}
