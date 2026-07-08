//! Native implementation of PostgreSQL's `unaccent` extension.
//!
//! `unaccent(text)` — and the dictionary-qualified `unaccent(regdictionary,
//! text)` form — strips accents and other diacritic marks from its input.
//! PostgreSQL drives this from a rules file (`unaccent.rules`); GuardianDB
//! reproduces the same output pipeline natively:
//!
//! 1. Unicode NFD decomposition splits precomposed characters into a base
//!    character plus combining marks, and every combining mark (general
//!    category `Mn`) is dropped — this covers the bulk of the rules file
//!    (`é` → `e`, `Î` → `I`, `ź` → `z`, ...).
//! 2. A fixed table applies the expansions the rules file lists for
//!    characters NFD alone cannot handle: ligatures and letters with no
//!    canonical decomposition (`Æ` → `AE`, `ß` → `ss`, `Ł` → `L`, `ø` → `o`,
//!    ...) plus the typographic punctuation the rules file maps (dashes,
//!    curly quotes, `…`, `©`, `®`, `™`, the vulgar fractions, `№`).
//! 3. NFC recomposition restores canonical form for whatever remains.
//!
//! Only the extension's own dictionary exists: the two-argument form accepts
//! `unaccent` (optionally schema-qualified as `public.unaccent`, folded
//! case-insensitively like an unquoted identifier) and rejects any other
//! dictionary name with the undefined-object error PostgreSQL raises. The
//! function is STRICT: any NULL argument yields NULL — before dictionary
//! validation, exactly as PostgreSQL's strictness short-circuit behaves.

use super::{ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_text, no_such};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Registry entry for `CREATE EXTENSION unaccent`.
pub static DEF: ExtensionDef = ExtensionDef {
    name: "unaccent",
    default_version: "1.1",
    comment: "text search dictionary that removes accents",
    requires: &[],
    functions: &["unaccent"],
    types: &[],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// Function-call entry point for the extension registry.
fn call(_ctx: &ExtCtx, func: &str, args: &[SqlValue]) -> Result<SqlValue> {
    match func {
        "unaccent" => unaccent(args),
        _ => Err(no_such(func)),
    }
}

/// `unaccent(text)` / `unaccent(dictionary, text)`.
fn unaccent(args: &[SqlValue]) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    let text = match args.len() {
        1 => arg_text(args, 0, "unaccent")?,
        2 => {
            check_dictionary(&arg_text(args, 0, "unaccent")?)?;
            arg_text(args, 1, "unaccent")?
        }
        _ => {
            return Err(SqlError::UndefinedFunction(format!(
                "unaccent({})",
                args.iter()
                    .map(|a| a.type_of().name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    Ok(SqlValue::Text(strip_accents(&text)))
}

/// Validate the dictionary argument of the two-argument form. The extension
/// ships exactly one text search dictionary, `public.unaccent`; anything else
/// fails the way PostgreSQL's `regdictionary` lookup would.
fn check_dictionary(dict: &str) -> Result<()> {
    let name = dict.trim();
    let unqualified = name
        .strip_prefix("public.")
        .or_else(|| name.strip_prefix("PUBLIC."))
        .unwrap_or(name);
    if unqualified.eq_ignore_ascii_case("unaccent") {
        return Ok(());
    }
    Err(SqlError::UndefinedObject(format!(
        "text search dictionary \"{dict}\""
    )))
}

/// The accent-stripping pipeline: NFD, drop combining marks, expand the
/// rules-file specials, NFC-recompose the remainder.
fn strip_accents(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        match special(ch) {
            Some(s) => out.push_str(s),
            None => out.push(ch),
        }
    }
    out.as_str().nfc().collect()
}

/// Expansions from PostgreSQL's `unaccent.rules` for characters that NFD
/// decomposition alone does not reduce: ligatures, letters whose diacritic is
/// part of the code point (stroke, bar, eth, thorn), and the typographic
/// punctuation the rules file maps to ASCII.
fn special(ch: char) -> Option<&'static str> {
    Some(match ch {
        'æ' => "ae",
        'Æ' => "AE",
        'œ' => "oe",
        'Œ' => "OE",
        'ø' => "o",
        'Ø' => "O",
        'đ' | 'ð' => "d",
        'Đ' | 'Ð' => "D",
        'þ' => "th",
        'Þ' => "TH",
        'ß' => "ss",
        'ẞ' => "SS",
        'ł' => "l",
        'Ł' => "L",
        'ħ' => "h",
        'Ħ' => "H",
        'ı' => "i",
        'ĸ' => "k",
        'ŋ' => "n",
        'Ŋ' => "N",
        'ⱥ' => "a",
        'ȼ' => "c",
        // En and em dashes.
        '\u{2013}' | '\u{2014}' => "-",
        // Single curly quotes and the low-9 single quote.
        '\u{2018}' | '\u{2019}' | '\u{201A}' => "'",
        // Double curly quotes and the low-9 double quote.
        '\u{201C}' | '\u{201D}' | '\u{201E}' => "\"",
        // Horizontal ellipsis.
        '\u{2026}' => "...",
        '©' => "(C)",
        '®' => "(R)",
        // Trade mark sign.
        '\u{2122}' => "(TM)",
        '½' => "1/2",
        '¼' => "1/4",
        '¾' => "3/4",
        // Numero sign.
        '\u{2116}' => "No",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn run(args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: Utc::now(),
            vars: &vars,
        };
        call(&ctx, "unaccent", args)
    }

    fn txt(s: &str) -> SqlValue {
        SqlValue::Text(s.to_string())
    }

    fn out(args: &[SqlValue]) -> String {
        match run(args).unwrap() {
            SqlValue::Text(s) => s,
            other => panic!("expected text result, got {other:?}"),
        }
    }

    #[test]
    fn strips_diacritics_like_postgres() {
        assert_eq!(out(&[txt("Hôtel")]), "Hotel");
        assert_eq!(out(&[txt("ÀÉÎÕÜ")]), "AEIOU");
        assert_eq!(out(&[txt("naïve café")]), "naive cafe");
    }

    #[test]
    fn expands_rules_file_specials() {
        assert_eq!(out(&[txt("Æther")]), "AEther");
        assert_eq!(out(&[txt("straße")]), "strasse");
        assert_eq!(out(&[txt("Łódź")]), "Lodz");
        assert_eq!(out(&[txt("smørrebrød")]), "smorrebrod");
        assert_eq!(out(&[txt("Þórður")]), "THordur");
        assert_eq!(out(&[txt("œuvre")]), "oeuvre");
    }

    #[test]
    fn maps_typographic_punctuation() {
        assert_eq!(out(&[txt("“smart” — quotes…")]), "\"smart\" - quotes...");
        assert_eq!(out(&[txt("‘a’ ‚b‛?")]), "'a' 'b‛?");
        assert_eq!(out(&[txt("© ® ™ № ½ ¼ ¾")]), "(C) (R) (TM) No 1/2 1/4 3/4");
    }

    #[test]
    fn ascii_and_empty_pass_through() {
        assert_eq!(
            out(&[txt("plain ASCII text 123!")]),
            "plain ASCII text 123!"
        );
        assert_eq!(out(&[txt("")]), "");
    }

    #[test]
    fn null_arguments_are_strict() {
        assert!(matches!(run(&[SqlValue::Null]).unwrap(), SqlValue::Null));
        assert!(matches!(
            run(&[SqlValue::Null, txt("Hôtel")]).unwrap(),
            SqlValue::Null
        ));
        assert!(matches!(
            run(&[txt("unaccent"), SqlValue::Null]).unwrap(),
            SqlValue::Null
        ));
        // Strictness short-circuits before dictionary validation, as in
        // PostgreSQL: a bad dictionary with a NULL input still yields NULL.
        assert!(matches!(
            run(&[txt("other_dict"), SqlValue::Null]).unwrap(),
            SqlValue::Null
        ));
    }

    #[test]
    fn two_argument_form_accepts_the_unaccent_dictionary() {
        assert_eq!(out(&[txt("unaccent"), txt("Hôtel")]), "Hotel");
        assert_eq!(out(&[txt("public.unaccent"), txt("Łódź")]), "Lodz");
    }

    #[test]
    fn two_argument_form_rejects_unknown_dictionaries() {
        match run(&[txt("other_dict"), txt("Hôtel")]) {
            Err(SqlError::UndefinedObject(d)) => {
                assert_eq!(d, "text search dictionary \"other_dict\"");
            }
            other => panic!("expected UndefinedObject error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_function_is_an_internal_error() {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: Utc::now(),
            vars: &vars,
        };
        assert!(matches!(
            call(&ctx, "not_unaccent", &[txt("x")]),
            Err(SqlError::Internal(_))
        ));
    }
}
