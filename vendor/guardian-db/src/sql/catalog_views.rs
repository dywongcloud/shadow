//! Virtual `information_schema` and `pg_catalog` tables synthesized from the
//! relational catalog.
//!
//! These are generated on demand whenever a query's FROM references one of them,
//! which is what TypeORM, node-postgres, psql and GUI clients rely on for schema
//! introspection, migrations and `synchronize`.

use crate::relational::catalog::Table;
use crate::relational::{Catalog, SqlType, SqlValue};
use crate::sql::error::Result;
use crate::sql::row::{FieldRef, RowSchema, RowSet, Tuple};

const DB_OID: i32 = 16000;

/// Return the rows of a known introspection view, or `None` if `name` is not one.
pub fn view_rows(
    catalog: &Catalog,
    schema: Option<&str>,
    name: &str,
    alias: &str,
) -> Result<Option<RowSet>> {
    // Resolve which catalog the view lives in.
    let in_info = schema == Some("information_schema");
    let in_pg = schema == Some("pg_catalog") || (schema.is_none() && name.starts_with("pg_"));
    let unq = schema.is_none();

    let built = match (in_info || (unq && !name.starts_with("pg_")), name) {
        (true, "tables") => Some(tables(catalog)),
        (true, "columns") => Some(columns(catalog)),
        (true, "schemata") => Some(schemata(catalog)),
        (true, "table_constraints") => Some(table_constraints(catalog)),
        (true, "key_column_usage") => Some(key_column_usage(catalog)),
        (true, "constraint_column_usage") => Some(constraint_column_usage(catalog)),
        (true, "referential_constraints") => Some(referential_constraints(catalog)),
        (true, "views") => Some(views(catalog)),
        _ => None,
    };
    let built = built.or_else(|| match (in_pg, name) {
        (true, "pg_namespace") => Some(pg_namespace(catalog)),
        (true, "pg_class") => Some(pg_class(catalog)),
        (true, "pg_attribute") => Some(pg_attribute(catalog)),
        (true, "pg_type") => Some(pg_type(catalog)),
        (true, "pg_index") => Some(pg_index(catalog)),
        (true, "pg_constraint") => Some(pg_constraint(catalog)),
        (true, "pg_database") => Some(pg_database(catalog)),
        (true, "pg_indexes") => Some(pg_indexes(catalog)),
        (true, "pg_attrdef") => Some(pg_attrdef(catalog)),
        (true, "pg_description") => Some(empty(&[
            ("objoid", SqlType::Integer),
            ("classoid", SqlType::Integer),
            ("objsubid", SqlType::Integer),
            ("description", SqlType::Text),
        ])),
        (true, "pg_enum") => Some(empty(&[
            ("oid", SqlType::Integer),
            ("enumtypid", SqlType::Integer),
            ("enumsortorder", SqlType::Real),
            ("enumlabel", SqlType::Text),
        ])),
        (true, "pg_collation") => Some(empty(&[
            ("oid", SqlType::Integer),
            ("collname", SqlType::Text),
            ("collnamespace", SqlType::Integer),
        ])),
        (true, "pg_roles") => Some(pg_roles()),
        (true, "pg_tables") => Some(pg_tables(catalog)),
        (true, "pg_policies") => Some(pg_policies(catalog)),
        (true, "pg_extension") => Some(pg_extension(catalog)),
        (true, "pg_available_extensions") => Some(pg_available_extensions(catalog)),
        (true, "pg_available_extension_versions") => Some(pg_available_extension_versions(catalog)),
        (true, "pg_proc") => Some(pg_proc(catalog)),
        (true, "pg_trigger") => Some(pg_trigger(catalog)),
        (true, "pg_depend") => Some(pg_depend(catalog)),
        (true, "pg_am") => Some(pg_am()),
        (true, "pg_settings") => Some(empty(&[
            ("name", SqlType::Text),
            ("setting", SqlType::Text),
        ])),
        (true, "pg_inherits") => Some(empty(&[
            ("inhrelid", SqlType::Integer),
            ("inhparent", SqlType::Integer),
            ("inhseqno", SqlType::Integer),
        ])),
        _ => None,
    });

    Ok(built.map(|rs| relabel(rs, alias)))
}

// ---------------------------------------------------------------------------
// information_schema
// ---------------------------------------------------------------------------

fn tables(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("table_catalog", SqlType::Text),
        ("table_schema", SqlType::Text),
        ("table_name", SqlType::Text),
        ("table_type", SqlType::Text),
        ("self_referencing_column_name", SqlType::Text),
        ("reference_generation", SqlType::Text),
        ("is_insertable_into", SqlType::Text),
        ("is_typed", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        rows.push(vec![
            t(&catalog.database),
            t(&table.schema),
            t(&table.name),
            t("BASE TABLE"),
            null(),
            null(),
            t("YES"),
            t("NO"),
        ]);
    }
    for v in catalog.views() {
        rows.push(vec![
            t(&catalog.database),
            t(&v.schema),
            t(&v.name),
            t("VIEW"),
            null(),
            null(),
            t("NO"),
            t("NO"),
        ]);
    }
    rs(cols, rows)
}

fn columns(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("table_catalog", SqlType::Text),
        ("table_schema", SqlType::Text),
        ("table_name", SqlType::Text),
        ("column_name", SqlType::Text),
        ("ordinal_position", SqlType::Integer),
        ("column_default", SqlType::Text),
        ("is_nullable", SqlType::Text),
        ("data_type", SqlType::Text),
        ("character_maximum_length", SqlType::Integer),
        ("numeric_precision", SqlType::Integer),
        ("numeric_scale", SqlType::Integer),
        ("datetime_precision", SqlType::Integer),
        ("udt_name", SqlType::Text),
        ("udt_schema", SqlType::Text),
        ("is_identity", SqlType::Text),
        ("is_generated", SqlType::Text),
        ("collation_name", SqlType::Text),
        ("is_updatable", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        for col in &table.columns {
            let (maxlen, prec, scale) = type_metrics(&col.ty);
            rows.push(vec![
                t(&catalog.database),
                t(&table.schema),
                t(&table.name),
                t(&col.name),
                i4(col.ordinal as i32 + 1),
                col.default.clone().map(SqlValue::Text).unwrap_or(null()),
                t(if col.nullable { "YES" } else { "NO" }),
                t(&col.ty.information_schema_name()),
                maxlen,
                prec,
                scale,
                if col.ty.is_temporal() { i4(6) } else { null() },
                t(&col.ty.udt_name()),
                t("pg_catalog"),
                t(if col.identity_sequence.is_some() {
                    "YES"
                } else {
                    "NO"
                }),
                t("NEVER"),
                null(),
                t("YES"),
            ]);
        }
    }
    rs(cols, rows)
}

fn schemata(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("catalog_name", SqlType::Text),
        ("schema_name", SqlType::Text),
        ("schema_owner", SqlType::Text),
        ("default_character_set_name", SqlType::Text),
        ("sql_path", SqlType::Text),
    ];
    let rows = catalog
        .schemas()
        .map(|s| {
            vec![
                t(&catalog.database),
                t(&s.name),
                t(&s.owner),
                null(),
                null(),
            ]
        })
        .collect();
    rs(cols, rows)
}

fn table_constraints(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("constraint_catalog", SqlType::Text),
        ("constraint_schema", SqlType::Text),
        ("constraint_name", SqlType::Text),
        ("table_catalog", SqlType::Text),
        ("table_schema", SqlType::Text),
        ("table_name", SqlType::Text),
        ("constraint_type", SqlType::Text),
        ("is_deferrable", SqlType::Text),
        ("initially_deferred", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        if let Some(pk) = &table.primary_key {
            rows.push(constraint_row(catalog, table, &pk.name, "PRIMARY KEY"));
        }
        for u in &table.uniques {
            let name = if u.name.is_empty() {
                format!("{}_{}_key", table.name, u.columns.join("_"))
            } else {
                u.name.clone()
            };
            rows.push(constraint_row(catalog, table, &name, "UNIQUE"));
        }
        for fk in &table.foreign_keys {
            rows.push(constraint_row(catalog, table, &fk.name, "FOREIGN KEY"));
        }
        for c in &table.checks {
            rows.push(constraint_row(catalog, table, &c.name, "CHECK"));
        }
    }
    rs(cols, rows)
}

fn constraint_row(catalog: &Catalog, table: &Table, name: &str, ctype: &str) -> Tuple {
    vec![
        t(&catalog.database),
        t(&table.schema),
        t(name),
        t(&catalog.database),
        t(&table.schema),
        t(&table.name),
        t(ctype),
        t("NO"),
        t("NO"),
    ]
}

fn key_column_usage(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("constraint_catalog", SqlType::Text),
        ("constraint_schema", SqlType::Text),
        ("constraint_name", SqlType::Text),
        ("table_catalog", SqlType::Text),
        ("table_schema", SqlType::Text),
        ("table_name", SqlType::Text),
        ("column_name", SqlType::Text),
        ("ordinal_position", SqlType::Integer),
        ("position_in_unique_constraint", SqlType::Integer),
    ];
    let mut rows = Vec::new();
    let mut push = |table: &Table, name: &str, columns: &[String]| {
        for (i, col) in columns.iter().enumerate() {
            rows.push(vec![
                t(&catalog.database),
                t(&table.schema),
                t(name),
                t(&catalog.database),
                t(&table.schema),
                t(&table.name),
                t(col),
                i4(i as i32 + 1),
                null(),
            ]);
        }
    };
    for table in catalog.tables() {
        if let Some(pk) = &table.primary_key {
            push(table, &pk.name, &pk.columns);
        }
        for u in &table.uniques {
            let name = if u.name.is_empty() {
                format!("{}_{}_key", table.name, u.columns.join("_"))
            } else {
                u.name.clone()
            };
            push(table, &name, &u.columns);
        }
        for fk in &table.foreign_keys {
            push(table, &fk.name, &fk.columns);
        }
    }
    rs(cols, rows)
}

fn constraint_column_usage(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("table_catalog", SqlType::Text),
        ("table_schema", SqlType::Text),
        ("table_name", SqlType::Text),
        ("column_name", SqlType::Text),
        ("constraint_catalog", SqlType::Text),
        ("constraint_schema", SqlType::Text),
        ("constraint_name", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        for fk in &table.foreign_keys {
            for col in &fk.ref_columns {
                rows.push(vec![
                    t(&catalog.database),
                    t(&fk.ref_schema),
                    t(&fk.ref_table),
                    t(col),
                    t(&catalog.database),
                    t(&table.schema),
                    t(&fk.name),
                ]);
            }
        }
    }
    rs(cols, rows)
}

fn referential_constraints(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("constraint_catalog", SqlType::Text),
        ("constraint_schema", SqlType::Text),
        ("constraint_name", SqlType::Text),
        ("unique_constraint_catalog", SqlType::Text),
        ("unique_constraint_schema", SqlType::Text),
        ("unique_constraint_name", SqlType::Text),
        ("match_option", SqlType::Text),
        ("update_rule", SqlType::Text),
        ("delete_rule", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        for fk in &table.foreign_keys {
            rows.push(vec![
                t(&catalog.database),
                t(&table.schema),
                t(&fk.name),
                t(&catalog.database),
                t(&fk.ref_schema),
                t(&format!("{}_pkey", fk.ref_table)),
                t("NONE"),
                t(fk.on_update.as_sql()),
                t(fk.on_delete.as_sql()),
            ]);
        }
    }
    rs(cols, rows)
}

fn views(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("table_catalog", SqlType::Text),
        ("table_schema", SqlType::Text),
        ("table_name", SqlType::Text),
        ("view_definition", SqlType::Text),
        ("check_option", SqlType::Text),
        ("is_updatable", SqlType::Text),
    ];
    let rows = catalog
        .views()
        .map(|v| {
            vec![
                t(&catalog.database),
                t(&v.schema),
                t(&v.name),
                t(&v.query),
                t("NONE"),
                t("NO"),
            ]
        })
        .collect();
    rs(cols, rows)
}

// ---------------------------------------------------------------------------
// pg_catalog
// ---------------------------------------------------------------------------

fn schema_oid(catalog: &Catalog, name: &str) -> i32 {
    catalog
        .schemas()
        .find(|s| s.name == name)
        .map(|s| s.oid as i32)
        .unwrap_or(0)
}

fn pg_namespace(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("nspname", SqlType::Text),
        ("nspowner", SqlType::Integer),
        ("nspacl", SqlType::Text),
    ];
    let rows = catalog
        .schemas()
        .map(|s| vec![i4(s.oid as i32), t(&s.name), i4(10), null()])
        .collect();
    rs(cols, rows)
}

fn pg_class(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("relname", SqlType::Text),
        ("relnamespace", SqlType::Integer),
        ("reltype", SqlType::Integer),
        ("relowner", SqlType::Integer),
        ("relam", SqlType::Integer),
        ("relkind", SqlType::Char(Some(1))),
        ("relnatts", SqlType::SmallInt),
        ("relhasindex", SqlType::Boolean),
        ("relhaspkey", SqlType::Boolean),
        ("relhasrules", SqlType::Boolean),
        ("relhastriggers", SqlType::Boolean),
        ("relrowsecurity", SqlType::Boolean),
        ("relforcerowsecurity", SqlType::Boolean),
        ("relpersistence", SqlType::Char(Some(1))),
        ("relispartition", SqlType::Boolean),
        ("reltuples", SqlType::Real),
        ("relpages", SqlType::Integer),
        ("relchecks", SqlType::SmallInt),
        ("reltablespace", SqlType::Integer),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        let nidx = catalog.indexes_for_table(&table.schema, &table.name).len();
        rows.push(vec![
            i4(table.oid as i32),
            t(&table.name),
            i4(schema_oid(catalog, &table.schema)),
            i4(0),
            i4(10),
            i4(0),
            t("r"),
            i2(table.columns.len() as i16),
            b(nidx > 0),
            b(table.primary_key.is_some()),
            b(false),
            b(false),
            b(table.rls_enabled),
            b(table.rls_forced),
            t("p"),
            b(false),
            SqlValue::Float4(table.columns.len() as f32),
            i4(0),
            i2(table.checks.len() as i16),
            i4(0),
        ]);
    }
    // Indexes also live in pg_class with relkind 'i'.
    for idx in catalog.indexes() {
        rows.push(vec![
            i4(idx.oid as i32),
            t(&idx.name),
            i4(schema_oid(catalog, &idx.schema)),
            i4(0),
            i4(10),
            i4(403),
            t("i"),
            i2(idx.columns.len() as i16),
            b(false),
            b(false),
            b(false),
            b(false),
            b(false),
            b(false),
            t("p"),
            b(false),
            SqlValue::Float4(0.0),
            i4(1),
            i2(0),
            i4(0),
        ]);
    }
    for v in catalog.views() {
        rows.push(vec![
            i4(v.oid as i32),
            t(&v.name),
            i4(schema_oid(catalog, &v.schema)),
            i4(0),
            i4(10),
            i4(0),
            t("v"),
            i2(v.columns.len() as i16),
            b(false),
            b(false),
            b(false),
            b(false),
            b(false),
            b(false),
            t("p"),
            b(false),
            SqlValue::Float4(0.0),
            i4(0),
            i2(0),
            i4(0),
        ]);
    }
    rs(cols, rows)
}

fn pg_attribute(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("attrelid", SqlType::Integer),
        ("attname", SqlType::Text),
        ("atttypid", SqlType::Integer),
        ("attstattarget", SqlType::Integer),
        ("attlen", SqlType::SmallInt),
        ("attnum", SqlType::SmallInt),
        ("attndims", SqlType::Integer),
        ("atttypmod", SqlType::Integer),
        ("attnotnull", SqlType::Boolean),
        ("atthasdef", SqlType::Boolean),
        ("attisdropped", SqlType::Boolean),
        ("attislocal", SqlType::Boolean),
        ("attidentity", SqlType::Char(Some(1))),
        ("attgenerated", SqlType::Char(Some(1))),
        ("attcollation", SqlType::Integer),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        for col in &table.columns {
            rows.push(vec![
                i4(table.oid as i32),
                t(&col.name),
                i4(col.ty.oid() as i32),
                i4(-1),
                i2(col.ty.type_len()),
                i2(col.ordinal as i16 + 1),
                i4(if matches!(col.ty, SqlType::Array(_)) {
                    1
                } else {
                    0
                }),
                i4(-1),
                b(!col.nullable),
                b(col.default.is_some()),
                b(false),
                b(true),
                t(if col.identity_sequence.is_some() {
                    "d"
                } else {
                    ""
                }),
                t(""),
                i4(0),
            ]);
        }
    }
    rs(cols, rows)
}

fn pg_type(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("typname", SqlType::Text),
        ("typnamespace", SqlType::Integer),
        ("typlen", SqlType::SmallInt),
        ("typtype", SqlType::Char(Some(1))),
        ("typcategory", SqlType::Char(Some(1))),
        ("typrelid", SqlType::Integer),
        ("typelem", SqlType::Integer),
        ("typarray", SqlType::Integer),
        ("typbasetype", SqlType::Integer),
        ("typnotnull", SqlType::Boolean),
        ("typdelim", SqlType::Char(Some(1))),
    ];
    let pg = schema_oid(catalog, "pg_catalog");
    let base = [
        (16, "bool", 1, "b"),
        (17, "bytea", -1, "b"),
        (20, "int8", 8, "N"),
        (21, "int2", 2, "N"),
        (23, "int4", 4, "N"),
        (25, "text", -1, "S"),
        (114, "json", -1, "U"),
        (700, "float4", 4, "N"),
        (701, "float8", 8, "N"),
        (1042, "bpchar", -1, "S"),
        (1043, "varchar", -1, "S"),
        (1082, "date", 4, "D"),
        (1083, "time", 8, "D"),
        (1114, "timestamp", 8, "D"),
        (1184, "timestamptz", 8, "D"),
        (1700, "numeric", -1, "N"),
        (2950, "uuid", 16, "U"),
        (3614, "tsvector", -1, "U"),
        (3615, "tsquery", -1, "U"),
        (3802, "jsonb", -1, "U"),
    ];
    let rows = base
        .iter()
        .map(|(oid, name, len, cat)| {
            vec![
                i4(*oid),
                t(name),
                i4(pg),
                i2(*len as i16),
                t("b"),
                t(cat),
                i4(0),
                i4(0),
                i4(0),
                i4(0),
                b(false),
                t(","),
            ]
        })
        .collect();
    rs(cols, rows)
}

fn pg_index(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("indexrelid", SqlType::Integer),
        ("indrelid", SqlType::Integer),
        ("indnatts", SqlType::SmallInt),
        ("indnkeyatts", SqlType::SmallInt),
        ("indisunique", SqlType::Boolean),
        ("indisprimary", SqlType::Boolean),
        ("indisclustered", SqlType::Boolean),
        ("indisvalid", SqlType::Boolean),
        ("indisready", SqlType::Boolean),
        // indkey is PostgreSQL's int2vector; we expose it as a smallint[] so that
        // `attnum = ANY(indkey)` works in introspection queries.
        ("indkey", SqlType::Array(Box::new(SqlType::SmallInt))),
        ("indpred", SqlType::Text),
        ("indexprs", SqlType::Text),
        ("indoption", SqlType::Array(Box::new(SqlType::SmallInt))),
        ("indcollation", SqlType::Array(Box::new(SqlType::Integer))),
        ("indclass", SqlType::Array(Box::new(SqlType::Integer))),
    ];
    let mut rows = Vec::new();
    for idx in catalog.indexes() {
        let table = catalog
            .resolve_table_name(Some(&idx.schema), &idx.table)
            .and_then(|q| catalog.get_table(&q).cloned());
        let table_oid = table.as_ref().map(|t| t.oid).unwrap_or(0);
        let indkey = SqlValue::Array(
            idx.columns
                .iter()
                .map(|c| {
                    SqlValue::Int2(
                        table
                            .as_ref()
                            .and_then(|t| t.column_index(c))
                            .map(|i| i as i16 + 1)
                            .unwrap_or(0),
                    )
                })
                .collect(),
        );
        let zeros = SqlValue::Array(idx.columns.iter().map(|_| SqlValue::Int2(0)).collect());
        rows.push(vec![
            i4(idx.oid as i32),
            i4(table_oid as i32),
            i2(idx.columns.len() as i16),
            i2(idx.columns.len() as i16),
            b(idx.unique),
            b(idx.primary),
            b(false),
            b(true),
            b(true),
            indkey,
            null(),
            null(),
            zeros.clone(),
            SqlValue::Array(idx.columns.iter().map(|_| i4(0)).collect()),
            SqlValue::Array(idx.columns.iter().map(|_| i4(0)).collect()),
        ]);
    }
    rs(cols, rows)
}

fn pg_constraint(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("conname", SqlType::Text),
        ("connamespace", SqlType::Integer),
        ("contype", SqlType::Char(Some(1))),
        ("condeferrable", SqlType::Boolean),
        ("condeferred", SqlType::Boolean),
        ("convalidated", SqlType::Boolean),
        ("conrelid", SqlType::Integer),
        ("confrelid", SqlType::Integer),
        ("conkey", SqlType::Array(Box::new(SqlType::SmallInt))),
        ("confkey", SqlType::Array(Box::new(SqlType::SmallInt))),
        ("confupdtype", SqlType::Char(Some(1))),
        ("confdeltype", SqlType::Char(Some(1))),
        // Foreign key `MATCH` mode: 'f' (FULL), 's' (SIMPLE), 'p' (PARTIAL —
        // never actually stored, since `MATCH PARTIAL` is rejected at DDL
        // time). Blank (' ') on every non-foreign-key row, matching
        // `confupdtype`/`confdeltype`'s existing convention above.
        ("confmatchtype", SqlType::Char(Some(1))),
    ];
    let mut rows = Vec::new();
    let mut oid = 30000;
    for table in catalog.tables() {
        let nsoid = schema_oid(catalog, &table.schema);
        let colnums = |cols: &[String]| -> SqlValue {
            SqlValue::Array(
                cols.iter()
                    .map(|c| {
                        SqlValue::Int2(table.column_index(c).map(|i| i as i16 + 1).unwrap_or(0))
                    })
                    .collect(),
            )
        };
        if let Some(pk) = &table.primary_key {
            rows.push(vec![
                i4(oid),
                t(&pk.name),
                i4(nsoid),
                t("p"),
                b(false),
                b(false),
                b(true),
                i4(table.oid as i32),
                i4(0),
                colnums(&pk.columns),
                SqlValue::Array(vec![]),
                t(" "),
                t(" "),
                t(" "),
            ]);
            oid += 1;
        }
        for u in &table.uniques {
            let name = if u.name.is_empty() {
                format!("{}_{}_key", table.name, u.columns.join("_"))
            } else {
                u.name.clone()
            };
            rows.push(vec![
                i4(oid),
                t(&name),
                i4(nsoid),
                t("u"),
                b(false),
                b(false),
                b(true),
                i4(table.oid as i32),
                i4(0),
                colnums(&u.columns),
                SqlValue::Array(vec![]),
                t(" "),
                t(" "),
                t(" "),
            ]);
            oid += 1;
        }
        for fk in &table.foreign_keys {
            let ref_table = catalog
                .resolve_table_name(Some(&fk.ref_schema), &fk.ref_table)
                .and_then(|q| catalog.get_table(&q).cloned());
            let frelid = ref_table.as_ref().map(|t| t.oid).unwrap_or(0);
            let confkey = SqlValue::Array(
                fk.ref_columns
                    .iter()
                    .map(|c| {
                        SqlValue::Int2(
                            ref_table
                                .as_ref()
                                .and_then(|t| t.column_index(c))
                                .map(|i| i as i16 + 1)
                                .unwrap_or(0),
                        )
                    })
                    .collect(),
            );
            rows.push(vec![
                i4(oid),
                t(&fk.name),
                i4(nsoid),
                t("f"),
                b(false),
                b(false),
                b(true),
                i4(table.oid as i32),
                i4(frelid as i32),
                colnums(&fk.columns),
                confkey,
                t(action_char(fk.on_update)),
                t(action_char(fk.on_delete)),
                t(match_type_char(fk.match_type)),
            ]);
            oid += 1;
        }
        for c in &table.checks {
            rows.push(vec![
                i4(oid),
                t(&c.name),
                i4(nsoid),
                t("c"),
                b(false),
                b(false),
                b(true),
                i4(table.oid as i32),
                i4(0),
                SqlValue::Array(vec![]),
                SqlValue::Array(vec![]),
                t(" "),
                t(" "),
                t(" "),
            ]);
            oid += 1;
        }
    }
    rs(cols, rows)
}

fn action_char(a: crate::relational::catalog::ReferentialAction) -> &'static str {
    use crate::relational::catalog::ReferentialAction::*;
    match a {
        NoAction => "a",
        Restrict => "r",
        Cascade => "c",
        SetNull => "n",
        SetDefault => "d",
    }
}

/// `pg_constraint.confmatchtype` for a foreign key's `MATCH` mode
/// (PostgreSQL's `FKCONSTR_MATCH_*` constants in `pg_constraint.h`): `'f'`
/// (FULL), `'s'` (SIMPLE). PostgreSQL also has `'p'` (PARTIAL), but
/// [`MatchType`](crate::relational::catalog::MatchType) has no such variant
/// to match on here — `MATCH PARTIAL` is rejected at DDL time (see
/// `crate::sql::ddl::fk_match_type`) and can never reach this function.
fn match_type_char(m: crate::relational::catalog::MatchType) -> &'static str {
    use crate::relational::catalog::MatchType::*;
    match m {
        Simple => "s",
        Full => "f",
    }
}

fn pg_database(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("datname", SqlType::Text),
        ("datdba", SqlType::Integer),
        ("encoding", SqlType::Integer),
        ("datcollate", SqlType::Text),
        ("datctype", SqlType::Text),
        ("datistemplate", SqlType::Boolean),
        ("datallowconn", SqlType::Boolean),
    ];
    let rows = vec![vec![
        i4(DB_OID),
        t(&catalog.database),
        i4(10),
        i4(6),
        t("en_US.UTF-8"),
        t("en_US.UTF-8"),
        b(false),
        b(true),
    ]];
    rs(cols, rows)
}

fn pg_indexes(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("schemaname", SqlType::Text),
        ("tablename", SqlType::Text),
        ("indexname", SqlType::Text),
        ("tablespace", SqlType::Text),
        ("indexdef", SqlType::Text),
    ];
    let rows = catalog
        .indexes()
        .map(|idx| {
            let unique = if idx.unique { "UNIQUE " } else { "" };
            let def = format!(
                "CREATE {unique}INDEX {} ON {}.{} USING btree ({})",
                idx.name,
                idx.schema,
                idx.table,
                idx.columns.join(", ")
            );
            vec![t(&idx.schema), t(&idx.table), t(&idx.name), null(), t(&def)]
        })
        .collect();
    rs(cols, rows)
}

fn pg_attrdef(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("adrelid", SqlType::Integer),
        ("adnum", SqlType::SmallInt),
        ("adbin", SqlType::Text),
        ("adsrc", SqlType::Text),
    ];
    let mut rows = Vec::new();
    let mut oid = 40000;
    for table in catalog.tables() {
        for col in &table.columns {
            if let Some(def) = &col.default {
                rows.push(vec![
                    i4(oid),
                    i4(table.oid as i32),
                    i2(col.ordinal as i16 + 1),
                    t(def),
                    t(def),
                ]);
                oid += 1;
            }
        }
    }
    rs(cols, rows)
}

fn pg_tables(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("schemaname", SqlType::Text),
        ("tablename", SqlType::Text),
        ("tableowner", SqlType::Text),
        ("tablespace", SqlType::Text),
        ("hasindexes", SqlType::Boolean),
        ("hasrules", SqlType::Boolean),
        ("hastriggers", SqlType::Boolean),
        ("rowsecurity", SqlType::Boolean),
    ];
    let rows = catalog
        .tables()
        .map(|table| {
            let nidx = catalog.indexes_for_table(&table.schema, &table.name).len();
            vec![
                t(&table.schema),
                t(&table.name),
                t("guardian"),
                null(),
                b(nidx > 0),
                b(false),
                b(false),
                b(table.rls_enabled),
            ]
        })
        .collect();
    rs(cols, rows)
}

fn pg_policies(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("schemaname", SqlType::Text),
        ("tablename", SqlType::Text),
        ("policyname", SqlType::Text),
        ("permissive", SqlType::Text),
        ("roles", SqlType::Array(Box::new(SqlType::Text))),
        ("cmd", SqlType::Text),
        ("qual", SqlType::Text),
        ("with_check", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        for p in &table.policies {
            let roles = if p.roles.is_empty() {
                SqlValue::Array(vec![t("public")])
            } else {
                SqlValue::Array(p.roles.iter().map(|r| t(r)).collect())
            };
            rows.push(vec![
                t(&table.schema),
                t(&table.name),
                t(&p.name),
                t(if p.permissive {
                    "PERMISSIVE"
                } else {
                    "RESTRICTIVE"
                }),
                roles,
                t(p.cmd.as_sql()),
                p.using_expr.as_deref().map(t).unwrap_or_else(null),
                p.check_expr.as_deref().map(t).unwrap_or_else(null),
            ]);
        }
    }
    rs(cols, rows)
}

fn pg_roles() -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("rolname", SqlType::Text),
        ("rolsuper", SqlType::Boolean),
        ("rolcanlogin", SqlType::Boolean),
    ];
    rs(cols, vec![vec![i4(10), t("guardian"), b(true), b(true)]])
}

/// `pg_proc`: user-defined functions (`CREATE FUNCTION`). Builtins and
/// extension functions are not cataloged here — like the rest of this
/// module, only what `CREATE ...` actually recorded is reflected.
fn pg_proc(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("proname", SqlType::Text),
        ("pronamespace", SqlType::Integer),
        ("proowner", SqlType::Integer),
        ("prolang", SqlType::Text),
        ("provolatile", SqlType::Char(Some(1))),
        ("proisstrict", SqlType::Boolean),
        ("prorettype", SqlType::Integer),
        ("pronargs", SqlType::SmallInt),
        ("proargtypes", SqlType::Text),
        ("prosrc", SqlType::Text),
    ];
    let rows = catalog
        .functions()
        .map(|f| {
            vec![
                i4(f.oid as i32),
                t(&f.name),
                i4(schema_oid(catalog, &f.schema)),
                i4(10),
                t(f.language.as_sql()),
                t(&f.volatility.as_char().to_string()),
                b(f.strict),
                // 2279 = PostgreSQL's `trigger` pseudo-type oid.
                i4(if f.returns_trigger {
                    2279
                } else {
                    f.return_type.oid() as i32
                }),
                i2(f.args.len() as i16),
                t(&f.args
                    .iter()
                    .map(|a| a.ty.oid().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")),
                t(&f.body),
            ]
        })
        .collect();
    rs(cols, rows)
}

/// `pg_trigger`: user triggers (`CREATE TRIGGER`). `tgtype` carries the
/// PostgreSQL bitmask (ROW=1, BEFORE=2, INSERT=4, DELETE=8, UPDATE=16;
/// TRUNCATE/INSTEAD bits are never set — those forms are rejected at DDL),
/// `tgattr` the 1-based column ordinals of an `UPDATE OF` list (int2vector
/// analog, the `proargtypes` text convention), and `tgqual` the raw `WHEN`
/// text (the `pg_policies.qual` convention).
fn pg_trigger(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("tgrelid", SqlType::Integer),
        ("tgname", SqlType::Text),
        ("tgfoid", SqlType::Integer),
        ("tgtype", SqlType::SmallInt),
        ("tgenabled", SqlType::Char(Some(1))),
        ("tgisinternal", SqlType::Boolean),
        ("tgconstraint", SqlType::Integer),
        ("tgdeferrable", SqlType::Boolean),
        ("tginitdeferred", SqlType::Boolean),
        ("tgnargs", SqlType::SmallInt),
        ("tgattr", SqlType::Text),
        ("tgqual", SqlType::Text),
    ];
    let mut rows = Vec::new();
    for table in catalog.tables() {
        for trg in &table.triggers {
            let fnoid = catalog
                .find_function(Some(&trg.function_schema), &trg.function_name, 0)
                .map(|f| f.oid as i32)
                .unwrap_or(0);
            let update_of: Vec<String> = trg
                .events
                .iter()
                .filter_map(|e| match e {
                    crate::relational::TriggerEventDef::Update { columns } => Some(columns),
                    _ => None,
                })
                .flatten()
                .filter_map(|c| table.column_index(c).map(|i| (i + 1).to_string()))
                .collect();
            rows.push(vec![
                i4(trg.oid as i32),
                i4(table.oid as i32),
                t(&trg.name),
                i4(fnoid),
                i2(crate::sql::trigger::tgtype(trg)),
                t(if trg.enabled { "O" } else { "D" }),
                b(false),
                i4(0),
                b(false),
                b(false),
                i2(0),
                t(&update_of.join(" ")),
                trg.when_expr.as_deref().map(t).unwrap_or_else(null),
            ]);
        }
    }
    rs(cols, rows)
}

fn pg_extension(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("extname", SqlType::Text),
        ("extowner", SqlType::Integer),
        ("extnamespace", SqlType::Integer),
        ("extrelocatable", SqlType::Boolean),
        ("extversion", SqlType::Text),
    ];
    let pg = schema_oid(catalog, "pg_catalog");
    let rows = catalog
        .extensions()
        .enumerate()
        .map(|(i, (name, version))| {
            vec![
                i4(16384 + i as i32),
                t(name),
                i4(10),
                i4(pg),
                b(false),
                t(version),
            ]
        })
        .collect();
    rs(cols, rows)
}

fn pg_available_extensions(catalog: &Catalog) -> RowSet {
    // `runtime` is a GuardianDB extension column (PostgreSQL has no such
    // concept): 'native' for engine-implemented extensions, 'sidecar' for
    // extensions delegated to the managed PostgreSQL sidecar runtime.
    let cols = &[
        ("name", SqlType::Text),
        ("default_version", SqlType::Text),
        ("installed_version", SqlType::Text),
        ("runtime", SqlType::Text),
        ("comment", SqlType::Text),
    ];
    let rows = crate::sql::ext::available()
        .iter()
        .map(|d| {
            vec![
                t(d.name),
                t(d.default_version),
                catalog
                    .extension_version(d.name)
                    .map(t)
                    .unwrap_or_else(null),
                t(match d.strategy {
                    crate::sql::ext::RuntimeStrategy::Native => "native",
                    crate::sql::ext::RuntimeStrategy::SidecarPostgres => "sidecar",
                }),
                t(d.comment),
            ]
        })
        .collect();
    rs(cols, rows)
}

fn pg_available_extension_versions(catalog: &Catalog) -> RowSet {
    let cols = &[
        ("name", SqlType::Text),
        ("version", SqlType::Text),
        ("installed", SqlType::Boolean),
        ("superuser", SqlType::Boolean),
        ("trusted", SqlType::Boolean),
        ("relocatable", SqlType::Boolean),
        ("requires", SqlType::Array(Box::new(SqlType::Text))),
        ("comment", SqlType::Text),
    ];
    let rows = crate::sql::ext::available()
        .iter()
        .map(|d| {
            vec![
                t(d.name),
                t(d.default_version),
                b(catalog.extension_installed(d.name)),
                b(!d.trusted),
                b(d.trusted),
                b(false),
                if d.requires.is_empty() {
                    null()
                } else {
                    SqlValue::Array(d.requires.iter().map(|r| t(r)).collect())
                },
                t(d.comment),
            ]
        })
        .collect();
    rs(cols, rows)
}

/// `pg_catalog.pg_depend`, restricted to the dependencies GuardianDB tracks:
/// each installed extension depends on the `pg_catalog` namespace it lives in,
/// and each table column of an extension-owned type depends on the extension's
/// `pg_extension` row (the same relationship that blocks `DROP EXTENSION`).
fn pg_depend(catalog: &Catalog) -> RowSet {
    // PostgreSQL catalog class OIDs.
    const PG_CLASS: i32 = 1259;
    const PG_NAMESPACE: i32 = 2615;
    const PG_EXTENSION: i32 = 3079;
    let cols = &[
        ("classid", SqlType::Integer),
        ("objid", SqlType::Integer),
        ("objsubid", SqlType::Integer),
        ("refclassid", SqlType::Integer),
        ("refobjid", SqlType::Integer),
        ("refobjsubid", SqlType::Integer),
        ("deptype", SqlType::Char(Some(1))),
    ];
    let pg = schema_oid(catalog, "pg_catalog");
    // Extension row OIDs mirror the pg_extension view: 16384 + position.
    let ext_oid: std::collections::HashMap<&str, i32> = catalog
        .extensions()
        .enumerate()
        .map(|(i, (name, _))| (name, 16384 + i as i32))
        .collect();
    let mut rows: Vec<Tuple> = catalog
        .extensions()
        .enumerate()
        .map(|(i, _)| {
            vec![
                i4(PG_EXTENSION),
                i4(16384 + i as i32),
                i4(0),
                i4(PG_NAMESPACE),
                i4(pg),
                i4(0),
                t("n"),
            ]
        })
        .collect();
    for dep in crate::sql::ext::column_dependencies(catalog) {
        if let Some(&eoid) = ext_oid.get(dep.extension) {
            rows.push(vec![
                i4(PG_CLASS),
                i4(dep.table_oid as i32),
                i4(dep.attnum as i32),
                i4(PG_EXTENSION),
                i4(eoid),
                i4(0),
                t("n"),
            ]);
        }
    }
    rs(cols, rows)
}

fn pg_am() -> RowSet {
    let cols = &[
        ("oid", SqlType::Integer),
        ("amname", SqlType::Text),
        ("amhandler", SqlType::Integer),
        ("amtype", SqlType::Char(Some(1))),
    ];
    rs(
        cols,
        vec![
            vec![i4(403), t("btree"), i4(0), t("i")],
            vec![i4(405), t("hash"), i4(0), t("i")],
        ],
    )
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn rs(cols: &[(&str, SqlType)], rows: Vec<Tuple>) -> RowSet {
    let fields = cols
        .iter()
        .map(|(name, ty)| FieldRef {
            table: None,
            name: (*name).to_string(),
            ty: ty.clone(),
        })
        .collect();
    RowSet {
        schema: RowSchema::new(fields),
        rows,
    }
}

fn empty(cols: &[(&str, SqlType)]) -> RowSet {
    rs(cols, Vec::new())
}

fn relabel(mut rs: RowSet, alias: &str) -> RowSet {
    for f in &mut rs.schema.fields {
        f.table = Some(alias.to_string());
    }
    rs.schema.rebuild_lookup();
    rs
}

fn type_metrics(ty: &SqlType) -> (SqlValue, SqlValue, SqlValue) {
    match ty {
        SqlType::Varchar(Some(n)) | SqlType::Char(Some(n)) => (i4(*n as i32), null(), null()),
        SqlType::SmallInt => (null(), i4(16), i4(0)),
        SqlType::Integer => (null(), i4(32), i4(0)),
        SqlType::BigInt => (null(), i4(64), i4(0)),
        SqlType::Real => (null(), i4(24), null()),
        SqlType::DoublePrecision => (null(), i4(53), null()),
        SqlType::Numeric { precision, scale } => (
            null(),
            precision.map(|p| i4(p as i32)).unwrap_or(null()),
            scale.map(|s| i4(s as i32)).unwrap_or(null()),
        ),
        _ => (null(), null(), null()),
    }
}

fn t(s: &str) -> SqlValue {
    SqlValue::Text(s.to_string())
}
fn null() -> SqlValue {
    SqlValue::Null
}
fn i4(n: i32) -> SqlValue {
    SqlValue::Int4(n)
}
fn i2(n: i16) -> SqlValue {
    SqlValue::Int2(n)
}
fn b(x: bool) -> SqlValue {
    SqlValue::Bool(x)
}
