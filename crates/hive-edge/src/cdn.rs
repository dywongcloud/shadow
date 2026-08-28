//! CDN edge cache, modeled on Vercel's CDN
//! (<https://vercel.com/docs/how-vercel-cdn-works>, `/docs/caching/cdn-cache`):
//!
//! * Cache states surfaced via `x-hive-cache`: **HIT / MISS / STALE / REVALIDATED**
//!   (mirrors Vercel's `x-vercel-cache`).
//! * **stale-while-revalidate** — a fresh-but-past-max-age-within-SWR response is
//!   served immediately as `STALE` while an async refresh runs.
//! * Header precedence for cache directives:
//!   `Vercel-CDN-Cache-Control` > `CDN-Cache-Control` > `Cache-Control`.

use hive_core::now_ms;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheState {
    Hit,
    Miss,
    Stale,
    Revalidated,
}

impl CacheState {
    pub fn header(self) -> &'static str {
        match self {
            CacheState::Hit => "HIT",
            CacheState::Miss => "MISS",
            CacheState::Stale => "STALE",
            CacheState::Revalidated => "REVALIDATED",
        }
    }
}

#[derive(Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Past this, the entry is stale (max-age boundary).
    fresh_until_ms: u64,
    /// Past this, the entry is unusable even as stale (max-age + SWR).
    usable_until_ms: u64,
}

/// Result of a cache lookup.
pub enum Lookup {
    /// Fresh hit — serve directly.
    Hit(CachedResponse),
    /// Stale-but-usable — serve now, refresh in the background.
    Stale(CachedResponse),
    /// Nothing usable.
    Miss,
}

pub struct CdnCache {
    map: Mutex<HashMap<String, CachedResponse>>,
    hits: AtomicU64,
    misses: AtomicU64,
    stale: AtomicU64,
    stores: AtomicU64,
    max_entries: usize,
}

impl CdnCache {
    pub fn new() -> CdnCache {
        CdnCache {
            map: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            max_entries: 10_000,
        }
    }

    pub fn key(host: &str, path_q: &str) -> String {
        format!("{host}{path_q}")
    }

    /// Look up an entry, classifying it as Hit / Stale / Miss.
    pub fn lookup(&self, key: &str) -> Lookup {
        let now = now_ms();
        let mut map = self.map.lock();
        if let Some(e) = map.get(key) {
            if now < e.fresh_until_ms {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Lookup::Hit(e.clone());
            }
            if now < e.usable_until_ms {
                self.stale.fetch_add(1, Ordering::Relaxed);
                return Lookup::Stale(e.clone());
            }
            map.remove(key); // fully expired
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        Lookup::Miss
    }

    /// Mark an entry refreshed after a stale-while-revalidate background fetch.
    pub fn note_revalidated(&self) {
        // Accounting handled via store(); this is a hook for symmetry/tests.
    }

    /// Cache a 200 GET response if its (precedence-resolved) directives allow.
    /// Returns true if stored.
    pub fn maybe_store(
        &self,
        key: &str,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> bool {
        if status != 200 {
            return false;
        }
        // Integrity gates: a captured body that is provably not the response
        // the origin declared must NEVER enter the cache — an origin instance
        // dying mid-stream once otherwise poisons an immutable entry that
        // every later client replays. Witnessed live (nodes-wtf 2026-08-26):
        // a 59-byte truncated prefix of a 6142-byte gzip chunk was cached as
        // a complete `Content-Encoding: gzip` 200 and served to every browser
        // as ERR_CONTENT_DECODING_FAILED until the next process restart.
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        if let Some(declared) = header("content-length").and_then(|v| v.trim().parse::<u64>().ok())
        {
            if declared != body.len() as u64 {
                return false;
            }
        }
        if header("content-encoding").is_some_and(|v| v.eq_ignore_ascii_case("gzip")) {
            // Full decode, bounded by the body we already hold in memory: the
            // only proof a gzip stream is complete is decoding it to its end.
            use std::io::Read;
            let mut decoder = flate2::read::GzDecoder::new(body);
            let mut sink = [0_u8; 16 * 1024];
            loop {
                match decoder.read(&mut sink) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
        }
        let Some((ttl, swr)) = cache_policy(headers) else {
            return false;
        };
        // Cacheable if fresh for a while OR usable as stale-while-revalidate.
        if ttl == 0 && swr == 0 {
            return false;
        }
        let now = now_ms();
        let mut map = self.map.lock();
        if map.len() >= self.max_entries {
            map.clear();
        }
        map.insert(
            key.to_string(),
            CachedResponse {
                status,
                headers: headers.to_vec(),
                body: body.to_vec(),
                fresh_until_ms: now + ttl * 1000,
                usable_until_ms: now + (ttl + swr) * 1000,
            },
        );
        self.stores.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn purge(&self) {
        self.map.lock().clear();
    }

    /// (hits, misses, stale, stored_entries, hit_ratio).
    pub fn stats(&self) -> (u64, u64, u64, usize, f64) {
        let h = self.hits.load(Ordering::Relaxed);
        let m = self.misses.load(Ordering::Relaxed);
        let s = self.stale.load(Ordering::Relaxed);
        let total = h + m + s;
        let ratio = if total > 0 {
            (h + s) as f64 / total as f64
        } else {
            0.0
        };
        (h, m, s, self.map.lock().len(), ratio)
    }
}

impl Default for CdnCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `(max-age, stale-while-revalidate)` seconds using Vercel's header
/// precedence: `Vercel-CDN-Cache-Control` > `CDN-Cache-Control` > `Cache-Control`.
/// Returns None if not cacheable.
pub fn cache_policy(headers: &[(String, String)]) -> Option<(u64, u64)> {
    for name in [
        "vercel-cdn-cache-control",
        "cdn-cache-control",
        "cache-control",
    ] {
        if let Some(v) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.to_lowercase())
        {
            if v.contains("no-store") || v.contains("private") || v.contains("no-cache") {
                return None;
            }
            let max_age = directive(&v, "s-maxage=").or_else(|| directive(&v, "max-age="));
            if let Some(ttl) = max_age {
                let swr = directive(&v, "stale-while-revalidate=").unwrap_or(0);
                return Some((ttl, swr));
            }
        }
    }
    None
}

fn directive(cc: &str, key: &str) -> Option<u64> {
    for token in cc.split(',') {
        let token = token.trim();
        if let Some(rest) = token.strip_prefix(key) {
            if let Ok(n) = rest.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_states() {
        let cdn = CdnCache::new();
        let k = CdnCache::key("app", "/a");
        assert!(matches!(cdn.lookup(&k), Lookup::Miss));
        // Vercel-CDN-Cache-Control wins over Cache-Control.
        let stored = cdn.maybe_store(
            &k,
            200,
            &[
                ("cache-control".into(), "max-age=0".into()),
                ("vercel-cdn-cache-control".into(), "max-age=60".into()),
            ],
            b"x",
        );
        assert!(stored);
        assert!(matches!(cdn.lookup(&k), Lookup::Hit(_)));
    }

    #[test]
    fn stale_while_revalidate() {
        let cdn = CdnCache::new();
        let k = CdnCache::key("app", "/swr");
        // max-age=0 but SWR=60 -> immediately stale-but-usable.
        cdn.maybe_store(
            &k,
            200,
            &[(
                "cache-control".into(),
                "max-age=0, stale-while-revalidate=60".into(),
            )],
            b"body",
        );
        match cdn.lookup(&k) {
            Lookup::Stale(r) => assert_eq!(r.body, b"body"),
            _ => panic!("expected STALE"),
        }
    }

    #[test]
    fn no_store_not_cached() {
        let cdn = CdnCache::new();
        let k = CdnCache::key("app", "/api");
        assert!(!cdn.maybe_store(
            &k,
            200,
            &[("cache-control".into(), "no-store".into())],
            b"x"
        ));
        assert!(matches!(cdn.lookup(&k), Lookup::Miss));
    }
}
