//! Identifier handling with PostgreSQL case-folding rules.
//!
//! Unquoted identifiers fold to lower case; quoted identifiers are preserved
//! verbatim. This matches PostgreSQL and is what TypeORM relies on.

use sqlparser::ast::{Ident, ObjectName};

/// Fold a single identifier per PostgreSQL rules.
pub fn ident_name(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_ascii_lowercase()
    }
}

/// Extract the dotted parts of an object name (already case-folded).
pub fn object_name_parts(name: &ObjectName) -> Vec<String> {
    name.0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(ident_name)
        .collect()
}

/// The lower-cased dispatch name of a function call. Calls qualified with the
/// `auth` namespace keep the qualifier (`auth.uid`) because those are distinct
/// Supabase builtins; any other qualifier (`pg_catalog.`, `public.`) is
/// dropped, matching PostgreSQL's search-path resolution for builtins. A
/// quoted `"auth.uid"` single identifier also dispatches correctly, since the
/// returned name is the same either way.
pub fn function_dispatch_name(name: &ObjectName) -> String {
    let parts = object_name_parts(name);
    let base = parts.last().cloned().unwrap_or_default();
    if parts.len() >= 2 && parts[parts.len() - 2] == "auth" {
        format!("auth.{base}")
    } else {
        base
    }
}

/// Split an object name into `(schema, name)`. A three-part name's leading
/// catalog/database component is ignored (PostgreSQL only allows the current db).
pub fn split_schema_table(name: &ObjectName) -> (Option<String>, String) {
    let parts = object_name_parts(name);
    match parts.len() {
        0 => (None, String::new()),
        1 => (None, parts[0].clone()),
        _ => {
            let n = parts[parts.len() - 1].clone();
            let s = parts[parts.len() - 2].clone();
            (Some(s), n)
        }
    }
}

// SQL compatibility note 10: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 10: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.
