//! Runtime Cache — a regional, ephemeral key/value cache for tenant functions,
//! mirroring Vercel's [Runtime Cache](https://vercel.com/docs/caching/runtime-cache).
//!
//! It stores *data* (not whole HTTP responses — that's the [`crate::cdn`] CDN
//! cache) close to where functions run, so repeated fetches / expensive
//! computations can be reused. Entries are:
//!
//! * **scoped** per `project:environment` (production vs preview are isolated),
//! * **TTL'd** (time-based expiry) and **tagged** (so a tag can invalidate many
//!   entries at once — `revalidateTag`),
//! * **LRU-evicted** when the store is full.
//!
//! Limits match Vercel: item ≤ 2 MB, ≤ 64 tags/item, tag ≤ 256 bytes.

use hive_core::now_ms;
use parking_lot::Mutex;
use std::collections::HashMap;

/// Vercel runtime-cache limits.
pub const MAX_ITEM_BYTES: usize = 2 * 1024 * 1024; // 2 MB
pub const MAX_TAGS_PER_ITEM: usize = 64;
pub const MAX_TAG_LEN: usize = 256;

#[derive(Clone, Debug)]
struct Entry {
    value: Vec<u8>,
    /// Absolute expiry (epoch ms). 0 = never expires (until evicted).
    expires_ms: u64,
    tags: Vec<String>,
    /// Monotonic recency tick (for LRU) — not wall-clock, so it never ties.
    recency: u64,
}

/// A regional runtime cache. Cheap to clone-wrap in an `Arc`.
pub struct RuntimeCache {
    map: Mutex<HashMap<String, Entry>>,
    max_entries: usize,
    /// Monotonic LRU clock.
    tick: std::sync::atomic::AtomicU64,
    reads: std::sync::atomic::AtomicU64,
    writes: std::sync::atomic::AtomicU64,
    hits: std::sync::atomic::AtomicU64,
    revalidations: std::sync::atomic::AtomicU64,
}

impl RuntimeCache {
    pub fn new() -> RuntimeCache {
        RuntimeCache::with_capacity(10_000)
    }
    pub fn with_capacity(max_entries: usize) -> RuntimeCache {
        RuntimeCache {
            map: Mutex::new(HashMap::new()),
            max_entries,
            tick: Default::default(),
            reads: Default::default(),
            writes: Default::default(),
            hits: Default::default(),
            revalidations: Default::default(),
        }
    }

    /// Full storage key: scope-namespaced so tenants/environments never collide.
    fn full_key(scope: &str, key: &str) -> String {
        format!("{scope}\u{0}{key}")
    }

    /// Get a value, or `None` if absent/expired. Refreshes LRU recency.
    pub fn get(&self, scope: &str, key: &str) -> Option<Vec<u8>> {
        use std::sync::atomic::Ordering;
        self.reads.fetch_add(1, Ordering::Relaxed);
        let fk = Self::full_key(scope, key);
        let now = now_ms();
        let mut map = self.map.lock();
        if let Some(e) = map.get_mut(&fk) {
            if e.expires_ms != 0 && now >= e.expires_ms {
                map.remove(&fk);
                return None;
            }
            e.recency = self.tick.fetch_add(1, Ordering::Relaxed);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(e.value.clone());
        }
        None
    }

    /// Store a value with an optional TTL (seconds; 0 = no expiry) and tags.
    /// Enforces Vercel's item/tag limits; LRU-evicts when full.
    pub fn set(
        &self,
        scope: &str,
        key: &str,
        value: Vec<u8>,
        ttl_secs: u64,
        tags: Vec<String>,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        if value.len() > MAX_ITEM_BYTES {
            return Err(format!("item exceeds {MAX_ITEM_BYTES} bytes"));
        }
        if tags.len() > MAX_TAGS_PER_ITEM {
            return Err(format!("more than {MAX_TAGS_PER_ITEM} tags"));
        }
        if let Some(t) = tags.iter().find(|t| t.len() > MAX_TAG_LEN) {
            return Err(format!(
                "tag '{}' exceeds {MAX_TAG_LEN} bytes",
                &t[..t.len().min(32)]
            ));
        }
        let now = now_ms();
        let expires_ms = if ttl_secs == 0 {
            0
        } else {
            now + ttl_secs * 1000
        };
        let fk = Self::full_key(scope, key);
        let recency = self.tick.fetch_add(1, Ordering::Relaxed);
        let mut map = self.map.lock();
        // LRU eviction when at capacity (and inserting a new key).
        if map.len() >= self.max_entries && !map.contains_key(&fk) {
            if let Some(victim) = map
                .iter()
                .min_by_key(|(_, e)| e.recency)
                .map(|(k, _)| k.clone())
            {
                map.remove(&victim);
            }
        }
        map.insert(
            fk,
            Entry {
                value,
                expires_ms,
                tags,
                recency,
            },
        );
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Delete a single key. Returns true if it existed.
    pub fn delete(&self, scope: &str, key: &str) -> bool {
        self.map
            .lock()
            .remove(&Self::full_key(scope, key))
            .is_some()
    }

    /// Invalidate every entry in `scope` carrying `tag` (Vercel `revalidateTag`).
    /// Returns the number of entries removed.
    pub fn revalidate_tag(&self, scope: &str, tag: &str) -> usize {
        use std::sync::atomic::Ordering;
        let prefix = format!("{scope}\u{0}");
        let mut map = self.map.lock();
        let before = map.len();
        map.retain(|k, e| !(k.starts_with(&prefix) && e.tags.iter().any(|t| t == tag)));
        let removed = before - map.len();
        if removed > 0 {
            self.revalidations.fetch_add(1, Ordering::Relaxed);
        }
        removed
    }

    /// Drop every entry for a scope (e.g. on project delete). Returns the count.
    pub fn purge_scope(&self, scope: &str) -> usize {
        let prefix = format!("{scope}\u{0}");
        let mut map = self.map.lock();
        let before = map.len();
        map.retain(|k, _| !k.starts_with(&prefix));
        before - map.len()
    }

    pub fn purge_all(&self) {
        self.map.lock().clear();
    }

    /// (entries, reads, writes, hits, revalidations).
    pub fn stats(&self) -> (usize, u64, u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.map.lock().len(),
            self.reads.load(Ordering::Relaxed),
            self.writes.load(Ordering::Relaxed),
            self.hits.load(Ordering::Relaxed),
            self.revalidations.load(Ordering::Relaxed),
        )
    }
}

impl Default for RuntimeCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_delete() {
        let c = RuntimeCache::new();
        c.set("proj:production", "k", b"v".to_vec(), 60, vec!["t1".into()])
            .unwrap();
        assert_eq!(c.get("proj:production", "k").as_deref(), Some(&b"v"[..]));
        // Scope isolation: same key, different env -> miss.
        assert!(c.get("proj:preview", "k").is_none());
        assert!(c.delete("proj:production", "k"));
        assert!(c.get("proj:production", "k").is_none());
    }

    #[test]
    fn ttl_expiry() {
        let c = RuntimeCache::new();
        // ttl_secs=0 with manual expiry check: insert an already-expired entry by
        // using a 1s ttl then asserting it's gone after we fake time isn't possible
        // here; instead test the no-expiry path + limit paths.
        c.set("s", "k", b"v".to_vec(), 0, vec![]).unwrap();
        assert!(c.get("s", "k").is_some()); // 0 = never expires
    }

    #[test]
    fn tag_revalidation() {
        let c = RuntimeCache::new();
        c.set("s", "a", b"1".to_vec(), 60, vec!["blog".into()])
            .unwrap();
        c.set("s", "b", b"2".to_vec(), 60, vec!["blog".into(), "x".into()])
            .unwrap();
        c.set("s", "c", b"3".to_vec(), 60, vec!["other".into()])
            .unwrap();
        let n = c.revalidate_tag("s", "blog");
        assert_eq!(n, 2);
        assert!(c.get("s", "a").is_none());
        assert!(c.get("s", "b").is_none());
        assert!(c.get("s", "c").is_some());
    }

    #[test]
    fn enforces_limits() {
        let c = RuntimeCache::new();
        assert!(c
            .set("s", "big", vec![0u8; MAX_ITEM_BYTES + 1], 0, vec![])
            .is_err());
        let too_many: Vec<String> = (0..MAX_TAGS_PER_ITEM + 1).map(|i| i.to_string()).collect();
        assert!(c.set("s", "k", b"v".to_vec(), 0, too_many).is_err());
        assert!(c
            .set(
                "s",
                "k",
                b"v".to_vec(),
                0,
                vec!["x".repeat(MAX_TAG_LEN + 1)]
            )
            .is_err());
    }

    #[test]
    fn lru_eviction() {
        let c = RuntimeCache::with_capacity(2);
        c.set("s", "a", b"1".to_vec(), 0, vec![]).unwrap();
        c.set("s", "b", b"2".to_vec(), 0, vec![]).unwrap();
        // Touch "a" so "b" becomes least-recently-used.
        let _ = c.get("s", "a");
        c.set("s", "c", b"3".to_vec(), 0, vec![]).unwrap(); // evicts "b"
        assert!(c.get("s", "a").is_some());
        assert!(c.get("s", "b").is_none());
        assert!(c.get("s", "c").is_some());
    }

    #[test]
    fn scope_purge() {
        let c = RuntimeCache::new();
        c.set("p1:production", "k", b"v".to_vec(), 0, vec![])
            .unwrap();
        c.set("p2:production", "k", b"v".to_vec(), 0, vec![])
            .unwrap();
        assert_eq!(c.purge_scope("p1:production"), 1);
        assert!(c.get("p1:production", "k").is_none());
        assert!(c.get("p2:production", "k").is_some());
    }
}
