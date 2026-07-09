//! Full-text-search value model: the `tsvector` and `tsquery` types.
//!
//! This module owns the *value* level only — the data shapes, PostgreSQL raw
//! text input (`tsvector_in`/`tsquery_in`, what `::tsvector` / `::tsquery`
//! casts run, with no linguistic normalization), the canonical text output,
//! and `@@` match evaluation. Configuration-driven processing (`to_tsvector`,
//! stemming, stop words) lives in `crate::sql::fts`.

use crate::relational::error::{RelError, Result};

/// Highest storable lexeme position (PostgreSQL `MAXENTRYPOS - 1`); larger
/// input positions clamp here, like `tsvector_in`'s `LIMITPOS`.
pub const MAX_POS: u16 = 16383;

/// Most positions kept per lexeme (PostgreSQL `MAXNUMPOS`); extras are dropped.
pub const MAX_NUM_POS: usize = 256;

/// One `tsvector` entry: a lexeme with its (possibly empty) position list.
/// Invariants: positions are sorted, unique, `1..=MAX_POS`, at most
/// [`MAX_NUM_POS`] long. The derived ordering (word first) is the ordering
/// `tsvector` comparison uses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TsLexeme {
    pub word: String,
    pub positions: Vec<u16>,
}

/// A `tsquery` operator tree. The empty query is represented as `None` at the
/// `SqlValue` level, so every node here is non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsQueryNode {
    Lexeme(String),
    Not(Box<TsQueryNode>),
    And(Box<TsQueryNode>, Box<TsQueryNode>),
    Or(Box<TsQueryNode>, Box<TsQueryNode>),
}

impl TsQueryNode {
    /// Total node count, operators included (`numnode('(fat & rat) | cat')` = 5).
    pub fn count_nodes(&self) -> i32 {
        match self {
            TsQueryNode::Lexeme(_) => 1,
            TsQueryNode::Not(c) => 1 + c.count_nodes(),
            TsQueryNode::And(a, b) | TsQueryNode::Or(a, b) => 1 + a.count_nodes() + b.count_nodes(),
        }
    }

    /// Every lexeme operand in tree order, duplicates and negated ones
    /// included (what PostgreSQL's ranking iterates over).
    pub fn lexemes<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            TsQueryNode::Lexeme(w) => out.push(w),
            TsQueryNode::Not(c) => c.lexemes(out),
            TsQueryNode::And(a, b) | TsQueryNode::Or(a, b) => {
                a.lexemes(out);
                b.lexemes(out);
            }
        }
    }
}

/// Sort by word, merge duplicate lexemes (union of positions), and enforce
/// the per-lexeme position invariants.
pub fn normalize_lexemes(raw: Vec<(String, Vec<u16>)>) -> Vec<TsLexeme> {
    let mut sorted = raw;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<TsLexeme> = Vec::with_capacity(sorted.len());
    for (word, positions) in sorted {
        match out.last_mut() {
            Some(last) if last.word == word => last.positions.extend(positions),
            _ => out.push(TsLexeme { word, positions }),
        }
    }
    for lex in &mut out {
        lex.positions.iter_mut().for_each(|p| {
            *p = (*p).clamp(1, MAX_POS);
        });
        lex.positions.sort_unstable();
        lex.positions.dedup();
        lex.positions.truncate(MAX_NUM_POS);
    }
    out
}

/// `@@` evaluation: `&`/`|`/`!` over lexeme presence. `!` matches when the
/// operand does not. The lexeme list must be sorted (the invariant).
pub fn eval_match(lexemes: &[TsLexeme], node: &TsQueryNode) -> bool {
    match node {
        TsQueryNode::Lexeme(w) => lexemes.binary_search_by(|l| l.word.as_str().cmp(w)).is_ok(),
        TsQueryNode::Not(c) => !eval_match(lexemes, c),
        TsQueryNode::And(a, b) => eval_match(lexemes, a) && eval_match(lexemes, b),
        TsQueryNode::Or(a, b) => eval_match(lexemes, a) || eval_match(lexemes, b),
    }
}

// ---------------------------------------------------------------------------
// Text output.
// ---------------------------------------------------------------------------

/// Quote a lexeme for text output. PostgreSQL always single-quotes lexemes in
/// `tsvector`/`tsquery` output; embedded quotes double, backslashes escape.
fn quote_lexeme(word: &str) -> String {
    format!("'{}'", word.replace('\\', "\\\\").replace('\'', "''"))
}

/// `tsvector` text output: `'cat':3 'fat':2,4` — lexemes in sorted order,
/// always quoted, positions comma-joined after `:`.
pub fn format_tsvector(lexemes: &[TsLexeme]) -> String {
    lexemes
        .iter()
        .map(|l| {
            let mut s = quote_lexeme(&l.word);
            if !l.positions.is_empty() {
                s.push(':');
                s.push_str(
                    &l.positions
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            s
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `tsquery` text output, matching PostgreSQL's infix printer: lexemes are
/// quoted; `!` binds tighter than `&`, which binds tighter than `|`; a child
/// is parenthesized (as `( ... )`) only when its operator binds looser than
/// its parent — so `'fat' & ( 'rat' | 'cat' )` but `!'a' & 'b' | 'c'`.
pub fn format_tsquery(root: Option<&TsQueryNode>) -> String {
    let mut out = String::new();
    if let Some(node) = root {
        infix(node, 0, &mut out);
    }
    out
}

/// Operator priority as PostgreSQL defines it (OR=0, AND=1, NOT=3; the
/// unsupported phrase operator would be 2).
fn priority(node: &TsQueryNode) -> u8 {
    match node {
        TsQueryNode::Or(..) => 0,
        TsQueryNode::And(..) => 1,
        TsQueryNode::Not(_) => 3,
        TsQueryNode::Lexeme(_) => u8::MAX,
    }
}

fn infix(node: &TsQueryNode, parent_priority: u8, out: &mut String) {
    let wrap = priority(node) < parent_priority;
    if wrap {
        out.push_str("( ");
    }
    match node {
        TsQueryNode::Lexeme(w) => out.push_str(&quote_lexeme(w)),
        TsQueryNode::Not(c) => {
            out.push('!');
            infix(c, priority(node), out);
        }
        TsQueryNode::And(a, b) | TsQueryNode::Or(a, b) => {
            let p = priority(node);
            infix(a, p, out);
            out.push_str(if matches!(node, TsQueryNode::And(..)) {
                " & "
            } else {
                " | "
            });
            infix(b, p, out);
        }
    }
    if wrap {
        out.push_str(" )");
    }
}

// ---------------------------------------------------------------------------
// Raw text input (`::tsvector` / `::tsquery` — no linguistic processing).
// ---------------------------------------------------------------------------

fn tsvector_syntax(input: &str) -> RelError {
    RelError::Syntax(format!("syntax error in tsvector: \"{input}\""))
}

fn tsquery_syntax(input: &str) -> RelError {
    RelError::Syntax(format!("syntax error in tsquery: \"{input}\""))
}

/// One lexeme token: quoted (`'...'`, embedded quote doubled) or bare (up to
/// whitespace or a `stop` character), with backslash escaping in both forms.
/// Returns `None` on an empty or unterminated token.
fn lexeme_token(chars: &mut std::iter::Peekable<std::str::Chars>, stop: &[char]) -> Option<String> {
    let mut out = String::new();
    if chars.peek() == Some(&'\'') {
        chars.next();
        loop {
            match chars.next()? {
                '\'' => {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        out.push('\'');
                    } else {
                        return if out.is_empty() { None } else { Some(out) };
                    }
                }
                '\\' => out.push(chars.next()?),
                c => out.push(c),
            }
        }
    }
    loop {
        match chars.peek() {
            Some('\\') => {
                chars.next();
                out.push(chars.next()?);
            }
            Some(c) if !c.is_whitespace() && !stop.contains(c) => {
                out.push(*c);
                chars.next();
            }
            _ => break,
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// PostgreSQL's raw `tsvector` input: whitespace-separated lexemes (quoted or
/// bare), each optionally followed by `:pos[,pos...]`. Lexemes are stored *as
/// given* — no lowercasing, no stemming. Position weight labels `A`/`B`/`C`
/// are out of subset (`setweight` is unsupported) and fail with a typed
/// `0A000`; the default label `D` is accepted as a no-op. Positions clamp to
/// [`MAX_POS`] like PostgreSQL's `LIMITPOS`.
pub fn parse_tsvector(input: &str) -> Result<Vec<TsLexeme>> {
    let mut chars = input.chars().peekable();
    let mut raw: Vec<(String, Vec<u16>)> = Vec::new();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let word = lexeme_token(&mut chars, &[':']).ok_or_else(|| tsvector_syntax(input))?;
        let mut positions = Vec::new();
        if chars.peek() == Some(&':') {
            chars.next();
            loop {
                let mut digits = String::new();
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    digits.push(chars.next().unwrap());
                }
                let pos: u32 = digits.parse().map_err(|_| tsvector_syntax(input))?;
                if pos == 0 {
                    return Err(tsvector_syntax(input));
                }
                positions.push(pos.min(MAX_POS as u32) as u16);
                match chars.peek() {
                    Some('A' | 'a' | 'B' | 'b' | 'C' | 'c') => {
                        return Err(RelError::FeatureNotSupported(format!(
                            "tsvector position weight labels (\"{}{}\") are not supported \
                             (setweight is out of the full-text-search subset)",
                            digits,
                            chars.peek().unwrap()
                        )));
                    }
                    Some('D' | 'd') => {
                        // The default weight: a no-op, like PostgreSQL prints it.
                        chars.next();
                    }
                    _ => {}
                }
                match chars.peek() {
                    Some(',') => {
                        chars.next();
                    }
                    Some(c) if c.is_whitespace() => break,
                    None => break,
                    Some(_) => return Err(tsvector_syntax(input)),
                }
            }
        }
        raw.push((word, positions));
    }
    Ok(normalize_lexemes(raw))
}

/// Tokens of the raw `tsquery` grammar.
enum QTok {
    Open,
    Close,
    And,
    Or,
    Not,
    Lexeme(String),
}

fn tsquery_tokens(input: &str) -> Result<Vec<QTok>> {
    let mut chars = input.chars().peekable();
    let mut out = Vec::new();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        match c {
            '(' => {
                chars.next();
                out.push(QTok::Open);
            }
            ')' => {
                chars.next();
                out.push(QTok::Close);
            }
            '&' => {
                chars.next();
                out.push(QTok::And);
            }
            '|' => {
                chars.next();
                out.push(QTok::Or);
            }
            '!' => {
                chars.next();
                out.push(QTok::Not);
            }
            '<' => {
                // `<->` / `<N>`: the position-aware phrase operator.
                return Err(RelError::FeatureNotSupported(
                    "the tsquery phrase operator <-> is not supported (position-aware \
                     phrase search is out of the full-text-search subset)"
                        .into(),
                ));
            }
            _ => {
                let word = lexeme_token(&mut chars, &['(', ')', '&', '|', '!', ':', '<'])
                    .ok_or_else(|| tsquery_syntax(input))?;
                if chars.peek() == Some(&':') {
                    chars.next();
                    let mut label = String::new();
                    while chars
                        .peek()
                        .is_some_and(|c| matches!(c, 'A'..='D' | 'a'..='d' | '*'))
                    {
                        label.push(chars.next().unwrap());
                    }
                    if label.is_empty() {
                        return Err(tsquery_syntax(input));
                    }
                    if label.contains('*') {
                        return Err(RelError::FeatureNotSupported(
                            "tsquery prefix matching (:*) is not supported".into(),
                        ));
                    }
                    return Err(RelError::FeatureNotSupported(format!(
                        "tsquery weight restrictions (\":{label}\") are not supported \
                         (setweight is out of the full-text-search subset)"
                    )));
                }
                out.push(QTok::Lexeme(word));
            }
        }
    }
    Ok(out)
}

/// PostgreSQL's raw `tsquery` input: lexemes (as given, unnormalized)
/// combined with `&`, `|`, `!` and parentheses. `!` binds tighter than `&`,
/// `&` tighter than `|`. The empty string is the valid empty query (`None`).
pub fn parse_tsquery(input: &str) -> Result<Option<TsQueryNode>> {
    let tokens = tsquery_tokens(input)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let mut pos = 0;
    let node = parse_or(&tokens, &mut pos, input)?;
    if pos != tokens.len() {
        return Err(tsquery_syntax(input));
    }
    Ok(Some(node))
}

fn parse_or(tokens: &[QTok], pos: &mut usize, input: &str) -> Result<TsQueryNode> {
    let mut left = parse_and(tokens, pos, input)?;
    while matches!(tokens.get(*pos), Some(QTok::Or)) {
        *pos += 1;
        let right = parse_and(tokens, pos, input)?;
        left = TsQueryNode::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_and(tokens: &[QTok], pos: &mut usize, input: &str) -> Result<TsQueryNode> {
    let mut left = parse_not(tokens, pos, input)?;
    while matches!(tokens.get(*pos), Some(QTok::And)) {
        *pos += 1;
        let right = parse_not(tokens, pos, input)?;
        left = TsQueryNode::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_not(tokens: &[QTok], pos: &mut usize, input: &str) -> Result<TsQueryNode> {
    if matches!(tokens.get(*pos), Some(QTok::Not)) {
        *pos += 1;
        return Ok(TsQueryNode::Not(Box::new(parse_not(tokens, pos, input)?)));
    }
    match tokens.get(*pos) {
        Some(QTok::Open) => {
            *pos += 1;
            let inner = parse_or(tokens, pos, input)?;
            if !matches!(tokens.get(*pos), Some(QTok::Close)) {
                return Err(tsquery_syntax(input));
            }
            *pos += 1;
            Ok(inner)
        }
        Some(QTok::Lexeme(w)) => {
            *pos += 1;
            Ok(TsQueryNode::Lexeme(w.clone()))
        }
        _ => Err(tsquery_syntax(input)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsvector_raw_parse_matches_pg() {
        // PG: SELECT 'fat:2 cat:3'::tsvector => 'cat':3 'fat':2
        let v = parse_tsvector("fat:2 cat:3").unwrap();
        assert_eq!(format_tsvector(&v), "'cat':3 'fat':2");
        // PG: SELECT 'The Fat Rats'::tsvector => 'Fat' 'Rats' 'The' (no processing)
        let v = parse_tsvector("The Fat Rats").unwrap();
        assert_eq!(format_tsvector(&v), "'Fat' 'Rats' 'The'");
        // Quoted lexemes keep spaces; doubled quote escapes.
        let v = parse_tsvector("'fat rat':1 'don''t':2").unwrap();
        assert_eq!(format_tsvector(&v), "'don''t':2 'fat rat':1");
        // Duplicate lexemes merge positions, sorted and deduped.
        let v = parse_tsvector("a:3,1 a:2,3").unwrap();
        assert_eq!(format_tsvector(&v), "'a':1,2,3");
        // Positions clamp to MAX_POS; label D is the accepted no-op.
        let v = parse_tsvector("a:99999 b:5D").unwrap();
        assert_eq!(format_tsvector(&v), "'a':16383 'b':5");
    }

    #[test]
    fn tsvector_raw_parse_errors() {
        for bad in ["a:", "a:0", "a:x", ":1", "'a", "a:1,"] {
            assert!(
                matches!(parse_tsvector(bad), Err(RelError::Syntax(_))),
                "{bad:?} should be a tsvector syntax error"
            );
        }
        // Weight labels other than D are typed-unsupported, not syntax errors.
        assert!(matches!(
            parse_tsvector("a:1A"),
            Err(RelError::FeatureNotSupported(_))
        ));
    }

    #[test]
    fn tsquery_parse_precedence_and_display() {
        // PG: SELECT 'fat & rat'::tsquery => 'fat' & 'rat'
        let q = parse_tsquery("fat & rat").unwrap();
        assert_eq!(format_tsquery(q.as_ref()), "'fat' & 'rat'");
        // ! binds tighter than &, & tighter than |.
        let q = parse_tsquery("!a & b | c").unwrap();
        assert_eq!(
            q,
            Some(TsQueryNode::Or(
                Box::new(TsQueryNode::And(
                    Box::new(TsQueryNode::Not(Box::new(TsQueryNode::Lexeme("a".into())))),
                    Box::new(TsQueryNode::Lexeme("b".into())),
                )),
                Box::new(TsQueryNode::Lexeme("c".into())),
            ))
        );
        assert_eq!(format_tsquery(q.as_ref()), "!'a' & 'b' | 'c'");
        // PG: SELECT 'fat & (rat | cat)'::tsquery => 'fat' & ( 'rat' | 'cat' )
        let q = parse_tsquery("fat & (rat | cat)").unwrap();
        assert_eq!(format_tsquery(q.as_ref()), "'fat' & ( 'rat' | 'cat' )");
        // PG: SELECT '!(a | b)'::tsquery => !( 'a' | 'b' )
        let q = parse_tsquery("!(a | b)").unwrap();
        assert_eq!(format_tsquery(q.as_ref()), "!( 'a' | 'b' )");
        // Empty input is the empty query.
        assert_eq!(parse_tsquery("").unwrap(), None);
        assert_eq!(format_tsquery(None), "");
    }

    #[test]
    fn tsquery_parse_errors_and_exclusions() {
        for bad in ["a b", "a &", "& a", "(a", "a)", "()", "!"] {
            assert!(
                matches!(parse_tsquery(bad), Err(RelError::Syntax(_))),
                "{bad:?} should be a tsquery syntax error"
            );
        }
        for unsupported in ["a <-> b", "a <2> b", "a:*", "a:A"] {
            assert!(
                matches!(
                    parse_tsquery(unsupported),
                    Err(RelError::FeatureNotSupported(_))
                ),
                "{unsupported:?} should be typed-unsupported"
            );
        }
    }

    #[test]
    fn match_evaluation() {
        let doc = normalize_lexemes(vec![
            ("cat".into(), vec![3]),
            ("fat".into(), vec![2]),
            ("rat".into(), vec![]),
        ]);
        let m = |q: &str| eval_match(&doc, &parse_tsquery(q).unwrap().unwrap());
        assert!(m("cat"));
        assert!(!m("dog"));
        assert!(m("cat & rat"));
        assert!(!m("cat & dog"));
        assert!(m("cat | dog"));
        assert!(m("!dog"));
        assert!(!m("!cat"));
        assert!(m("cat & !dog"));
        assert!(m("!(dog & cat)"));
        assert!(!m("!(fat | cat)"));
    }

    #[test]
    fn numnode_counts() {
        let n = |q: &str| {
            parse_tsquery(q)
                .unwrap()
                .map(|n| n.count_nodes())
                .unwrap_or(0)
        };
        // PG: numnode('(fat & rat) | cat') = 5, numnode('foo & bar') = 3.
        assert_eq!(n("(fat & rat) | cat"), 5);
        assert_eq!(n("foo & bar"), 3);
        assert_eq!(n("foo"), 1);
        assert_eq!(n("!foo"), 2);
        assert_eq!(n(""), 0);
    }
}
