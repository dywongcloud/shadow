//! Output helpers — colorized JSON, and multi-color human tables for list
//! commands. Color is gated by `owo-colors`' `if_supports_color`, which
//! degrades to plain text automatically under `NO_COLOR`, `TERM=dumb`, or a
//! non-tty stream (piped output) — never applied in `--json` mode, which
//! stays byte-plain so it pipes cleanly into `jq`.

use owo_colors::{OwoColorize, Stream};
use serde_json::Value;
use std::fmt::Write as _;

/// Global output mode. When `--json` is set, everything prints as raw JSON so
/// the CLI is scriptable / pipeable into `jq`.
#[derive(Clone, Copy)]
pub struct Out {
    pub json: bool,
}

impl Out {
    pub fn value(&self, v: &Value) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
            );
            return;
        }
        let mut s = String::new();
        colored_json(v, 0, &mut s);
        println!("{s}");
    }

    /// Print a table from an array of objects, picking the given columns. Falls
    /// back to colored JSON in `--json` mode or when the value isn't an array.
    pub fn table(&self, v: &Value, cols: &[(&str, &str)]) {
        if self.json {
            self.value(v);
            return;
        }
        let Some(rows) = v.as_array() else {
            self.value(v);
            return;
        };
        if rows.is_empty() {
            println!("(none)");
            return;
        }
        let headers: Vec<&str> = cols.iter().map(|(_, h)| *h).collect();
        let keys: Vec<&str> = cols.iter().map(|(k, _)| *k).collect();
        // Compute column widths from PLAIN text — color escapes must never
        // enter the width math, or alignment breaks the moment color is on.
        let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| keys.iter().map(|k| cell(row, k)).collect::<Vec<_>>())
            .collect();
        for row in &cells {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
        let pad = |s: &str, width: usize| format!("{:<width$}", s, width = width);

        let header_line = headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let padded = pad(h, widths[i]);
                format!("{}", padded.if_supports_color(Stream::Stdout, |t| t.bold()))
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{header_line}");

        let sep_line = widths
            .iter()
            .map(|w| {
                let dashes = "-".repeat(*w);
                format!(
                    "{}",
                    dashes.if_supports_color(Stream::Stdout, |t| t.dimmed())
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{sep_line}");

        for row in &cells {
            let line = row
                .iter()
                .enumerate()
                .map(|(i, c)| status_color(c, pad(c, widths[i])))
                .collect::<Vec<_>>()
                .join("  ");
            println!("{line}");
        }
        println!("\n{} row(s)", rows.len());
    }
}

/// Extract a printable cell value for a dotted key (e.g. "git.branch").
fn cell(row: &Value, key: &str) -> String {
    let mut cur = row;
    for part in key.split('.') {
        cur = match cur.get(part) {
            Some(v) => v,
            None => return "—".into(),
        };
    }
    match cur {
        Value::Null => "—".into(),
        Value::String(s) => {
            if s.is_empty() {
                "—".into()
            } else {
                strip_control_chars(s)
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// A table cell is printed as a single line of raw terminal output, so any
/// Unicode control character in a server-supplied value (ESC included) would
/// otherwise reach the terminal unescaped — a compromised/malicious backend
/// response could inject cursor moves, screen clears, or OSC sequences
/// (clipboard writes, spoofed hyperlinks) into the user's terminal via what
/// looks like an ordinary project/deployment name. Strip them; the JSON path
/// (`colored_json`) is already safe because `serde_json::to_string` escapes
/// control characters as `\u00XX` per the JSON spec.
fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

const GOOD_WORDS: &[&str] = &[
    "ready",
    "healthy",
    "true",
    "active",
    "online",
    "production",
    "success",
    "succeeded",
];
const BAD_WORDS: &[&str] = &[
    "error",
    "false",
    "unhealthy",
    "failed",
    "offline",
    "revoked",
    "deleted",
];
const NEUTRAL_WORDS: &[&str] = &["pending", "building", "preview", "unknown", "queued"];

/// Colorize an already-padded cell string by its RAW (unpadded) value's
/// meaning, matching known status/state vocabulary case-insensitively.
/// Padding trailing spaces ride along inside the color span, which is
/// visually identical to uncolored spaces.
fn status_color(raw: &str, padded: String) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    if GOOD_WORDS.contains(&lower.as_str()) {
        format!(
            "{}",
            padded.if_supports_color(Stream::Stdout, |t| t.green())
        )
    } else if BAD_WORDS.contains(&lower.as_str()) {
        format!("{}", padded.if_supports_color(Stream::Stdout, |t| t.red()))
    } else if NEUTRAL_WORDS.contains(&lower.as_str()) {
        format!(
            "{}",
            padded.if_supports_color(Stream::Stdout, |t| t.yellow())
        )
    } else {
        padded
    }
}

/// Recursive colored JSON pretty-printer: 2-space indent (matching
/// `serde_json::to_string_pretty`'s layout exactly), keys cyan, strings
/// green, numbers yellow, `true` green / `false` red, `null` dimmed.
fn colored_json(v: &Value, indent: usize, out: &mut String) {
    match v {
        Value::Null => {
            let _ = write!(
                out,
                "{}",
                "null".if_supports_color(Stream::Stdout, |t| t.dimmed())
            );
        }
        Value::Bool(true) => {
            let _ = write!(
                out,
                "{}",
                "true".if_supports_color(Stream::Stdout, |t| t.green())
            );
        }
        Value::Bool(false) => {
            let _ = write!(
                out,
                "{}",
                "false".if_supports_color(Stream::Stdout, |t| t.red())
            );
        }
        Value::Number(n) => {
            let s = n.to_string();
            let _ = write!(
                out,
                "{}",
                s.if_supports_color(Stream::Stdout, |t| t.yellow())
            );
        }
        Value::String(s) => {
            let quoted = serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"));
            let _ = write!(
                out,
                "{}",
                quoted.if_supports_color(Stream::Stdout, |t| t.green())
            );
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            let pad_in = "  ".repeat(indent + 1);
            let n = items.len();
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad_in);
                colored_json(item, indent + 1, out);
                if i + 1 < n {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let pad_in = "  ".repeat(indent + 1);
            let n = map.len();
            for (i, (k, val)) in map.iter().enumerate() {
                out.push_str(&pad_in);
                let key = serde_json::to_string(k).unwrap_or_else(|_| format!("{k:?}"));
                let _ = write!(
                    out,
                    "{}",
                    key.if_supports_color(Stream::Stdout, |t| t.cyan())
                );
                out.push_str(": ");
                colored_json(val, indent + 1, out);
                if i + 1 < n {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push('}');
        }
    }
}
