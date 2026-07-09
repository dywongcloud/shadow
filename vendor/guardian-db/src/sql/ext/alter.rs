//! `ALTER EXTENSION` — recognized and hand-parsed by the session, because
//! sqlparser 0.62 has no `AlterExtension` AST node.
//!
//! [`crate::sql::engine::Session::execute`] splits its input into top-level
//! statements ([`crate::sql::parser::split_statements`]) and routes segments
//! whose first two keywords are `ALTER EXTENSION` here; every other segment
//! goes through the general parser unchanged. Supported grammar:
//!
//! ```text
//! ALTER EXTENSION name UPDATE [ TO 'version' ]
//! ALTER EXTENSION name SET SCHEMA schema
//! ALTER EXTENSION name ADD  member_object
//! ALTER EXTENSION name DROP member_object
//! ```

use crate::sql::error::{Result, SqlError};

/// A parsed `ALTER EXTENSION` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterExtension {
    /// Extension name (unquoted names fold to lower case, like PostgreSQL).
    pub name: String,
    pub action: AlterExtensionAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterExtensionAction {
    /// `UPDATE [TO 'version']`.
    Update { to: Option<String> },
    /// `SET SCHEMA name`.
    SetSchema(String),
    /// `ADD member_object` (raw object text, e.g. `FUNCTION f(text)`).
    Add(String),
    /// `DROP member_object`.
    Drop(String),
}

/// Whether a statement's first two keywords are `ALTER EXTENSION` (skipping
/// leading whitespace and comments).
pub fn is_alter_extension(sql: &str) -> bool {
    let mut lx = Lexer::new(sql);
    matches!(lx.next_token(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("alter"))
        && matches!(lx.next_token(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("extension"))
}

/// Hand-parse one `ALTER EXTENSION` statement (no trailing `;`).
pub fn parse_alter_extension(sql: &str) -> Result<AlterExtension> {
    let mut lx = Lexer::new(sql.trim().trim_end_matches(';'));
    expect_keyword(&mut lx, "ALTER")?;
    expect_keyword(&mut lx, "EXTENSION")?;
    let name = expect_identifier(&mut lx, "extension name")?;
    let action = match lx.next_token() {
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("update") => match lx.next_token() {
            None => AlterExtensionAction::Update { to: None },
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("to") => {
                let to = match lx.next_token() {
                    Some(Token::Str(v)) | Some(Token::Quoted(v)) | Some(Token::Word(v)) => v,
                    None => return Err(syntax("expected a version after UPDATE TO")),
                };
                expect_end(&mut lx)?;
                AlterExtensionAction::Update { to: Some(to) }
            }
            Some(other) => {
                return Err(syntax(format!(
                    "expected TO after UPDATE, found \"{}\"",
                    other.text()
                )));
            }
        },
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("set") => {
            expect_keyword(&mut lx, "SCHEMA")?;
            let schema = expect_identifier(&mut lx, "schema name")?;
            expect_end(&mut lx)?;
            AlterExtensionAction::SetSchema(schema)
        }
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("add") => {
            AlterExtensionAction::Add(expect_member_object(&mut lx, "ADD")?)
        }
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("drop") => {
            AlterExtensionAction::Drop(expect_member_object(&mut lx, "DROP")?)
        }
        Some(other) => {
            return Err(syntax(format!(
                "expected UPDATE, SET SCHEMA, ADD or DROP after ALTER EXTENSION \"{name}\", \
                 found \"{}\"",
                other.text()
            )));
        }
        None => {
            return Err(syntax(format!(
                "expected UPDATE, SET SCHEMA, ADD or DROP after ALTER EXTENSION \"{name}\""
            )));
        }
    };
    Ok(AlterExtension { name, action })
}

fn syntax(msg: impl Into<String>) -> SqlError {
    SqlError::Syntax(msg.into())
}

fn expect_keyword(lx: &mut Lexer, kw: &str) -> Result<()> {
    match lx.next_token() {
        Some(Token::Word(w)) if w.eq_ignore_ascii_case(kw) => Ok(()),
        Some(other) => Err(syntax(format!("expected {kw}, found \"{}\"", other.text()))),
        None => Err(syntax(format!("expected {kw}"))),
    }
}

/// An identifier: unquoted words fold to lower case, quoted stay verbatim.
fn expect_identifier(lx: &mut Lexer, what: &str) -> Result<String> {
    match lx.next_token() {
        Some(Token::Word(w)) => Ok(w.to_ascii_lowercase()),
        Some(Token::Quoted(q)) => Ok(q),
        Some(Token::Str(s)) => Err(syntax(format!("expected {what}, found string '{s}'"))),
        None => Err(syntax(format!("expected {what}"))),
    }
}

/// The raw member-object text after `ADD`/`DROP` (kept verbatim for the
/// feature-not-supported message).
fn expect_member_object(lx: &mut Lexer, verb: &str) -> Result<String> {
    let rest = lx.rest().trim();
    if rest.is_empty() {
        return Err(syntax(format!("expected a member object after {verb}")));
    }
    Ok(rest.to_string())
}

fn expect_end(lx: &mut Lexer) -> Result<()> {
    match lx.next_token() {
        None => Ok(()),
        Some(tok) => Err(syntax(format!(
            "unexpected token at end of ALTER EXTENSION: \"{}\"",
            tok.text()
        ))),
    }
}

// ---------------------------------------------------------------------------
// A tiny quote-and-comment-aware lexer (only what ALTER EXTENSION needs).
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Token {
    /// Unquoted word (identifier or keyword; original case).
    Word(String),
    /// `"quoted identifier"` (escapes resolved).
    Quoted(String),
    /// `'string literal'` (escapes resolved).
    Str(String),
}

impl Token {
    fn text(&self) -> &str {
        match self {
            Token::Word(s) | Token::Quoted(s) | Token::Str(s) => s,
        }
    }
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    /// Remaining input (after whitespace/comments).
    fn rest(&mut self) -> &'a str {
        self.skip_ws_and_comments();
        &self.src[self.pos..]
    }

    fn skip_ws_and_comments(&mut self) {
        let bytes = self.src.as_bytes();
        loop {
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if bytes.get(self.pos) == Some(&b'-') && bytes.get(self.pos + 1) == Some(&b'-') {
                while self.pos < bytes.len() && bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            if bytes.get(self.pos) == Some(&b'/') && bytes.get(self.pos + 1) == Some(&b'*') {
                let mut depth = 1u32;
                self.pos += 2;
                while self.pos < bytes.len() && depth > 0 {
                    if bytes[self.pos] == b'/' && bytes.get(self.pos + 1) == Some(&b'*') {
                        depth += 1;
                        self.pos += 2;
                    } else if bytes[self.pos] == b'*' && bytes.get(self.pos + 1) == Some(&b'/') {
                        depth -= 1;
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                    }
                }
                continue;
            }
            break;
        }
    }

    fn next_token(&mut self) -> Option<Token> {
        self.skip_ws_and_comments();
        let bytes = self.src.as_bytes();
        let c = *bytes.get(self.pos)?;
        match c {
            b'"' => Some(Token::Quoted(self.take_quoted(b'"'))),
            b'\'' => Some(Token::Str(self.take_quoted(b'\''))),
            _ if is_word_byte(c) => {
                let start = self.pos;
                while self.pos < bytes.len() && is_word_byte(bytes[self.pos]) {
                    self.pos += 1;
                }
                Some(Token::Word(self.src[start..self.pos].to_string()))
            }
            _ => {
                // Single punctuation character (e.g. `(`), returned verbatim.
                self.pos += 1;
                Some(Token::Word(self.src[self.pos - 1..self.pos].to_string()))
            }
        }
    }

    /// Consume a `quote`-delimited token, resolving doubled-quote escapes.
    fn take_quoted(&mut self, quote: u8) -> String {
        let bytes = self.src.as_bytes();
        self.pos += 1; // opening quote
        let mut out = String::new();
        while self.pos < bytes.len() {
            if bytes[self.pos] == quote {
                if bytes.get(self.pos + 1) == Some(&quote) {
                    out.push(quote as char);
                    self.pos += 2;
                } else {
                    self.pos += 1;
                    break;
                }
            } else {
                out.push(self.src[self.pos..].chars().next().unwrap());
                self.pos += self.src[self.pos..].chars().next().unwrap().len_utf8();
            }
        }
        out
    }
}

/// Characters allowed in an unquoted word: identifier characters plus `.`
/// (for bare versions like `1.6`).
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_alter_extension() {
        assert!(is_alter_extension("ALTER EXTENSION pg_trgm UPDATE"));
        assert!(is_alter_extension("  alter\n/* c */ extension x update"));
        assert!(is_alter_extension("-- lead\nALTER EXTENSION x UPDATE"));
        assert!(!is_alter_extension("ALTER TABLE t ADD COLUMN c INT"));
        assert!(!is_alter_extension("CREATE EXTENSION pg_trgm"));
        assert!(!is_alter_extension("SELECT 'ALTER EXTENSION'"));
    }

    #[test]
    fn parses_update_forms() {
        let cmd = parse_alter_extension("ALTER EXTENSION pg_trgm UPDATE").unwrap();
        assert_eq!(cmd.name, "pg_trgm");
        assert_eq!(cmd.action, AlterExtensionAction::Update { to: None });

        let cmd = parse_alter_extension("ALTER EXTENSION PG_TRGM UPDATE TO '1.6';").unwrap();
        assert_eq!(cmd.name, "pg_trgm");
        assert_eq!(
            cmd.action,
            AlterExtensionAction::Update {
                to: Some("1.6".into())
            }
        );

        // Bare and quoted-identifier versions are accepted too.
        let cmd = parse_alter_extension("ALTER EXTENSION \"uuid-ossp\" UPDATE TO 1.1").unwrap();
        assert_eq!(cmd.name, "uuid-ossp");
        assert_eq!(
            cmd.action,
            AlterExtensionAction::Update {
                to: Some("1.1".into())
            }
        );
    }

    #[test]
    fn parses_set_schema_add_drop() {
        let cmd = parse_alter_extension("ALTER EXTENSION citext SET SCHEMA util").unwrap();
        assert_eq!(cmd.action, AlterExtensionAction::SetSchema("util".into()));

        let cmd = parse_alter_extension("ALTER EXTENSION citext ADD FUNCTION f(text)").unwrap();
        assert_eq!(
            cmd.action,
            AlterExtensionAction::Add("FUNCTION f(text)".into())
        );

        let cmd = parse_alter_extension("ALTER EXTENSION citext DROP TYPE citext").unwrap();
        assert_eq!(cmd.action, AlterExtensionAction::Drop("TYPE citext".into()));
    }

    #[test]
    fn rejects_malformed_statements() {
        for bad in [
            "ALTER EXTENSION",
            "ALTER EXTENSION pg_trgm",
            "ALTER EXTENSION pg_trgm FROBNICATE",
            "ALTER EXTENSION pg_trgm UPDATE TO",
            "ALTER EXTENSION pg_trgm UPDATE TO '1.6' EXTRA",
            "ALTER EXTENSION pg_trgm SET SCHEMA",
            "ALTER EXTENSION pg_trgm ADD",
        ] {
            let err = parse_alter_extension(bad).unwrap_err();
            assert_eq!(err.sqlstate(), "42601", "for `{bad}`: {err}");
        }
    }
}
