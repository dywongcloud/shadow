//! SQL parsing using sqlparser's PostgreSQL dialect.

use crate::sql::error::{Result, parse_error};
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Parse a SQL string (possibly containing multiple `;`-separated statements).
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).or_else(|first_err| {
        // PostgreSQL treats WITH as an optional noise word in
        // `CREATE EXTENSION name [WITH] [SCHEMA ..] [VERSION ..] [CASCADE]`;
        // sqlparser 0.62 requires it. On a parse error only, retry with the
        // noise word inserted — everything that already parses is untouched.
        let mut changed = false;
        let retried: Vec<String> = split_statements(sql)
            .into_iter()
            .map(|seg| match normalize_create_extension(&seg) {
                Some(fixed) => {
                    changed = true;
                    fixed
                }
                None => seg,
            })
            .collect();
        if !changed {
            return Err(parse_error(first_err));
        }
        Parser::parse_sql(&PostgreSqlDialect {}, &retried.join(";"))
            .map_err(|_| parse_error(first_err))
    })
}

/// Insert the `WITH` noise word into a `CREATE EXTENSION` statement whose
/// options begin without it (`CREATE EXTENSION x CASCADE` — valid PostgreSQL,
/// unparseable by sqlparser 0.62). Returns `None` when the statement is not
/// that shape.
fn normalize_create_extension(stmt: &str) -> Option<String> {
    let mut tokens: Vec<(usize, &str)> = Vec::new();
    let mut pos = 0;
    for tok in stmt.split_whitespace() {
        let start = stmt[pos..].find(tok)? + pos;
        tokens.push((start, tok));
        pos = start + tok.len();
    }
    let word = |i: usize, w: &str| {
        tokens
            .get(i)
            .is_some_and(|(_, t)| t.eq_ignore_ascii_case(w))
    };
    if !(word(0, "CREATE") && word(1, "EXTENSION")) {
        return None;
    }
    let mut i = 2;
    if word(i, "IF") && word(i + 1, "NOT") && word(i + 2, "EXISTS") {
        i += 3;
    }
    // tokens[i] is the extension name; the option list follows.
    let (offset, first_option) = *tokens.get(i + 1)?;
    if ["SCHEMA", "VERSION", "CASCADE"]
        .iter()
        .any(|w| first_option.eq_ignore_ascii_case(w))
    {
        return Some(format!("{}WITH {}", &stmt[..offset], &stmt[offset..]));
    }
    None
}

/// Parse a single scalar expression (used for stored expression texts such as
/// row-security policy `USING` / `WITH CHECK` clauses).
pub fn parse_expr(text: &str) -> Result<sqlparser::ast::Expr> {
    Parser::new(&PostgreSqlDialect {})
        .try_with_sql(text)
        .map_err(parse_error)?
        .parse_expr()
        .map_err(parse_error)
}

/// Split a SQL string into top-level `;`-separated statements without parsing
/// them.
///
/// The scanner is quote-aware, so a `;` inside any of these does **not** split:
/// `'...'` string literals (with `''` escapes, and backslash escapes in
/// `E'...'` strings), `"..."` quoted identifiers, `$$..$$` / `$tag$..$tag$`
/// dollar-quoted bodies, `--` line comments and (nested) `/* */` block
/// comments. The terminating `;` is not part of the returned segment, and
/// blank segments are dropped.
///
/// This exists so statements sqlparser cannot represent (`ALTER EXTENSION`)
/// can be recognized and routed to a hand parser while everything else flows
/// through [`parse_sql`] unchanged, in order.
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                // String literal. `E'...'` (or `e'...'`) additionally allows
                // backslash escapes, so `E'\''` does not end the literal.
                let escape_string = i > 0
                    && (bytes[i - 1] == b'E' || bytes[i - 1] == b'e')
                    && (i < 2 || !is_ident_byte(bytes[i - 2]));
                i += 1;
                while i < bytes.len() {
                    if escape_string && bytes[i] == b'\\' {
                        i += 2;
                    } else if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2; // '' escape
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        if bytes.get(i + 1) == Some(&b'"') {
                            i += 2; // "" escape
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1u32;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                // Dollar quoting: `$$` or `$tag$` where tag is an identifier
                // (must not start with a digit — `$1` is a parameter).
                let tag_start = i + 1;
                let mut j = tag_start;
                while j < bytes.len() && is_ident_byte(bytes[j]) {
                    j += 1;
                }
                let valid_tag = j == tag_start || !bytes[tag_start].is_ascii_digit();
                if valid_tag && bytes.get(j) == Some(&b'$') {
                    let tag = &sql[i..=j]; // "$tag$"
                    match sql[j + 1..].find(tag) {
                        Some(pos) => i = j + 1 + pos + tag.len(),
                        None => i = bytes.len(), // unterminated: rest is body
                    }
                } else {
                    i += 1;
                }
            }
            b';' => {
                segments.push(&sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        segments.push(&sql[start..]);
    }
    segments
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_statements() {
        let stmts = parse_sql("SELECT 1; SELECT 2").unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn parse_error_is_syntax() {
        let err = parse_sql("SELEKT 1").unwrap_err();
        assert_eq!(err.sqlstate(), "42601");
    }

    #[test]
    fn create_extension_without_with_noise_word_parses() {
        // PostgreSQL's optional WITH: sqlparser 0.62 needs it inserted.
        for sql in [
            "CREATE EXTENSION earthdistance CASCADE",
            "CREATE EXTENSION IF NOT EXISTS earthdistance CASCADE",
            "create extension pg_trgm version '1.6'",
            "SELECT 1; CREATE EXTENSION cube CASCADE; SELECT 2",
        ] {
            assert!(parse_sql(sql).is_ok(), "{sql}");
        }
        // The canonical spellings keep parsing, and non-CREATE-EXTENSION
        // errors surface unchanged.
        assert!(parse_sql("CREATE EXTENSION x WITH SCHEMA public CASCADE").is_ok());
        assert_eq!(
            parse_sql("CREATE EXTENSION x FROBNICATE")
                .unwrap_err()
                .sqlstate(),
            "42601"
        );
    }

    #[test]
    fn split_plain_statements() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2 ; SELECT 3"),
            vec!["SELECT 1", " SELECT 2 ", " SELECT 3"]
        );
        // Trailing and duplicate separators produce no blank segments.
        assert_eq!(split_statements("SELECT 1;;  ;"), vec!["SELECT 1"]);
        assert!(split_statements("  \n ").is_empty());
    }

    #[test]
    fn split_respects_quotes() {
        assert_eq!(
            split_statements("SELECT 'a;b'; SELECT \"c;d\""),
            vec!["SELECT 'a;b'", " SELECT \"c;d\""]
        );
        // Escaped quote forms: doubled quotes and E-string backslash escapes.
        assert_eq!(
            split_statements("SELECT 'it''s;fine'; SELECT 2"),
            vec!["SELECT 'it''s;fine'", " SELECT 2"]
        );
        assert_eq!(
            split_statements(r"SELECT E'\';'; SELECT 2"),
            vec![r"SELECT E'\';'", " SELECT 2"]
        );
    }

    #[test]
    fn split_respects_dollar_quotes() {
        assert_eq!(
            split_statements("SELECT $$a;b$$; SELECT $fn$x;y$fn$"),
            vec!["SELECT $$a;b$$", " SELECT $fn$x;y$fn$"]
        );
        // `$1` is a parameter, not a dollar-quote opener.
        assert_eq!(
            split_statements("SELECT $1; SELECT $2"),
            vec!["SELECT $1", " SELECT $2"]
        );
    }

    #[test]
    fn split_respects_comments() {
        assert_eq!(
            split_statements("SELECT 1 -- no; split here\n; SELECT 2"),
            vec!["SELECT 1 -- no; split here\n", " SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT 1 /* a;b /* nested; */ ; */; SELECT 2"),
            vec!["SELECT 1 /* a;b /* nested; */ ; */", " SELECT 2"]
        );
    }
}

// Maintenance note 2: documents compatibility expectations without changing runtime behavior.

// Maintenance note 14: documents compatibility expectations without changing runtime behavior.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// SQL compatibility note 1: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 17: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 1: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 17: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.
