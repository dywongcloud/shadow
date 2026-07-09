//! Native implementation of PostgreSQL's `pg_trgm` extension: trigram-based
//! text similarity measurement.
//!
//! A trigram is a group of three consecutive characters taken from a string.
//! Matching PostgreSQL's extraction rules, the input is lowercased and split
//! into words (maximal runs of alphanumeric characters); each word is padded
//! with two leading spaces and one trailing space, and every three-character
//! window of the padded word (characters, not bytes) yields one trigram. The
//! trigram *set* of a string is the deduplicated union over its words.
//!
//! `similarity(a, b)` is the Jaccard ratio `|∩| / |∪|` of the two trigram
//! sets. `word_similarity(a, b)` is the greatest similarity between the
//! trigram set of `a` and any continuous extent of the ordered trigram
//! sequence of `b`, while `strict_word_similarity` restricts extents to
//! whole-word ranges of `b`. The `%`, `<%`, `%>`, `<<%` and `%>>` operators
//! compare those measures against the extension's three threshold GUCs, and
//! `<->` is the similarity distance `1 - similarity`.

use super::{ExtCtx, ExtensionDef, GucSpec, RuntimeStrategy, any_null, arg_f64, arg_text, no_such};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};
use std::collections::HashSet;

/// GUC read by `%` and `<->` (settable via `set_limit`, shown by `show_limit`).
const SIMILARITY_THRESHOLD: &str = "pg_trgm.similarity_threshold";
/// GUC read by the `<%` / `%>` word-similarity operators.
const WORD_SIMILARITY_THRESHOLD: &str = "pg_trgm.word_similarity_threshold";
/// GUC read by the `<<%` / `%>>` strict word-similarity operators.
const STRICT_WORD_SIMILARITY_THRESHOLD: &str = "pg_trgm.strict_word_similarity_threshold";

pub static DEF: ExtensionDef = ExtensionDef {
    name: "pg_trgm",
    default_version: "1.6",
    comment: "text similarity measurement and index searching based on trigrams",
    requires: &[],
    functions: &[
        "similarity",
        "show_trgm",
        "word_similarity",
        "strict_word_similarity",
        "set_limit",
        "show_limit",
    ],
    types: &[],
    gucs: &[
        GucSpec {
            name: SIMILARITY_THRESHOLD,
            default: "0.3",
        },
        GucSpec {
            name: WORD_SIMILARITY_THRESHOLD,
            default: "0.6",
        },
        GucSpec {
            name: STRICT_WORD_SIMILARITY_THRESHOLD,
            default: "0.5",
        },
    ],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// Function-call entry point registered in [`DEF`]. All six functions are
/// strict: any SQL NULL argument yields NULL.
fn call(ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "similarity" => {
            let a = arg_text(args, 0, name)?;
            let b = arg_text(args, 1, name)?;
            Ok(SqlValue::Float4(similarity(&a, &b)))
        }
        "show_trgm" => {
            let s = arg_text(args, 0, name)?;
            let mut trgms: Vec<String> = trigram_set(&s).into_iter().collect();
            trgms.sort_unstable();
            Ok(SqlValue::Array(
                trgms.into_iter().map(SqlValue::Text).collect(),
            ))
        }
        "word_similarity" => {
            let a = arg_text(args, 0, name)?;
            let b = arg_text(args, 1, name)?;
            Ok(SqlValue::Float4(word_similarity(&a, &b)))
        }
        "strict_word_similarity" => {
            let a = arg_text(args, 0, name)?;
            let b = arg_text(args, 1, name)?;
            Ok(SqlValue::Float4(strict_word_similarity(&a, &b)))
        }
        "set_limit" => {
            let limit = arg_f64(args, 0, name)? as f32;
            ctx.set_var(SIMILARITY_THRESHOLD, limit.to_string());
            Ok(SqlValue::Float4(limit))
        }
        "show_limit" => Ok(SqlValue::Float4(ctx.get_f32(SIMILARITY_THRESHOLD, 0.3))),
        _ => Err(no_such(name)),
    }
}

/// pg_trgm operator dispatch (`%`, `<%`, `%>`, `<<%`, `%>>`, `<->`), called
/// from the engine's extension-operator fallthrough with text operands. SQL
/// NULL semantics: any NULL operand yields NULL.
pub fn operator(ctx: &ExtCtx, op: &str, left: &SqlValue, right: &SqlValue) -> Result<SqlValue> {
    if left.is_null() || right.is_null() {
        return Ok(SqlValue::Null);
    }
    let (l, r) = match (left, right) {
        (SqlValue::Text(l) | SqlValue::Citext(l), SqlValue::Text(r) | SqlValue::Citext(r)) => {
            (l.as_str(), r.as_str())
        }
        _ => {
            return Err(SqlError::Internal(format!(
                "pg_trgm operator {op} dispatched with non-text operands"
            )));
        }
    };
    match op {
        "%" => Ok(SqlValue::Bool(
            similarity(l, r) >= ctx.get_f32(SIMILARITY_THRESHOLD, 0.3),
        )),
        "<%" => Ok(SqlValue::Bool(
            word_similarity(l, r) >= ctx.get_f32(WORD_SIMILARITY_THRESHOLD, 0.6),
        )),
        "%>" => Ok(SqlValue::Bool(
            word_similarity(r, l) >= ctx.get_f32(WORD_SIMILARITY_THRESHOLD, 0.6),
        )),
        "<<%" => Ok(SqlValue::Bool(
            strict_word_similarity(l, r) >= ctx.get_f32(STRICT_WORD_SIMILARITY_THRESHOLD, 0.5),
        )),
        "%>>" => Ok(SqlValue::Bool(
            strict_word_similarity(r, l) >= ctx.get_f32(STRICT_WORD_SIMILARITY_THRESHOLD, 0.5),
        )),
        "<->" => Ok(SqlValue::Float8(1.0 - f64::from(similarity(l, r)))),
        _ => Err(no_such(op)),
    }
}

/// The words of `s`: maximal runs of alphanumeric characters, lowercased.
fn words(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Trigrams of one (already lowercased, non-empty) word: pad with two leading
/// spaces and one trailing space, then take every 3-character window.
fn word_trigrams(word: &str) -> Vec<String> {
    let mut padded: Vec<char> = vec![' ', ' '];
    padded.extend(word.chars());
    padded.push(' ');
    padded.windows(3).map(|w| w.iter().collect()).collect()
}

/// The trigram set of `s`: the deduplicated union over its words.
fn trigram_set(s: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for word in words(s) {
        set.extend(word_trigrams(&word));
    }
    set
}

/// The ordered trigram sequence of `s`: word trigrams in text order,
/// deduplicated keeping first occurrences. `word_similarity` extents are the
/// contiguous slices of this sequence.
fn trigram_sequence(s: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut seq = Vec::new();
    for word in words(s) {
        for trgm in word_trigrams(&word) {
            if seen.insert(trgm.clone()) {
                seq.push(trgm);
            }
        }
    }
    seq
}

/// Jaccard similarity of two trigram sets; 0 when the union is empty (which
/// makes `similarity('', '')` 0, matching PostgreSQL).
fn set_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

/// `similarity(a, b)`: Jaccard ratio of the trigram sets of the two strings.
fn similarity(a: &str, b: &str) -> f32 {
    set_similarity(&trigram_set(a), &trigram_set(b))
}

/// `word_similarity(query, text)`: the greatest similarity between the
/// trigram set of `query` and any continuous extent of the ordered trigram
/// sequence of `text` (per the PostgreSQL documentation's definition).
fn word_similarity(query: &str, text: &str) -> f32 {
    let qset = trigram_set(query);
    let seq = trigram_sequence(text);
    let mut best = 0.0_f32;
    for start in 0..seq.len() {
        let mut inter = 0_usize;
        // Extent trigrams outside the query set; they only grow the union.
        let mut extra = 0_usize;
        for trgm in &seq[start..] {
            if qset.contains(trgm) {
                inter += 1;
            } else {
                extra += 1;
            }
            let union = qset.len() + extra;
            if union > 0 {
                best = best.max(inter as f32 / union as f32);
            }
        }
    }
    best
}

/// `strict_word_similarity(query, text)`: like [`word_similarity`], but
/// extents must start and end at word boundaries of `text` — the maximum of
/// `similarity(query, w)` over every contiguous word range `w` of `text`.
fn strict_word_similarity(query: &str, text: &str) -> f32 {
    let qset = trigram_set(query);
    let ws = words(text);
    let mut best = 0.0_f32;
    for start in 0..ws.len() {
        let mut extent: HashSet<String> = HashSet::new();
        for word in &ws[start..] {
            extent.extend(word_trigrams(word));
            best = best.max(set_similarity(&qset, &extent));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    type Vars = RefCell<HashMap<String, String>>;

    fn ctx(vars: &Vars) -> ExtCtx<'_> {
        ExtCtx {
            now: chrono::Utc::now(),
            vars,
        }
    }

    fn text(s: &str) -> SqlValue {
        SqlValue::Text(s.to_string())
    }

    fn f32_of(v: SqlValue) -> f32 {
        match v {
            SqlValue::Float4(f) => f,
            other => panic!("expected float4, got {other:?}"),
        }
    }

    fn bool_of(v: SqlValue) -> bool {
        match v {
            SqlValue::Bool(b) => b,
            other => panic!("expected bool, got {other:?}"),
        }
    }

    #[test]
    fn similarity_matches_postgres() {
        assert_eq!(similarity("word", "two words"), 0.363_636_37);
        assert_eq!(similarity("dog", "dog"), 1.0);
        assert_eq!(similarity("", ""), 0.0);
        assert_eq!(similarity("a", "b"), 0.0);
    }

    #[test]
    fn word_similarity_matches_postgres() {
        assert_eq!(word_similarity("word", "two words"), 0.8);
        assert_eq!(word_similarity("dog", "dog"), 1.0);
    }

    #[test]
    fn strict_word_similarity_matches_postgres() {
        assert_eq!(strict_word_similarity("word", "two words"), 0.571_428_6);
        assert_eq!(strict_word_similarity("dog", "dog"), 1.0);
    }

    #[test]
    fn show_trgm_matches_postgres_sorted_output() {
        let vars = Vars::default();
        let c = ctx(&vars);
        match call(&c, "show_trgm", &[text("word")]).unwrap() {
            SqlValue::Array(items) => {
                let texts: Vec<String> = items
                    .into_iter()
                    .map(|v| match v {
                        SqlValue::Text(s) => s,
                        other => panic!("expected text element, got {other:?}"),
                    })
                    .collect();
                assert_eq!(texts, ["  w", " wo", "ord", "rd ", "wor"]);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn percent_operator_respects_set_limit() {
        let vars = Vars::default();
        let c = ctx(&vars);
        // similarity('word','two words') = 0.36363637 >= default 0.3.
        assert!(bool_of(
            operator(&c, "%", &text("word"), &text("two words")).unwrap()
        ));
        let set = call(&c, "set_limit", &[SqlValue::Float8(0.5)]).unwrap();
        assert_eq!(f32_of(set), 0.5);
        assert!(!bool_of(
            operator(&c, "%", &text("word"), &text("two words")).unwrap()
        ));
        let shown = call(&c, "show_limit", &[]).unwrap();
        assert_eq!(f32_of(shown), 0.5);
    }

    #[test]
    fn word_similarity_operators_and_distance() {
        let vars = Vars::default();
        let c = ctx(&vars);
        // word_similarity('word','two words') = 0.8 >= 0.6 default.
        assert!(bool_of(
            operator(&c, "<%", &text("word"), &text("two words")).unwrap()
        ));
        assert!(bool_of(
            operator(&c, "%>", &text("two words"), &text("word")).unwrap()
        ));
        // strict_word_similarity('word','two words') = 0.5714286 >= 0.5 default.
        assert!(bool_of(
            operator(&c, "<<%", &text("word"), &text("two words")).unwrap()
        ));
        assert!(bool_of(
            operator(&c, "%>>", &text("two words"), &text("word")).unwrap()
        ));
        match operator(&c, "<->", &text("dog"), &text("dog")).unwrap() {
            SqlValue::Float8(d) => assert_eq!(d, 0.0),
            other => panic!("expected float8, got {other:?}"),
        }
        assert!(operator(&c, "<=>", &text("a"), &text("b")).is_err());
    }

    #[test]
    fn citext_operands_accepted() {
        let vars = Vars::default();
        let c = ctx(&vars);
        let l = SqlValue::Citext("WORD".to_string());
        assert!(bool_of(operator(&c, "%", &l, &text("word")).unwrap()));
        let sim = call(&c, "similarity", &[l, SqlValue::Citext("word".to_string())]).unwrap();
        assert_eq!(f32_of(sim), 1.0);
    }

    #[test]
    fn null_arguments_yield_null() {
        let vars = Vars::default();
        let c = ctx(&vars);
        assert!(
            call(&c, "similarity", &[SqlValue::Null, text("x")])
                .unwrap()
                .is_null()
        );
        assert!(call(&c, "set_limit", &[SqlValue::Null]).unwrap().is_null());
        assert!(
            operator(&c, "%", &SqlValue::Null, &text("x"))
                .unwrap()
                .is_null()
        );
        assert!(
            operator(&c, "<->", &text("x"), &SqlValue::Null)
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn empty_strings_have_no_trigrams() {
        let vars = Vars::default();
        let c = ctx(&vars);
        match call(&c, "show_trgm", &[text("")]).unwrap() {
            SqlValue::Array(items) => assert!(items.is_empty()),
            other => panic!("expected array, got {other:?}"),
        }
        assert_eq!(similarity("", "word"), 0.0);
        assert_eq!(word_similarity("", "two words"), 0.0);
        assert_eq!(word_similarity("word", ""), 0.0);
        assert_eq!(strict_word_similarity("word", ""), 0.0);
    }

    #[test]
    fn unicode_words_pad_by_chars() {
        // 'héllo' -> padded "  héllo " -> six 3-character windows.
        let set = trigram_set("héllo");
        assert_eq!(set.len(), 6);
        assert!(set.contains(" hé"));
        assert!(set.contains("hél"));
        assert!(set.contains("lo "));
        // Uppercase accented input lowercases like PostgreSQL under a UTF-8 locale.
        assert_eq!(similarity("HÉLLO", "héllo"), 1.0);
    }
}
