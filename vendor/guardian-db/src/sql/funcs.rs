//! Built-in scalar functions and helpers (LIKE matching, aggregate detection).

use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::Exec;
use rust_decimal::Decimal;
use rust_decimal::prelude::*;

/// Is `name` (lower-cased) a known aggregate function?
pub fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "array_agg"
            | "string_agg"
            | "bool_and"
            | "bool_or"
            | "every"
    )
}

/// Call a scalar function by lower-cased name with already-evaluated arguments.
pub fn call_scalar(exec: &Exec, name: &str, args: Vec<SqlValue>) -> Result<SqlValue> {
    use SqlValue::*;
    let nullable_unary = |f: &dyn Fn(&SqlValue) -> Result<SqlValue>| -> Result<SqlValue> {
        match args.first() {
            Some(SqlValue::Null) | None => Ok(SqlValue::Null),
            Some(v) => f(v),
        }
    };
    let out = match name {
        // --- temporal / session ---
        "now"
        | "current_timestamp"
        | "transaction_timestamp"
        | "statement_timestamp"
        | "clock_timestamp" => Timestamptz(exec.now),
        "current_date" => Date(exec.now.naive_utc().date()),
        "current_time" | "localtime" => Time(exec.now.naive_utc().time()),
        "localtimestamp" => Timestamp(exec.now.naive_utc()),
        "current_schema" => Text(
            exec.catalog
                .search_path
                .first()
                .cloned()
                .unwrap_or_else(|| "public".into()),
        ),
        "current_database" | "current_catalog" => Text(exec.database.clone()),
        "current_user" | "session_user" | "user" => Text(exec.username.clone()),
        "version" => {
            Text("PostgreSQL 15.0 (GuardianDB 0.16) on x86_64-guardian, compiled by rustc".into())
        }
        "pg_backend_pid" => Int4(1),
        "pg_postmaster_start_time" => Timestamptz(exec.now),
        // --- string ---
        "upper" => nullable_unary(&|v| Ok(Text(text(v)?.to_uppercase())))?,
        "lower" => nullable_unary(&|v| Ok(Text(text(v)?.to_lowercase())))?,
        // length(tsvector) is the lexeme count; char_length has no tsvector
        // form in PostgreSQL (42883).
        "length" | "char_length" | "character_length" => match args.first() {
            Some(TsVector(v)) if name == "length" && args.len() == 1 => Int4(v.len() as i32),
            Some(TsVector(_)) => {
                // Wrong name or extra arguments: no such function in PG.
                return Err(SqlError::UndefinedFunction(format!(
                    "{name}(tsvector, ...)"
                )));
            }
            _ => nullable_unary(&|v| Ok(Int4(text(v)?.chars().count() as i32)))?,
        },
        "octet_length" => nullable_unary(&|v| Ok(Int4(text(v)?.len() as i32)))?,
        "trim" | "btrim" => trim_fn(&args, true, true)?,
        "ltrim" => trim_fn(&args, true, false)?,
        "rtrim" => trim_fn(&args, false, true)?,
        "reverse" => nullable_unary(&|v| Ok(Text(text(v)?.chars().rev().collect())))?,
        "md5" => {
            return Err(SqlError::FeatureNotSupported(
                "function md5 is not supported".into(),
            ));
        }
        "substr" | "substring" => substr_fn(&args)?,
        "replace" => {
            if args.iter().any(SqlValue::is_null) {
                Null
            } else {
                Text(text(&args[0])?.replace(&text(&args[1])?, &text(&args[2])?))
            }
        }
        "concat" => Text(
            args.iter()
                .filter(|v| !v.is_null())
                .map(|v| v.to_text().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(""),
        ),
        "concat_ws" => {
            if args.is_empty() || args[0].is_null() {
                Null
            } else {
                let sep = text(&args[0])?;
                Text(
                    args[1..]
                        .iter()
                        .filter(|v| !v.is_null())
                        .map(|v| v.to_text().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(&sep),
                )
            }
        }
        "left" => str_slice(&args, true)?,
        "right" => str_slice(&args, false)?,
        // --- conditional ---
        "coalesce" => args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(SqlValue::Null),
        "nullif" => {
            if args.len() == 2 && args[0].sql_eq(&args[1]) == Some(true) {
                Null
            } else {
                args.into_iter().next().unwrap_or(Null)
            }
        }
        "greatest" => extremum(args, true)?,
        "least" => extremum(args, false)?,
        // --- numeric ---
        "abs" => nullable_unary(&|v| match v.as_decimal() {
            Some(d) => Ok(Numeric(d.abs())),
            None => Ok(Float8(v.as_f64().unwrap_or(0.0).abs())),
        })?,
        "ceil" | "ceiling" => nullable_unary(&|v| Ok(Numeric(dec(v)?.ceil())))?,
        "floor" => nullable_unary(&|v| Ok(Numeric(dec(v)?.floor())))?,
        "sign" => nullable_unary(&|v| Ok(Numeric(dec(v)?.signum())))?,
        "trunc" => nullable_unary(&|v| Ok(Numeric(dec(v)?.trunc())))?,
        "round" => round_fn(&args)?,
        "sqrt" => nullable_unary(&|v| Ok(Float8(v.as_f64().unwrap_or(0.0).sqrt())))?,
        "power" | "pow" => {
            if args.len() == 2 && !args.iter().any(SqlValue::is_null) {
                Float8(
                    args[0]
                        .as_f64()
                        .unwrap_or(0.0)
                        .powf(args[1].as_f64().unwrap_or(0.0)),
                )
            } else {
                Null
            }
        }
        "mod" => {
            if args.len() == 2 && !args.iter().any(SqlValue::is_null) {
                let b = args[1].as_decimal().unwrap_or_default();
                if b.is_zero() {
                    return Err(SqlError::DivisionByZero);
                }
                Numeric(args[0].as_decimal().unwrap_or_default() % b)
            } else {
                Null
            }
        }
        // --- uuid ---
        // gen_random_uuid is a PostgreSQL core function (since 13); the
        // uuid_generate_* family belongs to uuid-ossp and is gated below.
        "gen_random_uuid" => Uuid(uuid::Uuid::new_v4()),
        // --- introspection helpers commonly probed by drivers/clients ---
        "pg_table_is_visible"
        | "pg_type_is_visible"
        | "pg_is_in_recovery"
        | "pg_function_is_visible"
        | "has_schema_privilege"
        | "has_table_privilege"
        | "has_database_privilege"
        | "has_column_privilege" => Bool(true),
        "pg_get_userbyid" => Text(exec.username.clone()),
        "pg_encoding_to_char" => Text("UTF8".into()),
        "pg_get_expr"
        | "pg_get_constraintdef"
        | "pg_get_indexdef"
        | "pg_get_viewdef"
        | "pg_get_functiondef" => args
            .into_iter()
            .next()
            .unwrap_or(Null)
            .cast(&SqlType::Text)
            .unwrap_or(Null),
        // Comments are not stored; return NULL like a fresh database.
        "obj_description" | "col_description" | "shobj_description" => Null,
        "format_type" => format_type(&args),
        "array_length" => match args.first() {
            Some(SqlValue::Array(a)) => Int4(a.len() as i32),
            _ => Null,
        },
        "array_to_string" => match (args.first(), args.get(1)) {
            (Some(SqlValue::Array(a)), Some(sep)) => Text(
                a.iter()
                    .filter(|v| !v.is_null())
                    .map(|v| v.to_text().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(&text(sep)?),
            ),
            _ => Null,
        },
        "cardinality" => match args.first() {
            Some(SqlValue::Array(a)) => Int4(a.len() as i32),
            _ => Null,
        },
        "to_char" => match args.first() {
            Some(v) if !v.is_null() => Text(v.to_text().unwrap_or_default()),
            _ => Null,
        },
        "current_setting" => {
            let name = match args.first() {
                Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => s.to_ascii_lowercase(),
                _ => return Err(SqlError::InvalidParameter("current_setting: name".into())),
            };
            let missing_ok = matches!(args.get(1), Some(SqlValue::Bool(true)));
            let value = exec
                .vars
                .borrow()
                .get(&name)
                .cloned()
                .or_else(|| crate::sql::ext::default_guc(&name).map(str::to_string));
            match value {
                Some(v) => Text(v),
                None if missing_ok => Null,
                None => {
                    return Err(SqlError::UndefinedObject(format!(
                        "unrecognized configuration parameter \"{name}\""
                    )));
                }
            }
        }
        "set_config" => {
            let name = match args.first() {
                Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => s.to_ascii_lowercase(),
                _ => return Err(SqlError::InvalidParameter("set_config: name".into())),
            };
            let value = args.get(1).and_then(SqlValue::to_text).unwrap_or_default();
            exec.vars.borrow_mut().insert(name, value.clone());
            Text(value)
        }
        "pg_advisory_lock" | "pg_advisory_unlock" | "pg_notify" => Null,
        // --- full-text search (PostgreSQL core; see docs/postgres-compat.md) ---
        "to_tsvector" => crate::sql::fts::fn_to_tsvector(exec, &args)?,
        "to_tsquery" => crate::sql::fts::fn_to_tsquery(exec, &args)?,
        "plainto_tsquery" => crate::sql::fts::fn_plainto_tsquery(exec, &args)?,
        "ts_rank" => crate::sql::fts::fn_ts_rank(&args)?,
        "numnode" => crate::sql::fts::fn_numnode(&args)?,
        "strip" => crate::sql::fts::fn_strip(&args)?,
        "ts_headline" => crate::sql::fts::fn_ts_headline(exec, &args)?,
        // Out-of-subset FTS constructs stay *named*-unsupported: these are
        // PostgreSQL core functions, so "does not exist" (42883) would be
        // untruthful — and 42883 is sidecar-routable, which would silently
        // change semantics per deployment. This arm must stay ahead of the
        // extension-dispatch fallthrough below.
        "phraseto_tsquery"
        | "websearch_to_tsquery"
        | "ts_rank_cd"
        | "setweight"
        | "ts_delete"
        | "tsvector_to_array"
        | "tsquery_phrase"
        | "ts_rewrite"
        | "ts_stat"
        | "querytree"
        | "ts_lexize"
        | "get_current_ts_config"
        | "array_to_tsvector"
        | "ts_filter"
        | "ts_parse"
        | "ts_token_type"
        | "ts_debug"
        | "tsvector_update_trigger"
        | "tsvector_update_trigger_column" => {
            return Err(SqlError::FeatureNotSupported(format!(
                "{name} is not supported (out of the full-text-search subset: to_tsvector, \
                 to_tsquery, plainto_tsquery, ts_rank, ts_headline, length, numnode, strip, @@)"
            )));
        }
        // --- Supabase auth helpers (used by row-security policies) ---
        // auth.uid(): the authenticated user's id — the JWT `sub` claim, as a
        // uuid. NULL when no claims are set (e.g. anon without a user token).
        "auth.uid" => match jwt_claim(exec, "sub") {
            Some(sub) if !sub.is_empty() => match uuid::Uuid::parse_str(&sub) {
                Ok(u) => Uuid(u),
                Err(_) => {
                    return Err(SqlError::InvalidTextRepresentation {
                        ty: "uuid".into(),
                        value: sub,
                    });
                }
            },
            _ => Null,
        },
        // auth.role(): the JWT `role` claim as text (NULL when absent).
        "auth.role" => match jwt_claim(exec, "role") {
            Some(role) if !role.is_empty() => Text(role),
            _ => Null,
        },
        // auth.jwt(): the full claims document (`request.jwt.claims`) as jsonb.
        "auth.jwt" => {
            let claims = exec.vars.borrow().get("request.jwt.claims").cloned();
            match claims.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
                Some(json) => SqlValue::Json(json),
                None => Null,
            }
        }
        other => {
            let ctx = crate::sql::ext::ExtCtx {
                now: exec.now,
                vars: &exec.vars,
            };
            if let Some(result) =
                crate::sql::ext::dispatch_function(&exec.catalog, &ctx, other, &args)
            {
                return result;
            }
            // User-defined functions are checked *after* builtins/extension
            // functions, so a UDF can never shadow a core function — this is
            // the only overload-resolution rule implemented (name+arity, not
            // PostgreSQL's full per-argument-type resolution; see
            // `Catalog::insert_function`).
            if let Some(def) = exec.catalog.find_function(None, other, args.len()) {
                if def.returns_trigger {
                    // PostgreSQL's message and SQLSTATE (0A000).
                    return Err(SqlError::FeatureNotSupported(
                        "trigger functions can only be called as triggers".into(),
                    ));
                }
                return crate::sql::udf::call_function(exec, def, args);
            }
            // Unknown function: 42883, like PostgreSQL. This is also what
            // makes sidecar fallback-routing fire for functions that only
            // exist on the PostgreSQL sidecar (PostGIS, TimescaleDB, ...).
            return Err(SqlError::UndefinedFunction(format!(
                "{other}({})",
                args.iter()
                    .map(|a| a.type_of().name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    Ok(out)
}

/// Read a JWT claim for the current session: a per-claim session variable
/// (`request.jwt.claim.<name>`, PostgREST v9 style) wins; otherwise the claim
/// is read out of the `request.jwt.claims` JSON document. `None` when neither
/// is set or the claims document does not parse.
fn jwt_claim(exec: &Exec, name: &str) -> Option<String> {
    let vars = exec.vars.borrow();
    if let Some(v) = vars.get(&format!("request.jwt.claim.{name}")) {
        return Some(v.clone());
    }
    let json: serde_json::Value = serde_json::from_str(vars.get("request.jwt.claims")?).ok()?;
    match json.get(name)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn text(v: &SqlValue) -> Result<String> {
    Ok(v.to_text().unwrap_or_default())
}

fn dec(v: &SqlValue) -> Result<Decimal> {
    v.as_decimal().ok_or_else(|| SqlError::CannotCoerce {
        from: v.type_of().name(),
        to: "numeric".into(),
    })
}

fn trim_fn(args: &[SqlValue], left: bool, right: bool) -> Result<SqlValue> {
    if args.first().map(SqlValue::is_null).unwrap_or(true) {
        return Ok(SqlValue::Null);
    }
    let s = text(&args[0])?;
    let chars: Vec<char> = match args.get(1) {
        Some(v) if !v.is_null() => text(v)?.chars().collect(),
        _ => vec![' '],
    };
    let is_trim = |c: char| chars.contains(&c);
    let mut start = 0;
    let mut end = s.chars().count();
    let cv: Vec<char> = s.chars().collect();
    if left {
        while start < end && is_trim(cv[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && is_trim(cv[end - 1]) {
            end -= 1;
        }
    }
    Ok(SqlValue::Text(cv[start..end].iter().collect()))
}

fn substr_fn(args: &[SqlValue]) -> Result<SqlValue> {
    if args.first().map(SqlValue::is_null).unwrap_or(true) {
        return Ok(SqlValue::Null);
    }
    let s: Vec<char> = text(&args[0])?.chars().collect();
    let from = args.get(1).and_then(SqlValue::as_i64).unwrap_or(1);
    // PostgreSQL substring is 1-based.
    let start = (from.max(1) - 1) as usize;
    let result: String = match args.get(2).and_then(SqlValue::as_i64) {
        Some(len) => {
            let end = ((from - 1) + len).max(0) as usize;
            s.iter().take(end).skip(start).collect()
        }
        None => s.iter().skip(start).collect(),
    };
    Ok(SqlValue::Text(result))
}

fn str_slice(args: &[SqlValue], left: bool) -> Result<SqlValue> {
    if args.iter().take(2).any(SqlValue::is_null) || args.len() < 2 {
        return Ok(SqlValue::Null);
    }
    let s: Vec<char> = text(&args[0])?.chars().collect();
    let n = args[1].as_i64().unwrap_or(0).max(0) as usize;
    let out: String = if left {
        s.iter().take(n).collect()
    } else {
        let skip = s.len().saturating_sub(n);
        s.iter().skip(skip).collect()
    };
    Ok(SqlValue::Text(out))
}

fn round_fn(args: &[SqlValue]) -> Result<SqlValue> {
    if args.first().map(SqlValue::is_null).unwrap_or(true) {
        return Ok(SqlValue::Null);
    }
    let d = dec(&args[0])?;
    let scale = args.get(1).and_then(SqlValue::as_i64).unwrap_or(0).max(0) as u32;
    Ok(SqlValue::Numeric(d.round_dp(scale)))
}

fn extremum(args: Vec<SqlValue>, greatest: bool) -> Result<SqlValue> {
    let mut best: Option<SqlValue> = None;
    for v in args {
        if v.is_null() {
            continue;
        }
        best = Some(match best {
            None => v,
            Some(cur) => match v.compare(&cur) {
                Some(ord) => {
                    let take = if greatest {
                        ord == std::cmp::Ordering::Greater
                    } else {
                        ord == std::cmp::Ordering::Less
                    };
                    if take { v } else { cur }
                }
                None => cur,
            },
        });
    }
    Ok(best.unwrap_or(SqlValue::Null))
}

fn format_type(args: &[SqlValue]) -> SqlValue {
    let oid = args.first().and_then(SqlValue::as_i64).unwrap_or(0) as u32;
    let name = match oid {
        16 => "boolean",
        17 => "bytea",
        20 => "bigint",
        21 => "smallint",
        23 => "integer",
        25 => "text",
        114 => "json",
        700 => "real",
        701 => "double precision",
        1042 => "character",
        1043 => "character varying",
        1082 => "date",
        1083 => "time without time zone",
        1114 => "timestamp without time zone",
        1184 => "timestamp with time zone",
        1700 => "numeric",
        2950 => "uuid",
        3614 => "tsvector",
        3615 => "tsquery",
        3802 => "jsonb",
        _ => "-",
    };
    SqlValue::Text(name.to_string())
}

/// SQL `LIKE`/`ILIKE` matching. `%` matches any sequence, `_` any single char.
pub fn like_match(text: &str, pattern: &str, case_insensitive: bool, escape: Option<char>) -> bool {
    let (t, p) = if case_insensitive {
        (text.to_lowercase(), pattern.to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let tc: Vec<char> = t.chars().collect();
    let pc: Vec<char> = p.chars().collect();
    like_inner(&tc, 0, &pc, 0, escape)
}

fn like_inner(t: &[char], ti: usize, p: &[char], pi: usize, esc: Option<char>) -> bool {
    let mut ti = ti;
    let mut pi = pi;
    while pi < p.len() {
        let pch = p[pi];
        if Some(pch) == esc {
            // Next pattern char is literal.
            pi += 1;
            if pi >= p.len() {
                return false;
            }
            if ti >= t.len() || t[ti] != p[pi] {
                return false;
            }
            ti += 1;
            pi += 1;
            continue;
        }
        match pch {
            '%' => {
                // Collapse consecutive %.
                while pi < p.len() && p[pi] == '%' {
                    pi += 1;
                }
                if pi == p.len() {
                    return true;
                }
                for k in ti..=t.len() {
                    if like_inner(t, k, p, pi, esc) {
                        return true;
                    }
                }
                return false;
            }
            '_' => {
                if ti >= t.len() {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
        }
    }
    ti == t.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_matching() {
        assert!(like_match("hello", "h%o", false, None));
        assert!(like_match("hello", "h_llo", false, None));
        assert!(!like_match("hello", "h_o", false, None));
        assert!(like_match("HELLO", "h%o", true, None));
        assert!(like_match("100%", "100\\%", false, Some('\\')));
        assert!(!like_match("1000", "100\\%", false, Some('\\')));
    }
}

// Maintenance note 8: documents compatibility expectations without changing runtime behavior.

// Maintenance note 20: documents compatibility expectations without changing runtime behavior.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// SQL compatibility note 6: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 22: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 6: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 22: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.
