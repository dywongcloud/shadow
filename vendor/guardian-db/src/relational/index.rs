//! In-memory secondary index structures and key derivation.
//!
//! Indexes are real, ordered structures (BTree-backed) that the executor consults
//! for point and range lookups and that enforce uniqueness. They are maintained by
//! the engine alongside each table's materialized view and rebuilt from storage on
//! refresh, mirroring GuardianDB's local-first "refresh then operate" model.

use crate::relational::value::SqlValue;
use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;

/// Separator between composite key components. `0x1f` (unit separator) cannot
/// appear in the textual key forms produced by [`SqlValue::index_key`].
const SEP: char = '\u{1f}';

/// Build a composite index key from the ordered key column values.
///
/// Returns `None` when any component is SQL NULL, signalling that the row should
/// not participate in unique enforcement (PostgreSQL treats NULLs as distinct).
pub fn composite_key(values: &[SqlValue]) -> Option<String> {
    if values.iter().any(|v| v.is_null()) {
        return None;
    }
    let mut out = String::with_capacity(values.len() * 16);
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(SEP);
        }
        v.write_index_key(&mut out);
    }
    Some(out)
}

/// A composite key that *includes* NULLs (used for ordered range scans where NULL
/// ordering matters). NULLs sort last (PostgreSQL default for ASC).
pub fn ordered_key(values: &[SqlValue]) -> String {
    let mut out = String::with_capacity(values.len() * 16);
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(SEP);
        }
        if v.is_null() {
            out.push('\u{10fffe}'); // sorts after any normal key component
        } else {
            v.write_index_key(&mut out);
        }
    }
    out
}

/// An ordered secondary index: composite key -> set of row ids.
#[derive(Debug, Default, Clone)]
pub struct SecondaryIndex {
    /// key -> row ids, kept ascending.
    ///
    /// Was `BTreeMap<String, BTreeSet<String>>`. Rebuilding that per statement
    /// allocated a tree node per key AND per (key, row) pair, with a fresh
    /// `String` key for every row of every index — measured ~0.47 ms per row on
    /// a 21.5k-row table, most of it this. musql's row store indexes are exactly
    /// this shape (`rsEqIndex{ rows map[int64][]uint64 }` in
    /// engine/row_store_eqindex.go): a hash map to a flat row-id vector, with
    /// order produced by sorting when it is needed rather than maintained on
    /// every insert. Nothing in the production query path needs ordered
    /// iteration — `range` below has no callers outside this file's tests.
    map: HashMap<String, Vec<String>>,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
}

impl SecondaryIndex {
    pub fn new(unique: bool) -> Self {
        Self {
            map: HashMap::new(),
            unique,
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn insert(&mut self, key: String, row_id: String) {
        let v = self.map.entry(key).or_default();
        // Sorted + deduped, preserving BTreeSet semantics; binary search keeps
        // the per-row cost at O(log n) with one small memmove instead of an
        // allocation per member.
        if let Err(at) = v.binary_search(&row_id) {
            v.insert(at, row_id);
        }
    }

    pub fn remove(&mut self, key: &str, row_id: &str) {
        let mut drop_key = false;
        if let Some(v) = self.map.get_mut(key) {
            if let Ok(at) = v.binary_search(&row_id.to_string()) {
                v.remove(at);
            }
            drop_key = v.is_empty();
        }
        if drop_key {
            self.map.remove(key);
        }
    }

    /// Row ids for an exact key match.
    ///
    /// Returns a reference to the internal set (or a shared empty set) to avoid
    /// cloning on the hot path — callers that just iterate or check `.len()` pay
    /// no allocation cost.
    pub fn get(&self, key: &str) -> &[String] {
        static EMPTY: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        self.map
            .get(key)
            .map(Vec::as_slice)
            .unwrap_or_else(|| EMPTY.get_or_init(Vec::new).as_slice())
    }

    /// Returns the row id currently occupying `key`, if any (for unique checks).
    pub fn unique_occupant(&self, key: &str) -> Option<String> {
        self.map.get(key).and_then(|v| v.first().cloned())
    }

    /// Row ids whose key falls within `[lo, hi]`.
    /// Row ids whose key falls within `[lo, hi]`.
    ///
    /// Now a scan-and-filter, since the map is no longer ordered. That is the
    /// same trade musql makes: its indexes serve equality (`rsEqIndex`) and
    /// ordering is produced by sorting the flat row-id vector when a query
    /// needs it. There are no production callers of this method — only this
    /// file's tests — so no query path pays the difference.
    pub fn range(&self, lo: Bound<String>, hi: Bound<String>) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for (key, rows) in &self.map {
            let in_lo = match &lo {
                Bound::Unbounded => true,
                Bound::Included(a) => key >= a,
                Bound::Excluded(a) => key > a,
            };
            let in_hi = match &hi {
                Bound::Unbounded => true,
                Bound::Included(b) => key <= b,
                Bound::Excluded(b) => key < b,
            };
            if in_lo && in_hi {
                out.extend(rows.iter().cloned());
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_key_skips_nulls() {
        assert!(composite_key(&[SqlValue::Int4(1), SqlValue::Null]).is_none());
        assert!(composite_key(&[SqlValue::Int4(1), SqlValue::Int4(2)]).is_some());
    }

    #[test]
    fn secondary_index_point_lookup() {
        let mut idx = SecondaryIndex::new(false);
        let k = composite_key(&[SqlValue::Text("a".into())]).unwrap();
        idx.insert(k.clone(), "row1".into());
        idx.insert(k.clone(), "row2".into());
        assert_eq!(idx.get(&k).len(), 2);
        idx.remove(&k, "row1");
        assert_eq!(idx.get(&k).len(), 1);
    }

    #[test]
    fn secondary_index_range() {
        let mut idx = SecondaryIndex::new(false);
        for n in 1..=5 {
            let k = ordered_key(&[SqlValue::Int4(n)]);
            idx.insert(k, format!("row{n}"));
        }
        let lo = ordered_key(&[SqlValue::Int4(2)]);
        let hi = ordered_key(&[SqlValue::Int4(4)]);
        let got = idx.range(Bound::Included(lo), Bound::Included(hi));
        assert_eq!(got.len(), 3);
    }
}
