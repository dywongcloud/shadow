//! PostgreSQL extension support: the `CREATE EXTENSION` mechanism and the
//! native implementations of the supported extension set.
//!
//! GuardianDB's SQL engine is a from-scratch Rust engine, so PostgreSQL's
//! binary extension ABI (C shared libraries loaded into the server) cannot
//! apply. Instead, a fixed registry of extensions is implemented natively.
//! `CREATE EXTENSION` flips a per-database catalog flag (persisted in the
//! replicated catalog document, so installs replicate like any other DDL) and
//! gates the extension's functions, operators, and types. Anything not in the
//! registry fails `CREATE EXTENSION` with a typed error naming
//! `pg_available_extensions` — never silently.
//!
//! Registry lookups are linear over a fixed, single-digit-sized set; every
//! function-dispatch miss costs one short scan, which is noise next to the
//! per-statement table loads.

pub mod alter;
pub mod citext;
pub mod cube;
pub mod earthdistance;
pub mod fuzzystrmatch;
pub mod hstore;
pub mod intarray;
pub mod ltree;
pub mod pg_trgm;
pub mod pgcrypto;
pub mod sidecar;
pub mod unaccent;
pub mod uuid_ossp;
pub mod vector;

use crate::relational::catalog::Catalog;
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use chrono::{DateTime, Utc};
use std::cell::RefCell;
use std::collections::HashMap;

/// A configuration variable owned by an extension (set via `SET name = value`).
pub struct GucSpec {
    pub name: &'static str,
    pub default: &'static str,
}

/// Context passed to extension function calls.
pub struct ExtCtx<'a> {
    /// Statement timestamp (same instant `now()` reports).
    pub now: DateTime<Utc>,
    /// Session variables (`SET x.y = v`); extensions read their GUCs and may
    /// write them (e.g. `set_limit`). Written values persist for the session.
    pub vars: &'a RefCell<HashMap<String, String>>,
}

impl ExtCtx<'_> {
    /// Read a session variable, falling back to the registered GUC default.
    pub fn get_var(&self, name: &str) -> Option<String> {
        if let Some(v) = self.vars.borrow().get(name) {
            return Some(v.clone());
        }
        default_guc(name).map(str::to_string)
    }

    pub fn set_var(&self, name: &str, value: impl Into<String>) {
        self.vars
            .borrow_mut()
            .insert(name.to_string(), value.into());
    }

    /// Read a float-valued GUC with a hard fallback.
    pub fn get_f32(&self, name: &str, fallback: f32) -> f32 {
        self.get_var(name)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(fallback)
    }
}

type CallFn = fn(&ExtCtx, &str, &[SqlValue]) -> Result<SqlValue>;

/// How an extension's objects are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStrategy {
    /// Implemented natively inside the GuardianDB engine.
    Native,
    /// Delegated to a managed PostgreSQL sidecar process: `CREATE EXTENSION`
    /// and the statements that use the extension's objects are forwarded over
    /// the wire protocol (see [`sidecar`]).
    SidecarPostgres,
}

/// A registry extension: natively implemented, an accepted no-op shim, or a
/// sidecar-routed PostgreSQL extension.
pub struct ExtensionDef {
    pub name: &'static str,
    pub default_version: &'static str,
    pub comment: &'static str,
    /// Extensions that must be installed first (`CASCADE` installs them).
    pub requires: &'static [&'static str],
    /// Scalar function names this extension provides (lower-case).
    pub functions: &'static [&'static str],
    /// SQL type names this extension provides (lower-case).
    pub types: &'static [&'static str],
    /// Configuration variables this extension owns.
    pub gucs: &'static [GucSpec],
    /// `pg_available_extension_versions.trusted`.
    pub trusted: bool,
    /// Function-call entry point; `None` for extensions that contribute no
    /// callable objects (index-method shims, procedural languages, and every
    /// sidecar-routed extension).
    pub call: Option<CallFn>,
    /// Where the extension runs (surfaced as the GuardianDB-specific
    /// `pg_available_extensions.runtime` column).
    pub strategy: RuntimeStrategy,
}

/// The `plpgsql` shim: PostgreSQL ships every database with it installed. Our
/// engine has no procedural language, but reporting it installed keeps ORMs
/// and migration tools (which assume its presence) working; `DROP EXTENSION
/// plpgsql` is honoured like in PostgreSQL.
static PLPGSQL: ExtensionDef = ExtensionDef {
    name: "plpgsql",
    default_version: "1.0",
    comment: "PL/pgSQL procedural language (accepted for compatibility; \
              function bodies are not executable in GuardianDB)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
    strategy: RuntimeStrategy::Native,
};

/// Index-method shims: GuardianDB indexes are engine-native, so GIN/GiST
/// operator-class extensions install as no-ops purely so migrations that
/// `CREATE EXTENSION btree_gin` succeed. `CREATE INDEX` behaviour is unchanged.
static BTREE_GIN: ExtensionDef = ExtensionDef {
    name: "btree_gin",
    default_version: "1.3",
    comment: "no-op compatibility shim (GuardianDB indexes are engine-native)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
    strategy: RuntimeStrategy::Native,
};

static BTREE_GIST: ExtensionDef = ExtensionDef {
    name: "btree_gist",
    default_version: "1.7",
    comment: "no-op compatibility shim (GuardianDB indexes are engine-native)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
    strategy: RuntimeStrategy::Native,
};

/// Sidecar-routed extensions: these cannot be reimplemented natively (C code,
/// planner hooks, background workers), so GuardianDB delegates them to a
/// managed PostgreSQL sidecar. They contribute no local functions or types;
/// the statements that use them reach the sidecar through the routing rules
/// in [`crate::sql::engine::Session`].
static PG_STAT_STATEMENTS: ExtensionDef = ExtensionDef {
    name: "pg_stat_statements",
    default_version: "1.10",
    comment: "track planning and execution statistics of all SQL statements executed \
              (runs on the PostgreSQL sidecar runtime)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: false,
    call: None,
    strategy: RuntimeStrategy::SidecarPostgres,
};

static POSTGIS: ExtensionDef = ExtensionDef {
    name: "postgis",
    default_version: "3.4.2",
    comment: "PostGIS geometry and geography spatial types and functions \
              (runs on the PostgreSQL sidecar runtime)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
    strategy: RuntimeStrategy::SidecarPostgres,
};

static TIMESCALEDB: ExtensionDef = ExtensionDef {
    name: "timescaledb",
    default_version: "2.15.2",
    comment: "Enables scalable inserts and complex queries for time-series data \
              (runs on the PostgreSQL sidecar runtime)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: false,
    call: None,
    strategy: RuntimeStrategy::SidecarPostgres,
};

/// Every extension that `CREATE EXTENSION` accepts, in `pg_available_extensions`
/// order. Names not in this list are rejected with a typed error.
static AVAILABLE: [&ExtensionDef; 18] = [
    &BTREE_GIN,
    &BTREE_GIST,
    &citext::DEF,
    &cube::DEF,
    &earthdistance::DEF,
    &fuzzystrmatch::DEF,
    &hstore::DEF,
    &intarray::DEF,
    &ltree::DEF,
    &PG_STAT_STATEMENTS,
    &pg_trgm::DEF,
    &pgcrypto::DEF,
    &PLPGSQL,
    &POSTGIS,
    &TIMESCALEDB,
    &unaccent::DEF,
    &uuid_ossp::DEF,
    &vector::DEF,
];

pub fn available() -> &'static [&'static ExtensionDef] {
    &AVAILABLE
}

/// The error for installing or using a sidecar-routed extension while no
/// sidecar DSN is configured, naming both configuration channels.
pub fn sidecar_unconfigured(name: &str) -> SqlError {
    SqlError::FeatureNotSupported(format!(
        "extension \"{name}\" runs on the PostgreSQL sidecar runtime and no sidecar is \
         configured — SET guardian.sidecar_dsn = \
         'postgres://user:pass@host:port/db?sslmode=disable' for this session, or set the \
         GUARDIAN_PG_SIDECAR_DSN environment variable"
    ))
}

pub fn find(name: &str) -> Option<&'static ExtensionDef> {
    let lower = name.to_ascii_lowercase();
    available().iter().copied().find(|d| d.name == lower)
}

/// The extension that provides scalar function `func`, if any.
pub fn function_owner(func: &str) -> Option<&'static ExtensionDef> {
    available()
        .iter()
        .copied()
        .find(|d| d.functions.contains(&func))
}

/// The GUC default registered by any extension, if `name` is a known GUC.
pub fn default_guc(name: &str) -> Option<&'static str> {
    available()
        .iter()
        .flat_map(|d| d.gucs.iter())
        .find(|g| g.name == name)
        .map(|g| g.default)
}

/// The extension name that provides SQL type `ty`, if it is extension-owned.
pub fn owning_extension(ty: &SqlType) -> Option<&'static str> {
    match ty {
        SqlType::Citext => Some("citext"),
        SqlType::Vector(_) => Some("vector"),
        SqlType::HStore => Some("hstore"),
        SqlType::Ltree => Some("ltree"),
        SqlType::Cube => Some("cube"),
        SqlType::Array(inner) => owning_extension(inner),
        _ => None,
    }
}

/// DDL gate: error (like PostgreSQL's `type "citext" does not exist`) when a
/// column uses an extension type whose extension is not installed.
pub fn check_type_usable(catalog: &Catalog, ty: &SqlType) -> Result<()> {
    if let Some(owner) = owning_extension(ty)
        && !catalog.extension_installed(owner)
    {
        return Err(SqlError::UndefinedType(format!(
            "{} — provided by extension \"{owner}\"; run CREATE EXTENSION {owner}",
            ty.name()
        )));
    }
    Ok(())
}

/// A table column whose type is provided by an extension. Drives the
/// `DROP EXTENSION ... RESTRICT` dependency check and the `pg_depend` view.
pub struct ColumnDependency {
    /// OID of the owning table (its `pg_class` object).
    pub table_oid: u32,
    /// Schema-qualified table name.
    pub table: String,
    /// 1-based column attribute number (`pg_depend.objsubid`).
    pub attnum: usize,
    /// The extension providing the column's type.
    pub extension: &'static str,
}

/// Every table column whose type is extension-owned, in catalog (schema,
/// table, ordinal) order.
pub fn column_dependencies(catalog: &Catalog) -> Vec<ColumnDependency> {
    let mut out = Vec::new();
    for table in catalog.tables() {
        for col in &table.columns {
            if let Some(extension) = owning_extension(&col.ty) {
                out.push(ColumnDependency {
                    table_oid: table.oid,
                    table: format!("{}.{}", table.schema, table.name),
                    attnum: col.ordinal + 1,
                    extension,
                });
            }
        }
    }
    out
}

/// The per-name semantics of `DROP EXTENSION` for locally-managed extensions:
/// installed check, dependent-table RESTRICT, and the explicit CASCADE
/// refusal (no implicit data destruction). Shared between the synchronous
/// executor and the session's sidecar-aware drop path. Returns whether the
/// catalog changed.
pub fn drop_native_extension(
    catalog: &mut Catalog,
    name: &str,
    if_exists: bool,
    cascade_or_restrict: Option<sqlparser::ast::ReferentialAction>,
) -> Result<bool> {
    use sqlparser::ast::ReferentialAction as RA;
    if !catalog.extension_installed(name) {
        if if_exists {
            return Ok(false);
        }
        return Err(SqlError::UndefinedObject(format!("extension \"{name}\"")));
    }
    let dependents = dependent_tables(catalog, name);
    if !dependents.is_empty() {
        if matches!(cascade_or_restrict, Some(RA::Cascade)) {
            return Err(SqlError::FeatureNotSupported(format!(
                "DROP EXTENSION {name} CASCADE would drop columns of: {} — \
                 drop or alter those tables first",
                dependents.join(", ")
            )));
        }
        return Err(SqlError::FeatureNotSupported(format!(
            "cannot drop extension {name} because other objects depend on it: {}",
            dependents.join(", ")
        )));
    }
    catalog.uninstall_extension(name);
    Ok(true)
}

/// Tables whose columns depend on `ext` (blocks `DROP EXTENSION ... RESTRICT`).
pub fn dependent_tables(catalog: &Catalog, ext: &str) -> Vec<String> {
    let mut out: Vec<String> = column_dependencies(catalog)
        .into_iter()
        .filter(|d| d.extension == ext)
        .map(|d| d.table)
        .collect();
    // Multiple dependent columns of one table are adjacent (catalog order).
    out.dedup();
    out
}

/// Scalar-function dispatch, called from the engine's unknown-function
/// fallthrough. `None` = the name belongs to no known extension (caller keeps
/// its generic error). `Some(Err)` = owned but not installed, or the call
/// itself failed.
pub fn dispatch_function(
    catalog: &Catalog,
    ctx: &ExtCtx,
    name: &str,
    args: &[SqlValue],
) -> Option<Result<SqlValue>> {
    let def = function_owner(name)?;
    if !catalog.extension_installed(def.name) {
        return Some(Err(SqlError::UndefinedFunction(format!(
            "{name}({}) — provided by extension \"{}\"; run CREATE EXTENSION \"{}\"",
            args.iter()
                .map(|a| a.type_of().name())
                .collect::<Vec<_>>()
                .join(", "),
            def.name,
            def.name
        ))));
    }
    let call = def.call?;
    Some(call(ctx, name, args))
}

/// Operator dispatch for extension-owned operators (pg_trgm's `%`/`<->`
/// family, pgvector's distances, and the hstore/intarray/ltree/cube operator
/// sets). Returns `None` when neither operand shape nor installed extensions
/// claim the operator, so the caller's normal error path applies.
/// SQL NULL semantics: any NULL operand yields NULL.
pub fn dispatch_operator(
    catalog: &Catalog,
    ctx: &ExtCtx,
    op: &str,
    left: &SqlValue,
    right: &SqlValue,
) -> Option<Result<SqlValue>> {
    let text_pair = both_text(left, right);
    let vector_pair = matches!(
        (left, right),
        (SqlValue::Vector(_), SqlValue::Vector(_))
            | (SqlValue::Vector(_), SqlValue::Null)
            | (SqlValue::Null, SqlValue::Vector(_))
            | (SqlValue::Null, SqlValue::Null)
    );
    // Operand-shape claims for the tier-2 extensions: an operator is routed
    // when at least one operand is unmistakably the extension's type (the
    // other may be NULL or a text/array literal the module coerces itself).
    let is_hstore = |v: &SqlValue| matches!(v, SqlValue::HStore(_));
    let is_ltree = |v: &SqlValue| matches!(v, SqlValue::Ltree(_));
    let is_cube = |v: &SqlValue| matches!(v, SqlValue::Cube { .. });
    let is_array = |v: &SqlValue| matches!(v, SqlValue::Array(_));
    let hstore_side = is_hstore(left) || is_hstore(right);
    let ltree_side = is_ltree(left) || is_ltree(right);
    let cube_side = is_cube(left) || is_cube(right);
    let array_side = is_array(left) || is_array(right);
    match op {
        // pg_trgm operators on text operands.
        "%" | "<%" | "%>" | "<<%" | "%>>"
            if text_pair && catalog.extension_installed("pg_trgm") =>
        {
            Some(pg_trgm::operator(ctx, op, left, right))
        }
        "<->" if text_pair && catalog.extension_installed("pg_trgm") => {
            Some(pg_trgm::operator(ctx, op, left, right))
        }
        // pgvector distance operators.
        "<->" | "<#>" | "<=>" | "<+>" if vector_pair && catalog.extension_installed("vector") => {
            Some(vector::operator(op, left, right))
        }
        // cube distance.
        "<->" if cube_side && catalog.extension_installed("cube") => {
            Some(cube::operator(op, left, right))
        }
        // hstore: key lookup, concat, exists, delete, containment.
        "->" | "||" | "?" | "-" if hstore_side && catalog.extension_installed("hstore") => {
            Some(hstore::operator(op, left, right))
        }
        // Containment and overlap route by operand type.
        "@>" | "<@" if hstore_side && catalog.extension_installed("hstore") => {
            Some(hstore::operator(op, left, right))
        }
        "@>" | "<@" if ltree_side && catalog.extension_installed("ltree") => {
            Some(ltree::operator(op, left, right))
        }
        "@>" | "<@" | "&&" if cube_side && catalog.extension_installed("cube") => {
            Some(cube::operator(op, left, right))
        }
        // intarray: overlap, containment, append/remove, union/intersection,
        // index-of. The int-element check lives in the module (CannotCoerce
        // for non-integer arrays).
        "&&" | "@>" | "<@" | "+" | "-" | "|" | "&" | "#"
            if array_side && catalog.extension_installed("intarray") =>
        {
            Some(intarray::operator(op, left, right))
        }
        // ltree lquery match (`path ~ 'a.*'` in either operand order).
        "~" if ltree_side && catalog.extension_installed("ltree") => {
            Some(ltree::operator(op, left, right))
        }
        _ => None,
    }
}

fn both_text(l: &SqlValue, r: &SqlValue) -> bool {
    let textual =
        |v: &SqlValue| matches!(v, SqlValue::Text(_) | SqlValue::Citext(_) | SqlValue::Null);
    textual(l) && textual(r)
}

/// NULL-propagation helper shared by the extension modules: if any argument is
/// SQL NULL the function result is NULL (all supported functions are strict).
pub(crate) fn any_null(args: &[SqlValue]) -> bool {
    args.iter().any(SqlValue::is_null)
}

/// Extract a text argument (Text or Citext) at `idx`.
pub(crate) fn arg_text(args: &[SqlValue], idx: usize, func: &str) -> Result<String> {
    match args.get(idx) {
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone()),
        Some(other) => other
            .to_text()
            .ok_or_else(|| bad_arg(func, idx, "text", other)),
        None => Err(SqlError::UndefinedFunction(format!(
            "{func}: missing argument {}",
            idx + 1
        ))),
    }
}

/// Extract a bytea argument at `idx` (text coerces to its UTF-8 bytes,
/// matching PostgreSQL's implicit text -> bytea behaviour in pgcrypto calls).
pub(crate) fn arg_bytes(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<u8>> {
    match args.get(idx) {
        Some(SqlValue::Bytea(b)) => Ok(b.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone().into_bytes()),
        Some(other) => Err(bad_arg(func, idx, "bytea", other)),
        None => Err(SqlError::UndefinedFunction(format!(
            "{func}: missing argument {}",
            idx + 1
        ))),
    }
}

pub(crate) fn arg_i64(args: &[SqlValue], idx: usize, func: &str) -> Result<i64> {
    args.get(idx)
        .and_then(SqlValue::as_i64)
        .ok_or_else(|| match args.get(idx) {
            Some(other) => bad_arg(func, idx, "integer", other),
            None => SqlError::UndefinedFunction(format!("{func}: missing argument {}", idx + 1)),
        })
}

pub(crate) fn arg_f64(args: &[SqlValue], idx: usize, func: &str) -> Result<f64> {
    args.get(idx)
        .and_then(SqlValue::as_f64)
        .ok_or_else(|| match args.get(idx) {
            Some(other) => bad_arg(func, idx, "double precision", other),
            None => SqlError::UndefinedFunction(format!("{func}: missing argument {}", idx + 1)),
        })
}

pub(crate) fn arg_vector(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<f32>> {
    match args.get(idx) {
        Some(SqlValue::Vector(v)) => Ok(v.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::Vector(None))? {
                SqlValue::Vector(v) => Ok(v),
                _ => unreachable!("from_text(vector) yields Vector"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "vector", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// Extract a cube argument (Cube, or text parsed as cube) at `idx` as its
/// `(ll, ur)` corners. Shared by the `cube` and `earthdistance` modules.
pub(crate) fn arg_cube(args: &[SqlValue], idx: usize, func: &str) -> Result<(Vec<f64>, Vec<f64>)> {
    match args.get(idx) {
        Some(SqlValue::Cube { ll, ur }) => Ok((ll.clone(), ur.clone())),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::Cube)? {
                SqlValue::Cube { ll, ur } => Ok((ll, ur)),
                _ => unreachable!("from_text(cube) yields Cube"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "cube", other)),
        None => Err(missing_arg(func, idx)),
    }
}

pub(crate) fn bad_arg(func: &str, idx: usize, want: &str, got: &SqlValue) -> SqlError {
    SqlError::CannotCoerce {
        from: got.type_of().name(),
        to: format!("{want} (argument {} of {func})", idx + 1),
    }
}

pub(crate) fn missing_arg(func: &str, idx: usize) -> SqlError {
    SqlError::UndefinedFunction(format!("{func}: missing argument {}", idx + 1))
}

/// Unknown sub-function name inside an extension module: internal error,
/// because `functions` and the dispatch match must stay in sync.
pub(crate) fn no_such(func: &str) -> SqlError {
    SqlError::Internal(format!("extension function {func} not routed"))
}
