//! Short-TTL server-side cache for expensive fleet-fan-out reads
//! (`/v1/metrics`, `/v1/workflows/runs`, `/v1/speed-insights`, etc.).
//!
//! The client already caches these paths (`ui/lib/api.ts`'s `PATH_TTL`), but
//! that only de-dupes repeat requests from the SAME browser tab. Multiple
//! open tabs, multiple team members viewing the same tenant, or the
//! dashboard's own co-mounted components each independently trigger a full
//! cross-node fan-out within the same few seconds — this cache collapses
//! those into one real fan-out per TTL window, tenant, and query shape.
//!
//! Deliberately NOT a `HashMap` behind the existing per-store locks: this is
//! cross-cutting (keyed by arbitrary path+tenant, not owned by any one
//! store), so it lives as its own small store on `CloudState`, the same
//! pattern as `MetricsStore`/`IncidentStore` etc.

use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Entry {
    at: Instant,
    value: Value,
}

pub struct ResponseCache {
    map: RwLock<HashMap<String, Entry>>,
}

impl ResponseCache {
    pub fn new() -> ResponseCache {
        ResponseCache {
            map: RwLock::new(HashMap::new()),
        }
    }

    /// A fresh (within `ttl`) cached value for `key`, if any.
    pub fn get(&self, key: &str, ttl: Duration) -> Option<Value> {
        let map = self.map.read();
        map.get(key)
            .filter(|e| e.at.elapsed() < ttl)
            .map(|e| e.value.clone())
    }

    /// Store `value` under `key`. Opportunistically sheds entries older than
    /// 60s once the map grows past a few thousand keys, so a long-running
    /// node with many distinct tenants/queries doesn't grow this unbounded —
    /// a cache this short-lived has no business holding stale entries anyway.
    pub fn set(&self, key: String, value: Value) {
        let mut map = self.map.write();
        map.insert(
            key,
            Entry {
                at: Instant::now(),
                value,
            },
        );
        if map.len() > 4_000 {
            map.retain(|_, e| e.at.elapsed() < Duration::from_secs(60));
        }
    }
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new()
    }
}
