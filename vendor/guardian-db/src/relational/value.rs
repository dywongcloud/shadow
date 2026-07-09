//! The runtime value model: [`SqlValue`].
//!
//! A `SqlValue` is the in-memory representation of a single SQL datum. It knows how
//! to:
//!   * encode to / decode from the canonical JSON shape used by GuardianDB storage,
//!   * render PostgreSQL text output for the wire protocol,
//!   * parse PostgreSQL text input (literals and bound parameters),
//!   * compare with SQL three-valued-logic semantics,
//!   * cast between types.

use crate::relational::error::{RelError, Result};
use crate::relational::types::SqlType;
use base64::Engine;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::*;
use serde_json::Value as Json;
use std::cmp::Ordering;

/// A single SQL value.
#[derive(Debug, Clone)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Decimal),
    Text(String),
    Bytea(Vec<u8>),
    Uuid(uuid::Uuid),
    Date(NaiveDate),
    Time(NaiveTime),
    Timestamp(NaiveDateTime),
    Timestamptz(DateTime<Utc>),
    /// Covers both `json` and `jsonb`.
    Json(Json),
    Array(Vec<SqlValue>),
    /// Case-insensitive text (`citext` extension). Stored and rendered verbatim;
    /// compares and indexes case-insensitively.
    Citext(String),
    /// Fixed-dimension float vector (`vector` / pgvector extension).
    Vector(Vec<f32>),
    /// Key/value store (`hstore` extension). `None` values are hstore NULLs.
    HStore(std::collections::BTreeMap<String, Option<String>>),
    /// Hierarchical label path (`ltree` extension), stored as its validated
    /// text form (dot-separated labels).
    Ltree(String),
    /// N-dimensional cube (`cube` extension): the two corners as given on
    /// input (PostgreSQL preserves corner order in the text form; accessors
    /// and predicates normalize per-dimension). `ll.len() == ur.len()`, and a
    /// point has `ll == ur`.
    Cube {
        ll: Vec<f64>,
        ur: Vec<f64>,
    },
    /// Full-text document vector (`tsvector`): sorted unique lexemes, each
    /// with an optional sorted position list.
    TsVector(Vec<crate::relational::fts::TsLexeme>),
    /// Full-text query (`tsquery`) operator tree; `None` is the empty query.
    TsQuery(Option<crate::relational::fts::TsQueryNode>),
}

impl SqlValue {
    pub fn is_null(&self) -> bool {
        matches!(self, SqlValue::Null)
    }

    /// Best-effort runtime type, used when a column type is not known statically.
    pub fn type_of(&self) -> SqlType {
        match self {
            SqlValue::Null => SqlType::Unknown,
            SqlValue::Bool(_) => SqlType::Boolean,
            SqlValue::Int2(_) => SqlType::SmallInt,
            SqlValue::Int4(_) => SqlType::Integer,
            SqlValue::Int8(_) => SqlType::BigInt,
            SqlValue::Float4(_) => SqlType::Real,
            SqlValue::Float8(_) => SqlType::DoublePrecision,
            SqlValue::Numeric(_) => SqlType::Numeric {
                precision: None,
                scale: None,
            },
            SqlValue::Text(_) => SqlType::Text,
            SqlValue::Bytea(_) => SqlType::Bytea,
            SqlValue::Uuid(_) => SqlType::Uuid,
            SqlValue::Date(_) => SqlType::Date,
            SqlValue::Time(_) => SqlType::Time,
            SqlValue::Timestamp(_) => SqlType::Timestamp,
            SqlValue::Timestamptz(_) => SqlType::Timestamptz,
            SqlValue::Json(_) => SqlType::Jsonb,
            SqlValue::Array(items) => {
                let inner = items.first().map(|v| v.type_of()).unwrap_or(SqlType::Text);
                SqlType::Array(Box::new(inner))
            }
            SqlValue::Citext(_) => SqlType::Citext,
            SqlValue::Vector(v) => SqlType::Vector(Some(v.len() as u32)),
            SqlValue::HStore(_) => SqlType::HStore,
            SqlValue::Ltree(_) => SqlType::Ltree,
            SqlValue::Cube { .. } => SqlType::Cube,
            SqlValue::TsVector(_) => SqlType::TsVector,
            SqlValue::TsQuery(_) => SqlType::TsQuery,
        }
    }

    // ----------------------------------------------------------------------
    // Storage encoding (canonical JSON form persisted in GuardianDB documents).
    // ----------------------------------------------------------------------

    /// Encode to the canonical JSON form stored in a GuardianDB document.
    pub fn encode_json(&self) -> Json {
        match self {
            SqlValue::Null => Json::Null,
            SqlValue::Bool(b) => Json::Bool(*b),
            SqlValue::Int2(n) => Json::from(*n),
            SqlValue::Int4(n) => Json::from(*n),
            SqlValue::Int8(n) => Json::from(*n),
            SqlValue::Float4(n) => serde_json::Number::from_f64(*n as f64)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            SqlValue::Float8(n) => serde_json::Number::from_f64(*n)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            // Numerics are stored as strings to preserve exact precision.
            SqlValue::Numeric(d) => Json::String(d.normalize().to_string()),
            SqlValue::Text(s) => Json::String(s.clone()),
            SqlValue::Bytea(b) => Json::String(base64::engine::general_purpose::STANDARD.encode(b)),
            SqlValue::Uuid(u) => Json::String(u.to_string()),
            SqlValue::Date(d) => Json::String(d.format("%Y-%m-%d").to_string()),
            SqlValue::Time(t) => Json::String(t.format("%H:%M:%S%.f").to_string()),
            SqlValue::Timestamp(ts) => Json::String(ts.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
            SqlValue::Timestamptz(ts) => Json::String(ts.to_rfc3339()),
            SqlValue::Json(v) => v.clone(),
            SqlValue::Array(items) => Json::Array(items.iter().map(|v| v.encode_json()).collect()),
            SqlValue::Citext(s) => Json::String(s.clone()),
            SqlValue::Vector(v) => Json::Array(
                v.iter()
                    .map(|f| {
                        serde_json::Number::from_f64(*f as f64)
                            .map(Json::Number)
                            .unwrap_or(Json::Null)
                    })
                    .collect(),
            ),
            // hstore stores as a JSON object; hstore NULL values become JSON null.
            SqlValue::HStore(map) => Json::Object(
                map.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_ref().map_or(Json::Null, |s| Json::String(s.clone())),
                        )
                    })
                    .collect(),
            ),
            SqlValue::Ltree(path) => Json::String(path.clone()),
            SqlValue::Cube { ll, ur } => {
                let nums = |v: &[f64]| {
                    Json::Array(
                        v.iter()
                            .map(|f| {
                                serde_json::Number::from_f64(*f)
                                    .map(Json::Number)
                                    .unwrap_or(Json::Null)
                            })
                            .collect(),
                    )
                };
                let mut obj = serde_json::Map::new();
                obj.insert("ll".into(), nums(ll));
                obj.insert("ur".into(), nums(ur));
                Json::Object(obj)
            }
            // tsvector/tsquery store as their canonical text form, which the
            // raw parsers read back losslessly (like ltree).
            SqlValue::TsVector(v) => Json::String(crate::relational::fts::format_tsvector(v)),
            SqlValue::TsQuery(q) => {
                Json::String(crate::relational::fts::format_tsquery(q.as_ref()))
            }
        }
    }

    /// Decode the canonical JSON form back to a [`SqlValue`] using the column type.
    pub fn decode_json(value: &Json, ty: &SqlType) -> Result<SqlValue> {
        if value.is_null() {
            return Ok(SqlValue::Null);
        }
        let bad = |t: &str| RelError::InvalidTextRepresentation {
            ty: t.to_string(),
            value: value.to_string(),
        };
        let out = match ty {
            SqlType::Boolean => SqlValue::Bool(value.as_bool().ok_or_else(|| bad("boolean"))?),
            SqlType::SmallInt => {
                SqlValue::Int2(json_as_i64(value).ok_or_else(|| bad("smallint"))? as i16)
            }
            SqlType::Integer => {
                SqlValue::Int4(json_as_i64(value).ok_or_else(|| bad("integer"))? as i32)
            }
            SqlType::BigInt => SqlValue::Int8(json_as_i64(value).ok_or_else(|| bad("bigint"))?),
            SqlType::Real => {
                SqlValue::Float4(json_as_f64(value).ok_or_else(|| bad("real"))? as f32)
            }
            SqlType::DoublePrecision => {
                SqlValue::Float8(json_as_f64(value).ok_or_else(|| bad("double precision"))?)
            }
            SqlType::Numeric { .. } => {
                let d = match value {
                    Json::String(s) => Decimal::from_str(s)
                        .or_else(|_| Decimal::from_scientific(s))
                        .map_err(|_| bad("numeric"))?,
                    Json::Number(n) => {
                        Decimal::from_str(&n.to_string()).map_err(|_| bad("numeric"))?
                    }
                    _ => return Err(bad("numeric")),
                };
                SqlValue::Numeric(d)
            }
            SqlType::Text | SqlType::Varchar(_) | SqlType::Char(_) => {
                SqlValue::Text(value.as_str().ok_or_else(|| bad("text"))?.to_string())
            }
            SqlType::Bytea => {
                let s = value.as_str().ok_or_else(|| bad("bytea"))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|_| bad("bytea"))?;
                SqlValue::Bytea(bytes)
            }
            SqlType::Uuid => {
                let s = value.as_str().ok_or_else(|| bad("uuid"))?;
                SqlValue::Uuid(uuid::Uuid::parse_str(s).map_err(|_| bad("uuid"))?)
            }
            SqlType::Date => parse_date(value.as_str().ok_or_else(|| bad("date"))?)?,
            SqlType::Time => parse_time(value.as_str().ok_or_else(|| bad("time"))?)?,
            SqlType::Timestamp => parse_timestamp(value.as_str().ok_or_else(|| bad("timestamp"))?)?,
            SqlType::Timestamptz => {
                parse_timestamptz(value.as_str().ok_or_else(|| bad("timestamptz"))?)?
            }
            SqlType::Json | SqlType::Jsonb => SqlValue::Json(value.clone()),
            SqlType::Array(inner) => {
                let arr = value.as_array().ok_or_else(|| bad("array"))?;
                let mut items = Vec::with_capacity(arr.len());
                for item in arr {
                    items.push(SqlValue::decode_json(item, inner)?);
                }
                SqlValue::Array(items)
            }
            SqlType::Citext => {
                SqlValue::Citext(value.as_str().ok_or_else(|| bad("citext"))?.to_string())
            }
            SqlType::Vector(dims) => {
                let arr = value.as_array().ok_or_else(|| bad("vector"))?;
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    out.push(json_as_f64(item).ok_or_else(|| bad("vector"))? as f32);
                }
                check_vector_dims(&out, *dims)?;
                SqlValue::Vector(out)
            }
            SqlType::HStore => {
                let obj = value.as_object().ok_or_else(|| bad("hstore"))?;
                let mut map = std::collections::BTreeMap::new();
                for (k, v) in obj {
                    let val = match v {
                        Json::Null => None,
                        Json::String(s) => Some(s.clone()),
                        _ => return Err(bad("hstore")),
                    };
                    map.insert(k.clone(), val);
                }
                SqlValue::HStore(map)
            }
            SqlType::Ltree => {
                let s = value.as_str().ok_or_else(|| bad("ltree"))?;
                parse_ltree_text(s)?
            }
            SqlType::Cube => {
                let obj = value.as_object().ok_or_else(|| bad("cube"))?;
                let corner = |key: &str| -> Result<Vec<f64>> {
                    let arr = obj
                        .get(key)
                        .and_then(Json::as_array)
                        .ok_or_else(|| bad("cube"))?;
                    arr.iter()
                        .map(|item| json_as_f64(item).ok_or_else(|| bad("cube")))
                        .collect()
                };
                let (ll, ur) = (corner("ll")?, corner("ur")?);
                if ll.len() != ur.len() || ll.is_empty() {
                    return Err(bad("cube"));
                }
                SqlValue::Cube { ll, ur }
            }
            SqlType::TsVector => SqlValue::TsVector(crate::relational::fts::parse_tsvector(
                value.as_str().ok_or_else(|| bad("tsvector"))?,
            )?),
            SqlType::TsQuery => SqlValue::TsQuery(crate::relational::fts::parse_tsquery(
                value.as_str().ok_or_else(|| bad("tsquery"))?,
            )?),
            SqlType::Unknown => SqlValue::Json(value.clone()),
        };
        Ok(out)
    }

    // ----------------------------------------------------------------------
    // Wire-protocol text representation.
    // ----------------------------------------------------------------------

    /// PostgreSQL text output. Returns `None` for SQL NULL.
    pub fn to_text(&self) -> Option<String> {
        let s = match self {
            SqlValue::Null => return None,
            SqlValue::Bool(b) => if *b { "t" } else { "f" }.to_string(),
            SqlValue::Int2(n) => n.to_string(),
            SqlValue::Int4(n) => n.to_string(),
            SqlValue::Int8(n) => n.to_string(),
            SqlValue::Float4(n) => format_float_f32(*n),
            SqlValue::Float8(n) => format_float(*n),
            SqlValue::Numeric(d) => d.normalize().to_string(),
            SqlValue::Text(s) => s.clone(),
            SqlValue::Bytea(b) => format!("\\x{}", hex_encode(b)),
            SqlValue::Uuid(u) => u.to_string(),
            SqlValue::Date(d) => d.format("%Y-%m-%d").to_string(),
            SqlValue::Time(t) => t.format("%H:%M:%S%.f").to_string(),
            SqlValue::Timestamp(ts) => ts.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
            SqlValue::Timestamptz(ts) => ts.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string(),
            SqlValue::Json(v) => v.to_string(),
            SqlValue::Array(items) => format_array(items),
            SqlValue::Citext(s) => s.clone(),
            SqlValue::Vector(v) => format_vector(v),
            SqlValue::HStore(map) => format_hstore(map),
            SqlValue::Ltree(path) => path.clone(),
            SqlValue::Cube { ll, ur } => format_cube(ll, ur),
            SqlValue::TsVector(v) => crate::relational::fts::format_tsvector(v),
            SqlValue::TsQuery(q) => crate::relational::fts::format_tsquery(q.as_ref()),
        };
        Some(s)
    }

    /// Parse PostgreSQL text input into a value of the requested type.
    pub fn from_text(text: &str, ty: &SqlType) -> Result<SqlValue> {
        let bad = || RelError::InvalidTextRepresentation {
            ty: ty.name(),
            value: text.to_string(),
        };
        let out = match ty {
            SqlType::Boolean => match text.trim().to_ascii_lowercase().as_str() {
                "t" | "true" | "yes" | "on" | "1" => SqlValue::Bool(true),
                "f" | "false" | "no" | "off" | "0" => SqlValue::Bool(false),
                _ => return Err(bad()),
            },
            SqlType::SmallInt => SqlValue::Int2(text.trim().parse().map_err(|_| bad())?),
            SqlType::Integer => SqlValue::Int4(text.trim().parse().map_err(|_| bad())?),
            SqlType::BigInt => SqlValue::Int8(text.trim().parse().map_err(|_| bad())?),
            SqlType::Real => SqlValue::Float4(text.trim().parse().map_err(|_| bad())?),
            SqlType::DoublePrecision => SqlValue::Float8(text.trim().parse().map_err(|_| bad())?),
            SqlType::Numeric { .. } => SqlValue::Numeric(
                Decimal::from_str(text.trim())
                    .or_else(|_| Decimal::from_scientific(text.trim()))
                    .map_err(|_| bad())?,
            ),
            SqlType::Text | SqlType::Varchar(_) | SqlType::Char(_) => {
                SqlValue::Text(text.to_string())
            }
            SqlType::Bytea => {
                let t = text.trim();
                let bytes = if let Some(hex) = t.strip_prefix("\\x") {
                    hex_decode(hex).map_err(|_| bad())?
                } else {
                    t.as_bytes().to_vec()
                };
                SqlValue::Bytea(bytes)
            }
            SqlType::Uuid => SqlValue::Uuid(uuid::Uuid::parse_str(text.trim()).map_err(|_| bad())?),
            SqlType::Date => parse_date(text.trim())?,
            SqlType::Time => parse_time(text.trim())?,
            SqlType::Timestamp => parse_timestamp(text.trim())?,
            SqlType::Timestamptz => parse_timestamptz(text.trim())?,
            SqlType::Json | SqlType::Jsonb => {
                SqlValue::Json(serde_json::from_str(text).map_err(|_| bad())?)
            }
            SqlType::Array(inner) => parse_array_text(text, inner)?,
            SqlType::Citext => SqlValue::Citext(text.to_string()),
            SqlType::Vector(dims) => {
                let v = parse_vector_text(text)?;
                check_vector_dims(&v, *dims)?;
                SqlValue::Vector(v)
            }
            SqlType::HStore => parse_hstore_text(text)?,
            SqlType::Ltree => parse_ltree_text(text)?,
            SqlType::Cube => parse_cube_text(text)?,
            // PostgreSQL raw input: lexemes as given, no config processing.
            SqlType::TsVector => SqlValue::TsVector(crate::relational::fts::parse_tsvector(text)?),
            SqlType::TsQuery => SqlValue::TsQuery(crate::relational::fts::parse_tsquery(text)?),
            SqlType::Unknown => SqlValue::Text(text.to_string()),
        };
        Ok(out)
    }

    // ----------------------------------------------------------------------
    // Numeric / boolean accessors.
    // ----------------------------------------------------------------------

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SqlValue::Int2(n) => Some(*n as f64),
            SqlValue::Int4(n) => Some(*n as f64),
            SqlValue::Int8(n) => Some(*n as f64),
            SqlValue::Float4(n) => Some(*n as f64),
            SqlValue::Float8(n) => Some(*n),
            SqlValue::Numeric(d) => d.to_f64(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SqlValue::Int2(n) => Some(*n as i64),
            SqlValue::Int4(n) => Some(*n as i64),
            SqlValue::Int8(n) => Some(*n),
            SqlValue::Numeric(d) => d.to_i64(),
            SqlValue::Float4(n) => Some(*n as i64),
            SqlValue::Float8(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            SqlValue::Int2(n) => Some(Decimal::from(*n)),
            SqlValue::Int4(n) => Some(Decimal::from(*n)),
            SqlValue::Int8(n) => Some(Decimal::from(*n)),
            SqlValue::Numeric(d) => Some(*d),
            SqlValue::Float4(n) => Decimal::from_f32(*n),
            SqlValue::Float8(n) => Decimal::from_f64(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SqlValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            SqlValue::Text(s) | SqlValue::Citext(s) => Some(s),
            _ => None,
        }
    }

    /// Key string used for hash/btree index entries and primary-key derivation.
    pub fn index_key(&self) -> String {
        match self {
            SqlValue::Null => "\u{0}null".to_string(),
            SqlValue::Text(s) => format!("s:{s}"),
            SqlValue::Citext(s) => format!("s:{}", s.to_lowercase()),
            SqlValue::Bool(b) => format!("b:{b}"),
            SqlValue::Uuid(u) => format!("u:{u}"),
            // Separate labels with NUL so byte order matches the label-wise
            // ltree ordering ('a.b' sorts before 'a-b', unlike raw text).
            SqlValue::Ltree(p) => format!("l:{}", p.replace('.', "\u{0}")),
            v if v.type_of().is_numeric() => {
                // Normalise all numerics to a decimal string so 1 == 1.0 in the index.
                match v.as_decimal() {
                    Some(d) => format!("n:{}", d.normalize()),
                    None => format!("n:{}", v.as_f64().unwrap_or(f64::NAN)),
                }
            }
            other => format!("x:{}", other.to_text().unwrap_or_default()),
        }
    }

    // ----------------------------------------------------------------------
    // Comparison with SQL three-valued logic.
    // ----------------------------------------------------------------------

    /// Compare two values. Returns `None` if either is NULL (SQL UNKNOWN) or the
    /// types are not comparable.
    pub fn compare(&self, other: &SqlValue) -> Option<Ordering> {
        use SqlValue::*;
        if self.is_null() || other.is_null() {
            return None;
        }
        // Numeric cross-type comparison via decimal where possible, else f64.
        if self.type_of().is_numeric() && other.type_of().is_numeric() {
            if let (Some(a), Some(b)) = (self.as_decimal(), other.as_decimal()) {
                return a.partial_cmp(&b);
            }
            return self.as_f64()?.partial_cmp(&other.as_f64()?);
        }
        match (self, other) {
            (Bool(a), Bool(b)) => a.partial_cmp(b),
            (Text(a), Text(b)) => Some(a.cmp(b)),
            // citext comparison is case-insensitive and wins over plain text
            // (PostgreSQL: the citext side determines the comparison semantics).
            (Citext(a), Citext(b)) | (Citext(a), Text(b)) | (Text(a), Citext(b)) => {
                Some(a.to_lowercase().cmp(&b.to_lowercase()))
            }
            (Vector(a), Vector(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.partial_cmp(y) {
                        Some(Ordering::Equal) => continue,
                        other => return other,
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            // ltree ordering is label-by-label (PostgreSQL compares levels,
            // so 'a.b' < 'aa' even though '.' > 'a' would say otherwise).
            (Ltree(a), Ltree(b)) => Some(ltree_labels(a).cmp(&ltree_labels(b))),
            (Ltree(a), Text(b)) | (Text(a), Ltree(b)) => {
                Some(ltree_labels(a).cmp(&ltree_labels(b)))
            }
            // cube comparison normalizes corners per dimension (missing
            // dimensions read as 0), matching contrib/cube's cube_cmp: the
            // minimal corners are compared first, then the maximal ones —
            // so '(3),(1)' = '(1),(3)'.
            (Cube { ll: al, ur: au }, Cube { ll: bl, ur: bu }) => {
                let dim = al.len().max(bl.len());
                let coord = |v: &[f64], i: usize| v.get(i).copied().unwrap_or(0.0);
                for i in 0..dim {
                    let (a_min, b_min) = (
                        coord(al, i).min(coord(au, i)),
                        coord(bl, i).min(coord(bu, i)),
                    );
                    match a_min.partial_cmp(&b_min) {
                        Some(Ordering::Equal) => continue,
                        other => return other,
                    }
                }
                for i in 0..dim {
                    let (a_max, b_max) = (
                        coord(al, i).max(coord(au, i)),
                        coord(bl, i).max(coord(bu, i)),
                    );
                    match a_max.partial_cmp(&b_max) {
                        Some(Ordering::Equal) => continue,
                        other => return other,
                    }
                }
                // Coordinates tie: lower dimensionality sorts first, so
                // '(1)' <> '(1,0)' (PG-verified).
                Some(al.len().cmp(&bl.len()))
            }
            // hstore has no natural order; equality (and a deterministic
            // order for sorting) come from the canonical sorted pair list.
            (HStore(a), HStore(b)) => Some(a.iter().cmp(b.iter())),
            // tsvector compares lexeme-wise (word, then positions), which the
            // sorted-lexeme invariant makes a plain list comparison; tsquery
            // equality/order comes from the canonical text form (fallthrough).
            (TsVector(a), TsVector(b)) => Some(a.cmp(b)),
            (Uuid(a), Uuid(b)) => Some(a.cmp(b)),
            (Bytea(a), Bytea(b)) => Some(a.cmp(b)),
            (Date(a), Date(b)) => Some(a.cmp(b)),
            (Time(a), Time(b)) => Some(a.cmp(b)),
            (Timestamp(a), Timestamp(b)) => Some(a.cmp(b)),
            (Timestamptz(a), Timestamptz(b)) => Some(a.cmp(b)),
            (Timestamp(a), Timestamptz(b)) => a.and_utc().partial_cmp(b),
            (Timestamptz(a), Timestamp(b)) => a.partial_cmp(&b.and_utc()),
            (Json(a), Json(b)) => Some(a.to_string().cmp(&b.to_string())),
            (Array(a), Array(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.compare(y) {
                        Some(Ordering::Equal) => continue,
                        other => return other,
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            // Last resort: compare textual forms (e.g. text vs uuid).
            _ => self.to_text().zip(other.to_text()).map(|(a, b)| a.cmp(&b)),
        }
    }

    /// SQL equality with three-valued logic. `None` means UNKNOWN (NULL involved).
    pub fn sql_eq(&self, other: &SqlValue) -> Option<bool> {
        self.compare(other).map(|o| o == Ordering::Equal)
    }

    /// Truth value used by WHERE/HAVING/CHECK. NULL and non-boolean → `None` (UNKNOWN).
    pub fn truthy(&self) -> Option<bool> {
        match self {
            SqlValue::Bool(b) => Some(*b),
            SqlValue::Null => None,
            _ => None,
        }
    }

    // ----------------------------------------------------------------------
    // Casting.
    // ----------------------------------------------------------------------

    /// Cast this value to `target`. NULL casts to NULL of any type.
    pub fn cast(&self, target: &SqlType) -> Result<SqlValue> {
        if self.is_null() {
            return Ok(SqlValue::Null);
        }
        // Fast path: already the right shape.
        let bad = |to: &SqlType| RelError::CannotCoerce {
            from: self.type_of().name(),
            to: to.name(),
        };
        let out = match target {
            SqlType::Boolean => match self {
                SqlValue::Bool(_) => self.clone(),
                SqlValue::Text(s) => SqlValue::from_text(s, target)?,
                v if v.type_of().is_numeric() => SqlValue::Bool(v.as_f64().unwrap_or(0.0) != 0.0),
                _ => return Err(bad(target)),
            },
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => {
                let n = match self {
                    SqlValue::Text(s) => s.trim().parse::<f64>().map_err(|_| {
                        RelError::InvalidTextRepresentation {
                            ty: target.name(),
                            value: s.clone(),
                        }
                    })?,
                    SqlValue::Bool(b) => {
                        if *b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    v => v.as_f64().ok_or_else(|| bad(target))?,
                };
                let r = n.round();
                match target {
                    SqlType::SmallInt => {
                        SqlValue::Int2(
                            checked_int(r, i16::MIN as f64, i16::MAX as f64, target)? as i16
                        )
                    }
                    SqlType::Integer => {
                        SqlValue::Int4(
                            checked_int(r, i32::MIN as f64, i32::MAX as f64, target)? as i32
                        )
                    }
                    _ => SqlValue::Int8(
                        checked_int(r, i64::MIN as f64, i64::MAX as f64, target)? as i64
                    ),
                }
            }
            SqlType::Real => SqlValue::Float4(self.cast_f64(target)? as f32),
            SqlType::DoublePrecision => SqlValue::Float8(self.cast_f64(target)?),
            SqlType::Numeric { .. } => match self {
                SqlValue::Text(s) => SqlValue::from_text(s, target)?,
                v => SqlValue::Numeric(v.as_decimal().ok_or_else(|| bad(target))?),
            },
            SqlType::Text | SqlType::Varchar(_) | SqlType::Char(_) => {
                SqlValue::Text(self.to_text().unwrap_or_default())
            }
            SqlType::Citext => SqlValue::Citext(self.to_text().unwrap_or_default()),
            SqlType::Vector(dims) => match self {
                SqlValue::Vector(v) => {
                    check_vector_dims(v, *dims)?;
                    self.clone()
                }
                SqlValue::Text(s) | SqlValue::Citext(s) => {
                    let v = parse_vector_text(s)?;
                    check_vector_dims(&v, *dims)?;
                    SqlValue::Vector(v)
                }
                SqlValue::Array(items) => {
                    let mut v = Vec::with_capacity(items.len());
                    for it in items {
                        v.push(it.as_f64().ok_or_else(|| bad(target))? as f32);
                    }
                    check_vector_dims(&v, *dims)?;
                    SqlValue::Vector(v)
                }
                _ => return Err(bad(target)),
            },
            SqlType::HStore => match self {
                SqlValue::HStore(_) => self.clone(),
                SqlValue::Text(s) | SqlValue::Citext(s) => parse_hstore_text(s)?,
                _ => return Err(bad(target)),
            },
            SqlType::Ltree => match self {
                SqlValue::Ltree(_) => self.clone(),
                SqlValue::Text(s) | SqlValue::Citext(s) => parse_ltree_text(s)?,
                _ => return Err(bad(target)),
            },
            SqlType::Cube => match self {
                SqlValue::Cube { .. } => self.clone(),
                SqlValue::Text(s) | SqlValue::Citext(s) => parse_cube_text(s)?,
                _ => return Err(bad(target)),
            },
            SqlType::TsVector => match self {
                SqlValue::TsVector(_) => self.clone(),
                SqlValue::Text(s) | SqlValue::Citext(s) => SqlValue::from_text(s, target)?,
                _ => return Err(bad(target)),
            },
            SqlType::TsQuery => match self {
                SqlValue::TsQuery(_) => self.clone(),
                SqlValue::Text(s) | SqlValue::Citext(s) => SqlValue::from_text(s, target)?,
                _ => return Err(bad(target)),
            },
            SqlType::Json | SqlType::Jsonb => match self {
                SqlValue::Json(_) => self.clone(),
                SqlValue::Text(s) => {
                    SqlValue::Json(serde_json::from_str(s).map_err(|_| bad(target))?)
                }
                v => SqlValue::Json(v.encode_json()),
            },
            SqlType::Uuid => match self {
                SqlValue::Uuid(_) => self.clone(),
                SqlValue::Text(s) => SqlValue::from_text(s, target)?,
                _ => return Err(bad(target)),
            },
            SqlType::Date | SqlType::Time | SqlType::Timestamp | SqlType::Timestamptz => match self
            {
                SqlValue::Text(s) => SqlValue::from_text(s, target)?,
                SqlValue::Timestamp(ts) if matches!(target, SqlType::Date) => {
                    SqlValue::Date(ts.date())
                }
                SqlValue::Timestamptz(ts) if matches!(target, SqlType::Date) => {
                    SqlValue::Date(ts.naive_utc().date())
                }
                SqlValue::Timestamp(ts) if matches!(target, SqlType::Timestamptz) => {
                    SqlValue::Timestamptz(ts.and_utc())
                }
                SqlValue::Timestamptz(ts) if matches!(target, SqlType::Timestamp) => {
                    SqlValue::Timestamp(ts.naive_utc())
                }
                v if std::mem::discriminant(&v.type_of()) == std::mem::discriminant(target) => {
                    v.clone()
                }
                _ => return Err(bad(target)),
            },
            SqlType::Bytea => match self {
                SqlValue::Bytea(_) => self.clone(),
                SqlValue::Text(s) => SqlValue::Bytea(s.clone().into_bytes()),
                _ => return Err(bad(target)),
            },
            SqlType::Array(inner) => match self {
                SqlValue::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for it in items {
                        out.push(it.cast(inner)?);
                    }
                    SqlValue::Array(out)
                }
                SqlValue::Text(s) => parse_array_text(s, inner)?,
                _ => return Err(bad(target)),
            },
            SqlType::Unknown => self.clone(),
        };
        Ok(out)
    }

    fn cast_f64(&self, target: &SqlType) -> Result<f64> {
        match self {
            SqlValue::Text(s) => {
                s.trim()
                    .parse::<f64>()
                    .map_err(|_| RelError::InvalidTextRepresentation {
                        ty: target.name(),
                        value: s.clone(),
                    })
            }
            v => v.as_f64().ok_or_else(|| RelError::CannotCoerce {
                from: self.type_of().name(),
                to: target.name(),
            }),
        }
    }
}

fn checked_int(r: f64, min: f64, max: f64, ty: &SqlType) -> Result<f64> {
    if r.is_nan() || r < min || r > max {
        return Err(RelError::NumericValueOutOfRange(ty.name()));
    }
    Ok(r)
}

fn json_as_i64(v: &Json) -> Option<i64> {
    match v {
        Json::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Json::String(s) => s.trim().parse().ok(),
        Json::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

fn json_as_f64(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn format_float(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else {
        let s = format!("{n}");
        s
    }
}

/// float4 text output: shortest round-trip form of the f32 itself (widening
/// through f64 would print excess digits, e.g. 0.36363637 -> 0.3636363744735718).
fn format_float_f32(n: f32) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        format!("{n}")
    }
}

fn format_array(items: &[SqlValue]) -> String {
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item.to_text() {
            None => out.push_str("NULL"),
            // Nested arrays print unquoted ({{a,1},{b,2}}), like PostgreSQL.
            Some(t) if matches!(item, SqlValue::Array(_)) => out.push_str(&t),
            Some(t) => {
                let needs_quote = t.is_empty()
                    || t.contains([',', '{', '}', '"', '\\', ' '])
                    || t.eq_ignore_ascii_case("null");
                if needs_quote {
                    out.push('"');
                    out.push_str(&t.replace('\\', "\\\\").replace('"', "\\\""));
                    out.push('"');
                } else {
                    out.push_str(&t);
                }
            }
        }
    }
    out.push('}');
    out
}

fn parse_array_text(text: &str, inner: &SqlType) -> Result<SqlValue> {
    let t = text.trim();
    let inner_str = t
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| RelError::InvalidTextRepresentation {
            ty: format!("{}[]", inner.name()),
            value: text.to_string(),
        })?;
    if inner_str.trim().is_empty() {
        return Ok(SqlValue::Array(Vec::new()));
    }
    let mut items = Vec::new();
    for raw in split_array_elems(inner_str) {
        if raw.eq_ignore_ascii_case("null") {
            items.push(SqlValue::Null);
        } else {
            let unquoted = raw
                .trim()
                .trim_matches('"')
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            items.push(SqlValue::from_text(&unquoted, inner)?);
        }
    }
    Ok(SqlValue::Array(items))
}

fn split_array_elems(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_date(s: &str) -> Result<SqlValue> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(SqlValue::Date)
        .map_err(|_| RelError::InvalidTextRepresentation {
            ty: "date".into(),
            value: s.into(),
        })
}

fn parse_time(s: &str) -> Result<SqlValue> {
    NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .map(SqlValue::Time)
        .map_err(|_| RelError::InvalidTextRepresentation {
            ty: "time".into(),
            value: s.into(),
        })
}

fn parse_timestamp(s: &str) -> Result<SqlValue> {
    let s = s.trim();
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(ts) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(SqlValue::Timestamp(ts));
        }
    }
    // Allow a date-only timestamp.
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(SqlValue::Timestamp(d.and_hms_opt(0, 0, 0).unwrap()));
    }
    // Fall back to parsing an offset timestamp and dropping the zone.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(SqlValue::Timestamp(dt.naive_utc()));
    }
    Err(RelError::InvalidTextRepresentation {
        ty: "timestamp".into(),
        value: s.into(),
    })
}

fn parse_timestamptz(s: &str) -> Result<SqlValue> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(SqlValue::Timestamptz(dt.with_timezone(&Utc)));
    }
    // Accept "YYYY-MM-DD HH:MM:SS+00" style.
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Ok(SqlValue::Timestamptz(dt.with_timezone(&Utc)));
        }
    }
    // Treat a naive timestamp as UTC.
    if let Ok(SqlValue::Timestamp(ts)) = parse_timestamp(s) {
        return Ok(SqlValue::Timestamptz(ts.and_utc()));
    }
    Err(RelError::InvalidTextRepresentation {
        ty: "timestamptz".into(),
        value: s.into(),
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn format_vector(v: &[f32]) -> String {
    let mut out = String::from("[");
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format_float(*f as f64));
    }
    out.push(']');
    out
}

fn parse_vector_text(text: &str) -> Result<Vec<f32>> {
    let bad = || RelError::InvalidTextRepresentation {
        ty: "vector".into(),
        value: text.to_string(),
    };
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(bad)?;
    if inner.trim().is_empty() {
        return Err(bad()); // PG: vector must have at least 1 dimension
    }
    inner
        .split(',')
        .map(|p| p.trim().parse::<f32>().map_err(|_| bad()))
        .collect()
}

/// Canonical hstore text output, matching contrib/hstore: pairs sorted by
/// (key length, key bytes) — the extension's internal order, which is why
/// PostgreSQL prints `"b"=>"1", "aa"=>"2"` — keys and values always quoted
/// (`\` and `"` escaped), hstore NULLs as unquoted `NULL`, `", "` separator.
fn format_hstore(map: &std::collections::BTreeMap<String, Option<String>>) -> String {
    let mut pairs: Vec<(&String, &Option<String>)> = map.iter().collect();
    pairs.sort_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    pairs
        .iter()
        .map(|(k, v)| match v {
            Some(val) => format!("{}=>{}", quote(k), quote(val)),
            None => format!("{}=>NULL", quote(k)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse hstore text input (`'a=>1, "b b"=>NULL'`). Tokens may be quoted
/// (backslash escapes) or bare; a bare, case-insensitive `NULL` value is the
/// hstore NULL. Duplicate keys keep the first occurrence (what contrib/hstore
/// does). Syntax errors are `42601`, matching PostgreSQL.
fn parse_hstore_text(text: &str) -> Result<SqlValue> {
    let err =
        |what: &str| RelError::Syntax(format!("syntax error in hstore: {what}, in \"{text}\""));
    let mut chars = text.chars().peekable();
    let mut map = std::collections::BTreeMap::new();
    loop {
        skip_spaces(&mut chars);
        if chars.peek().is_none() {
            break;
        }
        let (key, _) = hstore_token(&mut chars).ok_or_else(|| err("expected key"))?;
        skip_spaces(&mut chars);
        if !(chars.next() == Some('=') && chars.next() == Some('>')) {
            return Err(err("expected =>"));
        }
        skip_spaces(&mut chars);
        let (val, quoted) = hstore_token(&mut chars).ok_or_else(|| err("expected value"))?;
        let value = if !quoted && val.eq_ignore_ascii_case("null") {
            None
        } else {
            Some(val)
        };
        map.entry(key).or_insert(value);
        skip_spaces(&mut chars);
        match chars.next() {
            None => break,
            Some(',') => continue,
            Some(_) => return Err(err("expected , or end")),
        }
    }
    Ok(SqlValue::HStore(map))
}

fn skip_spaces(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
}

/// One hstore key or value token. Returns `(text, was_quoted)`; `None` on a
/// malformed or empty token (including an unterminated quote).
fn hstore_token(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<(String, bool)> {
    let mut out = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        loop {
            match chars.next()? {
                '"' => return Some((out, true)),
                '\\' => out.push(chars.next()?),
                c => out.push(c),
            }
        }
    }
    loop {
        match chars.peek() {
            Some('\\') => {
                chars.next();
                out.push(chars.next()?);
            }
            Some(c) if !c.is_whitespace() && !matches!(c, ',' | '"' | '=') => {
                out.push(*c);
                chars.next();
            }
            _ => break,
        }
    }
    if out.is_empty() {
        None
    } else {
        Some((out, false))
    }
}

/// The label sequence of an ltree path (empty path = zero labels).
pub(crate) fn ltree_labels(path: &str) -> Vec<&str> {
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('.').collect()
    }
}

/// Parse and validate ltree text input: dot-separated labels of alphanumerics,
/// `_` and `-` (hyphens per PostgreSQL 16), each 1..=255 characters; the empty
/// string is the valid zero-level path. Syntax errors are `42601` like
/// PostgreSQL's `ltree syntax error`.
fn parse_ltree_text(text: &str) -> Result<SqlValue> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(SqlValue::Ltree(String::new()));
    }
    for label in t.split('.') {
        let ok = !label.is_empty()
            && label.chars().count() <= 255
            && label
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
        if !ok {
            return Err(RelError::Syntax(format!("ltree syntax error in \"{t}\"")));
        }
    }
    Ok(SqlValue::Ltree(t.to_string()))
}

/// cube text output, matching contrib/cube: coordinates joined by `", "`,
/// corners preserved as given, and a cube whose corners coincide printed as a
/// single point — `(1, 2)` / `(1, 2),(3, 4)`.
fn format_cube(ll: &[f64], ur: &[f64]) -> String {
    let list = |v: &[f64]| {
        v.iter()
            .map(|f| format_float(*f))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if ll == ur {
        format!("({})", list(ll))
    } else {
        format!("({}),({})", list(ll), list(ur))
    }
}

/// Parse cube text input: `'1'`, `'1,2'`, `'(1,2)'`, `'(1,2),(3,4)'` and the
/// bracketed `'[(1),(2)]'` form. Corner dimension mismatches and empty cubes
/// are rejected. Errors are `22P02` like PostgreSQL's
/// `invalid input syntax for cube`.
fn parse_cube_text(text: &str) -> Result<SqlValue> {
    let bad = || RelError::InvalidTextRepresentation {
        ty: "cube".into(),
        value: text.to_string(),
    };
    let parse_list = |s: &str| -> Result<Vec<f64>> {
        let s = s.trim();
        if s.is_empty() {
            return Err(bad());
        }
        s.split(',')
            .map(|p| p.trim().parse::<f64>().map_err(|_| bad()))
            .collect()
    };
    let mut t = text.trim();
    if let Some(inner) = t.strip_prefix('[') {
        t = inner.strip_suffix(']').ok_or_else(bad)?.trim();
    }
    if let Some(rest) = t.strip_prefix('(') {
        let close = rest.find(')').ok_or_else(bad)?;
        let first = parse_list(&rest[..close])?;
        let after = rest[close + 1..].trim();
        if after.is_empty() {
            return Ok(SqlValue::Cube {
                ll: first.clone(),
                ur: first,
            });
        }
        let second_group = after.strip_prefix(',').ok_or_else(bad)?.trim();
        let inner = second_group
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .ok_or_else(bad)?;
        let second = parse_list(inner)?;
        if first.len() != second.len() {
            return Err(bad());
        }
        return Ok(SqlValue::Cube {
            ll: first,
            ur: second,
        });
    }
    let point = parse_list(t)?;
    Ok(SqlValue::Cube {
        ll: point.clone(),
        ur: point,
    })
}

fn check_vector_dims(v: &[f32], dims: Option<u32>) -> Result<()> {
    if let Some(d) = dims
        && v.len() as u32 != d
    {
        return Err(RelError::DatatypeMismatch {
            column: String::new(),
            expected: format!("vector({d})"),
            actual: format!("vector({})", v.len()),
        });
    }
    Ok(())
}

fn hex_decode(s: &str) -> std::result::Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_integer() {
        let v = SqlValue::Int4(42);
        let json = v.encode_json();
        let back = SqlValue::decode_json(&json, &SqlType::Integer).unwrap();
        assert!(matches!(back, SqlValue::Int4(42)));
    }

    #[test]
    fn json_round_trip_numeric_precision() {
        let v = SqlValue::Numeric(Decimal::from_str("123456789.123456789").unwrap());
        let json = v.encode_json();
        assert!(json.is_string());
        let back = SqlValue::decode_json(
            &json,
            &SqlType::Numeric {
                precision: None,
                scale: None,
            },
        )
        .unwrap();
        assert_eq!(back.to_text().unwrap(), "123456789.123456789");
    }

    #[test]
    fn bytea_round_trip() {
        let v = SqlValue::Bytea(vec![0, 1, 2, 255]);
        let json = v.encode_json();
        let back = SqlValue::decode_json(&json, &SqlType::Bytea).unwrap();
        assert_eq!(back.to_text().unwrap(), "\\x000102ff");
    }

    #[test]
    fn bool_text_output() {
        assert_eq!(SqlValue::Bool(true).to_text().unwrap(), "t");
        assert_eq!(SqlValue::Bool(false).to_text().unwrap(), "f");
        assert_eq!(SqlValue::Null.to_text(), None);
    }

    #[test]
    fn numeric_cross_type_comparison() {
        let a = SqlValue::Int4(1);
        let b = SqlValue::Numeric(Decimal::from_str("1.0").unwrap());
        assert_eq!(a.compare(&b), Some(Ordering::Equal));
        assert_eq!(a.sql_eq(&b), Some(true));
    }

    #[test]
    fn null_comparison_is_unknown() {
        let a = SqlValue::Null;
        let b = SqlValue::Int4(1);
        assert_eq!(a.compare(&b), None);
        assert_eq!(a.sql_eq(&b), None);
    }

    #[test]
    fn cast_text_to_int() {
        let v = SqlValue::Text("42".into());
        let casted = v.cast(&SqlType::Integer).unwrap();
        assert!(matches!(casted, SqlValue::Int4(42)));
    }

    #[test]
    fn cast_out_of_range_errors() {
        let v = SqlValue::Int8(100_000);
        assert!(matches!(
            v.cast(&SqlType::SmallInt),
            Err(RelError::NumericValueOutOfRange(_))
        ));
    }

    #[test]
    fn array_text_round_trip() {
        let v = SqlValue::Array(vec![SqlValue::Int4(1), SqlValue::Int4(2), SqlValue::Null]);
        let text = v.to_text().unwrap();
        assert_eq!(text, "{1,2,NULL}");
        let back = parse_array_text(&text, &SqlType::Integer).unwrap();
        if let SqlValue::Array(items) = back {
            assert_eq!(items.len(), 3);
            assert!(items[2].is_null());
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn index_key_normalizes_numeric() {
        assert_eq!(
            SqlValue::Int4(1).index_key(),
            SqlValue::Numeric(Decimal::from(1)).index_key()
        );
    }

    // ------------------------------------------------------------------
    // hstore / ltree / cube value plumbing. Expected values generated from
    // live PostgreSQL 16.13 with contrib hstore 1.8 / ltree 1.2 / cube 1.5.
    // ------------------------------------------------------------------

    #[test]
    fn hstore_text_round_trip_uses_pg_key_order() {
        // PG: SELECT 'zz=>1, b=>2, aa=>3, a=>4'::hstore
        //     => "a"=>"4", "b"=>"2", "aa"=>"3", "zz"=>"1"  (length, then bytes)
        let v = SqlValue::from_text("zz=>1, b=>2, aa=>3, a=>4", &SqlType::HStore).unwrap();
        assert_eq!(
            v.to_text().unwrap(),
            r#""a"=>"4", "b"=>"2", "aa"=>"3", "zz"=>"1""#
        );
    }

    #[test]
    fn hstore_null_values_and_quoting() {
        // PG: SELECT '"a"=>"1", b=>NULL'::hstore => "a"=>"1", "b"=>NULL
        let v = SqlValue::from_text(r#""a"=>"1", b=>NULL"#, &SqlType::HStore).unwrap();
        assert_eq!(v.to_text().unwrap(), r#""a"=>"1", "b"=>NULL"#);
        // Quoted "NULL" is the string, not the hstore NULL (PG-verified).
        let v = SqlValue::from_text(r#"a=>"NULL""#, &SqlType::HStore).unwrap();
        assert_eq!(v.to_text().unwrap(), r#""a"=>"NULL""#);
    }

    #[test]
    fn hstore_escapes_and_duplicates() {
        // PG: SELECT E'k\\"ey=>"v,1", "with space"=>"a\\\\b"'::hstore
        //     => "k\"ey"=>"v,1", "with space"=>"a\\b"
        let v =
            SqlValue::from_text(r#"k\"ey=>"v,1", "with space"=>"a\\b""#, &SqlType::HStore).unwrap();
        assert_eq!(
            v.to_text().unwrap(),
            r#""k\"ey"=>"v,1", "with space"=>"a\\b""#
        );
        // PG: SELECT 'a=>1, a=>2'::hstore => "a"=>"1" (first occurrence wins)
        let v = SqlValue::from_text("a=>1, a=>2", &SqlType::HStore).unwrap();
        assert_eq!(v.to_text().unwrap(), r#""a"=>"1""#);
    }

    #[test]
    fn hstore_empty_and_errors() {
        let v = SqlValue::from_text("", &SqlType::HStore).unwrap();
        assert_eq!(v.to_text().unwrap(), "");
        // PG: 'a'::hstore and 'a=>'::hstore are 42601 syntax errors.
        for bad in ["a", "a=>", "=>1", "a=>1 b=>2"] {
            assert!(
                matches!(
                    SqlValue::from_text(bad, &SqlType::HStore),
                    Err(RelError::Syntax(_))
                ),
                "{bad:?} should be an hstore syntax error"
            );
        }
    }

    #[test]
    fn hstore_json_round_trip() {
        let v = SqlValue::from_text("a=>1, b=>NULL", &SqlType::HStore).unwrap();
        let json = v.encode_json();
        assert_eq!(json, serde_json::json!({"a": "1", "b": null}));
        let back = SqlValue::decode_json(&json, &SqlType::HStore).unwrap();
        assert_eq!(back.sql_eq(&v), Some(true));
    }

    #[test]
    fn ltree_validation_and_round_trip() {
        let v = SqlValue::from_text("Top.Science.Astronomy", &SqlType::Ltree).unwrap();
        assert_eq!(v.to_text().unwrap(), "Top.Science.Astronomy");
        // Hyphenated labels are valid in PostgreSQL 16.
        assert!(SqlValue::from_text("a-b.c", &SqlType::Ltree).is_ok());
        // The empty path is valid (nlevel 0), like PG.
        assert!(SqlValue::from_text("", &SqlType::Ltree).is_ok());
        for bad in ["a..b", "a b", "a.", ".a", "a{1}"] {
            assert!(
                matches!(
                    SqlValue::from_text(bad, &SqlType::Ltree),
                    Err(RelError::Syntax(_))
                ),
                "{bad:?} should be an ltree syntax error"
            );
        }
    }

    #[test]
    fn ltree_orders_by_label_sequence() {
        // PG ORDER BY: a < a.b < a.b.c < a-b < aa < b.c
        let mut paths = ["b.c", "a", "a.b", "a.b.c", "aa", "a-b"]
            .map(|p| SqlValue::Ltree(p.into()))
            .to_vec();
        paths.sort_by(|a, b| a.compare(b).unwrap());
        let sorted: Vec<String> = paths.iter().map(|p| p.to_text().unwrap()).collect();
        assert_eq!(sorted, ["a", "a.b", "a.b.c", "a-b", "aa", "b.c"]);
        // index_key byte order agrees with compare() for range scans.
        let a = SqlValue::Ltree("a.b".into());
        let b = SqlValue::Ltree("a-b".into());
        assert_eq!(a.compare(&b), Some(Ordering::Less));
        assert!(a.index_key() < b.index_key());
    }

    #[test]
    fn cube_text_forms_match_pg() {
        // PG-verified input/output pairs.
        for (input, output) in [
            ("1", "(1)"),
            ("1,2", "(1, 2)"),
            ("(1,2)", "(1, 2)"),
            ("(1,2),(3,4)", "(1, 2),(3, 4)"),
            ("(1,2),(1,2)", "(1, 2)"), // coincident corners print as a point
            ("(3),(1)", "(3),(1)"),    // corner order is preserved
            ("(0.5, -1e2)", "(0.5, -100)"),
            ("[(1),(2)]", "(1),(2)"),
        ] {
            let v = SqlValue::from_text(input, &SqlType::Cube).unwrap();
            assert_eq!(v.to_text().unwrap(), output, "cube {input:?}");
        }
        // PG: '(1,2),(3)'::cube => 22P02 (different point dimensions).
        for bad in ["(1,2),(3)", "x", "", "(1,2"] {
            assert!(
                matches!(
                    SqlValue::from_text(bad, &SqlType::Cube),
                    Err(RelError::InvalidTextRepresentation { .. })
                ),
                "{bad:?} should be a cube input error"
            );
        }
    }

    #[test]
    fn cube_comparison_normalizes_corners() {
        // PG: SELECT '(3),(1)'::cube = '(1),(3)'::cube => t
        let a = SqlValue::from_text("(3),(1)", &SqlType::Cube).unwrap();
        let b = SqlValue::from_text("(1),(3)", &SqlType::Cube).unwrap();
        assert_eq!(a.sql_eq(&b), Some(true));
        // PG: '(1)'::cube = '(1,0)'::cube => f (dimension is the tiebreak)
        let c = SqlValue::from_text("(1)", &SqlType::Cube).unwrap();
        let d = SqlValue::from_text("(1,0)", &SqlType::Cube).unwrap();
        assert_eq!(c.sql_eq(&d), Some(false));
        // PG ORDER BY: (0, 5) < (1) < (1),(3) < (2)
        let mut cubes = ["(2)", "(1)", "(1),(3)", "(0,5)"]
            .map(|c| SqlValue::from_text(c, &SqlType::Cube).unwrap())
            .to_vec();
        cubes.sort_by(|a, b| a.compare(b).unwrap());
        let sorted: Vec<String> = cubes.iter().map(|c| c.to_text().unwrap()).collect();
        assert_eq!(sorted, ["(0, 5)", "(1)", "(1),(3)", "(2)"]);
    }

    #[test]
    fn cube_json_round_trip() {
        let v = SqlValue::from_text("(3),(1)", &SqlType::Cube).unwrap();
        let back = SqlValue::decode_json(&v.encode_json(), &SqlType::Cube).unwrap();
        assert_eq!(back.to_text().unwrap(), "(3),(1)");
    }

    #[test]
    fn extension_value_casts() {
        let h = SqlValue::Text("a=>1".into())
            .cast(&SqlType::HStore)
            .unwrap();
        assert!(matches!(h, SqlValue::HStore(_)));
        let l = SqlValue::Text("a.b".into()).cast(&SqlType::Ltree).unwrap();
        assert_eq!(l.cast(&SqlType::Text).unwrap().to_text().unwrap(), "a.b");
        assert!(SqlValue::Text("a b".into()).cast(&SqlType::Ltree).is_err());
        let c = SqlValue::Text("(1,2)".into()).cast(&SqlType::Cube).unwrap();
        assert_eq!(c.to_text().unwrap(), "(1, 2)");
        assert!(SqlValue::Int4(1).cast(&SqlType::HStore).is_err());
    }
}

// Maintenance note 11: documents compatibility expectations without changing runtime behavior.

// Maintenance note 23: documents compatibility expectations without changing runtime behavior.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// SQL compatibility note 12: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.

// SQL compatibility note 12: preserves documented behavior for window functions, recursive CTE validation, SQLSTATE mapping, and aggregate correctness without changing runtime semantics.
