//! CDN edge cache: HIT/MISS/STALE/REVALIDATED via `x-hive-cache`, stale-while-revalidate, and
//! directive precedence `Vercel-CDN-Cache-Control` > `CDN-Cache-Control` > `Cache-Control`.

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
    /// Epoch-ms before which a failed revalidation is not retried; 0 = no pending backoff.
    retry_not_before_ms: u64,
}

impl CachedResponse {
    /// True while this entry's failed-revalidation backoff has not elapsed yet.
    pub fn revalidation_backoff_active(&self) -> bool {
        now_ms() < self.retry_not_before_ms
    }
}

/// Statuses that count as a successful revalidation; any other status, network error or corrupt
/// body keeps the stale entry untouched and arms `REVALIDATION_FAILURE_BACKOFF_MS`.
const REVALIDATION_SUCCESS_STATUSES: [u16; 7] = [200, 301, 302, 307, 308, 404, 410];
const REVALIDATION_FAILURE_BACKOFF_MS: u64 = 30_000;

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

    /// Cache identity for one request: the deployment the host resolves to, then host + path.
    ///
    /// The deployment dimension is load-bearing, not decoration. A cached body is only meaningful
    /// for the deployment that produced it: an HTML document embeds BUILD-SCOPED asset names
    /// (`/_next/static/chunks/<content-hash>.js`), so once a promotion swaps the deployment behind
    /// a hostname, the predecessor's HTML references chunk names the new build never emitted and
    /// every one of them 404s (`ChunkLoadError`). With the deployment in the key, a promoted
    /// deployment simply MISSES its predecessor's entries and refetches — which is what makes this
    /// the right site for the fix: it needs no purge, no invalidation call site and no cross-node
    /// fanout, because every node's cache is independent and each one derives the same identity
    /// from the same live alias table. A stale entry can therefore never be *read*, only aged out.
    ///
    /// `deployment` is `None` for a host with no deployment record; those share one namespace, so
    /// the dimension never changes the identity of traffic the gateway cannot attribute anyway.
    pub fn key(host: &str, path_q: &str, deployment: Option<&str>) -> String {
        // US (0x1f) cannot occur in a host or a request-target, so the split is unambiguous.
        format!("{}\u{1f}{host}{path_q}", deployment.unwrap_or("-"))
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
            map.remove(key);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        Lookup::Miss
    }

    /// Mark an entry refreshed after a stale-while-revalidate background fetch; the accounting
    /// itself happens in `maybe_store`.
    pub fn note_revalidated(&self) {}

    /// Cache a GET response whose status is a revalidation success, whose body matches its headers
    /// and whose directives allow it; anything else is a failed revalidation (entry preserved,
    /// backoff armed). Returns true if stored.
    pub fn maybe_store(
        &self,
        key: &str,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> bool {
        if !REVALIDATION_SUCCESS_STATUSES.contains(&status) {
            self.note_revalidation_failed(key);
            return false;
        }
        if !body_matches_declared(headers, body) {
            self.note_revalidation_failed(key);
            return false;
        }
        let Some((ttl, swr)) = cache_policy(headers) else {
            return false;
        };
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
                retry_not_before_ms: 0,
            },
        );
        self.stores.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Record a failed revalidation: the existing entry's body/headers/TTL stay untouched and only
    /// its retry backoff advances; a no-op when `key` has no entry.
    pub fn note_revalidation_failed(&self, key: &str) {
        let mut map = self.map.lock();
        if let Some(entry) = map.get_mut(key) {
            entry.retry_not_before_ms = now_ms() + REVALIDATION_FAILURE_BACKOFF_MS;
        }
    }

    /// Drop ONE entry. Used when a response stops being servable at its key — a
    /// protected deployment must not keep a body cached before protection
    /// applied, and `purge()` would discard the whole node's cache to do it.
    /// Returns whether an entry was present.
    pub fn invalidate(&self, key: &str) -> bool {
        self.map.lock().remove(key).is_some()
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

/// A body whose length or gzip stream disagrees with its headers is a truncated capture, never a
/// cacheable response.
fn body_matches_declared(headers: &[(String, String)], body: &[u8]) -> bool {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    if let Some(declared) = header("content-length").and_then(|v| v.trim().parse::<u64>().ok()) {
        if declared != body.len() as u64 {
            return false;
        }
    }
    if header("content-encoding").is_some_and(|v| v.eq_ignore_ascii_case("gzip")) {
        return gzip_decodes_to_end(body);
    }
    true
}

fn gzip_decodes_to_end(body: &[u8]) -> bool {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(body);
    let mut sink = [0_u8; 16 * 1024];
    loop {
        match decoder.read(&mut sink) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// Resolve `(max-age, stale-while-revalidate)` seconds with precedence `Vercel-CDN-Cache-Control`
/// > `CDN-Cache-Control` > `Cache-Control`; None = not cacheable.
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
