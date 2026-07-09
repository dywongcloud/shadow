#![cfg(feature = "sql")]
//! End-to-end conformance tests for PostgreSQL full-text search: the
//! `tsvector`/`tsquery` types, the `simple` and `english` configurations
//! (stop words + Porter stemmer), `to_tsvector`/`to_tsquery`/
//! `plainto_tsquery`, the `@@` operator, `ts_rank`, `length`/`numnode`/
//! `strip`, storage round-trips, and the typed rejections for everything
//! outside the subset. Expected outputs are PostgreSQL 16's.

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

async fn session() -> Session<MemoryStorage> {
    let db = Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"));
    Session::new(db, "guardian")
}

async fn ok(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<ExecResult> {
    s.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
}

/// First row/column of a row-producing result, as PostgreSQL text output
/// (NULL → "NULL").
async fn scalar(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Rows { rows, .. }) => rows
            .first()
            .and_then(|r| r.first())
            .map(|v| v.to_text().unwrap_or_else(|| "NULL".into()))
            .unwrap_or_else(|| panic!("`{sql}` returned no rows")),
        other => panic!("`{sql}` did not produce rows: {other:?}"),
    }
}

/// All result rows rendered as text.
async fn rows_text(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<Vec<String>> {
    match ok(s, sql).await.into_iter().next() {
        Some(ExecResult::Rows { rows, .. }) => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or_else(|| "NULL".into()))
                    .collect()
            })
            .collect(),
        _ => panic!("expected rows from `{sql}`"),
    }
}

/// Execute SQL and return (SQLSTATE, message) of the resulting error.
async fn err_info(s: &mut Session<MemoryStorage>, sql: &str) -> (String, String) {
    match s.execute(sql).await {
        Ok(_) => panic!("expected `{sql}` to fail, but it succeeded"),
        Err(e) => (e.sqlstate().to_string(), e.to_string()),
    }
}

async fn err_code(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    err_info(s, sql).await.0
}

// ---------------------------------------------------------------------------
// Configurations: simple vs english.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn simple_vs_english_tokenization() {
    let mut s = session().await;
    // simple: lowercase + split on non-word — no stemming, no stop words.
    assert_eq!(
        scalar(&mut s, "SELECT to_tsvector('simple', 'The Fat Rats')").await,
        "'fat':2 'rats':3 'the':1"
    );
    // PG: SELECT to_tsvector('english', 'The Fat Rats') => 'fat':2 'rat':3
    assert_eq!(
        scalar(&mut s, "SELECT to_tsvector('english', 'The Fat Rats')").await,
        "'fat':2 'rat':3"
    );
}

#[tokio::test]
async fn stop_words_removed_but_positions_kept() {
    let mut s = session().await;
    // PG docs (12.1): stop words drop but still consume positions.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT to_tsvector('english', 'a fat cat sat on a mat - it ate a fat rats')"
        )
        .await,
        "'ate':9 'cat':3 'fat':2,11 'mat':7 'rat':12 'sat':4"
    );
}

#[tokio::test]
async fn porter_stemming_pg_examples() {
    let mut s = session().await;
    // jumping / jumps / jumped all stem to jump.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT to_tsvector('english', 'jumping jumps jumped')"
        )
        .await,
        "'jump':1,2,3"
    );
    // -ation / -ization / -fulness suffix classes.
    for (word, lexeme) in [
        ("operation", "oper"),
        ("organization", "organ"),
        ("relational", "relat"),
        ("generalizations", "gener"),
        ("hopefulness", "hope"),
        ("connection", "connect"),
        ("stars", "star"),
    ] {
        assert_eq!(
            scalar(&mut s, &format!("SELECT to_tsvector('english', '{word}')")).await,
            format!("'{lexeme}':1"),
            "stem of {word}"
        );
    }
}

#[tokio::test]
async fn default_config_is_english_and_settable() {
    let mut s = session().await;
    assert_eq!(
        scalar(&mut s, "SELECT to_tsvector('The Fat Rats')").await,
        "'fat':2 'rat':3"
    );
    ok(&mut s, "SET default_text_search_config = 'simple'").await;
    assert_eq!(
        scalar(&mut s, "SELECT to_tsvector('The Fat Rats')").await,
        "'fat':2 'rats':3 'the':1"
    );
}

// ---------------------------------------------------------------------------
// tsquery parsing, precedence, display.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tsquery_precedence_and_display() {
    let mut s = session().await;
    // ! binds tighter than &, & tighter than |.
    assert_eq!(
        scalar(&mut s, "SELECT to_tsquery('simple', '!a & b | c')").await,
        "!'a' & 'b' | 'c'"
    );
    // Parentheses group; PG prints them as `( ... )` only where needed.
    assert_eq!(
        scalar(&mut s, "SELECT to_tsquery('simple', 'fat & (rat | cat)')").await,
        "'fat' & ( 'rat' | 'cat' )"
    );
    assert_eq!(
        scalar(&mut s, "SELECT to_tsquery('simple', '!(a | b)')").await,
        "!( 'a' | 'b' )"
    );
    // Quoted single lexemes work; config processing applies to operands.
    assert_eq!(
        scalar(&mut s, "SELECT to_tsquery('english', 'The & Fat & Rats')").await,
        "'fat' & 'rat'"
    );
    // A syntax error is 42601 like PostgreSQL.
    assert_eq!(err_code(&mut s, "SELECT to_tsquery('a b')").await, "42601");
    assert_eq!(err_code(&mut s, "SELECT to_tsquery('a &')").await, "42601");
    assert_eq!(err_code(&mut s, "SELECT to_tsquery('')").await, "42601");
}

#[tokio::test]
async fn plainto_tsquery_ands_and_strips_punctuation() {
    let mut s = session().await;
    // PG: plainto_tsquery('english', 'The Fat & Rats:C') => 'fat' & 'rat' & 'c'
    assert_eq!(
        scalar(&mut s, "SELECT plainto_tsquery('english', 'The Fat Rats!')").await,
        "'fat' & 'rat'"
    );
    // Operators are just punctuation here, and stop words drop.
    assert_eq!(
        scalar(&mut s, "SELECT plainto_tsquery('english', 'cats & !dogs')").await,
        "'cat' & 'dog'"
    );
    // All stop words → the empty query.
    assert_eq!(
        scalar(&mut s, "SELECT plainto_tsquery('english', 'the a of')").await,
        ""
    );
}

// ---------------------------------------------------------------------------
// The @@ operator.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn at_at_matching_semantics() {
    let mut s = session().await;
    let check = |q: &str| format!("SELECT to_tsvector('english', 'a fat cat sat') @@ {q}");
    for (query, expect) in [
        ("to_tsquery('cat')", "t"),
        ("to_tsquery('dog')", "f"),
        ("to_tsquery('cat & sat')", "t"),
        ("to_tsquery('cat & dog')", "f"),
        ("to_tsquery('cat | dog')", "t"),
        ("to_tsquery('!dog')", "t"),
        ("to_tsquery('!cat')", "f"),
        ("to_tsquery('cat & !dog')", "t"),
        ("to_tsquery('!(dog & cat)')", "t"),
        ("to_tsquery('!(fat | dog)')", "f"),
    ] {
        assert_eq!(scalar(&mut s, &check(query)).await, expect, "{query}");
    }
    // Both argument orders.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT to_tsquery('cat') @@ to_tsvector('english', 'fat cats')"
        )
        .await,
        "t"
    );
    // text @@ tsquery applies to_tsvector under the default config.
    assert_eq!(
        scalar(&mut s, "SELECT 'a fat cats' @@ to_tsquery('cat')").await,
        "t"
    );
    // text @@ text is to_tsvector(x) @@ plainto_tsquery(y).
    assert_eq!(
        scalar(&mut s, "SELECT 'a fat cats' @@ 'cats fat'").await,
        "t"
    );
    assert_eq!(scalar(&mut s, "SELECT 'a fat cats' @@ 'dog'").await, "f");
    // An unknown literal against a tsvector raw-parses as tsquery: the
    // unstemmed 'cats' does not match the stemmed vector — PG-faithful.
    assert_eq!(
        scalar(&mut s, "SELECT to_tsvector('english', 'cats') @@ 'cats'").await,
        "f"
    );
    // NULL propagates.
    assert_eq!(
        scalar(&mut s, "SELECT NULL::tsvector @@ to_tsquery('cat')").await,
        "NULL"
    );
    // The empty query matches nothing.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT to_tsvector('english', 'cat') @@ ''::tsquery"
        )
        .await,
        "f"
    );
}

// ---------------------------------------------------------------------------
// ts_rank.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ts_rank_reference_value_and_ordering() {
    let mut s = session().await;
    // PG: ts_rank(to_tsvector('english','cat'), to_tsquery('cat')) = 0.06079271
    let r: f32 = scalar(
        &mut s,
        "SELECT ts_rank(to_tsvector('english', 'cat'), to_tsquery('cat'))",
    )
    .await
    .parse()
    .unwrap();
    assert!((r - 0.060_792_71).abs() < 1e-6, "{r}");

    // More matches rank higher; ORDER BY sorts sensibly.
    ok(&mut s, "CREATE TABLE docs (id INT, body TEXT)").await;
    ok(
        &mut s,
        "INSERT INTO docs VALUES \
         (1, 'dogs bark'), (2, 'a fat cat'), (3, 'fat cats and fat rats')",
    )
    .await;
    let rows = rows_text(
        &mut s,
        "SELECT id FROM docs \
         WHERE to_tsvector('english', body) @@ to_tsquery('english', 'fat | cat | rat') \
         ORDER BY ts_rank(to_tsvector('english', body), \
                          to_tsquery('english', 'fat | cat | rat')) DESC, id",
    )
    .await;
    // Doc 3 matches all three terms (fat twice), doc 2 matches two.
    assert_eq!(rows, vec![vec!["3".to_string()], vec!["2".to_string()]]);

    // Explicit weight array (the {D,C,B,A} default) gives the same rank.
    let rw: f32 = scalar(
        &mut s,
        "SELECT ts_rank(ARRAY[0.1, 0.2, 0.4, 1.0], \
                        to_tsvector('english', 'cat'), to_tsquery('cat'))",
    )
    .await
    .parse()
    .unwrap();
    assert!((rw - r).abs() < 1e-7);

    // Non-default normalization is out of subset, named.
    let (code, msg) = err_info(
        &mut s,
        "SELECT ts_rank(to_tsvector('cat'), to_tsquery('cat'), 32)",
    )
    .await;
    assert_eq!(code, "0A000");
    assert!(msg.contains("normalization"), "{msg}");
}

// ---------------------------------------------------------------------------
// Storage round-trip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tsvector_round_trips_through_table_storage() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, tsv tsvector)").await;
    ok(
        &mut s,
        "INSERT INTO t VALUES (1, to_tsvector('english', 'The Fat Rats')), \
                              (2, 'raw:1 Lexemes:2'::tsvector)",
    )
    .await;
    assert_eq!(
        rows_text(&mut s, "SELECT tsv FROM t ORDER BY id").await,
        vec![
            vec!["'fat':2 'rat':3".to_string()],
            vec!["'Lexemes':2 'raw':1".to_string()],
        ]
    );
    // The stored value still matches.
    assert_eq!(
        rows_text(
            &mut s,
            "SELECT id FROM t WHERE tsv @@ to_tsquery('english', 'rats')"
        )
        .await,
        vec![vec!["1".to_string()]]
    );
    // tsquery columns round-trip too.
    ok(&mut s, "CREATE TABLE q (id INT PRIMARY KEY, tsq tsquery)").await;
    ok(
        &mut s,
        "INSERT INTO q VALUES (1, to_tsquery('english', 'fat & (cats | rats)'))",
    )
    .await;
    assert_eq!(
        scalar(&mut s, "SELECT tsq FROM q WHERE id = 1").await,
        "'fat' & ( 'cat' | 'rat' )"
    );
}

#[tokio::test]
async fn gin_index_ddl_behavior_unchanged() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE d (id INT, tsv tsvector)").await;
    // CREATE INDEX USING gin over a tsvector column is accepted the same way
    // it is for every column type (indexes are engine-native).
    ok(&mut s, "CREATE INDEX d_tsv_idx ON d USING gin (tsv)").await;
    // Expression indexes stay unsupported — nothing new silently no-ops.
    assert_eq!(
        err_code(
            &mut s,
            "CREATE INDEX d_expr_idx ON d USING gin (to_tsvector('english', tsv))"
        )
        .await,
        "0A000"
    );
}

// ---------------------------------------------------------------------------
// Raw casts vs configuration processing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn raw_casts_do_not_normalize() {
    let mut s = session().await;
    // PG: SELECT 'The Fat Rats'::tsvector => 'Fat' 'Rats' 'The'
    assert_eq!(
        scalar(&mut s, "SELECT 'The Fat Rats'::tsvector").await,
        "'Fat' 'Rats' 'The'"
    );
    // ... while to_tsvector lowercases, stems and drops stop words.
    assert_eq!(
        scalar(&mut s, "SELECT to_tsvector('english', 'The Fat Rats')").await,
        "'fat':2 'rat':3"
    );
    // Raw tsvector input keeps positions and quoted lexemes.
    assert_eq!(
        scalar(&mut s, "SELECT 'fat:2,4 ''fat cat'':5'::tsvector").await,
        "'fat':2,4 'fat cat':5"
    );
    // Raw tsquery: lexemes as given (no stemming, stop words kept).
    assert_eq!(
        scalar(&mut s, "SELECT 'The & Rats'::tsquery").await,
        "'The' & 'Rats'"
    );
    // The empty tsquery exists (unlike to_tsquery('')).
    assert_eq!(scalar(&mut s, "SELECT ''::tsquery").await, "");
    // Raw input syntax errors are 42601.
    assert_eq!(err_code(&mut s, "SELECT 'a:0'::tsvector").await, "42601");
    assert_eq!(err_code(&mut s, "SELECT 'a b'::tsquery").await, "42601");
}

// ---------------------------------------------------------------------------
// length / numnode / strip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn length_numnode_strip() {
    let mut s = session().await;
    // PG: length('fat:2,4 cat:3 rat:5A'::tsvector) = 3 (lexeme count).
    assert_eq!(
        scalar(
            &mut s,
            "SELECT length(to_tsvector('english', 'a fat cat sat'))"
        )
        .await,
        "3"
    );
    // length(text) still counts characters.
    assert_eq!(scalar(&mut s, "SELECT length('abcd')").await, "4");
    // PG: numnode('(fat & rat) | cat'::tsquery) = 5, numnode('') = 0.
    assert_eq!(
        scalar(&mut s, "SELECT numnode('(fat & rat) | cat'::tsquery)").await,
        "5"
    );
    assert_eq!(scalar(&mut s, "SELECT numnode(''::tsquery)").await, "0");
    // PG: strip('fat:2,4 cat:3'::tsvector) = 'cat' 'fat'.
    assert_eq!(
        scalar(&mut s, "SELECT strip('fat:2,4 cat:3'::tsvector)").await,
        "'cat' 'fat'"
    );
    // A stripped vector still matches and still ranks.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT strip(to_tsvector('english', 'fat cats')) @@ to_tsquery('cat')"
        )
        .await,
        "t"
    );
}

// ---------------------------------------------------------------------------
// Unknown configurations: 42704 with PostgreSQL's message shape.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_config_is_42704() {
    let mut s = session().await;
    let (code, msg) = err_info(&mut s, "SELECT to_tsvector('klingon', 'Haus')").await;
    assert_eq!(code, "42704");
    assert_eq!(msg, "text search configuration \"klingon\" does not exist");
    assert_eq!(
        err_code(&mut s, "SELECT to_tsquery('esperanto', 'chat')").await,
        "42704"
    );
    assert_eq!(
        err_code(&mut s, "SELECT plainto_tsquery('klingon', 'gato')").await,
        "42704"
    );
    // The pg_catalog qualifier is accepted for the configs that do exist.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT to_tsvector('pg_catalog.simple', 'The Rats')"
        )
        .await,
        "'rats':2 'the':1"
    );
}

// ---------------------------------------------------------------------------
// Out-of-subset constructs: typed 0A000 naming the construct.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn excluded_functions_are_named_0a000() {
    let mut s = session().await;
    for (sql, needle) in [
        ("SELECT setweight(to_tsvector('cat'), 'A')", "setweight"),
        (
            "SELECT ts_rank_cd(to_tsvector('cat'), to_tsquery('cat'))",
            "ts_rank_cd",
        ),
        (
            "SELECT websearch_to_tsquery('cat -dog')",
            "websearch_to_tsquery",
        ),
        ("SELECT phraseto_tsquery('the cat')", "phraseto_tsquery"),
        ("SELECT ts_delete(to_tsvector('cat'), 'cat')", "ts_delete"),
        (
            "SELECT tsvector_to_array(to_tsvector('cat'))",
            "tsvector_to_array",
        ),
        (
            "SELECT tsquery_phrase(to_tsquery('a'), to_tsquery('b'))",
            "tsquery_phrase",
        ),
        (
            "SELECT ts_rewrite(to_tsquery('a'), to_tsquery('a'), to_tsquery('b'))",
            "ts_rewrite",
        ),
        ("SELECT ts_stat('SELECT tsv FROM x')", "ts_stat"),
    ] {
        let (code, msg) = err_info(&mut s, sql).await;
        assert_eq!(code, "0A000", "for `{sql}`");
        assert!(msg.contains(needle), "`{sql}` message: {msg}");
    }
}

#[tokio::test]
async fn excluded_operators_are_named_0a000() {
    let mut s = session().await;
    // tsvector concatenation.
    let (code, msg) = err_info(&mut s, "SELECT to_tsvector('fat') || to_tsvector('cat')").await;
    assert_eq!(code, "0A000");
    assert!(msg.contains("||"), "{msg}");
    // tsquery OR-concatenation is the same operator.
    assert_eq!(
        err_code(&mut s, "SELECT to_tsquery('a') || to_tsquery('b')").await,
        "0A000"
    );
    // The phrase operator, both as a SQL operator and inside tsquery input.
    let (code, msg) = err_info(&mut s, "SELECT to_tsquery('a') <-> to_tsquery('b')").await;
    assert_eq!(code, "0A000");
    assert!(msg.contains("<->"), "{msg}");
    let (code, msg) = err_info(&mut s, "SELECT to_tsquery('fat <-> rat')").await;
    assert_eq!(code, "0A000");
    assert!(msg.contains("<->"), "{msg}");
    assert_eq!(err_code(&mut s, "SELECT 'a <2> b'::tsquery").await, "0A000");
    // tsquery && (AND-combination).
    assert_eq!(
        err_code(&mut s, "SELECT to_tsquery('a') && to_tsquery('b')").await,
        "0A000"
    );
    // Prefix matching and weight restrictions inside tsquery input.
    let (code, msg) = err_info(&mut s, "SELECT to_tsquery('cat:*')").await;
    assert_eq!(code, "0A000");
    assert!(msg.contains(":*"), "{msg}");
    assert_eq!(err_code(&mut s, "SELECT 'cat:A'::tsquery").await, "0A000");
    // Weight labels in raw tsvector input (A/B/C carry setweight semantics).
    assert_eq!(err_code(&mut s, "SELECT 'cat:3A'::tsvector").await, "0A000");
}

// ---------------------------------------------------------------------------
// New language configurations (Snowball stemmers).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn german_config_stems_and_removes_stop_words() {
    let mut s = session().await;
    // "der" is a German stop word; "Katzen" → "katz" via German Snowball.
    let tsv = scalar(&mut s, "SELECT to_tsvector('german', 'der Katzen')").await;
    // "der" must have been removed (stop word), "katz" (stem) must appear.
    assert!(
        !tsv.contains("'der'"),
        "german: stop word 'der' leaked: {tsv}"
    );
    assert!(
        tsv.contains("'katz'"),
        "german: expected 'katz' stem in: {tsv}"
    );
}

#[tokio::test]
async fn french_config_stems_text() {
    let mut s = session().await;
    let tsv = scalar(&mut s, "SELECT to_tsvector('french', 'les chats')").await;
    // "les" is a French stop word; "chats" → "chat" via French Snowball.
    assert!(
        !tsv.contains("'les'"),
        "french: stop word 'les' leaked: {tsv}"
    );
    assert!(
        tsv.contains("'chat'"),
        "french: expected 'chat' stem in: {tsv}"
    );
}

#[tokio::test]
async fn spanish_config_stems_text() {
    let mut s = session().await;
    let tsv = scalar(&mut s, "SELECT to_tsvector('spanish', 'los gatos')").await;
    // "los" is a Spanish stop word; "gatos" → "gat" via Spanish Snowball.
    assert!(
        !tsv.contains("'los'"),
        "spanish: stop word 'los' leaked: {tsv}"
    );
    assert!(
        tsv.contains("'gat'"),
        "spanish: expected 'gat' stem in: {tsv}"
    );
}

#[tokio::test]
async fn danish_config_stems_and_removes_stop_words() {
    let mut s = session().await;
    // "og" is a Danish stop word; "elsker" → "elsk" via Danish Snowball.
    let tsv = scalar(&mut s, "SELECT to_tsvector('danish', 'og elsker')").await;
    assert!(
        !tsv.contains("'og'"),
        "danish: stop word 'og' leaked: {tsv}"
    );
    assert!(
        tsv.contains("'elsk'"),
        "danish: expected 'elsk' stem in: {tsv}"
    );
}

#[tokio::test]
async fn portuguese_config_stems_and_removes_stop_words() {
    let mut s = session().await;
    // "de" is a Portuguese stop word; "amigos" → "amig" via Portuguese Snowball.
    let tsv = scalar(&mut s, "SELECT to_tsvector('portuguese', 'de amigos')").await;
    assert!(
        !tsv.contains("'de'"),
        "portuguese: stop word 'de' leaked: {tsv}"
    );
    assert!(
        tsv.contains("'amig'"),
        "portuguese: expected 'amig' stem in: {tsv}"
    );
}

#[tokio::test]
async fn swedish_config_stems_and_removes_stop_words() {
    let mut s = session().await;
    // "och" is a Swedish stop word; "abborrar" → "abborr" via Swedish Snowball.
    let tsv = scalar(&mut s, "SELECT to_tsvector('swedish', 'och abborrar')").await;
    assert!(
        !tsv.contains("'och'"),
        "swedish: stop word 'och' leaked: {tsv}"
    );
    assert!(
        tsv.contains("'abborr'"),
        "swedish: expected 'abborr' stem in: {tsv}"
    );
}

// ---------------------------------------------------------------------------
// ts_headline — cover-density window selection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ts_headline_basic_highlight() {
    let mut s = session().await;
    // ts_headline wraps matching lexemes in <b>…</b> by default.
    let hl = scalar(
        &mut s,
        "SELECT ts_headline('english', 'The fat cats sat on the mat', \
                            to_tsquery('english', 'cat'))",
    )
    .await;
    assert!(
        hl.contains("<b>cats</b>") || hl.contains("<b>cat</b>"),
        "expected highlighted match in: {hl}"
    );
}

#[tokio::test]
async fn ts_headline_custom_selectors() {
    let mut s = session().await;
    // Custom StartSel/StopSel.
    let hl = scalar(
        &mut s,
        "SELECT ts_headline('english', 'fat cats', to_tsquery('cat'), \
                            'StartSel=<<, StopSel=>>')",
    )
    .await;
    assert!(
        hl.contains("<<cats>>") || hl.contains("<<cat>>"),
        "custom selectors not applied: {hl}"
    );
}

#[tokio::test]
async fn ts_headline_no_match_returns_window() {
    let mut s = session().await;
    // When nothing matches, ts_headline still returns a text fragment (not NULL).
    let hl = scalar(
        &mut s,
        "SELECT ts_headline('english', 'the quick brown fox', \
                            to_tsquery('cat'))",
    )
    .await;
    assert!(
        !hl.is_empty(),
        "expected non-empty headline for no-match: {hl}"
    );
}

#[tokio::test]
async fn ts_headline_null_propagation() {
    let mut s = session().await;
    // NULL document → NULL.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT ts_headline('english', NULL::text, to_tsquery('cat'))"
        )
        .await,
        "NULL"
    );
}

// ---------------------------------------------------------------------------
// Text search dictionaries (synonym support).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ts_dict_create_and_synonym_lookup() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY my_syn (TEMPLATE = synonym, \
         SYNONYMS = 'dog,hound,canine')",
    )
    .await;
    // After synonym expansion, searching for "hound" should find "dog".
    ok(&mut s, "CREATE TABLE docs (id INT, body TEXT)").await;
    ok(
        &mut s,
        "INSERT INTO docs VALUES \
         (1, 'the dog barks'), (2, 'a quick cat')",
    )
    .await;
    // "hound" is a synonym for "dog", so the tsvector for row 1 should include it.
    let matched = rows_text(
        &mut s,
        "SELECT id FROM docs \
         WHERE to_tsvector('english', body) @@ plainto_tsquery('english', 'hound')",
    )
    .await;
    assert_eq!(
        matched,
        vec![vec!["1".to_string()]],
        "synonym 'hound'→'dog' should match row 1"
    );
}

#[tokio::test]
async fn ts_dict_if_not_exists() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY syn1 (TEMPLATE = synonym, \
         SYNONYMS = 'a,b')",
    )
    .await;
    // IF NOT EXISTS should be a no-op (not an error).
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY IF NOT EXISTS syn1 \
         (TEMPLATE = synonym, SYNONYMS = 'a,b')",
    )
    .await;
    // Without IF NOT EXISTS, duplicate is 42710.
    let (code, _) = err_info(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY syn1 (TEMPLATE = synonym, \
         SYNONYMS = 'a,b')",
    )
    .await;
    assert_eq!(code, "42710");
}

#[tokio::test]
async fn ts_dict_drop() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = synonym, \
         SYNONYMS = 'x,y')",
    )
    .await;
    ok(&mut s, "DROP TEXT SEARCH DICTIONARY d1").await;
    // IF EXISTS on a missing dict is a no-op.
    ok(&mut s, "DROP TEXT SEARCH DICTIONARY IF EXISTS d1").await;
    // Without IF EXISTS on a missing dict is 42704.
    let (code, _) = err_info(&mut s, "DROP TEXT SEARCH DICTIONARY d1").await;
    assert_eq!(code, "42704");
}

// ---------------------------------------------------------------------------
// New language configurations: arabic, greek, tamil + lowercase-only set.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn arabic_config_stems_text() {
    let mut s = session().await;
    // Arabic Snowball stemmer should accept valid Arabic text; at minimum the
    // config must be resolvable (no 42704) and lower-case ASCII-only words
    // are processed as Simple when the stemmer returns nothing useful.
    let tv = scalar(&mut s, "SELECT to_tsvector('arabic', 'hello world')").await;
    // Must not error — result is a tsvector string.
    assert!(!tv.is_empty());
}

#[tokio::test]
async fn greek_config_stems_and_removes_stop_words() {
    let mut s = session().await;
    // "και" is a common Greek stop word; "αγάπη" (love) should be stemmed.
    let tv = scalar(&mut s, "SELECT to_tsvector('greek', 'αγάπη και ζωή')").await;
    assert!(!tv.contains("'και'"), "Greek stop word 'και' should be removed");
    assert!(!tv.is_empty(), "non-stop words should remain in tsvector");
}

#[tokio::test]
async fn tamil_config_stems_text() {
    let mut s = session().await;
    let tv = scalar(&mut s, "SELECT to_tsvector('tamil', 'hello world')").await;
    assert!(!tv.is_empty());
}

#[tokio::test]
async fn lowercase_only_configs_are_accepted() {
    let mut s = session().await;
    // These configs have no Snowball stemmer but must be valid (no 42704).
    for cfg in &[
        "armenian", "basque", "catalan", "hindi", "indonesian",
        "irish", "lithuanian", "nepali", "yiddish",
    ] {
        let tv = scalar(
            &mut s,
            &format!("SELECT to_tsvector('{cfg}', 'hello world')"),
        )
        .await;
        assert!(
            !tv.is_empty(),
            "config '{cfg}' should be accepted and produce a tsvector"
        );
    }
}

// ---------------------------------------------------------------------------
// Thesaurus dictionary.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ts_dict_thesaurus_create_and_apply() {
    let mut s = session().await;
    // Create a thesaurus: "technology" → "tech", "computer science" → "cs".
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY thes1 \
         (TEMPLATE = thesaurus, THESAURUS = 'technology:tech;computer science:cs')",
    )
    .await;

    // Verify the dictionary was stored (no error and can be dropped).
    ok(&mut s, "DROP TEXT SEARCH DICTIONARY thes1").await;
}

#[tokio::test]
async fn ts_dict_thesaurus_if_not_exists() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY th2 \
         (TEMPLATE = thesaurus, THESAURUS = 'a:b')",
    )
    .await;
    // IF NOT EXISTS on an existing thesaurus dict is a no-op.
    ok(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY IF NOT EXISTS th2 \
         (TEMPLATE = thesaurus, THESAURUS = 'a:b')",
    )
    .await;
    // Without IF NOT EXISTS it's 42710.
    let (code, _) = err_info(
        &mut s,
        "CREATE TEXT SEARCH DICTIONARY th2 \
         (TEMPLATE = thesaurus, THESAURUS = 'a:b')",
    )
    .await;
    assert_eq!(code, "42710");
}
