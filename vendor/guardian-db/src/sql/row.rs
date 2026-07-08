//! The intermediate tuple model used by the executor.
//!
//! A [`RowSchema`] describes the columns of an intermediate result (each with an
//! optional originating table/alias and a type). A `Tuple` is a positionally
//! aligned vector of [`SqlValue`]s.

use std::collections::HashMap;

use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};

#[derive(Clone, Debug)]
pub struct FieldRef {
    /// The table or alias the column came from, if known.
    pub table: Option<String>,
    pub name: String,
    pub ty: SqlType,
}

/// O(1) column resolution schema.
///
/// `fields` is the authoritative list. The two lookup maps are acceleration
/// structures derived from it. After any direct mutation of `fields`, callers
/// must call [`RowSchema::rebuild_lookup`] to keep them in sync.
#[derive(Clone, Debug, Default)]
pub struct RowSchema {
    pub fields: Vec<FieldRef>,
    /// `name → [field indices]` — used for unqualified lookups and ambiguity
    /// detection.
    unqualified_lookup: HashMap<String, Vec<usize>>,
    /// `(table, name) → field index` — used for table-qualified lookups.
    qualified_lookup: HashMap<(String, String), usize>,
}

impl RowSchema {
    pub fn new(fields: Vec<FieldRef>) -> Self {
        let mut schema = Self {
            fields,
            unqualified_lookup: HashMap::new(),
            qualified_lookup: HashMap::new(),
        };
        schema.rebuild_lookup();
        schema
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Rebuild the O(1) lookup indexes from `self.fields`.
    ///
    /// Must be called after any direct mutation of `self.fields` (e.g. setting
    /// `f.table` or `f.name`).
    pub fn rebuild_lookup(&mut self) {
        self.unqualified_lookup.clear();
        self.qualified_lookup.clear();
        for (i, f) in self.fields.iter().enumerate() {
            self.unqualified_lookup
                .entry(f.name.clone())
                .or_default()
                .push(i);
            if let Some(t) = &f.table {
                self.qualified_lookup.insert((t.clone(), f.name.clone()), i);
            }
        }
    }

    /// Resolve a (possibly table-qualified) column reference to its index.
    pub fn resolve(&self, table: Option<&str>, column: &str) -> Result<usize> {
        match table {
            Some(t) => self
                .qualified_lookup
                .get(&(t.to_string(), column.to_string()))
                .copied()
                .ok_or_else(|| SqlError::UndefinedColumn(format!("{t}.{column}"))),
            None => match self.unqualified_lookup.get(column) {
                Some(indices) if indices.len() == 1 => Ok(indices[0]),
                Some(_) => Err(SqlError::Syntax(format!(
                    "column reference \"{column}\" is ambiguous"
                ))),
                None => Err(SqlError::UndefinedColumn(column.to_string())),
            },
        }
    }

    /// Concatenate two schemas (for joins).
    pub fn concat(&self, other: &RowSchema) -> RowSchema {
        let mut fields = self.fields.clone();
        fields.extend(other.fields.iter().cloned());
        RowSchema::new(fields)
    }
}

pub type Tuple = Vec<SqlValue>;

/// A materialized result set produced by the SELECT executor.
#[derive(Clone, Debug, Default)]
pub struct RowSet {
    pub schema: RowSchema,
    pub rows: Vec<Tuple>,
}
