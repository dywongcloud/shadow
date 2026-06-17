//! Request monitoring — rolls observed events into per-minute time buckets so
//! the dashboard can draw requests-over-time, error-rate, cache and edge-action
//! charts without re-scanning the raw event ring on every poll.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;

const BUCKET_MS: u64 = 60_000; // 1 minute
const MAX_BUCKETS: usize = 180; // 3 hours of history

#[derive(Clone, Default, Serialize)]
pub struct Bucket {
    pub t_ms: u64,
    pub requests: u64,
    pub errors: u64,   // status >= 500
    pub client_err: u64, // 4xx
    pub blocked: u64,  // waf-deny + bot-block + throttled
    pub cache_hits: u64,
    pub cache_miss: u64,
    /// Per-project request counts in this bucket.
    #[serde(skip)]
    pub by_project: HashMap<String, u64>,
}

pub struct MetricsStore {
    // bucket start ms -> bucket
    buckets: RwLock<std::collections::BTreeMap<u64, Bucket>>,
    // path -> count (rolling, for "top paths")
    paths: RwLock<HashMap<String, u64>>,
    status_classes: RwLock<HashMap<String, u64>>, // "2xx","3xx","4xx","5xx"
}

impl MetricsStore {
    pub fn new() -> MetricsStore {
        MetricsStore {
            buckets: RwLock::new(std::collections::BTreeMap::new()),
            paths: RwLock::new(HashMap::new()),
            status_classes: RwLock::new(HashMap::new()),
        }
    }

    pub fn record(&self, ev: &crate::state::Event) {
        let t = (ev.ts_ms / BUCKET_MS) * BUCKET_MS;
        {
            let mut b = self.buckets.write();
            let entry = b.entry(t).or_insert_with(|| Bucket { t_ms: t, ..Default::default() });
            entry.requests += 1;
            if ev.status >= 500 {
                entry.errors += 1;
            } else if (400..500).contains(&ev.status) {
                entry.client_err += 1;
            }
            match ev.action.as_str() {
                "waf-deny" | "bot-block" | "throttled" => entry.blocked += 1,
                "cache-hit" | "cache-stale" => entry.cache_hits += 1,
                "cache-store" | "cache-revalidate" => entry.cache_miss += 1,
                _ => {}
            }
            if !ev.project.is_empty() {
                *entry.by_project.entry(ev.project.clone()).or_insert(0) += 1;
            }
            // Evict old buckets.
            while b.len() > MAX_BUCKETS {
                let oldest = *b.keys().next().unwrap();
                b.remove(&oldest);
            }
        }
        if ev.status > 0 {
            let class = format!("{}xx", ev.status / 100);
            *self.status_classes.write().entry(class).or_insert(0) += 1;
        }
        if !ev.path.is_empty() {
            let mut p = self.paths.write();
            *p.entry(ev.path.clone()).or_insert(0) += 1;
            if p.len() > 500 {
                // Keep the map bounded: drop the smallest entries occasionally.
                let mut items: Vec<(String, u64)> = p.drain().collect();
                items.sort_by(|a, b| b.1.cmp(&a.1));
                items.truncate(200);
                *p = items.into_iter().collect();
            }
        }
    }

    /// Time series for the last `minutes`, optionally filtered to a project.
    pub fn series(&self, minutes: usize, now_ms: u64, project: Option<&str>) -> Vec<Bucket> {
        let start = now_ms.saturating_sub((minutes as u64) * BUCKET_MS);
        let b = self.buckets.read();
        let mut out: Vec<Bucket> = Vec::with_capacity(minutes);
        // Fill every minute slot (zero-filled) so charts have continuous x-axis.
        let first = (start / BUCKET_MS) * BUCKET_MS;
        let last = (now_ms / BUCKET_MS) * BUCKET_MS;
        let mut t = first;
        while t <= last {
            match b.get(&t) {
                Some(bk) => {
                    if let Some(p) = project {
                        let reqs = bk.by_project.get(p).copied().unwrap_or(0);
                        out.push(Bucket { t_ms: t, requests: reqs, ..Default::default() });
                    } else {
                        let mut clone = bk.clone();
                        clone.by_project = HashMap::new();
                        out.push(clone);
                    }
                }
                None => out.push(Bucket { t_ms: t, ..Default::default() }),
            }
            t += BUCKET_MS;
        }
        out
    }

    pub fn status_distribution(&self) -> HashMap<String, u64> {
        self.status_classes.read().clone()
    }

    pub fn top_paths(&self, n: usize) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = self.paths.read().iter().map(|(k, c)| (k.clone(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }
}

impl Default for MetricsStore {
    fn default() -> Self {
        Self::new()
    }
}
