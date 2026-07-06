//! Request monitoring — rolls observed events into per-minute time buckets so
//! the dashboard can draw requests-over-time, error-rate, cache and edge-action
//! charts without re-scanning the raw event ring on every poll.
//!
//! TENANT-NATIVE: every series/paths/status counter is keyed by TENANT (the owning
//! team of the request's project). A tenant read (`Some(tenant)`) sees ONLY its own
//! traffic — no cross-tenant leak of request volumes, status codes or URL paths.
//! A `None` read aggregates across all tenants and is reserved for the platform
//! OPERATOR (owner ops console), never a tenant-facing endpoint.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

const BUCKET_MS: u64 = 60_000; // 1 minute
const MAX_BUCKETS: usize = 180; // 3 hours of history
/// Events with no owning project (host-rejected, platform-internal) are recorded
/// under this reserved tenant so they never count toward any real team's metrics
/// and are only visible in the global (operator) aggregate.
pub const SYSTEM_TENANT: &str = "__system__";

#[derive(Clone, Default, Serialize)]
pub struct Bucket {
    pub t_ms: u64,
    pub requests: u64,
    pub errors: u64,     // status >= 500
    pub client_err: u64, // 4xx
    pub blocked: u64,    // waf-deny + bot-block + throttled
    pub cache_hits: u64,
    pub cache_miss: u64,
    /// Per-project request counts in this bucket (within the owning tenant).
    #[serde(skip)]
    pub by_project: HashMap<String, u64>,
}

impl Bucket {
    /// Accumulate `other` into `self` (used to merge across tenants for the global
    /// operator aggregate). `t_ms` is preserved from `self`.
    fn add(&mut self, other: &Bucket) {
        self.requests += other.requests;
        self.errors += other.errors;
        self.client_err += other.client_err;
        self.blocked += other.blocked;
        self.cache_hits += other.cache_hits;
        self.cache_miss += other.cache_miss;
        for (p, n) in &other.by_project {
            *self.by_project.entry(p.clone()).or_insert(0) += n;
        }
    }
}

#[derive(Default)]
struct TenantMetrics {
    buckets: BTreeMap<u64, Bucket>,          // bucket start ms -> bucket
    paths: HashMap<String, u64>,             // path -> count (rolling, "top paths")
    status_classes: HashMap<String, u64>,    // "2xx".."5xx" -> count
}

pub struct MetricsStore {
    // tenant -> its metrics. Keying by tenant is what makes reads non-leaky.
    by_tenant: RwLock<HashMap<String, TenantMetrics>>,
}

impl MetricsStore {
    pub fn new() -> MetricsStore {
        MetricsStore { by_tenant: RwLock::new(HashMap::new()) }
    }

    /// Record an event under its owning `tenant` (the team that owns the request's
    /// project; [`SYSTEM_TENANT`] when there is no project).
    pub fn record(&self, ev: &crate::state::Event, tenant: &str) {
        let t = (ev.ts_ms / BUCKET_MS) * BUCKET_MS;
        let mut map = self.by_tenant.write();
        let tm = map.entry(tenant.to_string()).or_default();
        {
            let entry = tm.buckets.entry(t).or_insert_with(|| Bucket { t_ms: t, ..Default::default() });
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
            while tm.buckets.len() > MAX_BUCKETS {
                let oldest = *tm.buckets.keys().next().unwrap();
                tm.buckets.remove(&oldest);
            }
        }
        if ev.status > 0 {
            let class = format!("{}xx", ev.status / 100);
            *tm.status_classes.entry(class).or_insert(0) += 1;
        }
        if !ev.path.is_empty() {
            *tm.paths.entry(ev.path.clone()).or_insert(0) += 1;
            if tm.paths.len() > 500 {
                let mut items: Vec<(String, u64)> = tm.paths.drain().collect();
                items.sort_by(|a, b| b.1.cmp(&a.1));
                items.truncate(200);
                tm.paths = items.into_iter().collect();
            }
        }
    }

    /// Merge the per-timestamp buckets for a tenant (`Some`) or all tenants (`None`)
    /// into one timestamp->Bucket map. The core of every scoped read.
    fn merged_buckets(&self, tenant: Option<&str>) -> BTreeMap<u64, Bucket> {
        let map = self.by_tenant.read();
        let mut out: BTreeMap<u64, Bucket> = BTreeMap::new();
        let mut fold = |tm: &TenantMetrics| {
            for (t, bk) in &tm.buckets {
                out.entry(*t).or_insert_with(|| Bucket { t_ms: *t, ..Default::default() }).add(bk);
            }
        };
        match tenant {
            Some(t) => {
                if let Some(tm) = map.get(t) {
                    fold(tm);
                }
            }
            None => {
                for tm in map.values() {
                    fold(tm);
                }
            }
        }
        out
    }

    /// Time series for the last `minutes`, scoped to `tenant` (`None` = global,
    /// operator-only), optionally narrowed to a single project.
    pub fn series(&self, minutes: usize, now_ms: u64, tenant: Option<&str>, project: Option<&str>) -> Vec<Bucket> {
        let start = now_ms.saturating_sub((minutes as u64) * BUCKET_MS);
        let merged = self.merged_buckets(tenant);
        let first = (start / BUCKET_MS) * BUCKET_MS;
        let last = (now_ms / BUCKET_MS) * BUCKET_MS;
        let mut out: Vec<Bucket> = Vec::with_capacity(minutes);
        let mut t = first;
        while t <= last {
            match merged.get(&t) {
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

    /// Per-project request totals over the last `minutes`, scoped to `tenant`
    /// (`None` = global), sorted desc.
    pub fn project_totals(&self, minutes: usize, now_ms: u64, tenant: Option<&str>) -> Vec<(String, u64)> {
        let start = now_ms.saturating_sub((minutes as u64) * BUCKET_MS);
        let merged = self.merged_buckets(tenant);
        let mut totals: HashMap<String, u64> = HashMap::new();
        for (t, bucket) in merged.iter() {
            if *t < start {
                continue;
            }
            for (p, n) in &bucket.by_project {
                *totals.entry(p.clone()).or_insert(0) += n;
            }
        }
        let mut v: Vec<(String, u64)> = totals.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }

    /// Status-class distribution scoped to `tenant` (`None` = global).
    pub fn status_distribution(&self, tenant: Option<&str>) -> HashMap<String, u64> {
        let map = self.by_tenant.read();
        match tenant {
            Some(t) => map.get(t).map(|tm| tm.status_classes.clone()).unwrap_or_default(),
            None => {
                let mut out: HashMap<String, u64> = HashMap::new();
                for tm in map.values() {
                    for (k, n) in &tm.status_classes {
                        *out.entry(k.clone()).or_insert(0) += n;
                    }
                }
                out
            }
        }
    }

    /// Top request paths scoped to `tenant` (`None` = global).
    pub fn top_paths(&self, tenant: Option<&str>, n: usize) -> Vec<(String, u64)> {
        let map = self.by_tenant.read();
        let mut merged: HashMap<String, u64> = HashMap::new();
        match tenant {
            Some(t) => {
                if let Some(tm) = map.get(t) {
                    for (k, c) in &tm.paths {
                        *merged.entry(k.clone()).or_insert(0) += c;
                    }
                }
            }
            None => {
                for tm in map.values() {
                    for (k, c) in &tm.paths {
                        *merged.entry(k.clone()).or_insert(0) += c;
                    }
                }
            }
        }
        let mut v: Vec<(String, u64)> = merged.into_iter().collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Event;

    fn ev(project: &str, path: &str, status: u16) -> Event {
        Event {
            ts_ms: 1_000_000,
            region: "r".into(),
            method: "GET".into(),
            host: "h".into(),
            path: path.into(),
            status,
            action: "allow".into(),
            detail: String::new(),
            project: project.into(),
            request_id: String::new(),
        }
    }

    #[test]
    fn tenant_reads_never_leak_across_tenants() {
        let m = MetricsStore::new();
        m.record(&ev("a-proj", "/a", 200), "team-a");
        m.record(&ev("a-proj", "/a", 500), "team-a");
        m.record(&ev("b-proj", "/secret-b", 200), "team-b");

        // team-a sees ONLY its own paths + statuses + projects.
        let a_paths: Vec<String> = m.top_paths(Some("team-a"), 10).into_iter().map(|(p, _)| p).collect();
        assert!(a_paths.contains(&"/a".to_string()));
        assert!(!a_paths.contains(&"/secret-b".to_string()), "team-a must NOT see team-b paths");
        let a_projects: Vec<String> = m.project_totals(60, 1_000_000, Some("team-a")).into_iter().map(|(p, _)| p).collect();
        assert_eq!(a_projects, vec!["a-proj".to_string()], "team-a sees only its project");
        let a_series = m.series(60, 1_000_000, Some("team-a"), None);
        assert_eq!(a_series.iter().map(|b| b.requests).sum::<u64>(), 2);
        assert_eq!(a_series.iter().map(|b| b.errors).sum::<u64>(), 1);

        // Global (operator) aggregate sees everything.
        let g_paths: Vec<String> = m.top_paths(None, 10).into_iter().map(|(p, _)| p).collect();
        assert!(g_paths.contains(&"/a".to_string()) && g_paths.contains(&"/secret-b".to_string()));
        let g_series = m.series(60, 1_000_000, None, None);
        assert_eq!(g_series.iter().map(|b| b.requests).sum::<u64>(), 3);
    }
}
