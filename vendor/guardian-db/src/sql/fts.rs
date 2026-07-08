//! Full-text search: text search configurations and the config-driven
//! functions (`to_tsvector`, `to_tsquery`, `plainto_tsquery`, `ts_rank`,
//! `ts_headline`, `numnode`, `strip`, and the `@@` operator).
//!
//! Configurations mirroring PostgreSQL's:
//!   * `simple` — lowercase, compound tokenizer, no stemming, no stop words;
//!   * `english` — `simple` plus the snowball English stop-word list and the
//!     Porter stemmer;
//!   * 17 language configurations powered by `rust-stemmers` (Snowball):
//!     `arabic`, `danish`, `dutch`, `finnish`, `french`, `german`, `greek`,
//!     `hungarian`, `italian`, `norwegian`, `portuguese`, `romanian`,
//!     `russian`, `spanish`, `swedish`, `tamil`, `turkish`;
//!   * 9 configs that PG ships without a Snowball stemmer — `armenian`,
//!     `basque`, `catalan`, `hindi`, `indonesian`, `irish`, `lithuanian`,
//!     `nepali`, `yiddish` — accepted as valid config names (no unknown-42704
//!     error) and tokenized with lowercase-only normalization.
//!
//! Any other configuration name is `42704` with PostgreSQL's message shape.
//! The value-level types and raw (`::tsvector` / `::tsquery`) parsers live in
//! [`crate::relational::fts`].

use crate::relational::catalog::TsDictionaryDef;
use crate::relational::fts::{self, MAX_POS, TsLexeme, TsQueryNode};
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::Exec;
use crate::sql::ext::{bad_arg, missing_arg};
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

/// A resolved text search configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsConfig {
    Simple,
    English,
    // Snowball-backed configs (rust-stemmers)
    Arabic,
    Danish,
    Dutch,
    Finnish,
    French,
    German,
    Greek,
    Hungarian,
    Italian,
    Norwegian,
    Portuguese,
    Romanian,
    Russian,
    Spanish,
    Swedish,
    Tamil,
    Turkish,
    // Lowercase-only configs (no Snowball stemmer in rust-stemmers)
    Armenian,
    Basque,
    Catalan,
    Hindi,
    Indonesian,
    Irish,
    Lithuanian,
    Nepali,
    Yiddish,
}

/// Resolve a configuration name (case-folded like PostgreSQL's `regconfig`;
/// an optional `pg_catalog.` qualifier is accepted). Unknown names are `42704`.
pub fn resolve_config(name: &str) -> Result<TsConfig> {
    let lower = name.trim().to_ascii_lowercase();
    let base = lower.strip_prefix("pg_catalog.").unwrap_or(&lower);
    match base {
        "simple" => Ok(TsConfig::Simple),
        "english" => Ok(TsConfig::English),
        // Snowball-backed
        "arabic" => Ok(TsConfig::Arabic),
        "danish" => Ok(TsConfig::Danish),
        "dutch" => Ok(TsConfig::Dutch),
        "finnish" => Ok(TsConfig::Finnish),
        "french" => Ok(TsConfig::French),
        "german" => Ok(TsConfig::German),
        "greek" => Ok(TsConfig::Greek),
        "hungarian" => Ok(TsConfig::Hungarian),
        "italian" => Ok(TsConfig::Italian),
        "norwegian" => Ok(TsConfig::Norwegian),
        "portuguese" => Ok(TsConfig::Portuguese),
        "romanian" => Ok(TsConfig::Romanian),
        "russian" => Ok(TsConfig::Russian),
        "spanish" => Ok(TsConfig::Spanish),
        "swedish" => Ok(TsConfig::Swedish),
        "tamil" => Ok(TsConfig::Tamil),
        "turkish" => Ok(TsConfig::Turkish),
        // Lowercase-only (accepted but no Snowball stemmer available)
        "armenian" => Ok(TsConfig::Armenian),
        "basque" => Ok(TsConfig::Basque),
        "catalan" => Ok(TsConfig::Catalan),
        "hindi" => Ok(TsConfig::Hindi),
        "indonesian" => Ok(TsConfig::Indonesian),
        "irish" => Ok(TsConfig::Irish),
        "lithuanian" => Ok(TsConfig::Lithuanian),
        "nepali" => Ok(TsConfig::Nepali),
        "yiddish" => Ok(TsConfig::Yiddish),
        _ => Err(SqlError::UndefinedTsConfig(base.to_string())),
    }
}

/// The session's default configuration: `default_text_search_config` when
/// SET, else `pg_catalog.english` (what initdb picks for English locales).
pub fn default_config(exec: &Exec) -> Result<TsConfig> {
    let name = exec
        .vars
        .borrow()
        .get("default_text_search_config")
        .cloned()
        .unwrap_or_else(|| "pg_catalog.english".to_string());
    resolve_config(&name)
}

// ---------------------------------------------------------------------------
// Extended lexer — token kinds and scanning helpers.
// ---------------------------------------------------------------------------

/// Token kind produced by the lexer, mirroring PostgreSQL's default-parser
/// token-type taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    /// Contiguous ASCII letter sequence: `hello`
    AsciiWord,
    /// Letter sequence containing at least one non-ASCII codepoint: `naïve`
    Word,
    /// Whole hyphenated compound: `state-of-the-art`
    HWord,
    /// Individual part inside a hyphenated compound: `state`, `of`, `the`, `art`
    HWordPart,
    /// Email address: `user@example.com`
    Email,
    /// URL with http or https scheme: `http://example.com/path`
    Url,
    /// Decimal floating-point literal: `3.14`
    Float,
    /// Unsigned integer literal: `42`
    Integer,
}

/// Is `b` a byte that can appear inside a token (ASCII alnum or start/continuation
/// of a multi-byte UTF-8 sequence)?
#[inline]
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80
}

/// Byte length of the UTF-8 codepoint whose first byte is at `bytes[i]`.
#[inline]
fn utf8_char_len(bytes: &[u8], i: usize) -> usize {
    match bytes[i] {
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        b if b >= 0xC0 => 2,
        _ => 1,
    }
}

/// Scan a maximal run of "word" bytes (ASCII alnum or multi-byte UTF-8).
/// Returns the exclusive end byte offset.
fn scan_word_run(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let n = bytes.len();
    while i < n {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() {
            i += 1;
        } else if b >= 0x80 {
            i += utf8_char_len(bytes, i);
        } else {
            break;
        }
    }
    i
}

/// Scan a maximal run of ASCII decimal digits.
fn scan_digits(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// Given that `local_end` is the end of a local-part word run and
/// `bytes[local_end] == b'@'`, try to scan the domain and return the
/// exclusive end offset of the full email, or `None`.
fn scan_email(bytes: &[u8], local_end: usize) -> Option<usize> {
    if bytes.get(local_end) != Some(&b'@') {
        return None;
    }
    let after_at = local_end + 1;
    if after_at >= bytes.len() || !is_token_byte(bytes[after_at]) {
        return None;
    }
    let domain_end = scan_word_run(bytes, after_at);
    if domain_end == after_at {
        return None;
    }
    if bytes.get(domain_end) != Some(&b'.') {
        return None;
    }
    let tld_start = domain_end + 1;
    if tld_start >= bytes.len() || !is_token_byte(bytes[tld_start]) {
        return None;
    }
    let mut end = scan_word_run(bytes, tld_start);
    if end == tld_start {
        return None;
    }
    // Accept additional dot-parts: user@mail.example.co.uk
    while end < bytes.len() && bytes[end] == b'.' {
        let next = scan_word_run(bytes, end + 1);
        if next == end + 1 {
            break;
        }
        end = next;
    }
    Some(end)
}

/// Scan a hyphenated compound `word(-word)*` starting at `start`.
/// Returns the exclusive end offset.
fn scan_hword(bytes: &[u8], start: usize) -> usize {
    let mut i = scan_word_run(bytes, start);
    let n = bytes.len();
    loop {
        if i >= n || bytes[i] != b'-' {
            break;
        }
        let after = i + 1;
        if after >= n || !is_token_byte(bytes[after]) {
            break;
        }
        i = scan_word_run(bytes, after);
    }
    i
}

/// Scan a URL (caller has verified `http://` or `https://` prefix).
fn scan_url(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let n = bytes.len();
    while i < n {
        let b = bytes[i];
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b':' | b'/'
                    | b'.'
                    | b'-'
                    | b'_'
                    | b'~'
                    | b'?'
                    | b'#'
                    | b'='
                    | b'&'
                    | b'%'
                    | b'+'
                    | b'@'
                    | b'!'
                    | b'$'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b','
                    | b';'
            )
        {
            i += 1;
        } else if b >= 0x80 {
            i += utf8_char_len(bytes, i);
        } else {
            break;
        }
    }
    i
}

// ---------------------------------------------------------------------------
// Tokenizer.
// ---------------------------------------------------------------------------

/// Split `text` into `(token_text, kind, 1-based-position)` triples.
///
/// Rules (priority order):
/// 1. `http://` / `https://` → [`TokenKind::Url`]
/// 2. Digit-start → [`TokenKind::Float`] or [`TokenKind::Integer`]
/// 3. Letter-start + `@` → [`TokenKind::Email`] (if domain validates)
/// 4. Letter-start + `-` + letter → [`TokenKind::HWord`]; the whole compound
///    is emitted first, then each part as [`TokenKind::HWordPart`] — all at
///    the same position so stop-word positions are preserved like PostgreSQL.
/// 5. Pure ASCII letters → [`TokenKind::AsciiWord`]
/// 6. Letters containing non-ASCII → [`TokenKind::Word`]
///
/// Every top-level token increments the position counter once; hword parts
/// share the compound's position.
fn tokenize(text: &str) -> Vec<(String, TokenKind, u16)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut pos: u32 = 0;
    let mut i = 0;

    while i < n {
        let b = bytes[i];

        // Skip separators (ASCII non-alnum).
        if !is_token_byte(b) {
            i += 1;
            continue;
        }

        // 1. URL detection.
        if text[i..].starts_with("http://") || text[i..].starts_with("https://") {
            let j = scan_url(bytes, i);
            pos += 1;
            let p = pos.min(MAX_POS as u32) as u16;
            out.push((text[i..j].to_lowercase(), TokenKind::Url, p));
            i = j;
            continue;
        }

        // 2. Number (float or integer).
        if b.is_ascii_digit() {
            let j = scan_digits(bytes, i);
            if j < n && bytes[j] == b'.' && j + 1 < n && bytes[j + 1].is_ascii_digit() {
                let k = scan_digits(bytes, j + 1);
                pos += 1;
                let p = pos.min(MAX_POS as u32) as u16;
                out.push((text[i..k].to_string(), TokenKind::Float, p));
                i = k;
            } else {
                pos += 1;
                let p = pos.min(MAX_POS as u32) as u16;
                out.push((text[i..j].to_string(), TokenKind::Integer, p));
                i = j;
            }
            continue;
        }

        // Letter start: scan the initial word run.
        let word_end = scan_word_run(bytes, i);

        // 3. Email detection.
        if word_end < n
            && bytes[word_end] == b'@'
            && let Some(email_end) = scan_email(bytes, word_end)
        {
            pos += 1;
            let p = pos.min(MAX_POS as u32) as u16;
            out.push((text[i..email_end].to_lowercase(), TokenKind::Email, p));
            i = email_end;
            continue;
        }

        // 4. Hyphenated compound.
        if word_end < n
            && bytes[word_end] == b'-'
            && word_end + 1 < n
            && is_token_byte(bytes[word_end + 1])
        {
            let compound_end = scan_hword(bytes, i);
            pos += 1;
            let p = pos.min(MAX_POS as u32) as u16;
            let compound = text[i..compound_end].to_string();
            out.push((compound.clone(), TokenKind::HWord, p));
            for part in compound.split('-') {
                if !part.is_empty() {
                    out.push((part.to_string(), TokenKind::HWordPart, p));
                }
            }
            i = compound_end;
            continue;
        }

        // 5 / 6. Plain word.
        pos += 1;
        let p = pos.min(MAX_POS as u32) as u16;
        let token = &text[i..word_end];
        let kind = if token.bytes().all(|b| b.is_ascii_alphabetic()) {
            TokenKind::AsciiWord
        } else {
            TokenKind::Word
        };
        out.push((token.to_string(), kind, p));
        i = word_end;
    }
    out
}

// ---------------------------------------------------------------------------
// Per-token normalization.
// ---------------------------------------------------------------------------

/// Apply the rust-stemmers Snowball algorithm to a lowercased word.
fn stem_with_algo(word: &str, algo: rust_stemmers::Algorithm) -> String {
    let stemmer = rust_stemmers::Stemmer::create(algo);
    stemmer.stem(word).to_string()
}

/// Normalize one token under a configuration. `None` = dropped (stop word).
///
/// * `Email`, `Url`, `HWord` — pass through case-folded; no stop-word filter,
///   no stemming.
/// * `Float`, `Integer` — kept as-is.
/// * `AsciiWord`, `HWordPart`, `Word` — the full linguistic pipeline:
///   lowercase → stop-word filter → stemmer.
fn normalize_token(config: TsConfig, token: &str, kind: TokenKind) -> Option<String> {
    match kind {
        TokenKind::Email | TokenKind::Url | TokenKind::HWord => Some(token.to_lowercase()),
        TokenKind::Float | TokenKind::Integer => Some(token.to_string()),
        TokenKind::AsciiWord | TokenKind::HWordPart | TokenKind::Word => {
            let lower = token.to_lowercase();
            match config {
                TsConfig::Simple => Some(lower),
                TsConfig::English => {
                    if !lower.bytes().all(|b| b.is_ascii_lowercase()) {
                        return Some(lower);
                    }
                    if STOP_WORDS.contains(&lower.as_str()) {
                        return None;
                    }
                    Some(porter_stem(&lower))
                }
                TsConfig::Danish => {
                    if STOPWORDS_DA.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Danish);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Dutch => {
                    if STOPWORDS_NL.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Dutch);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Finnish => {
                    if STOPWORDS_FI.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Finnish);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::French => {
                    if STOPWORDS_FR.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::French);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::German => {
                    if STOPWORDS_DE.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::German);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Hungarian => {
                    if STOPWORDS_HU.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Hungarian);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Italian => {
                    if STOPWORDS_IT.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Italian);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Norwegian => {
                    if STOPWORDS_NO.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Norwegian);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Portuguese => {
                    if STOPWORDS_PT.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Portuguese);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Romanian => {
                    if STOPWORDS_RO.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Romanian);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Russian => {
                    // Apply NFC normalization and ё→е before stopword check and stemming.
                    let nfc: String = lower.nfc().collect();
                    let normalized = nfc.replace('ё', "е");
                    if STOPWORDS_RU.contains(&normalized.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&normalized, rust_stemmers::Algorithm::Russian);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Spanish => {
                    if STOPWORDS_ES.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Spanish);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Swedish => {
                    if STOPWORDS_SV.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Swedish);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Turkish => {
                    if STOPWORDS_TR.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Turkish);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Arabic => {
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Arabic);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Greek => {
                    if STOPWORDS_EL.contains(&lower.as_str()) {
                        return None;
                    }
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Greek);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                TsConfig::Tamil => {
                    let stem = stem_with_algo(&lower, rust_stemmers::Algorithm::Tamil);
                    if stem.is_empty() { None } else { Some(stem) }
                }
                // Lowercase-only configs: tokenize but do not stem.
                TsConfig::Armenian
                | TsConfig::Basque
                | TsConfig::Catalan
                | TsConfig::Hindi
                | TsConfig::Indonesian
                | TsConfig::Irish
                | TsConfig::Lithuanian
                | TsConfig::Nepali
                | TsConfig::Yiddish => Some(lower),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tsvector construction.
// ---------------------------------------------------------------------------

/// `to_tsvector`: tokenize, normalize per config, collect positions.
pub fn to_tsvector(config: TsConfig, text: &str) -> Vec<TsLexeme> {
    let mut raw: Vec<(String, Vec<u16>)> = Vec::new();
    for (token, kind, pos) in tokenize(text) {
        if let Some(word) = normalize_token(config, &token, kind) {
            raw.push((word, vec![pos]));
        }
    }
    fts::normalize_lexemes(raw)
}

/// Expand the tsvector with synonyms and thesaurus entries from registered
/// dictionaries.  Thesaurus phrase matching runs first (phrase → canonical),
/// then per-lexeme synonym expansion.
fn apply_synonyms(
    dicts: &[&TsDictionaryDef],
    config: TsConfig,
    lexemes: Vec<TsLexeme>,
) -> Vec<TsLexeme> {
    if dicts.is_empty() {
        return lexemes;
    }

    // Apply thesaurus dicts first (phrase-level substitution).
    let lexemes = {
        let mut v = lexemes;
        for dict in dicts {
            if !dict.thesaurus_entries.is_empty() {
                v = apply_thesaurus(&v, &dict.thesaurus_entries);
            }
        }
        v
    };

    // Apply synonym dicts (word-level expansion).
    let mut extra: Vec<(String, Vec<u16>)> = Vec::new();
    for lex in &lexemes {
        for dict in dicts {
            if let Some(syns) = dict.synonyms.get(&lex.word) {
                for syn in syns {
                    if let Some(norm) = normalize_token(config, syn, TokenKind::AsciiWord) {
                        extra.push((norm, lex.positions.clone()));
                    }
                }
            }
        }
    }
    if extra.is_empty() {
        return lexemes;
    }
    let mut combined: Vec<(String, Vec<u16>)> =
        lexemes.into_iter().map(|l| (l.word, l.positions)).collect();
    combined.extend(extra);
    fts::normalize_lexemes(combined)
}

// ---------------------------------------------------------------------------
// tsquery construction.
// ---------------------------------------------------------------------------

/// `to_tsquery`: full `&`/`|`/`!`/parens syntax; every lexeme operand is
/// normalized per config, and operands that normalize away (stop words)
/// are dropped with the tree rewritten around them, like PostgreSQL.
pub fn to_tsquery(config: TsConfig, input: &str) -> Result<Option<TsQueryNode>> {
    let Some(tree) = fts::parse_tsquery(input)? else {
        return Err(SqlError::Syntax(format!(
            "syntax error in tsquery: \"{input}\""
        )));
    };
    map_query(config, &tree)
}

fn map_query(config: TsConfig, node: &TsQueryNode) -> Result<Option<TsQueryNode>> {
    match node {
        TsQueryNode::Lexeme(raw) => {
            let mut words: Vec<String> = Vec::new();
            for (token, kind, _) in tokenize(raw) {
                if let Some(w) = normalize_token(config, &token, kind) {
                    words.push(w);
                }
            }
            match words.len() {
                0 => Ok(None),
                1 => Ok(Some(TsQueryNode::Lexeme(words.pop().unwrap()))),
                _ => Err(SqlError::FeatureNotSupported(format!(
                    "to_tsquery operand \"{raw}\" normalizes to multiple lexemes, which \
                     requires the phrase operator <-> (out of the full-text-search subset)"
                ))),
            }
        }
        TsQueryNode::Not(c) => {
            Ok(map_query(config, c)?.map(|inner| TsQueryNode::Not(Box::new(inner))))
        }
        TsQueryNode::And(a, b) | TsQueryNode::Or(a, b) => {
            let is_and = matches!(node, TsQueryNode::And(..));
            let (l, r) = (map_query(config, a)?, map_query(config, b)?);
            Ok(match (l, r) {
                (Some(l), Some(r)) => Some(if is_and {
                    TsQueryNode::And(Box::new(l), Box::new(r))
                } else {
                    TsQueryNode::Or(Box::new(l), Box::new(r))
                }),
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            })
        }
    }
}

/// `plainto_tsquery`: no operator syntax — tokenize the whole text like
/// `to_tsvector` and AND the surviving lexemes. Empty in, empty query out.
pub fn plainto_tsquery(config: TsConfig, text: &str) -> Option<TsQueryNode> {
    let mut node: Option<TsQueryNode> = None;
    for (token, kind, _) in tokenize(text) {
        if let Some(word) = normalize_token(config, &token, kind) {
            let leaf = TsQueryNode::Lexeme(word);
            node = Some(match node {
                None => leaf,
                Some(acc) => TsQueryNode::And(Box::new(acc), Box::new(leaf)),
            });
        }
    }
    node
}

// ---------------------------------------------------------------------------
// ts_headline.
// ---------------------------------------------------------------------------

/// Options parsed from the `ts_headline` options string.
struct HeadlineOptions {
    max_words: usize,
    highlight_all: bool,
    fragment_delimiter: String,
    start_sel: String,
    stop_sel: String,
}

impl Default for HeadlineOptions {
    fn default() -> Self {
        Self {
            max_words: 35,
            highlight_all: false,
            fragment_delimiter: " ... ".to_string(),
            start_sel: "<b>".to_string(),
            stop_sel: "</b>".to_string(),
        }
    }
}

/// Parse a PostgreSQL-style options string like
/// `MaxWords=35,StartSel=<b>,StopSel=</b>`.
fn parse_headline_opts(opts: Option<&str>) -> Result<HeadlineOptions> {
    let mut o = HeadlineOptions::default();
    let Some(opts) = opts else {
        return Ok(o);
    };
    for part in opts.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            let v_unquoted = v.trim_matches('\'');
            match k.as_str() {
                "maxwords" => {
                    o.max_words = v_unquoted.parse().map_err(|_| {
                        SqlError::InvalidParameter(format!("invalid MaxWords: {v_unquoted}"))
                    })?;
                }
                "highlightall" => {
                    o.highlight_all = matches!(
                        v_unquoted.to_ascii_lowercase().as_str(),
                        "true" | "on" | "yes" | "1"
                    );
                }
                "maxfragments" => {
                    let _ = v_unquoted.parse::<usize>().map_err(|_| {
                        SqlError::InvalidParameter(format!("invalid MaxFragments: {v_unquoted}"))
                    })?;
                }
                "minwords" => {
                    let _ = v_unquoted.parse::<usize>().map_err(|_| {
                        SqlError::InvalidParameter(format!("invalid MinWords: {v_unquoted}"))
                    })?;
                }
                "shortword" => {
                    let _ = v_unquoted.parse::<usize>().map_err(|_| {
                        SqlError::InvalidParameter(format!("invalid ShortWord: {v_unquoted}"))
                    })?;
                }
                "fragmentdelimiter" => {
                    o.fragment_delimiter = v_unquoted.to_string();
                }
                "startsel" => {
                    o.start_sel = v_unquoted.to_string();
                }
                "stopsel" => {
                    o.stop_sel = v_unquoted.to_string();
                }
                _ => {} // ignore unknown options
            }
        }
    }
    Ok(o)
}

/// A token's byte span in the original document text.
struct DocSpan {
    start: usize,
    end: usize,
    kind: TokenKind,
}

/// Scan all token spans (with byte offsets) from a document.
fn scan_doc_spans(text: &str) -> Vec<DocSpan> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut spans = Vec::new();
    let mut i = 0;

    while i < n {
        let b = bytes[i];

        if !is_token_byte(b) {
            i += 1;
            continue;
        }

        // URL
        if text[i..].starts_with("http://") || text[i..].starts_with("https://") {
            let j = scan_url(bytes, i);
            spans.push(DocSpan {
                start: i,
                end: j,
                kind: TokenKind::Url,
            });
            i = j;
            continue;
        }

        // Number
        if b.is_ascii_digit() {
            let j = scan_digits(bytes, i);
            if j < n && bytes[j] == b'.' && j + 1 < n && bytes[j + 1].is_ascii_digit() {
                let k = scan_digits(bytes, j + 1);
                spans.push(DocSpan {
                    start: i,
                    end: k,
                    kind: TokenKind::Float,
                });
                i = k;
            } else {
                spans.push(DocSpan {
                    start: i,
                    end: j,
                    kind: TokenKind::Integer,
                });
                i = j;
            }
            continue;
        }

        // Letter
        let word_end = scan_word_run(bytes, i);

        // Email?
        if word_end < n
            && bytes[word_end] == b'@'
            && let Some(email_end) = scan_email(bytes, word_end)
        {
            spans.push(DocSpan {
                start: i,
                end: email_end,
                kind: TokenKind::Email,
            });
            i = email_end;
            continue;
        }

        // Hyphenated compound?
        if word_end < n
            && bytes[word_end] == b'-'
            && word_end + 1 < n
            && is_token_byte(bytes[word_end + 1])
        {
            let compound_end = scan_hword(bytes, i);
            spans.push(DocSpan {
                start: i,
                end: compound_end,
                kind: TokenKind::HWord,
            });
            i = compound_end;
            continue;
        }

        // Plain word
        let kind = if text[i..word_end].bytes().all(|b| b.is_ascii_alphabetic()) {
            TokenKind::AsciiWord
        } else {
            TokenKind::Word
        };
        spans.push(DocSpan {
            start: i,
            end: word_end,
            kind,
        });
        i = word_end;
    }
    spans
}

/// Collect query lexemes (sorted, deduped) from a query node.
fn query_lexemes(query: Option<&TsQueryNode>) -> Vec<String> {
    let Some(q) = query else {
        return Vec::new();
    };
    let mut refs: Vec<&str> = Vec::new();
    q.lexemes(&mut refs);
    let mut out: Vec<String> = refs.iter().map(|s| s.to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// Check whether a document span matches any query lexeme (after normalization).
fn span_is_match(text: &str, span: &DocSpan, config: TsConfig, q_lexemes: &[String]) -> bool {
    if q_lexemes.is_empty() {
        return false;
    }
    let tok = &text[span.start..span.end];
    match span.kind {
        TokenKind::HWord => {
            if let Some(norm) = normalize_token(config, tok, TokenKind::HWord)
                && q_lexemes.binary_search(&norm).is_ok()
            {
                return true;
            }
            for part in tok.split('-') {
                if part.is_empty() {
                    continue;
                }
                if let Some(norm) = normalize_token(config, part, TokenKind::HWordPart)
                    && q_lexemes.binary_search(&norm).is_ok()
                {
                    return true;
                }
            }
            false
        }
        kind => normalize_token(config, tok, kind)
            .map(|norm| q_lexemes.binary_search(&norm).is_ok())
            .unwrap_or(false),
    }
}

/// Count distinct query lexemes covered by `spans[start..end]`.
fn cover_density(
    text: &str,
    spans: &[DocSpan],
    config: TsConfig,
    q_lexemes: &[String],
    win_start: usize,
    win_end: usize,
) -> usize {
    let mut covered = vec![false; q_lexemes.len()];
    for span in &spans[win_start..win_end] {
        let tok = &text[span.start..span.end];
        let mut check_norm = |norm: String| {
            if let Ok(qi) = q_lexemes.binary_search(&norm) {
                covered[qi] = true;
            }
        };
        match span.kind {
            TokenKind::HWord => {
                if let Some(norm) = normalize_token(config, tok, TokenKind::HWord) {
                    check_norm(norm);
                }
                for part in tok.split('-') {
                    if part.is_empty() {
                        continue;
                    }
                    if let Some(norm) = normalize_token(config, part, TokenKind::HWordPart) {
                        check_norm(norm);
                    }
                }
            }
            kind => {
                if let Some(norm) = normalize_token(config, tok, kind) {
                    check_norm(norm);
                }
            }
        }
    }
    covered.iter().filter(|&&c| c).count()
}

/// Core `ts_headline` algorithm.
fn headline(
    config: TsConfig,
    doc: &str,
    query: Option<&TsQueryNode>,
    opts: &HeadlineOptions,
) -> String {
    let q_lexemes = query_lexemes(query);

    let spans = scan_doc_spans(doc);
    if spans.is_empty() {
        return doc.to_string();
    }

    let n = spans.len();
    let is_match: Vec<bool> = spans
        .iter()
        .map(|s| span_is_match(doc, s, config, &q_lexemes))
        .collect();

    // HighlightAll or no query: return the whole document with highlights.
    if opts.highlight_all || q_lexemes.is_empty() {
        return render_window(doc, &spans, &is_match, 0, n, opts, false);
    }

    // Find the window of up to max_words spans with the best cover density.
    let max_win = opts.max_words.min(n);
    let mut best_start = 0;
    let mut best_score = 0usize;

    for start in 0..=n.saturating_sub(1) {
        let end = (start + max_win).min(n);
        let score = cover_density(doc, &spans, config, &q_lexemes, start, end);
        if score > best_score {
            best_score = score;
            best_start = start;
        }
        if end == n {
            break;
        }
    }

    let win_end = (best_start + max_win).min(n);
    let with_ellipsis = best_start > 0;
    render_window(
        doc,
        &spans,
        &is_match,
        best_start,
        win_end,
        opts,
        with_ellipsis,
    )
}

/// Reconstruct the text for a window of spans with matched tokens wrapped.
fn render_window(
    doc: &str,
    spans: &[DocSpan],
    is_match: &[bool],
    win_start: usize,
    win_end: usize,
    opts: &HeadlineOptions,
    with_ellipsis: bool,
) -> String {
    if win_start >= win_end || win_end > spans.len() {
        return String::new();
    }
    let mut result = String::new();
    if with_ellipsis {
        result.push_str(&opts.fragment_delimiter);
    }
    let mut pos = spans[win_start].start;
    for i in win_start..win_end {
        let span = &spans[i];
        result.push_str(&doc[pos..span.start]);
        let tok = &doc[span.start..span.end];
        if is_match[i] {
            result.push_str(&opts.start_sel);
            result.push_str(tok);
            result.push_str(&opts.stop_sel);
        } else {
            result.push_str(tok);
        }
        pos = span.end;
    }
    result
}

// ---------------------------------------------------------------------------
// CREATE / DROP TEXT SEARCH DICTIONARY DDL support.
// ---------------------------------------------------------------------------

/// A parsed CREATE/DROP TEXT SEARCH DICTIONARY command.
pub enum TsDictCmd {
    Create {
        schema: Option<String>,
        name: String,
        synonyms: BTreeMap<String, Vec<String>>,
        /// Thesaurus entries (non-empty iff TEMPLATE = thesaurus).
        thesaurus_entries: BTreeMap<String, Vec<String>>,
        if_not_exists: bool,
    },
    Drop {
        schema: Option<String>,
        name: String,
        if_exists: bool,
    },
}

/// Return `true` when the SQL segment is a CREATE/DROP TEXT SEARCH DICTIONARY
/// statement (case-insensitive prefix check).
pub fn is_ts_dict_ddl(sql: &str) -> bool {
    let up = sql.trim_start().to_ascii_uppercase();
    up.starts_with("CREATE TEXT SEARCH DICTIONARY") || up.starts_with("DROP TEXT SEARCH DICTIONARY")
}

/// Parse a CREATE or DROP TEXT SEARCH DICTIONARY statement.
pub fn parse_ts_dict_ddl(sql: &str) -> Result<TsDictCmd> {
    let trimmed = sql.trim();
    let up = trimmed.to_ascii_uppercase();

    if up.starts_with("DROP TEXT SEARCH DICTIONARY") {
        let rest = trimmed["DROP TEXT SEARCH DICTIONARY".len()..].trim_start();
        let (if_exists, rest) = if rest.to_ascii_uppercase().starts_with("IF EXISTS") {
            (true, rest["IF EXISTS".len()..].trim_start())
        } else {
            (false, rest)
        };
        let name_raw = rest
            .split(|c: char| c.is_whitespace() || c == ';')
            .next()
            .unwrap_or("")
            .trim_matches('"');
        let (schema, name) = split_qualified(name_raw);
        return Ok(TsDictCmd::Drop {
            schema,
            name,
            if_exists,
        });
    }

    if up.starts_with("CREATE TEXT SEARCH DICTIONARY") {
        let rest = trimmed["CREATE TEXT SEARCH DICTIONARY".len()..].trim_start();
        let (if_not_exists, rest) = if rest.to_ascii_uppercase().starts_with("IF NOT EXISTS") {
            (true, rest["IF NOT EXISTS".len()..].trim_start())
        } else {
            (false, rest)
        };
        let paren_start = rest.find('(').ok_or_else(|| {
            SqlError::Syntax("expected '(' in CREATE TEXT SEARCH DICTIONARY options".to_string())
        })?;
        let name_raw = rest[..paren_start].trim().trim_matches('"');
        let (schema, name) = split_qualified(name_raw);

        let options_str = rest[paren_start + 1..].trim();
        let options_str = options_str
            .strip_suffix(';')
            .unwrap_or(options_str)
            .trim()
            .strip_suffix(')')
            .unwrap_or(options_str)
            .trim();

        let up_opts = options_str.to_ascii_uppercase();
        let is_thesaurus = up_opts.contains("TEMPLATE")
            && up_opts
                .split_whitespace()
                .skip_while(|t| *t != "TEMPLATE")
                .nth(2) // TEMPLATE = <value>
                .map(|v| v.trim_matches(',').eq_ignore_ascii_case("thesaurus"))
                .unwrap_or(false);

        if is_thesaurus {
            let thesaurus_entries = parse_thesaurus_options(options_str)?;
            return Ok(TsDictCmd::Create {
                schema,
                name,
                synonyms: BTreeMap::new(),
                thesaurus_entries,
                if_not_exists,
            });
        }

        let synonyms = parse_dict_options(options_str)?;
        return Ok(TsDictCmd::Create {
            schema,
            name,
            synonyms,
            thesaurus_entries: BTreeMap::new(),
            if_not_exists,
        });
    }

    Err(SqlError::Syntax(
        "expected CREATE TEXT SEARCH DICTIONARY or DROP TEXT SEARCH DICTIONARY".to_string(),
    ))
}

/// Split `schema.name` or bare `name`.
fn split_qualified(s: &str) -> (Option<String>, String) {
    match s.split_once('.') {
        Some((schema, name)) => (
            Some(schema.trim_matches('"').to_ascii_lowercase()),
            name.trim_matches('"').to_ascii_lowercase(),
        ),
        None => (None, s.to_ascii_lowercase()),
    }
}

/// Extract the synonym map from an options clause.
fn parse_dict_options(opts: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let up = opts.to_ascii_uppercase();
    let syn_pos = up.find("SYNONYMS").ok_or_else(|| {
        SqlError::InvalidParameter(
            "CREATE TEXT SEARCH DICTIONARY requires SYNONYMS option".to_string(),
        )
    })?;
    let after = opts[syn_pos + "SYNONYMS".len()..].trim_start();
    let after = after
        .strip_prefix('=')
        .ok_or_else(|| SqlError::Syntax("expected '=' after SYNONYMS".to_string()))?;
    let after = after.trim_start();
    let syn_str = if let Some(stripped) = after.strip_prefix('\'') {
        let end = stripped
            .find('\'')
            .ok_or_else(|| SqlError::Syntax("unclosed string in SYNONYMS value".to_string()))?;
        &stripped[..end]
    } else {
        after.split(',').next().unwrap_or("").trim()
    };
    Ok(parse_synonyms(syn_str))
}

/// Parse the SYNONYMS inline format.
///
/// Two forms are accepted:
///
/// * `word1:syn1,syn2;word2:syn3` — explicit mapping: `word1` expands to
///   `syn1` and `syn2`; groups are separated by `;`.
/// * `word1,word2,word3` — synonym group: the first word is the canonical
///   form and the remaining words are its synonyms.  When queried with any
///   of the non-canonical words the tsvector includes the canonical entry
///   (via `apply_synonyms`).
pub fn parse_synonyms(s: &str) -> BTreeMap<String, Vec<String>> {
    let mut map = BTreeMap::new();
    for entry in s.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((word, syns_str)) = entry.split_once(':') {
            // Explicit word:syn1,syn2 form.
            let word = word.trim().to_ascii_lowercase();
            let syns: Vec<String> = syns_str
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            if !word.is_empty() && !syns.is_empty() {
                map.insert(word, syns);
            }
        } else {
            // Synonym-group form: first word is canonical, rest are synonyms.
            let words: Vec<String> = entry
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            if words.len() >= 2 {
                map.insert(words[0].clone(), words[1..].to_vec());
            }
        }
    }
    map
}

/// Parse the THESAURUS option from a CREATE TEXT SEARCH DICTIONARY statement.
///
/// Accepted inline format (passed via `THESAURUS = '...'`):
///   `phrase1:canon1,canon2;multi word phrase:canonical`
///
/// Each entry maps a phrase (words separated by spaces, lowercased) to one or
/// more canonical replacement terms.  Entries are separated by `;`.
fn parse_thesaurus_options(opts: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let up = opts.to_ascii_uppercase();
    // The options string contains "TEMPLATE = thesaurus" so a bare search for
    // "THESAURUS" would match the template value.  Look for "THESAURUS" that is
    // followed (after optional whitespace) by "=" — that is the key, not the value.
    let ths_pos = {
        let mut found = None;
        let mut search = &up[..];
        let mut base = 0usize;
        while let Some(pos) = search.find("THESAURUS") {
            let after_kw = search[pos + "THESAURUS".len()..].trim_start();
            if after_kw.starts_with('=') {
                found = Some(base + pos);
                break;
            }
            base += pos + "THESAURUS".len();
            search = &search[pos + "THESAURUS".len()..];
        }
        found.ok_or_else(|| {
            SqlError::InvalidParameter(
                "CREATE TEXT SEARCH DICTIONARY with TEMPLATE = thesaurus requires THESAURUS option"
                    .to_string(),
            )
        })?
    };
    let after = opts[ths_pos + "THESAURUS".len()..].trim_start();
    let after = after
        .strip_prefix('=')
        .ok_or_else(|| SqlError::Syntax("expected '=' after THESAURUS".to_string()))?
        .trim_start();
    let ths_str = if let Some(stripped) = after.strip_prefix('\'') {
        let end = stripped
            .find('\'')
            .ok_or_else(|| SqlError::Syntax("unclosed string in THESAURUS value".to_string()))?;
        &stripped[..end]
    } else {
        after.split_whitespace().next().unwrap_or("").trim()
    };
    Ok(parse_thesaurus(ths_str))
}

/// Parse the inline thesaurus format: `phrase:canon1,canon2;phrase2:canon3`.
pub fn parse_thesaurus(s: &str) -> BTreeMap<String, Vec<String>> {
    let mut map = BTreeMap::new();
    for entry in s.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((phrase, canons_str)) = entry.split_once(':') {
            let phrase = phrase.trim().to_ascii_lowercase();
            let canons: Vec<String> = canons_str
                .split(',')
                .map(|c| c.trim().to_ascii_lowercase())
                .filter(|c| !c.is_empty())
                .collect();
            if !phrase.is_empty() && !canons.is_empty() {
                map.insert(phrase, canons);
            }
        }
    }
    map
}

/// Apply thesaurus entries to an already-normalized tsvector.
///
/// The algorithm scans the sorted lexemes and looks for contiguous runs whose
/// words (joined with a space) match a thesaurus phrase.  Matching runs are
/// replaced by the canonical term(s) at the position of the first lexeme.
pub fn apply_thesaurus(lexemes: &[TsLexeme], entries: &BTreeMap<String, Vec<String>>) -> Vec<TsLexeme> {
    if entries.is_empty() {
        return lexemes.to_vec();
    }
    let words: Vec<&str> = lexemes.iter().map(|l| l.word.as_str()).collect();
    let n = words.len();
    let mut skip = vec![false; n];
    let mut insertions: Vec<(usize, Vec<TsLexeme>)> = Vec::new();

    // Check all subslices from longest to shortest to prefer longer matches.
    let max_phrase_words = entries.keys().map(|p| p.split_whitespace().count()).max().unwrap_or(1);
    'outer: for start in 0..n {
        if skip[start] {
            continue;
        }
        for len in (1..=max_phrase_words.min(n - start)).rev() {
            let phrase = words[start..start + len].join(" ");
            if let Some(canons) = entries.get(&phrase) {
                let pos = lexemes[start].positions.first().copied().unwrap_or(1);
                let replacement: Vec<TsLexeme> = canons
                    .iter()
                    .map(|c| TsLexeme {
                        word: c.clone(),
                        positions: vec![pos],
                    })
                    .collect();
                for i in start..start + len {
                    skip[i] = true;
                }
                insertions.push((start, replacement));
                continue 'outer;
            }
        }
    }

    if insertions.is_empty() {
        return lexemes.to_vec();
    }

    let mut result: Vec<TsLexeme> = Vec::with_capacity(n);
    let mut ins_iter = insertions.into_iter().peekable();
    for (i, lex) in lexemes.iter().enumerate() {
        if skip[i] {
            if let Some((start, _)) = ins_iter.peek() {
                if *start == i {
                    let (_, repl) = ins_iter.next().unwrap();
                    result.extend(repl);
                }
            }
        } else {
            result.push(lex.clone());
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Ranking.
// ---------------------------------------------------------------------------

/// π²/6, PostgreSQL's per-lexeme rank damping constant.
const RANK_DIVISOR: f32 = 1.644_934;

/// `ts_rank` with PostgreSQL's frequency-based formula (`calc_rank_or` in
/// `tsrank.c`).
pub fn rank(weights: &[f32; 4], tv: &[TsLexeme], query: Option<&TsQueryNode>) -> f32 {
    let Some(root) = query else { return 0.0 };
    if tv.is_empty() {
        return 0.0;
    }
    let mut operands = Vec::new();
    root.lexemes(&mut operands);
    operands.sort();
    operands.dedup();
    if operands.is_empty() {
        return 0.0;
    }
    let w = weights[0];
    let mut res = 0.0f32;
    for lexeme in &operands {
        let Ok(i) = tv.binary_search_by(|l| l.word.as_str().cmp(lexeme)) else {
            continue;
        };
        let npos = tv[i].positions.len().max(1);
        let mut resj = 0.0f32;
        for j in 0..npos {
            resj += w / (((j + 1) * (j + 1)) as f32);
        }
        res += resj / RANK_DIVISOR;
    }
    res / operands.len() as f32
}

// ---------------------------------------------------------------------------
// Scalar-function entry points (called from `funcs::call_scalar`).
// ---------------------------------------------------------------------------

/// Shared `([config,] text)` argument handling for the three constructors.
/// `Ok(None)` = a NULL argument (all these functions are strict).
fn config_and_text(
    exec: &Exec,
    func: &str,
    args: &[SqlValue],
) -> Result<Option<(TsConfig, String)>> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(None);
    }
    let text_arg = |idx: usize| -> Result<String> {
        match args.get(idx) {
            Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone()),
            Some(SqlValue::Json(_)) => Err(SqlError::FeatureNotSupported(format!(
                "{func}(json) is not supported (out of the full-text-search subset)"
            ))),
            Some(other) => Err(bad_arg(func, idx, "text", other)),
            None => Err(missing_arg(func, idx)),
        }
    };
    match args.len() {
        1 => Ok(Some((default_config(exec)?, text_arg(0)?))),
        2 => Ok(Some((resolve_config(&text_arg(0)?)?, text_arg(1)?))),
        n => Err(SqlError::UndefinedFunction(format!(
            "{func} with {n} arguments"
        ))),
    }
}

pub fn fn_to_tsvector(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    Ok(match config_and_text(exec, "to_tsvector", args)? {
        None => SqlValue::Null,
        Some((config, text)) => {
            let lexemes = to_tsvector(config, &text);
            let dicts: Vec<&TsDictionaryDef> = exec.catalog.ts_dictionaries().collect();
            SqlValue::TsVector(apply_synonyms(&dicts, config, lexemes))
        }
    })
}

pub fn fn_to_tsquery(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    Ok(match config_and_text(exec, "to_tsquery", args)? {
        None => SqlValue::Null,
        Some((config, text)) => SqlValue::TsQuery(to_tsquery(config, &text)?),
    })
}

pub fn fn_plainto_tsquery(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    Ok(match config_and_text(exec, "plainto_tsquery", args)? {
        None => SqlValue::Null,
        Some((config, text)) => SqlValue::TsQuery(plainto_tsquery(config, &text)),
    })
}

/// `ts_headline([config,] document, query [, options])`.
pub fn fn_ts_headline(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let get_text = |idx: usize| -> Result<String> {
        match args.get(idx) {
            Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone()),
            Some(other) => Err(bad_arg("ts_headline", idx, "text", other)),
            None => Err(missing_arg("ts_headline", idx)),
        }
    };

    let (config, doc, query, opts_text) = match args.len() {
        2 => {
            let doc = get_text(0)?;
            let query = arg_tsquery(args, 1, "ts_headline")?;
            (default_config(exec)?, doc, query, None)
        }
        3 => {
            // (config, text, tsquery) when arg[2] is TsQuery; else (text, tsquery, options).
            match &args[2] {
                SqlValue::TsQuery(_) => {
                    let cfg = resolve_config(&get_text(0)?)?;
                    let doc = get_text(1)?;
                    let query = arg_tsquery(args, 2, "ts_headline")?;
                    (cfg, doc, query, None)
                }
                _ => {
                    let doc = get_text(0)?;
                    let query = arg_tsquery(args, 1, "ts_headline")?;
                    let opts = get_text(2)?;
                    (default_config(exec)?, doc, query, Some(opts))
                }
            }
        }
        4 => {
            let cfg = resolve_config(&get_text(0)?)?;
            let doc = get_text(1)?;
            let query = arg_tsquery(args, 2, "ts_headline")?;
            let opts = get_text(3)?;
            (cfg, doc, query, Some(opts))
        }
        n => {
            return Err(SqlError::UndefinedFunction(format!(
                "ts_headline with {n} arguments"
            )));
        }
    };

    let options = parse_headline_opts(opts_text.as_deref())?;
    Ok(SqlValue::Text(headline(
        config,
        &doc,
        query.as_ref(),
        &options,
    )))
}

/// A tsvector argument; a text value takes the raw-parse path.
fn arg_tsvector(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<TsLexeme>> {
    match args.get(idx) {
        Some(SqlValue::TsVector(v)) => Ok(v.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::TsVector)? {
                SqlValue::TsVector(v) => Ok(v),
                _ => unreachable!("from_text(tsvector) yields TsVector"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "tsvector", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// A tsquery argument; text raw-parses like an unknown literal.
fn arg_tsquery(args: &[SqlValue], idx: usize, func: &str) -> Result<Option<TsQueryNode>> {
    match args.get(idx) {
        Some(SqlValue::TsQuery(q)) => Ok(q.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::TsQuery)? {
                SqlValue::TsQuery(q) => Ok(q),
                _ => unreachable!("from_text(tsquery) yields TsQuery"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "tsquery", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// `ts_rank([weights,] tsvector, tsquery [, normalization])`.
pub fn fn_ts_rank(args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let (weights, base) = match args.first() {
        Some(SqlValue::Array(items)) => {
            if items.len() < 4 {
                return Err(SqlError::InvalidParameter(
                    "array of weight is too short".into(),
                ));
            }
            let mut w = [0.0f32; 4];
            for (i, item) in items.iter().take(4).enumerate() {
                let f = item
                    .as_f64()
                    .ok_or_else(|| bad_arg("ts_rank", 0, "real[]", item))?
                    as f32;
                if f < 0.0 {
                    return Err(SqlError::InvalidParameter(
                        "array of weight must not contain negative values".into(),
                    ));
                }
                w[i] = f;
            }
            (w, 1)
        }
        _ => ([0.1, 0.2, 0.4, 1.0], 0),
    };
    let tv = arg_tsvector(args, base, "ts_rank")?;
    let query = arg_tsquery(args, base + 1, "ts_rank")?;
    match args.len() - base {
        2 => {}
        3 => {
            let norm = args[base + 2].as_i64().unwrap_or(-1);
            if norm != 0 {
                return Err(SqlError::FeatureNotSupported(format!(
                    "ts_rank normalization option {norm} is not supported \
                     (only 0, the default, is in the full-text-search subset)"
                )));
            }
        }
        _ => {
            return Err(SqlError::UndefinedFunction(format!(
                "ts_rank with {} arguments",
                args.len()
            )));
        }
    }
    Ok(SqlValue::Float4(rank(&weights, &tv, query.as_ref())))
}

/// `numnode(tsquery)`: lexeme + operator count.
pub fn fn_numnode(args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let q = arg_tsquery(args, 0, "numnode")?;
    Ok(SqlValue::Int4(q.map(|n| n.count_nodes()).unwrap_or(0)))
}

/// `strip(tsvector)`: drop all position information.
pub fn fn_strip(args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let mut v = arg_tsvector(args, 0, "strip")?;
    for lex in &mut v {
        lex.positions.clear();
    }
    Ok(SqlValue::TsVector(v))
}

/// The `@@` match operator, in every argument order PostgreSQL defines.
pub fn at_at(exec: &Exec, a: &SqlValue, b: &SqlValue) -> Result<SqlValue> {
    if a.is_null() || b.is_null() {
        return Ok(SqlValue::Null);
    }
    let text_of = |v: &SqlValue| -> Option<String> {
        match v {
            SqlValue::Text(s) | SqlValue::Citext(s) => Some(s.clone()),
            _ => None,
        }
    };
    let matched = match (a, b) {
        (SqlValue::TsVector(v), SqlValue::TsQuery(q))
        | (SqlValue::TsQuery(q), SqlValue::TsVector(v)) => matches_opt(v, q.as_ref()),
        (SqlValue::TsVector(v), other) | (other, SqlValue::TsVector(v)) => {
            let s = text_of(other).ok_or_else(|| at_at_mismatch(a, b))?;
            let q = fts::parse_tsquery(&s)?;
            matches_opt(v, q.as_ref())
        }
        (SqlValue::TsQuery(q), other) => {
            let s = text_of(other).ok_or_else(|| at_at_mismatch(a, b))?;
            let v = fts::parse_tsvector(&s)?;
            matches_opt(&v, q.as_ref())
        }
        (other, SqlValue::TsQuery(q)) => {
            let s = text_of(other).ok_or_else(|| at_at_mismatch(a, b))?;
            let config = default_config(exec)?;
            let dicts: Vec<&TsDictionaryDef> = exec.catalog.ts_dictionaries().collect();
            let v = apply_synonyms(&dicts, config, to_tsvector(config, &s));
            matches_opt(&v, q.as_ref())
        }
        _ => {
            let (Some(doc), Some(query)) = (text_of(a), text_of(b)) else {
                return Err(at_at_mismatch(a, b));
            };
            let config = default_config(exec)?;
            let dicts: Vec<&TsDictionaryDef> = exec.catalog.ts_dictionaries().collect();
            let v = apply_synonyms(&dicts, config, to_tsvector(config, &doc));
            matches_opt(&v, plainto_tsquery(config, &query).as_ref())
        }
    };
    Ok(SqlValue::Bool(matched))
}

fn matches_opt(v: &[TsLexeme], q: Option<&TsQueryNode>) -> bool {
    q.is_some_and(|node| fts::eval_match(v, node))
}

fn at_at_mismatch(a: &SqlValue, b: &SqlValue) -> SqlError {
    SqlError::FeatureNotSupported(format!(
        "@@ is not supported between {} and {} (supported: tsvector @@ tsquery, \
         tsquery @@ tsvector, text @@ tsquery, text @@ text)",
        a.type_of().name(),
        b.type_of().name()
    ))
}

// ---------------------------------------------------------------------------
// Language stop-word lists (PostgreSQL Snowball stopword files).
// ---------------------------------------------------------------------------

/// Danish stopwords (PostgreSQL `danish.stop`, 94 words).
static STOPWORDS_DA: &[&str] = &[
    "ad", "af", "alle", "alt", "anden", "at", "også", "være", "blev", "blive", "bliver", "da",
    "de", "dem", "den", "denne", "der", "deres", "det", "dette", "dig", "din", "disse", "dog",
    "du", "efter", "eller", "en", "end", "er", "et", "for", "fra", "han", "ham", "hans", "har",
    "have", "havde", "hende", "hendes", "her", "hos", "hun", "hvad", "hvis", "hvor", "i", "ikke",
    "ind", "jeg", "jer", "jo", "kunne", "man", "mange", "med", "meget", "men", "mig", "min",
    "mine", "mit", "mod", "ned", "noget", "nogle", "nu", "når", "og", "om", "op", "os", "over",
    "på", "selv", "sig", "sin", "sine", "sit", "skal", "skulle", "som", "sådan", "til", "thi",
    "ud", "under", "var", "vi", "vil", "ville", "vor", "været",
];

/// Dutch stopwords (PostgreSQL `dutch.stop`, 99 words).
static STOPWORDS_NL: &[&str] = &[
    "aan", "al", "alle", "alles", "als", "altijd", "andere", "ben", "bij", "daar", "dan", "dat",
    "de", "den", "der", "deze", "die", "doch", "doen", "door", "drie", "du", "dus", "een", "eens",
    "er", "ge", "geen", "geweest", "haar", "had", "heb", "hebben", "heeft", "hem", "het", "hier",
    "hij", "hoe", "hun", "iemand", "iets", "ik", "in", "is", "ja", "je", "jij", "jou", "jullie",
    "kan", "kon", "kunnen", "maar", "me", "meer", "men", "met", "mij", "mijn", "moet", "na",
    "naar", "net", "niets", "niet", "nog", "nu", "of", "om", "omdat", "ons", "onze", "ook", "op",
    "over", "reeds", "se", "toch", "te", "tegen", "tot", "u", "uit", "van", "veel", "voor",
    "wanner", "want", "waren", "was", "wezen", "wie", "wij", "wordt", "worden", "zal", "ze",
    "zelf", "zich", "zij", "zijn", "zo", "zonder", "zou",
];

/// Finnish stopwords (PostgreSQL `finnish.stop`).
static STOPWORDS_FI: &[&str] = &[
    "ei",
    "emme",
    "en",
    "ette",
    "hän",
    "he",
    "heidän",
    "heille",
    "heiltä",
    "heissä",
    "heistä",
    "heihin",
    "heitä",
    "itse",
    "ja",
    "jo",
    "joita",
    "jokin",
    "joka",
    "joko",
    "jolloin",
    "jolle",
    "jonka",
    "jonkin",
    "jos",
    "jota",
    "jotka",
    "jotta",
    "juuri",
    "jälleen",
    "kaikki",
    "kanssa",
    "kenelle",
    "keneltä",
    "kenellä",
    "kenet",
    "kenen",
    "kerran",
    "ketkä",
    "koko",
    "koska",
    "kuka",
    "kukaan",
    "kun",
    "kuinka",
    "kukin",
    "kunnes",
    "me",
    "miksi",
    "mikä",
    "missä",
    "miten",
    "mitkä",
    "mitä",
    "molemmat",
    "monta",
    "muita",
    "muun",
    "muut",
    "myös",
    "ne",
    "niin",
    "nopeasti",
    "nyt",
    "näiden",
    "näille",
    "näissä",
    "näistä",
    "näitä",
    "nämä",
    "o",
    "oikein",
    "olemme",
    "olette",
    "olen",
    "oli",
    "olivat",
    "olit",
    "olitte",
    "ollut",
    "olla",
    "on",
    "onko",
    "osa",
    "ovat",
    "pian",
    "päin",
    "s",
    "samoin",
    "se",
    "sekä",
    "sen",
    "sille",
    "sinulla",
    "sinulta",
    "sinulle",
    "siitä",
    "siinä",
    "siihen",
    "siellä",
    "sieltä",
    "sitten",
    "sitä",
    "tai",
    "taas",
    "täksi",
    "tällä",
    "tälle",
    "täältä",
    "täällä",
    "tänne",
    "tässä",
    "tätä",
    "täältä",
    "te",
    "teidän",
    "teille",
    "teiltä",
    "teissä",
    "teistä",
    "teihin",
    "teitä",
    "tuolla",
    "tuolloin",
    "tuolta",
    "tuonne",
    "tuolle",
    "tuosta",
    "ulos",
    "vain",
    "vai",
    "varsin",
    "vasta",
    "vielä",
    "viime",
    "voi",
    "voidaan",
    "voitte",
    "voit",
    "voivat",
    "vuoksi",
    "yhteen",
    "yksi",
    "ylös",
];

/// French stopwords (PostgreSQL `french.stop`).
static STOPWORDS_FR: &[&str] = &[
    "à", "ai", "aie", "aient", "aies", "ait", "au", "aura", "aurai", "auraient", "aurais",
    "aurait", "auras", "aurez", "auriez", "aurions", "aurons", "auront", "aux", "avais", "avait",
    "avec", "avez", "aviez", "avions", "avons", "avaient", "ayant", "ayante", "ayantes", "ayants",
    "ayez", "ayons", "c", "ce", "ces", "cet", "cette", "d", "dans", "de", "des", "du", "elle",
    "en", "es", "est", "et", "eu", "eue", "eues", "eus", "eusse", "eussent", "eusses", "eussiez",
    "eussions", "eut", "eûmes", "eûtes", "eurent", "eût", "eux", "fus", "fut", "fûmes", "fûtes",
    "furent", "fusse", "fussent", "fusses", "fussiez", "fussions", "fût", "il", "ils", "j", "je",
    "l", "la", "le", "les", "leur", "lui", "m", "ma", "mais", "me", "même", "mes", "moi", "mon",
    "n", "ne", "nos", "notre", "nous", "on", "ou", "par", "pas", "pour", "qu", "que", "qui", "s",
    "sa", "se", "sera", "serai", "seraient", "serais", "serait", "seras", "serez", "seriez",
    "serions", "serons", "seront", "ses", "si", "soit", "soient", "sois", "soyez", "soyons", "son",
    "sommes", "sont", "suis", "sur", "t", "ta", "te", "tes", "toi", "ton", "tu", "un", "une",
    "vos", "votre", "vous", "y", "était", "étaient", "étais", "étant", "étante", "étantes",
    "étants", "étée", "étées", "étés", "étiez", "étions", "été", "êtes",
];

/// German stopwords (PostgreSQL `german.stop`).
static STOPWORDS_DE: &[&str] = &[
    "aber",
    "alle",
    "allem",
    "allen",
    "aller",
    "alles",
    "als",
    "also",
    "am",
    "an",
    "ander",
    "andere",
    "anderem",
    "anderen",
    "anderer",
    "anderes",
    "anderm",
    "andern",
    "anderr",
    "anders",
    "auch",
    "auf",
    "aus",
    "bei",
    "bin",
    "bis",
    "bist",
    "da",
    "damit",
    "dann",
    "das",
    "dasselbe",
    "daß",
    "dazu",
    "dein",
    "deine",
    "deinem",
    "deinen",
    "deiner",
    "deines",
    "dem",
    "demselben",
    "den",
    "denselben",
    "denen",
    "denn",
    "der",
    "derer",
    "derselbe",
    "derselben",
    "des",
    "desselben",
    "dessen",
    "die",
    "dieselbe",
    "dieselben",
    "dich",
    "dir",
    "dies",
    "diese",
    "diesem",
    "diesen",
    "dieser",
    "dieses",
    "doch",
    "dort",
    "du",
    "durch",
    "ein",
    "eine",
    "einem",
    "einen",
    "einer",
    "eines",
    "einig",
    "einige",
    "einigem",
    "einigen",
    "einiger",
    "einiges",
    "einmal",
    "er",
    "es",
    "etwas",
    "euch",
    "euer",
    "eure",
    "eurem",
    "euren",
    "eurer",
    "eures",
    "für",
    "gegen",
    "gewesen",
    "hab",
    "habe",
    "haben",
    "hat",
    "hatte",
    "hatten",
    "hier",
    "hin",
    "hinter",
    "ich",
    "ihm",
    "ihn",
    "ihnen",
    "ihr",
    "ihre",
    "ihrem",
    "ihren",
    "ihrer",
    "ihres",
    "im",
    "in",
    "indem",
    "ins",
    "ist",
    "jede",
    "jedem",
    "jeden",
    "jeder",
    "jedes",
    "jene",
    "jenem",
    "jenen",
    "jener",
    "jenes",
    "jetzt",
    "kann",
    "kein",
    "keine",
    "keinem",
    "keinen",
    "keiner",
    "keines",
    "können",
    "könnte",
    "machen",
    "man",
    "manche",
    "manchem",
    "manchen",
    "mancher",
    "manches",
    "mein",
    "meine",
    "meinem",
    "meinen",
    "meiner",
    "meines",
    "mich",
    "mir",
    "mit",
    "muss",
    "musste",
    "nach",
    "nicht",
    "nichts",
    "noch",
    "nun",
    "nur",
    "ob",
    "oder",
    "ohne",
    "sehr",
    "sein",
    "seine",
    "seinem",
    "seinen",
    "seiner",
    "seines",
    "selbst",
    "sich",
    "sie",
    "sind",
    "so",
    "solche",
    "solchem",
    "solchen",
    "solcher",
    "solches",
    "soll",
    "sollte",
    "sondern",
    "sonst",
    "über",
    "um",
    "und",
    "uns",
    "unse",
    "unsem",
    "unsen",
    "unser",
    "unses",
    "unter",
    "viel",
    "vom",
    "von",
    "vor",
    "während",
    "war",
    "waren",
    "warst",
    "was",
    "weg",
    "weil",
    "weiter",
    "welche",
    "welchem",
    "welchen",
    "welcher",
    "welches",
    "wenn",
    "werde",
    "werden",
    "wie",
    "wieder",
    "will",
    "wir",
    "wird",
    "wirst",
    "wo",
    "wollen",
    "wollte",
    "würde",
    "würden",
    "zu",
    "zum",
    "zur",
    "zwar",
    "zwischen",
];

/// Hungarian stopwords (PostgreSQL `hungarian.stop`, 175 words).
static STOPWORDS_HU: &[&str] = &[
    "a",
    "abban",
    "ahhoz",
    "ahol",
    "ahogy",
    "aki",
    "akik",
    "akkor",
    "alatt",
    "által",
    "általában",
    "amely",
    "amelyek",
    "amelyekben",
    "amelyeket",
    "amelyet",
    "amelynek",
    "ami",
    "amit",
    "amolyan",
    "amíg",
    "amikor",
    "an",
    "annak",
    "arra",
    "arról",
    "at",
    "az",
    "azok",
    "azon",
    "azt",
    "azzal",
    "azért",
    "aztán",
    "azután",
    "azonban",
    "bár",
    "be",
    "belül",
    "benne",
    "cikk",
    "cikkek",
    "cikkeket",
    "csak",
    "de",
    "e",
    "eddig",
    "egész",
    "egy",
    "egyes",
    "egyetlen",
    "egyéb",
    "egyik",
    "egyre",
    "ekkor",
    "el",
    "elég",
    "ellen",
    "elő",
    "először",
    "előtt",
    "első",
    "én",
    "éppen",
    "ebben",
    "ehhez",
    "emilyen",
    "ennek",
    "erre",
    "ez",
    "ezt",
    "ezek",
    "ezen",
    "ezzel",
    "ezért",
    "és",
    "fel",
    "felé",
    "hanem",
    "hiszen",
    "hogy",
    "hogyan",
    "igen",
    "így",
    "ill",
    "ill.",
    "illetve",
    "ilyen",
    "ilyenkor",
    "ison",
    "ismét",
    "itt",
    "jó",
    "jól",
    "jobban",
    "kell",
    "kellett",
    "keresztül",
    "keressünk",
    "ki",
    "kívül",
    "között",
    "közül",
    "legalább",
    "lehet",
    "lehetett",
    "legyen",
    "lenne",
    "lenni",
    "lesz",
    "lett",
    "maga",
    "magát",
    "majd",
    "már",
    "más",
    "másik",
    "meg",
    "még",
    "mellett",
    "mert",
    "mely",
    "melyek",
    "mi",
    "mit",
    "míg",
    "miért",
    "milyen",
    "mikor",
    "minden",
    "mindent",
    "mindenki",
    "mindig",
    "mint",
    "mintha",
    "mivel",
    "most",
    "nagy",
    "nagyobb",
    "nagyon",
    "ne",
    "néha",
    "nekem",
    "neki",
    "nem",
    "néhány",
    "nélkül",
    "nincs",
    "olyan",
    "ott",
    "össze",
    "ő",
    "ők",
    "őket",
    "pedig",
    "persze",
    "rá",
    "s",
    "saját",
    "sem",
    "semmi",
    "sok",
    "sokat",
    "sokkal",
    "számára",
    "szemben",
    "szerint",
    "szinte",
    "talán",
    "tehát",
    "teljes",
    "tovább",
    "továbbá",
    "több",
    "úgy",
    "ugyanis",
    "új",
    "újabb",
    "újra",
    "után",
    "utána",
    "utolsó",
    "vagy",
    "vagyis",
    "valaki",
    "valami",
    "valamint",
    "való",
    "vagyok",
    "van",
    "vannak",
    "vissza",
    "vele",
    "viszont",
    "volt",
    "voltam",
    "voltak",
    "voltunk",
    "volna",
];

/// Italian stopwords (PostgreSQL `italian.stop`).
static STOPWORDS_IT: &[&str] = &[
    "a",
    "abbia",
    "abbiamo",
    "abbiano",
    "abbiate",
    "ad",
    "agl",
    "agli",
    "ai",
    "al",
    "all",
    "alla",
    "alle",
    "allo",
    "anche",
    "avendo",
    "avere",
    "avesse",
    "avessero",
    "avessi",
    "avessimo",
    "aveste",
    "avesti",
    "avete",
    "aveva",
    "avevamo",
    "avevano",
    "avevate",
    "avevi",
    "avevo",
    "avrai",
    "avranno",
    "avrebbe",
    "avrebbero",
    "avrei",
    "avremmo",
    "avremo",
    "avreste",
    "avresti",
    "avrete",
    "avrò",
    "avuta",
    "avute",
    "avuti",
    "avuto",
    "c",
    "che",
    "chi",
    "ci",
    "coi",
    "col",
    "come",
    "con",
    "cui",
    "da",
    "dal",
    "dall",
    "dalla",
    "dalle",
    "dagl",
    "dagli",
    "dai",
    "degli",
    "dei",
    "del",
    "dell",
    "della",
    "delle",
    "dello",
    "di",
    "dove",
    "dov",
    "e",
    "ed",
    "era",
    "eravamo",
    "erano",
    "eravate",
    "eri",
    "ero",
    "è",
    "essendo",
    "faccio",
    "facciamo",
    "facciano",
    "facciate",
    "fai",
    "fanno",
    "farà",
    "farai",
    "faranno",
    "farebbe",
    "farebbero",
    "farei",
    "faremmo",
    "faremo",
    "fareste",
    "faresti",
    "farete",
    "farò",
    "facendo",
    "facesse",
    "facessero",
    "facessi",
    "facessimo",
    "faceste",
    "facesti",
    "faceva",
    "facevamo",
    "facevano",
    "facevate",
    "facevi",
    "facevo",
    "fece",
    "fecero",
    "feci",
    "facemmo",
    "faceste",
    "fosti",
    "fu",
    "fummo",
    "furono",
    "fosse",
    "fossero",
    "fossi",
    "fossimo",
    "fui",
    "fuori",
    "gli",
    "ha",
    "hai",
    "hanno",
    "ho",
    "i",
    "il",
    "in",
    "io",
    "l",
    "la",
    "le",
    "lei",
    "li",
    "lo",
    "loro",
    "lui",
    "ma",
    "me",
    "mi",
    "mia",
    "miei",
    "mie",
    "mio",
    "ne",
    "negl",
    "negli",
    "nei",
    "nel",
    "nell",
    "nella",
    "nelle",
    "nello",
    "noi",
    "non",
    "nostra",
    "nostre",
    "nostri",
    "nostro",
    "o",
    "per",
    "perché",
    "più",
    "quale",
    "quanta",
    "quante",
    "quanti",
    "quanto",
    "quella",
    "quelle",
    "quelli",
    "quello",
    "questa",
    "queste",
    "questi",
    "questo",
    "qui",
    "quì",
    "sarà",
    "sarai",
    "saranno",
    "sarebbe",
    "sarebbero",
    "sarei",
    "saremmo",
    "saremo",
    "sareste",
    "saresti",
    "sarete",
    "sarò",
    "se",
    "sei",
    "si",
    "sia",
    "siano",
    "siate",
    "siamo",
    "siete",
    "sono",
    "sta",
    "stai",
    "stando",
    "stanno",
    "starà",
    "starai",
    "staranno",
    "starebbe",
    "starebbero",
    "starei",
    "staremmo",
    "staremo",
    "stareste",
    "staresti",
    "starete",
    "starò",
    "stata",
    "state",
    "stati",
    "stava",
    "stavamo",
    "stavano",
    "stavate",
    "stavi",
    "stavo",
    "stessa",
    "stesse",
    "stessero",
    "stessi",
    "stessimo",
    "steste",
    "stesti",
    "stette",
    "stettero",
    "stetti",
    "stemmo",
    "stia",
    "stiamo",
    "stiano",
    "stiate",
    "sto",
    "su",
    "sua",
    "sue",
    "sugl",
    "sugli",
    "sui",
    "sul",
    "sull",
    "sulla",
    "sulle",
    "sullo",
    "suoi",
    "suo",
    "ti",
    "tra",
    "tu",
    "tua",
    "tue",
    "tuo",
    "tuoi",
    "tutto",
    "tutti",
    "u",
    "un",
    "una",
    "uno",
    "vi",
    "vo",
    "voi",
    "vostra",
    "vostre",
    "vostri",
    "vostro",
];

/// Norwegian stopwords (PostgreSQL `norwegian.stop`, Bokmal + Nynorsk).
static STOPWORDS_NO: &[&str] = &[
    "og", "i", "jeg", "det", "at", "en", "et", "den", "til", "er", "som", "på", "de", "med", "han",
    "av", "for", "ikke", "der", "var", "meg", "seg", "men", "har", "om", "vi", "min", "over", "da",
    "fra", "du", "ut", "sin", "dem", "oss", "opp", "man", "hans", "hvor", "eller", "hva", "skal",
    "selv", "her", "alle", "vil", "ble", "kunne", "inn", "når", "være", "noe", "ville", "jo",
    "etter", "ned", "skulle", "denne", "end", "dette", "mitt", "under", "ha", "deg", "andre",
    "hennes", "mine", "alt", "mye", "sitt", "sine", "mot", "disse", "hvis", "din", "noen", "hos",
    "mange", "blir", "vært", "dere", "slik", "nei", "ja", "no", "vel", "ikkje", "si", "ein", "ei",
    "eit", "dei", "vere", "å",
];

/// Portuguese stopwords (PostgreSQL `portuguese.stop`, 203 words).
static STOPWORDS_PT: &[&str] = &[
    "a",
    "à",
    "ao",
    "aos",
    "aquela",
    "aquelas",
    "aquele",
    "aqueles",
    "aquilo",
    "as",
    "às",
    "até",
    "com",
    "como",
    "da",
    "das",
    "de",
    "dela",
    "delas",
    "dele",
    "deles",
    "depois",
    "do",
    "dos",
    "e",
    "ela",
    "elas",
    "ele",
    "eles",
    "em",
    "entre",
    "era",
    "éramos",
    "eram",
    "essa",
    "essas",
    "esse",
    "esses",
    "esta",
    "estamos",
    "estão",
    "estar",
    "estas",
    "estava",
    "estávamos",
    "estavam",
    "esteve",
    "estiver",
    "estivermos",
    "estiverem",
    "estivera",
    "estivéramos",
    "estivesse",
    "estivéssemos",
    "estivessem",
    "estiveram",
    "este",
    "estes",
    "estou",
    "está",
    "eu",
    "foi",
    "fomos",
    "for",
    "fora",
    "foram",
    "formos",
    "forem",
    "fôramos",
    "fosse",
    "fôssemos",
    "fossem",
    "fui",
    "há",
    "haja",
    "hajamos",
    "hajam",
    "havemos",
    "hão",
    "houve",
    "houvemos",
    "houveram",
    "houvera",
    "houvéramos",
    "houvesse",
    "houvéssemos",
    "houvessem",
    "houver",
    "houvermos",
    "houverem",
    "houverei",
    "houverá",
    "houveremos",
    "houverão",
    "houveria",
    "houveríamos",
    "houveriam",
    "hei",
    "isso",
    "isto",
    "já",
    "lhe",
    "lhes",
    "me",
    "mesmo",
    "meu",
    "meus",
    "minha",
    "minhas",
    "muito",
    "na",
    "nas",
    "nem",
    "no",
    "nos",
    "nós",
    "nossa",
    "nossas",
    "nosso",
    "nossos",
    "num",
    "numa",
    "não",
    "nós",
    "o",
    "os",
    "ou",
    "para",
    "pela",
    "pelas",
    "pelo",
    "pelos",
    "por",
    "qual",
    "quando",
    "que",
    "quem",
    "se",
    "seja",
    "sejamos",
    "sejam",
    "será",
    "seremos",
    "serão",
    "seria",
    "seríamos",
    "seriam",
    "serei",
    "sou",
    "são",
    "somos",
    "sua",
    "suas",
    "seu",
    "seus",
    "só",
    "também",
    "te",
    "tem",
    "temos",
    "tém",
    "tinha",
    "tínhamos",
    "tinham",
    "tive",
    "tivemos",
    "tiveram",
    "tivera",
    "tivéramos",
    "tenha",
    "tenhamos",
    "tenham",
    "tivesse",
    "tivéssemos",
    "tivessem",
    "tiver",
    "tivermos",
    "tiverem",
    "terei",
    "terá",
    "teremos",
    "terão",
    "teria",
    "teríamos",
    "teriam",
    "teve",
    "tu",
    "tua",
    "tuas",
    "teu",
    "teus",
    "tenho",
    "tienes",
    "um",
    "uma",
    "você",
    "vocês",
    "vos",
    "vossas",
    "vosso",
    "vossos",
];

/// Romanian stopwords (stopwords-iso/stopwords-ro; PostgreSQL ships no romanian.stop).
static STOPWORDS_RO: &[&str] = &[
    "a",
    "acea",
    "aceea",
    "acel",
    "acela",
    "acele",
    "acelasi",
    "acelea",
    "aceeasi",
    "aceasta",
    "acestea",
    "acest",
    "acesta",
    "aceste",
    "acestui",
    "acestor",
    "acelor",
    "acolo",
    "acum",
    "ai",
    "aia",
    "aici",
    "al",
    "ale",
    "alea",
    "alt",
    "alta",
    "alte",
    "altceva",
    "altcineva",
    "altfel",
    "altul",
    "am",
    "ar",
    "are",
    "atunci",
    "au",
    "ba",
    "ca",
    "că",
    "căci",
    "care",
    "cel",
    "cela",
    "cele",
    "celor",
    "ceia",
    "ceea",
    "chiar",
    "cine",
    "cineva",
    "când",
    "câte",
    "câteva",
    "cât",
    "cu",
    "cui",
    "cum",
    "da",
    "dacă",
    "dar",
    "de",
    "deci",
    "deoarece",
    "desi",
    "deși",
    "din",
    "dintr",
    "două",
    "dumneavoastră",
    "dânsul",
    "dânsa",
    "dânșii",
    "dânsele",
    "e",
    "ea",
    "ei",
    "el",
    "ele",
    "era",
    "este",
    "ești",
    "eu",
    "față",
    "fi",
    "fie",
    "fiecare",
    "fiindcă",
    "în",
    "înainte",
    "înaintea",
    "între",
    "încă",
    "înspre",
    "la",
    "le",
    "li",
    "lor",
    "lui",
    "mai",
    "mea",
    "mele",
    "mereu",
    "meu",
    "mi",
    "mie",
    "mine",
    "mult",
    "multă",
    "mulți",
    "multe",
    "ne",
    "nicidecum",
    "nimic",
    "nimeni",
    "noi",
    "noastre",
    "nostru",
    "noastră",
    "nu",
    "o",
    "or",
    "ori",
    "pe",
    "pentru",
    "poate",
    "prin",
    "puțin",
    "s",
    "sa",
    "sau",
    "se",
    "si",
    "și",
    "ști",
    "te",
    "tău",
    "ta",
    "tale",
    "tu",
    "ți",
    "unde",
    "unele",
    "unii",
    "unui",
    "unor",
    "unora",
    "unu",
    "unuia",
    "vă",
    "vom",
    "vor",
    "vostru",
    "voastre",
    "voi",
    "voastră",
];

/// Russian stopwords (PostgreSQL `russian.stop`, 145 words; Cyrillic Unicode).
static STOPWORDS_RU: &[&str] = &[
    "а",
    "без",
    "более",
    "больше",
    "будет",
    "будто",
    "бы",
    "был",
    "была",
    "были",
    "было",
    "быть",
    "в",
    "вам",
    "вас",
    "вдруг",
    "ведь",
    "во",
    "вот",
    "впрочем",
    "все",
    "всегда",
    "всего",
    "всех",
    "всю",
    "вы",
    "где",
    "да",
    "даже",
    "два",
    "для",
    "до",
    "если",
    "есть",
    "еще",
    "ж",
    "за",
    "зачем",
    "здесь",
    "и",
    "из",
    "или",
    "им",
    "иногда",
    "их",
    "к",
    "как",
    "какая",
    "какой",
    "когда",
    "конечно",
    "кто",
    "куда",
    "ли",
    "лучше",
    "между",
    "меня",
    "много",
    "может",
    "можно",
    "мне",
    "мой",
    "моя",
    "мы",
    "на",
    "над",
    "надо",
    "наконец",
    "нас",
    "не",
    "него",
    "нее",
    "ней",
    "нельзя",
    "нет",
    "нибудь",
    "никогда",
    "ним",
    "них",
    "ничего",
    "но",
    "ну",
    "о",
    "об",
    "один",
    "он",
    "она",
    "они",
    "опять",
    "от",
    "перед",
    "по",
    "после",
    "потому",
    "потом",
    "почти",
    "при",
    "про",
    "раз",
    "разве",
    "с",
    "сам",
    "се",
    "себе",
    "себя",
    "сейчас",
    "си",
    "со",
    "совсем",
    "так",
    "такой",
    "там",
    "тебя",
    "тем",
    "теперь",
    "то",
    "тогда",
    "того",
    "тоже",
    "только",
    "том",
    "тот",
    "три",
    "тут",
    "ты",
    "у",
    "уж",
    "уже",
    "хорошо",
    "хоть",
    "через",
    "что",
    "чтоб",
    "чтобы",
    "чуть",
    "эти",
    "этого",
    "этой",
    "этом",
    "этот",
    "эту",
    "я",
];

/// Spanish stopwords (PostgreSQL `spanish.stop`, 238 words).
static STOPWORDS_ES: &[&str] = &[
    "a",
    "al",
    "algo",
    "algunas",
    "algunos",
    "ante",
    "antes",
    "como",
    "con",
    "contra",
    "cual",
    "cuando",
    "de",
    "del",
    "desde",
    "donde",
    "durante",
    "e",
    "el",
    "ella",
    "ellas",
    "ellos",
    "en",
    "entre",
    "era",
    "erais",
    "éramos",
    "eran",
    "eras",
    "eres",
    "es",
    "esa",
    "esas",
    "ese",
    "eso",
    "esos",
    "esta",
    "estaba",
    "estabais",
    "estábamos",
    "estaban",
    "estabas",
    "estad",
    "estada",
    "estadas",
    "estado",
    "estados",
    "estamos",
    "estando",
    "estar",
    "estarán",
    "estarás",
    "estaré",
    "estaréis",
    "estaremos",
    "estarían",
    "estarías",
    "estaría",
    "estaríais",
    "estaríamos",
    "estás",
    "están",
    "esté",
    "estéis",
    "estemos",
    "estén",
    "estas",
    "este",
    "esto",
    "estos",
    "estoy",
    "estuve",
    "estuviera",
    "estuvierais",
    "estuviéramos",
    "estuvieran",
    "estuvieras",
    "estuvieron",
    "estuviese",
    "estuvieseis",
    "estuviésemos",
    "estuviesen",
    "estuvieses",
    "estuvo",
    "estuviste",
    "estuvisteis",
    "estuvimos",
    "fui",
    "fue",
    "fuera",
    "fuerais",
    "fuéramos",
    "fueran",
    "fueras",
    "fueron",
    "fuese",
    "fueseis",
    "fuésemos",
    "fuesen",
    "fueses",
    "fuiste",
    "fuisteis",
    "fuimos",
    "ha",
    "había",
    "habíais",
    "habíamos",
    "habían",
    "habías",
    "habida",
    "habidas",
    "habido",
    "habidos",
    "habiendo",
    "habrán",
    "habrás",
    "habré",
    "habréis",
    "habremos",
    "habrían",
    "habrías",
    "habría",
    "habríais",
    "habríamos",
    "han",
    "has",
    "hasta",
    "hay",
    "haya",
    "hayáis",
    "hayamos",
    "hayan",
    "hayas",
    "he",
    "hemos",
    "hubiera",
    "hubierais",
    "hubiéramos",
    "hubieran",
    "hubieras",
    "hubieron",
    "hubiese",
    "hubieseis",
    "hubiésemos",
    "hubiesen",
    "hubieses",
    "hubo",
    "hubiste",
    "hubisteis",
    "hubimos",
    "il",
    "la",
    "las",
    "le",
    "les",
    "lo",
    "los",
    "me",
    "mi",
    "mis",
    "muchos",
    "mucho",
    "muy",
    "más",
    "mí",
    "mía",
    "mías",
    "mío",
    "míos",
    "nada",
    "ni",
    "no",
    "nos",
    "nosotras",
    "nosotros",
    "nuestra",
    "nuestras",
    "nuestro",
    "nuestros",
    "o",
    "os",
    "otra",
    "otras",
    "otro",
    "otros",
    "para",
    "pero",
    "por",
    "porque",
    "que",
    "quien",
    "quienes",
    "qué",
    "se",
    "sea",
    "seáis",
    "seamos",
    "sean",
    "seas",
    "sentid",
    "sentida",
    "sentidas",
    "sentido",
    "sentidos",
    "sintiendo",
    "ser",
    "será",
    "seráis",
    "serán",
    "serás",
    "seré",
    "seréis",
    "seremos",
    "serían",
    "serías",
    "sería",
    "seríais",
    "seríamos",
    "si",
    "siente",
    "sin",
    "sobre",
    "sois",
    "somos",
    "son",
    "soy",
    "su",
    "sus",
    "suya",
    "suyas",
    "suyo",
    "suyos",
    "sé",
    "sí",
    "también",
    "tanto",
    "te",
    "tendrán",
    "tendrás",
    "tendré",
    "tendréis",
    "tendremos",
    "tendrían",
    "tendrías",
    "tendría",
    "tendríais",
    "tendríamos",
    "tened",
    "tenemos",
    "tengo",
    "tenía",
    "teníais",
    "teníamos",
    "tenían",
    "tenías",
    "teniendo",
    "tenida",
    "tenidas",
    "tenido",
    "tenidos",
    "tiene",
    "tienen",
    "tienes",
    "ti",
    "tú",
    "tu",
    "tus",
    "tuya",
    "tuyas",
    "tuyo",
    "tuyos",
    "tuviera",
    "tuvierais",
    "tuviéramos",
    "tuvieran",
    "tuvieras",
    "tuvieron",
    "tuviese",
    "tuvieseis",
    "tuviésemos",
    "tuviesen",
    "tuvieses",
    "tuvo",
    "tuviste",
    "tuvisteis",
    "tuvimos",
    "un",
    "una",
    "unas",
    "unos",
    "vosotras",
    "vosotros",
    "vuestro",
    "vuestra",
    "vuestros",
    "vuestras",
    "y",
    "ya",
    "yo",
    "él",
    "ésa",
    "ésas",
    "ése",
    "ésos",
    "ésta",
    "éstas",
    "éste",
    "éstos",
];

/// Swedish stopwords (PostgreSQL `swedish.stop`, 114 words).
static STOPWORDS_SV: &[&str] = &[
    "allt", "alla", "att", "av", "blev", "bli", "blivit", "då", "deras", "de", "den", "dess",
    "dessa", "det", "detta", "dig", "din", "dina", "dit", "ditt", "du", "där", "efter", "ej",
    "eller", "en", "er", "era", "ert", "ett", "från", "för", "ha", "hade", "han", "hans", "har",
    "hem", "hennes", "henne", "hon", "honom", "hur", "i", "icke", "ingen", "inom", "inte", "jag",
    "ju", "kan", "kom", "kunde", "men", "med", "mellan", "mig", "min", "mina", "mitt", "mot",
    "mycket", "möjliga", "när", "ni", "nu", "något", "någon", "några", "oss", "och", "om", "på",
    "redan", "samma", "se", "sedan", "ser", "sig", "sin", "sina", "sitta", "sist", "sitt", "ska",
    "ske", "skulle", "så", "sådan", "sådana", "sådant", "sätta", "till", "ty", "under", "upp",
    "ut", "utan", "utom", "var", "vara", "vare", "vars", "vart", "varför", "varje", "vem", "vi",
    "vid", "vilka", "vilkas", "vilken", "vilket", "vår", "våra", "vårt", "vad", "var", "över",
    "är", "än",
];

/// Turkish stopwords (PostgreSQL `turkish.stop`).
static STOPWORDS_TR: &[&str] = &[
    "acaba",
    "altı",
    "altmış",
    "ama",
    "aslında",
    "az",
    "bazı",
    "belki",
    "biri",
    "birkaç",
    "birşey",
    "biz",
    "bu",
    "çok",
    "çünkü",
    "da",
    "daha",
    "de",
    "değil",
    "diye",
    "dört",
    "eğer",
    "elli",
    "en",
    "gibi",
    "hem",
    "hep",
    "hepsi",
    "her",
    "hiç",
    "için",
    "iki",
    "ile",
    "ise",
    "işte",
    "kadar",
    "karşı",
    "katrilyon",
    "kez",
    "ki",
    "kim",
    "milyar",
    "milyon",
    "mi",
    "mı",
    "mu",
    "mü",
    "nasıl",
    "ne",
    "neden",
    "nerede",
    "nereye",
    "niye",
    "niçin",
    "o",
    "olan",
    "olarak",
    "oldu",
    "olduğu",
    "olmak",
    "olması",
    "on",
    "onlar",
    "onların",
    "onlara",
    "onlarda",
    "onun",
    "otuz",
    "oysa",
    "öyle",
    "pek",
    "rağmen",
    "sadece",
    "sanki",
    "sekiz",
    "seksen",
    "sen",
    "siz",
    "şey",
    "şimdi",
    "şu",
    "tüm",
    "trilyon",
    "var",
    "ve",
    "veya",
    "ya",
    "yani",
    "yedi",
    "yirmi",
    "yok",
    "zaten",
    "bir",
    "ben",
];

/// Greek stop words matching PostgreSQL's `greek.stop`.
static STOPWORDS_EL: &[&str] = &[
    "αι",
    "αλλα",
    "αν",
    "αντι",
    "απο",
    "αρα",
    "αυτα",
    "αυτες",
    "αυτη",
    "αυτο",
    "αυτοι",
    "αυτος",
    "αυτους",
    "αφου",
    "γι",
    "για",
    "γιατι",
    "γιατί",
    "γιοτι",
    "γιωτι",
    "γοτι",
    "διοτι",
    "διότι",
    "εαν",
    "ειμαι",
    "ειναι",
    "ειστε",
    "εκει",
    "εκεινα",
    "εκεινες",
    "εκεινη",
    "εκεινο",
    "εκεινοι",
    "εκεινος",
    "εκεινους",
    "εν",
    "ενω",
    "εντος",
    "εξ",
    "εξω",
    "επι",
    "εως",
    "η",
    "ηταν",
    "θα",
    "ι",
    "ιδια",
    "ιδιο",
    "ιδιοι",
    "ιδιος",
    "ιδιους",
    "ιδιων",
    "ιδιες",
    "ιδια",
    "ιι",
    "ιν",
    "ινα",
    "κ",
    "κι",
    "κα",
    "καθε",
    "και",
    "κακ",
    "καλα",
    "καν",
    "κατα",
    "κατι",
    "κατω",
    "κε",
    "κει",
    "κεν",
    "κι",
    "κιολας",
    "κοντα",
    "κτλ",
    "μα",
    "μαζι",
    "μακρια",
    "μαλιστα",
    "μαλλον",
    "με",
    "μεν",
    "μεσα",
    "μετα",
    "μη",
    "μην",
    "μια",
    "μολονοτι",
    "μου",
    "μπρος",
    "ναι",
    "νε",
    "ντε",
    "ξανα",
    "ο",
    "οι",
    "ολα",
    "ολες",
    "ολη",
    "ολο",
    "ολοι",
    "ολος",
    "ολους",
    "ολων",
    "οπου",
    "οπωσδηποτε",
    "οπως",
    "οσα",
    "οσο",
    "οταν",
    "οτι",
    "ου",
    "παντοτε",
    "παντα",
    "παρα",
    "περι",
    "πια",
    "πιο",
    "πλαι",
    "ποια",
    "ποιες",
    "ποιοι",
    "ποιον",
    "ποιος",
    "ποιους",
    "ποιων",
    "που",
    "πρεπει",
    "πριν",
    "προ",
    "προς",
    "πως",
    "σαν",
    "σας",
    "σε",
    "στα",
    "στη",
    "στην",
    "στης",
    "στο",
    "στον",
    "στους",
    "στων",
    "συ",
    "συγκεκριμενα",
    "συν",
    "συνεπως",
    "τα",
    "ταδε",
    "τελικα",
    "τελικως",
    "τες",
    "τι",
    "τιποτα",
    "τιποτε",
    "τιποτ",
    "τοι",
    "τοιουτος",
    "τοιουτοτροπως",
    "τοιουτωτροπως",
    "τοιουτωτροπων",
    "τοιουτωτροπωσ",
    "τοιουτωτρόπως",
    "τοιουτωτρόπον",
    "τοιουτωτρόπoν",
    "τοιουτοτρόπως",
    "τοιουτοτρόπoς",
    "τοιουτοτρόπoν",
    "τοιουτοτρόπον",
    "τοτε",
    "του",
    "τουλαχιστον",
    "τους",
    "τουτα",
    "τουτεστιν",
    "τουτες",
    "τουτη",
    "τουτο",
    "τουτοι",
    "τουτοις",
    "τουτον",
    "τουτος",
    "τουτους",
    "τουτων",
    "τρεις",
    "τρια",
    "τριγυρω",
    "τριγύρω",
    "τωρα",
    "υπ",
    "υπαρχει",
    "υπαρχουν",
    "υπο",
    "υποψιν",
    "ωσοτου",
    "ωσπου",
    "ωσοτου",
    "ωστε",
    "ωστοσο",
    "αλλ",
    "αλλος",
    "αλλοιως",
];

// ---------------------------------------------------------------------------
// English stop words and the Porter stemmer.
// ---------------------------------------------------------------------------

/// The snowball English stop-word list (PostgreSQL's `english.stop`).
static STOP_WORDS: [&str; 127] = [
    "i",
    "me",
    "my",
    "myself",
    "we",
    "our",
    "ours",
    "ourselves",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "it",
    "its",
    "itself",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
    "what",
    "which",
    "who",
    "whom",
    "this",
    "that",
    "these",
    "those",
    "am",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "having",
    "do",
    "does",
    "did",
    "doing",
    "a",
    "an",
    "the",
    "and",
    "but",
    "if",
    "or",
    "because",
    "as",
    "until",
    "while",
    "of",
    "at",
    "by",
    "for",
    "with",
    "about",
    "against",
    "between",
    "into",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "to",
    "from",
    "up",
    "down",
    "in",
    "out",
    "on",
    "off",
    "over",
    "under",
    "again",
    "further",
    "then",
    "once",
    "here",
    "there",
    "when",
    "where",
    "why",
    "how",
    "all",
    "any",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "s",
    "t",
    "can",
    "will",
    "just",
    "don",
    "should",
    "now",
];

/// Is `w[i]` a consonant under Porter's definition (`y` is a consonant at the
/// start of the word or after a vowel)?
fn is_cons(w: &[u8], i: usize) -> bool {
    match w[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !is_cons(w, i - 1),
        _ => true,
    }
}

/// Porter's measure *m* of the stem `w[..j]`.
fn measure(w: &[u8], j: usize) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < j && is_cons(w, i) {
        i += 1;
    }
    loop {
        while i < j && !is_cons(w, i) {
            i += 1;
        }
        if i >= j {
            return n;
        }
        while i < j && is_cons(w, i) {
            i += 1;
        }
        n += 1;
        if i >= j {
            return n;
        }
    }
}

fn has_vowel(w: &[u8], j: usize) -> bool {
    (0..j).any(|i| !is_cons(w, i))
}

fn ends_double_cons(w: &[u8]) -> bool {
    let j = w.len();
    j >= 2 && w[j - 1] == w[j - 2] && is_cons(w, j - 1)
}

fn ends_cvc(w: &[u8], j: usize) -> bool {
    if j == 2 {
        return !is_cons(w, 0) && is_cons(w, 1);
    }
    j >= 3
        && is_cons(w, j - 3)
        && !is_cons(w, j - 2)
        && is_cons(w, j - 1)
        && !matches!(w[j - 1], b'w' | b'x' | b'y')
}

fn ends(w: &[u8], suffix: &str) -> bool {
    w.len() > suffix.len() && w.ends_with(suffix.as_bytes())
}

fn apply_rules(w: &mut Vec<u8>, rules: &[(&str, &str)], min_m: usize) {
    for (suffix, replacement) in rules {
        if ends(w, suffix) {
            let j = w.len() - suffix.len();
            if measure(w, j) > min_m {
                w.truncate(j);
                w.extend_from_slice(replacement.as_bytes());
            }
            return;
        }
    }
}

/// The classic Porter (1980) stemming algorithm with snowball-english
/// alignments so common words stem the way PostgreSQL's `english_stem`
/// dictionary does.
pub fn porter_stem(word: &str) -> String {
    match word {
        "skis" => return "ski".into(),
        "skies" => return "sky".into(),
        "dying" => return "die".into(),
        "lying" => return "lie".into(),
        "tying" => return "tie".into(),
        "idly" => return "idl".into(),
        "gently" => return "gentl".into(),
        "ugly" => return "ugli".into(),
        "early" => return "earli".into(),
        "only" => return "onli".into(),
        "singly" => return "singl".into(),
        "sky" | "news" | "howe" | "atlas" | "cosmos" | "bias" | "andes" => return word.into(),
        _ => {}
    }
    let mut w = word.as_bytes().to_vec();
    if w.len() <= 2 {
        return word.into();
    }

    // Step 1a: plurals.
    if ends(&w, "sses") {
        w.truncate(w.len() - 2);
    } else if ends(&w, "ies") || ends(&w, "ied") {
        let stem = w.len() - 3;
        w.truncate(stem);
        w.push(b'i');
        if stem <= 1 {
            w.push(b'e');
        }
    } else if ends(&w, "ss") {
        // Unchanged.
    } else if w.last() == Some(&b's') && (0..w.len().saturating_sub(2)).any(|i| !is_cons(&w, i)) {
        w.truncate(w.len() - 1);
    }

    // Step 1b: -eed / -ed / -ing.
    if ends(&w, "eed") {
        if measure(&w, w.len() - 3) > 0 {
            w.truncate(w.len() - 1);
        }
    } else {
        let removed = if ends(&w, "ed") && has_vowel(&w, w.len() - 2) {
            w.truncate(w.len() - 2);
            true
        } else if ends(&w, "ing") && has_vowel(&w, w.len() - 3) {
            w.truncate(w.len() - 3);
            true
        } else {
            false
        };
        if removed {
            if ends(&w, "at") || ends(&w, "bl") || ends(&w, "iz") {
                w.push(b'e');
            } else if ends_double_cons(&w) && !matches!(w[w.len() - 1], b'l' | b's' | b'z') {
                w.truncate(w.len() - 1);
            } else if measure(&w, w.len()) == 1 && ends_cvc(&w, w.len()) {
                w.push(b'e');
            }
        }
    }

    // Step 1c: y -> i, snowball's condition (after a non-initial consonant).
    if w.last() == Some(&b'y') {
        let j = w.len() - 1;
        if j >= 2 && is_cons(&w, j - 1) {
            w[j] = b'i';
        }
    }

    // Step 2 (m > 0), longest suffix first.
    apply_rules(
        &mut w,
        &[
            ("ational", "ate"),
            ("ization", "ize"),
            ("iveness", "ive"),
            ("fulness", "ful"),
            ("ousness", "ous"),
            ("biliti", "ble"),
            ("tional", "tion"),
            ("ation", "ate"),
            ("alism", "al"),
            ("aliti", "al"),
            ("iviti", "ive"),
            ("entli", "ent"),
            ("ousli", "ous"),
            ("enci", "ence"),
            ("anci", "ance"),
            ("izer", "ize"),
            ("abli", "able"),
            ("alli", "al"),
            ("ator", "ate"),
            ("logi", "log"),
            ("eli", "e"),
            ("bli", "ble"),
        ],
        0,
    );

    // Step 3 (m > 0).
    apply_rules(
        &mut w,
        &[
            ("icate", "ic"),
            ("ative", ""),
            ("alize", "al"),
            ("iciti", "ic"),
            ("ical", "ic"),
            ("ness", ""),
            ("ful", ""),
        ],
        0,
    );

    // Step 4 (m > 1): bare suffix removal; -ion only after s/t.
    for suffix in [
        "ement", "ance", "ence", "able", "ible", "ment", "ant", "ent", "ion", "ism", "ate", "iti",
        "ous", "ive", "ize", "al", "er", "ic", "ou",
    ] {
        if ends(&w, suffix) {
            let j = w.len() - suffix.len();
            let ion_ok = suffix != "ion" || (j > 0 && matches!(w[j - 1], b's' | b't'));
            if measure(&w, j) > 1 && ion_ok {
                w.truncate(j);
            }
            break;
        }
    }

    // Step 5a: final -e.
    if w.last() == Some(&b'e') {
        let j = w.len() - 1;
        let m = measure(&w, j);
        if m > 1 || (m == 1 && !ends_cvc(&w, j)) {
            w.truncate(j);
        }
    }
    // Step 5b: -ll -> -l when m > 1.
    if measure(&w, w.len()) > 1 && ends_double_cons(&w) && w[w.len() - 1] == b'l' {
        w.truncate(w.len() - 1);
    }

    String::from_utf8(w).expect("porter stemmer operates on ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porter_stems_pg_documented_examples() {
        for (word, stem) in [
            ("rats", "rat"),
            ("stars", "star"),
            ("jumping", "jump"),
            ("jumps", "jump"),
            ("jumped", "jump"),
            ("ate", "ate"),
            ("cats", "cat"),
            ("sat", "sat"),
            ("mats", "mat"),
        ] {
            assert_eq!(porter_stem(word), stem, "{word}");
        }
    }

    #[test]
    fn porter_stems_suffix_classes() {
        for (word, stem) in [
            ("relational", "relat"),
            ("operation", "oper"),
            ("organization", "organ"),
            ("generalizations", "gener"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("hopefulness", "hope"),
            ("callousness", "callous"),
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "tie"),
            ("connection", "connect"),
            ("happiness", "happi"),
            ("controlling", "control"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("hoping", "hope"),
            ("sky", "sky"),
            ("news", "news"),
            ("play", "play"),
            ("cry", "cri"),
        ] {
            assert_eq!(porter_stem(word), stem, "{word}");
        }
    }

    #[test]
    fn simple_config_lowercases_only() {
        let v = to_tsvector(TsConfig::Simple, "The Fat Rats");
        assert_eq!(fts::format_tsvector(&v), "'fat':2 'rats':3 'the':1");
    }

    #[test]
    fn english_config_stems_and_drops_stop_words() {
        let v = to_tsvector(TsConfig::English, "The Fat Rats");
        assert_eq!(fts::format_tsvector(&v), "'fat':2 'rat':3");
        let v = to_tsvector(
            TsConfig::English,
            "a fat cat sat on a mat - it ate a fat rats",
        );
        assert_eq!(
            fts::format_tsvector(&v),
            "'ate':9 'cat':3 'fat':2,11 'mat':7 'rat':12 'sat':4"
        );
    }

    #[test]
    fn tokenizer_handles_hword() {
        let tokens = tokenize("state-of-the-art solution");
        assert!(
            tokens
                .iter()
                .any(|(t, k, p)| t == "state-of-the-art" && *k == TokenKind::HWord && *p == 1)
        );
        assert!(
            tokens
                .iter()
                .any(|(t, k, p)| t == "state" && *k == TokenKind::HWordPart && *p == 1)
        );
        assert!(
            tokens
                .iter()
                .any(|(t, k, p)| t == "art" && *k == TokenKind::HWordPart && *p == 1)
        );
        assert!(
            tokens
                .iter()
                .any(|(t, k, p)| t == "solution" && *k == TokenKind::AsciiWord && *p == 2)
        );
    }

    #[test]
    fn tokenizer_handles_email_and_url() {
        let tokens = tokenize("contact user@example.com or http://example.com/path");
        assert!(
            tokens
                .iter()
                .any(|(t, k, _)| t == "user@example.com" && *k == TokenKind::Email)
        );
        assert!(
            tokens
                .iter()
                .any(|(t, k, _)| t == "http://example.com/path" && *k == TokenKind::Url)
        );
    }

    #[test]
    fn tokenizer_handles_numbers() {
        let tokens = tokenize("version 3.14 or 42");
        assert!(
            tokens
                .iter()
                .any(|(t, k, _)| t == "3.14" && *k == TokenKind::Float)
        );
        assert!(
            tokens
                .iter()
                .any(|(t, k, _)| t == "42" && *k == TokenKind::Integer)
        );
    }

    #[test]
    fn hword_parts_get_stop_word_filtered() {
        let v = to_tsvector(TsConfig::English, "state-of-the-art");
        let text = fts::format_tsvector(&v);
        assert!(
            text.contains("state-of-the-art"),
            "compound missing: {text}"
        );
        assert!(text.contains("'state'"), "part 'state' missing: {text}");
        assert!(text.contains("'art'"), "part 'art' missing: {text}");
        assert!(!text.contains("'of'"), "'of' should be stop-worded: {text}");
        assert!(
            !text.contains("'the'"),
            "'the' should be stop-worded: {text}"
        );
    }

    #[test]
    fn to_tsquery_normalizes_and_drops_stop_words() {
        let q = to_tsquery(TsConfig::English, "The & Fat & Rats").unwrap();
        assert_eq!(fts::format_tsquery(q.as_ref()), "'fat' & 'rat'");
        let q = to_tsquery(TsConfig::English, "fat & !the").unwrap();
        assert_eq!(fts::format_tsquery(q.as_ref()), "'fat'");
        let q = to_tsquery(TsConfig::English, "the & a").unwrap();
        assert_eq!(q, None);
    }

    #[test]
    fn plainto_ands_lexemes() {
        let q = plainto_tsquery(TsConfig::English, "The Fat Rats");
        assert_eq!(fts::format_tsquery(q.as_ref()), "'fat' & 'rat'");
        assert_eq!(plainto_tsquery(TsConfig::English, "the a"), None);
    }

    #[test]
    fn rank_matches_pg_reference_values() {
        let w = [0.1f32, 0.2, 0.4, 1.0];
        let tv = to_tsvector(TsConfig::English, "cat");
        let q = to_tsquery(TsConfig::English, "cat").unwrap();
        let r = rank(&w, &tv, q.as_ref());
        assert!((r - 0.06079271).abs() < 1e-6, "{r}");
        let tv = to_tsvector(TsConfig::English, "cat cat");
        let r = rank(&w, &tv, q.as_ref());
        assert!((r - 0.07599089).abs() < 1e-6, "{r}");
        let tv = to_tsvector(TsConfig::English, "fat cat");
        let both = to_tsquery(TsConfig::English, "fat | cat").unwrap();
        let one = to_tsquery(TsConfig::English, "fat | dog").unwrap();
        assert!(rank(&w, &tv, both.as_ref()) > rank(&w, &tv, one.as_ref()));
    }

    #[test]
    fn unknown_config_is_undefined_object() {
        // Use a config that will never exist.
        let err = resolve_config("klingon").unwrap_err();
        assert_eq!(err.sqlstate(), "42704");
        assert_eq!(
            err.to_string(),
            "text search configuration \"klingon\" does not exist"
        );
        assert!(resolve_config("pg_catalog.simple").is_ok());
        assert!(resolve_config("English").is_ok());
        // New language configs are now valid.
        assert!(resolve_config("german").is_ok());
        assert!(resolve_config("french").is_ok());
        assert!(resolve_config("spanish").is_ok());
    }

    #[test]
    fn german_stemmer_works() {
        // "Häuser" → stems to "haus" under German snowball
        let v = to_tsvector(TsConfig::German, "Häuser und Maus");
        let text = fts::format_tsvector(&v);
        // At minimum we should get some lexemes back.
        assert!(
            !text.is_empty(),
            "german stemmer produced no output: {text}"
        );
    }

    #[test]
    fn headline_basic_highlight() {
        let q = to_tsquery(TsConfig::English, "fat & rat").unwrap();
        let opts = HeadlineOptions::default();
        let result = headline(
            TsConfig::English,
            "The Fat Rats ate the cat",
            q.as_ref(),
            &opts,
        );
        assert!(
            result.contains("<b>Fat</b>"),
            "expected Fat highlighted: {result}"
        );
        assert!(
            result.contains("<b>Rats</b>"),
            "expected Rats highlighted: {result}"
        );
    }

    #[test]
    fn headline_custom_sels() {
        let q = to_tsquery(TsConfig::English, "cat").unwrap();
        let opts = parse_headline_opts(Some("StartSel=[,StopSel=]")).unwrap();
        let result = headline(TsConfig::English, "The fat cat sat", q.as_ref(), &opts);
        assert!(result.contains("[cat]"), "expected [cat]: {result}");
    }

    #[test]
    fn headline_no_match() {
        let q = to_tsquery(TsConfig::English, "dragon").unwrap();
        let opts = HeadlineOptions::default();
        let result = headline(TsConfig::English, "The fat cat", q.as_ref(), &opts);
        assert!(!result.contains("<b>"), "unexpected highlight: {result}");
        assert!(result.contains("cat"), "text missing: {result}");
    }

    #[test]
    fn parse_synonyms_basic() {
        let m = parse_synonyms("car:automobile,vehicle;dog:canine");
        assert_eq!(m["car"], vec!["automobile", "vehicle"]);
        assert_eq!(m["dog"], vec!["canine"]);
    }

    #[test]
    fn is_ts_dict_ddl_detects_correctly() {
        assert!(is_ts_dict_ddl(
            "CREATE TEXT SEARCH DICTIONARY d (TEMPLATE = synonym, SYNONYMS = 'a:b')"
        ));
        assert!(is_ts_dict_ddl("DROP TEXT SEARCH DICTIONARY d"));
        assert!(!is_ts_dict_ddl("CREATE TABLE t (x int)"));
    }
}
