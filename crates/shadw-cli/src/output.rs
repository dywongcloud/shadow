//! Output helpers — pretty JSON, and compact human tables for list commands.

use serde_json::Value;

/// Global output mode. When `--json` is set, everything prints as raw JSON so
/// the CLI is scriptable / pipeable into `jq`.
#[derive(Clone, Copy)]
pub struct Out {
    pub json: bool,
}

impl Out {
    pub fn value(&self, v: &Value) {
        println!(
            "{}",
            serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
        );
    }

    /// Print a table from an array of objects, picking the given columns. Falls
    /// back to pretty JSON in `--json` mode or when the value isn't an array.
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
        // Compute column widths.
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
        let line = |row: &[String]| {
            row.iter()
                .enumerate()
                .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
                .collect::<Vec<_>>()
                .join("  ")
        };
        println!(
            "{}",
            line(&headers.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        );
        println!(
            "{}",
            widths
                .iter()
                .map(|w| "-".repeat(*w))
                .collect::<Vec<_>>()
                .join("  ")
        );
        for row in &cells {
            println!("{}", line(row));
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
                s.clone()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}
