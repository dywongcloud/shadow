//! Error types for the relational layer, each carrying a PostgreSQL SQLSTATE code.
//!
//! SQLSTATE codes follow the PostgreSQL error code catalogue so that wire-protocol
//! clients (psql, node-postgres, TypeORM) observe the same `code` field they would
//! against a real PostgreSQL server.

use thiserror::Error;

/// A relational-layer error. Every variant maps to a stable 5-character SQLSTATE.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RelError {
    #[error("relation \"{0}\" does not exist")]
    UndefinedTable(String),

    #[error("relation \"{0}\" already exists")]
    DuplicateTable(String),

    #[error("column \"{0}\" does not exist")]
    UndefinedColumn(String),

    #[error("column \"{0}\" of relation \"{1}\" already exists")]
    DuplicateColumn(String, String),

    #[error("schema \"{0}\" does not exist")]
    UndefinedSchema(String),

    #[error("schema \"{0}\" already exists")]
    DuplicateSchema(String),

    #[error("index \"{0}\" does not exist")]
    UndefinedIndex(String),

    #[error("relation \"{0}\" already exists")]
    DuplicateIndex(String),

    #[error("duplicate key value violates unique constraint \"{constraint}\"")]
    UniqueViolation { constraint: String, detail: String },

    #[error(
        "null value in column \"{column}\" of relation \"{table}\" violates not-null constraint"
    )]
    NotNullViolation { column: String, table: String },

    #[error(
        "insert or update on table \"{table}\" violates foreign key constraint \"{constraint}\""
    )]
    ForeignKeyViolation {
        table: String,
        constraint: String,
        detail: String,
    },

    #[error(
        "update or delete on table \"{table}\" violates foreign key constraint \"{constraint}\" on table \"{referencing}\""
    )]
    ForeignKeyViolationReferenced {
        table: String,
        constraint: String,
        referencing: String,
        detail: String,
    },

    #[error("cannot drop {object} because other objects depend on it")]
    DependentObjectsStillExist { object: String, detail: String },

    #[error("new row for relation \"{table}\" violates check constraint \"{constraint}\"")]
    CheckViolation { table: String, constraint: String },

    #[error("column \"{column}\" is of type {expected} but expression is of type {actual}")]
    DatatypeMismatch {
        column: String,
        expected: String,
        actual: String,
    },

    #[error("invalid input syntax for type {ty}: \"{value}\"")]
    InvalidTextRepresentation { ty: String, value: String },

    #[error("value out of range for type {0}")]
    NumericValueOutOfRange(String),

    #[error("division by zero")]
    DivisionByZero,

    #[error("cannot cast type {from} to {to}")]
    CannotCoerce { from: String, to: String },

    #[error("type \"{0}\" does not exist")]
    UndefinedType(String),

    #[error("object \"{0}\" does not exist")]
    UndefinedObject(String),

    /// Unknown text search configuration (SQLSTATE 42704, PostgreSQL's
    /// message shape): only `simple` and `english` exist in this engine.
    #[error("text search configuration \"{0}\" does not exist")]
    UndefinedTsConfig(String),

    #[error("{0} already exists")]
    DuplicateObject(String),

    #[error("function {0} does not exist")]
    UndefinedFunction(String),

    #[error("{0}")]
    DuplicateFunction(String),

    /// Ambiguous, unqualified `DROP FUNCTION name` when more than one
    /// signature shares the name (SQLSTATE 42725).
    #[error("{0}")]
    AmbiguousFunction(String),

    /// Structurally invalid `CREATE FUNCTION` (SQLSTATE 42P13), e.g. a
    /// PL/pgSQL body whose control flow can fall off the end without a
    /// `RETURN`, or a definition missing a required clause.
    #[error("{0}")]
    InvalidFunctionDefinition(String),

    /// Structurally invalid non-function object definition (SQLSTATE 42P17),
    /// e.g. a trigger naming a function that does not return `trigger`, or a
    /// trigger `WHEN` condition referencing `OLD` on an INSERT trigger.
    #[error("{0}")]
    InvalidObjectDefinition(String),

    /// A referenced object is not in the state the operation requires
    /// (SQLSTATE 55000), e.g. `RETURN NEW` in a DELETE trigger, where the
    /// `NEW` record is not assigned (PostgreSQL raises the same code there).
    #[error("{0}")]
    ObjectNotInPrerequisiteState(String),

    #[error("syntax error: {0}")]
    Syntax(String),

    #[error("{0}")]
    FeatureNotSupported(String),

    /// Misplaced or malformed window-function usage (SQLSTATE 42P20), e.g.
    /// `OVER` in `WHERE`, or an invalid frame clause.
    #[error("{0}")]
    WindowingError(String),

    /// Invalid recursive-CTE structure (SQLSTATE 42P19), e.g. the recursive
    /// self-reference appearing more than once or inside a subquery.
    #[error("{0}")]
    InvalidRecursion(String),

    /// An uncaught PL/pgSQL `RAISE EXCEPTION` (SQLSTATE P0001, PostgreSQL's
    /// `plpgsql_raise` default for a `RAISE` with no explicit `SQLSTATE`).
    #[error("{0}")]
    RaisedException(String),

    /// `OVER` attached to a function that is neither a window function nor an
    /// aggregate (SQLSTATE 42809).
    #[error("{0}")]
    WrongObjectType(String),

    /// A statement exceeded an execution limit (SQLSTATE 54001), e.g. the
    /// `WITH RECURSIVE` iteration guard.
    #[error("{0}")]
    StatementTooComplex(String),

    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("constraint \"{0}\" is invalid")]
    InvalidConstraint(String),

    /// Permission denied (SQLSTATE 42501). Carries the full PostgreSQL-shaped
    /// message, e.g. `new row violates row-level security policy for table "t"`.
    #[error("{0}")]
    InsufficientPrivilege(String),

    #[error("deadlock detected")]
    DeadlockDetected { detail: String },

    #[error("could not obtain lock on {0}")]
    LockNotAvailable(String),

    #[error("current transaction is aborted, commands ignored until end of transaction block")]
    InFailedTransaction,

    #[error("storage error: {0}")]
    Storage(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// An error carrying an explicit SQLSTATE: either reported verbatim by
    /// the PostgreSQL sidecar runtime, or a local error re-tagged during
    /// sidecar routing (e.g. to append a routing hint without changing the
    /// code). `sqlstate` is validated to be 5 alphanumeric ASCII characters
    /// at construction.
    #[error("{message}")]
    Sidecar { sqlstate: String, message: String },
}

impl RelError {
    /// The 5-character SQLSTATE code for this error.
    pub fn sqlstate(&self) -> &str {
        match self {
            RelError::UndefinedTable(_) => "42P01",
            RelError::DuplicateTable(_) => "42P07",
            RelError::UndefinedColumn(_) => "42703",
            RelError::DuplicateColumn(_, _) => "42701",
            RelError::UndefinedSchema(_) => "3F000",
            RelError::DuplicateSchema(_) => "42P06",
            RelError::UndefinedIndex(_) => "42704",
            RelError::DuplicateIndex(_) => "42P07",
            RelError::UniqueViolation { .. } => "23505",
            RelError::NotNullViolation { .. } => "23502",
            RelError::ForeignKeyViolation { .. } => "23503",
            RelError::ForeignKeyViolationReferenced { .. } => "23503",
            RelError::DependentObjectsStillExist { .. } => "2BP01",
            RelError::CheckViolation { .. } => "23514",
            RelError::DatatypeMismatch { .. } => "42804",
            RelError::InvalidTextRepresentation { .. } => "22P02",
            RelError::NumericValueOutOfRange(_) => "22003",
            RelError::DivisionByZero => "22012",
            RelError::CannotCoerce { .. } => "42846",
            RelError::UndefinedType(_) => "42704",
            RelError::UndefinedObject(_) => "42704",
            RelError::UndefinedTsConfig(_) => "42704",
            RelError::DuplicateObject(_) => "42710",
            RelError::UndefinedFunction(_) => "42883",
            RelError::DuplicateFunction(_) => "42723",
            RelError::AmbiguousFunction(_) => "42725",
            RelError::InvalidFunctionDefinition(_) => "42P13",
            RelError::InvalidObjectDefinition(_) => "42P17",
            RelError::ObjectNotInPrerequisiteState(_) => "55000",
            RelError::Syntax(_) => "42601",
            RelError::FeatureNotSupported(_) => "0A000",
            RelError::WindowingError(_) => "42P20",
            RelError::InvalidRecursion(_) => "42P19",
            RelError::RaisedException(_) => "P0001",
            RelError::WrongObjectType(_) => "42809",
            RelError::StatementTooComplex(_) => "54001",
            RelError::InvalidParameter(_) => "22023",
            RelError::InvalidConstraint(_) => "42P10",
            RelError::InsufficientPrivilege(_) => "42501",
            RelError::DeadlockDetected { .. } => "40P01",
            RelError::LockNotAvailable(_) => "55P03",
            RelError::InFailedTransaction => "25P02",
            RelError::Storage(_) => "58030",
            RelError::Internal(_) => "XX000",
            RelError::Sidecar { sqlstate, .. } => sqlstate,
        }
    }

    /// The PostgreSQL severity for this error (always `ERROR` here).
    pub fn severity(&self) -> &'static str {
        "ERROR"
    }
}

pub type Result<T> = std::result::Result<T, RelError>;

// Maintenance note 12: documents compatibility expectations without changing runtime behavior.

// Maintenance note 24: documents compatibility expectations without changing runtime behavior.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// SQL compatibility note 11: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 11: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.
