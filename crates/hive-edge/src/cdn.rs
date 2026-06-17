//! CDN edge cache — cache cacheable responses at the edge keyed by host+path,
//! honoring `Cache-Control: max-age` / `s-maxage`. Reports hit/miss like a real
//! CDN (`x-hive-cache: HIT|MISS`).

use hive_core::now_ms;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    expires_ms: u64,
}

pub struct CdnCache {
    map: Mutex<HashMap<String, CachedResponse>>,
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
    max_entries: usize,
}

impl CdnCache {
    pub fn new() -> CdnCache {
        CdnCache {
            map: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            max_entries: 10_000,
        }
    }

    pub fn key(host: &str, path_q: &str) -> String {
        format!("{host}{path_q}")
    }

    /// Look up a fresh cached response, counting hit/miss.
    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        let now = now_ms();
        let mut map = self.map.lock();
        if let Some(e) = map.get(key) {
            if e.expires_ms > now {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(e.clone());
            }
            map.remove(key); // expired
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Cache a response if its headers say it's cacheable (only for GET 200).
    pub fn maybe_store(
        &self,
        key: &str,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) {
        if status != 200 {
            return;
        }
        let Some(ttl) = cache_ttl_secs(headers) else { return };
        if ttl == 0 {
            return;
        }
        let mut map = self.map.lock();
        if map.len() >= self.max_entries {
            map.clear(); // crude eviction; fine for a study
        }
        map.insert(
            key.to_string(),
            CachedResponse {
                status,
                headers: headers.to_vec(),
                body: body.to_vec(),
                expires_ms: now_ms() + ttl * 1000,
            },
        );
        self.stores.fetch_add(1, Ordering::Relaxed);
    }

    pub fn purge(&self) {
        self.map.lock().clear();
    }

    /// (hits, misses, stored_entries, hit_ratio).
    pub fn stats(&self) -> (u64, u64, usize, f64) {
        let h = self.hits.load(Ordering::Relaxed);
        let m = self.misses.load(Ordering::Relaxed);
        let total = h + m;
        let ratio = if total > 0 { h as f64 / total as f64 } else { 0.0 };
        (h, m, self.map.lock().len(), ratio)
    }
}

impl Default for CdnCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a TTL (seconds) from Cache-Control, preferring `s-maxage` then
/// `max-age`. Returns None if not cacheable (no-store/private/absent).
fn cache_ttl_secs(headers: &[(String, String)]) -> Option<u64> {
    let cc = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
        .map(|(_, v)| v.to_lowercase())?;
    if cc.contains("no-store") || cc.contains("private") || cc.contains("no-cache") {
        return None;
    }
    for token in cc.split(',') {
        let token = token.trim();
        for key in ["s-maxage=", "max-age="] {
            if let Some(rest) = token.strip_prefix(key) {
                if let Ok(n) = rest.trim().parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_per_cache_control() {
        let cdn = CdnCache::new();
        let k = CdnCache::key("app.localhost", "/style.css");
        assert!(cdn.get(&k).is_none()); // miss
        cdn.maybe_store(
            &k,
            200,
            &[("cache-control".into(), "public, max-age=60".into())],
            b"body",
        );
        let hit = cdn.get(&k).expect("should be cached");
        assert_eq!(hit.body, b"body");
        // no-store is not cached
        let k2 = CdnCache::key("app.localhost", "/api");
        cdn.maybe_store(&k2, 200, &[("cache-control".into(), "no-store".into())], b"x");
        assert!(cdn.get(&k2).is_none());
    }
}
