//! Request monitoring — rolls observed events into time buckets at THREE
//! resolutions simultaneously (minute/hour/day) so the dashboard can draw a
//! consumption chart at Daily, Weekly or Monthly granularity without
//! re-scanning the raw event ring on every poll.
//!
//! TENANT-NATIVE: every series/paths/status counter is keyed by TENANT (the owning
//! team of the request's project). A tenant read (`Some(tenant)`) sees ONLY its own
//! traffic — no cross-tenant leak of request volumes, status codes or URL paths.
//! A `None` read aggregates across all tenants and is reserved for the platform
//! OPERATOR (owner ops console), never a tenant-facing endpoint.
//!
//! Minute buckets are NOT persisted (high-churn, short retention — refill within
//! minutes of a restart). Hour/day buckets ARE persisted via [`RollupSnapshot`]
//! (see `persist.rs`'s `metrics_rollup` field) — without this a Weekly/Monthly
//! view would silently empty out on every deploy restart, defeating the point of
//! having a longer-retention resolution at all.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const BUCKET_MS: u64 = 60_000; // 1 minute
const MAX_BUCKETS: usize = 1_440; // 24 hours of minute-resolution history ("Daily")
const HOUR_MS: u64 = 3_600_000; // 1 hour
const MAX_HOUR_BUCKETS: usize = 720; // 30 days of hour-resolution history ("Weekly")
const DAY_MS: u64 = 86_400_000; // 1 day
const MAX_DAY_BUCKETS: usize = 400; // ~13 months of day-resolution history ("Monthly")
/// Events with no owning project (host-rejected, platform-internal) are recorded
/// under this reserved tenant so they never count toward any real team's metrics
/// and are only visible in the global (operator) aggregate.
pub const SYSTEM_TENANT: &str = "__system__";

/// Time resolution a series/project-totals read is bucketed at. Each maps to an
/// independently-retained ring buffer inside [`TenantMetrics`] — NOT a
/// downsampling of the minute buckets (a 30-day Weekly window at minute
/// resolution would be 43,200 points per tenant; hour/day buckets are
/// accumulated directly in [`MetricsStore::record`] instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    Minute,
    Hour,
    Day,
}

impl Granularity {
    fn bucket_ms(self) -> u64 {
        match self {
            Granularity::Minute => BUCKET_MS,
            Granularity::Hour => HOUR_MS,
            Granularity::Day => DAY_MS,
        }
    }

    /// Retention ceiling for this resolution, in minutes — the largest `minutes`
    /// span `series`/`project_totals` can usefully answer (older data has already
    /// been evicted). Callers (`metrics_get`) clamp the requested span to this.
    pub fn max_span_minutes(self) -> usize {
        match self {
            Granularity::Minute => MAX_BUCKETS,
            Granularity::Hour => MAX_HOUR_BUCKETS * 60,
            Granularity::Day => MAX_DAY_BUCKETS * 24 * 60,
        }
    }

    /// Parse the `gran` query-string value (`metrics_get`'s `MetricsQ.gran`).
    /// Unrecognized/absent -> `Minute` (today's existing behavior), never an
    /// error — this is a best-effort display parameter, not a validated input.
    pub fn parse(s: Option<&str>) -> Granularity {
        match s {
            Some("hour") => Granularity::Hour,
            Some("day") => Granularity::Day,
            _ => Granularity::Minute,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Bucket {
    pub t_ms: u64,
    pub requests: u64,
    pub errors: u64,     // status >= 500
    pub client_err: u64, // 4xx
    pub blocked: u64,    // waf-deny + bot-block + throttled
    pub cache_hits: u64,
    pub cache_miss: u64,
    /// Per-project request counts in this bucket (within the owning tenant).
    /// Persisted for hour/day rollups (`RollupSnapshot`) — Weekly/Monthly's
    /// per-project breakdown needs this to survive a restart same as the totals
    /// do; `#[serde(skip)]` would silently zero every project's history on load.
    #[serde(default)]
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

    /// Fold one event's counters into this bucket (shared by all three
    /// resolutions in [`MetricsStore::record`] — same event, three bucket sizes).
    fn accumulate(&mut self, ev: &crate::state::Event) {
        self.requests += 1;
        if ev.status >= 500 {
            self.errors += 1;
        } else if (400..500).contains(&ev.status) {
            self.client_err += 1;
        }
        match ev.action.as_str() {
            "waf-deny" | "bot-block" | "throttled" => self.blocked += 1,
            "cache-hit" | "cache-stale" => self.cache_hits += 1,
            "cache-store" | "cache-revalidate" => self.cache_miss += 1,
            _ => {}
        }
        if !ev.project.is_empty() {
            *self.by_project.entry(ev.project.clone()).or_insert(0) += 1;
        }
    }
}

/// Insert/accumulate `ev` into `map`'s bucket for `t` (floored to `bucket_ms`),
/// then evict oldest entries past `max_buckets`. One call site per resolution
/// in [`MetricsStore::record`], and the sole eviction policy for all three —
/// changing retention only ever means changing the `MAX_*_BUCKETS` constant.
fn accumulate_bucket(map: &mut BTreeMap<u64, Bucket>, t: u64, ev: &crate::state::Event, max_buckets: usize) {
    map.entry(t).or_insert_with(|| Bucket { t_ms: t, ..Default::default() }).accumulate(ev);
    while map.len() > max_buckets {
        let oldest = *map.keys().next().unwrap();
        map.remove(&oldest);
    }
}

#[derive(Default, Serialize, Deserialize)]
struct TenantMetrics {
    buckets: BTreeMap<u64, Bucket>,          // bucket start ms -> bucket (1 min, NOT persisted)
    hour_buckets: BTreeMap<u64, Bucket>,     // bucket start ms -> bucket (1 hour, persisted)
    day_buckets: BTreeMap<u64, Bucket>,      // bucket start ms -> bucket (1 day, persisted)
    #[serde(skip)]
    paths: HashMap<String, u64>,             // path -> count (rolling, "top paths")
    #[serde(skip)]
    status_classes: HashMap<String, u64>,    // "2xx".."5xx" -> count
}

pub struct MetricsStore {
    // tenant -> its metrics. Keying by tenant is what makes reads non-leaky.
    by_tenant: RwLock<HashMap<String, TenantMetrics>>,
}

/// Persisted slice of [`MetricsStore`] — hour/day rollups only (see the module
/// doc comment for why minute buckets are excluded). Wired into
/// `persist.rs`'s `PlatformSnapshot.metrics_rollup` / `capture()` / `restore()`,
/// the exact same pattern as `databases.data_snapshot()`/`data_load()`.
#[derive(Default, Serialize, Deserialize)]
pub struct RollupSnapshot {
    // tenant -> (hour_buckets, day_buckets). paths/status_classes/minute
    // buckets are intentionally absent — see TenantMetrics's own field comments.
    by_tenant: HashMap<String, (BTreeMap<u64, Bucket>, BTreeMap<u64, Bucket>)>,
}

impl MetricsStore {
    pub fn new() -> MetricsStore {
        MetricsStore { by_tenant: RwLock::new(HashMap::new()) }
    }

    /// Snapshot the hour/day rollups for persistence (`persist.rs::capture`).
    pub fn rollup_snapshot(&self) -> RollupSnapshot {
        let map = self.by_tenant.read();
        RollupSnapshot {
            by_tenant: map
                .iter()
                .map(|(t, tm)| (t.clone(), (tm.hour_buckets.clone(), tm.day_buckets.clone())))
                .collect(),
        }
    }

    /// Restore hour/day rollups from a snapshot (`persist.rs::restore`, boot-time
    /// only — called once before any live traffic has been recorded, so a plain
    /// overwrite is correct; a later call would discard rollups accumulated since
    /// boot, which no caller does).
    pub fn rollup_load(&self, snap: RollupSnapshot) {
        let mut map = self.by_tenant.write();
        for (t, (hours, days)) in snap.by_tenant {
            let tm = map.entry(t).or_default();
            tm.hour_buckets = hours;
            tm.day_buckets = days;
        }
    }

    /// Record an event under its owning `tenant` (the team that owns the request's
    /// project; [`SYSTEM_TENANT`] when there is no project).
    pub fn record(&self, ev: &crate::state::Event, tenant: &str) {
        let minute_t = (ev.ts_ms / BUCKET_MS) * BUCKET_MS;
        let hour_t = (ev.ts_ms / HOUR_MS) * HOUR_MS;
        let day_t = (ev.ts_ms / DAY_MS) * DAY_MS;
        let mut map = self.by_tenant.write();
        let tm = map.entry(tenant.to_string()).or_default();
        accumulate_bucket(&mut tm.buckets, minute_t, ev, MAX_BUCKETS);
        accumulate_bucket(&mut tm.hour_buckets, hour_t, ev, MAX_HOUR_BUCKETS);
        accumulate_bucket(&mut tm.day_buckets, day_t, ev, MAX_DAY_BUCKETS);
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

    /// Merge the per-timestamp buckets at `gran` resolution for a tenant (`Some`)
    /// or all tenants (`None`) into one timestamp->Bucket map. The core of every
    /// scoped read.
    fn merged_buckets(&self, gran: Granularity, tenant: Option<&str>) -> BTreeMap<u64, Bucket> {
        let map = self.by_tenant.read();
        fn pick(gran: Granularity, tm: &TenantMetrics) -> &BTreeMap<u64, Bucket> {
            match gran {
                Granularity::Minute => &tm.buckets,
                Granularity::Hour => &tm.hour_buckets,
                Granularity::Day => &tm.day_buckets,
            }
        }
        let mut out: BTreeMap<u64, Bucket> = BTreeMap::new();
        let mut fold = |tm: &TenantMetrics| {
            for (t, bk) in pick(gran, tm) {
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

    /// Time series for the last `minutes` at `gran` resolution, scoped to
    /// `tenant` (`None` = global, operator-only), optionally narrowed to a
    /// single project. `minutes` is a SPAN regardless of `gran` (e.g. Weekly
    /// passes `minutes=10080` for a 7-day span, bucketed hourly) — callers
    /// (`metrics_get`) are responsible for clamping it to
    /// `gran.max_span_minutes()` beforehand.
    pub fn series(&self, gran: Granularity, minutes: usize, now_ms: u64, tenant: Option<&str>, project: Option<&str>) -> Vec<Bucket> {
        let bucket_ms = gran.bucket_ms();
        let start = now_ms.saturating_sub((minutes as u64) * BUCKET_MS);
        let merged = self.merged_buckets(gran, tenant);
        let first = (start / bucket_ms) * bucket_ms;
        let last = (now_ms / bucket_ms) * bucket_ms;
        let mut out: Vec<Bucket> = Vec::with_capacity(((last.saturating_sub(first)) / bucket_ms + 1) as usize);
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
            t += bucket_ms;
        }
        out
    }

    /// Per-project request totals over the last `minutes` at `gran` resolution,
    /// scoped to `tenant` (`None` = global), sorted desc. Same span-vs-gran
    /// contract as `series` (see its doc comment).
    pub fn project_totals(&self, gran: Granularity, minutes: usize, now_ms: u64, tenant: Option<&str>) -> Vec<(String, u64)> {
        let start = now_ms.saturating_sub((minutes as u64) * BUCKET_MS);
        let merged = self.merged_buckets(gran, tenant);
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
            deployment: String::new(),
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
        let a_projects: Vec<String> = m.project_totals(Granularity::Minute, 60, 1_000_000, Some("team-a")).into_iter().map(|(p, _)| p).collect();
        assert_eq!(a_projects, vec!["a-proj".to_string()], "team-a sees only its project");
        let a_series = m.series(Granularity::Minute, 60, 1_000_000, Some("team-a"), None);
        assert_eq!(a_series.iter().map(|b| b.requests).sum::<u64>(), 2);
        assert_eq!(a_series.iter().map(|b| b.errors).sum::<u64>(), 1);

        // Global (operator) aggregate sees everything.
        let g_paths: Vec<String> = m.top_paths(None, 10).into_iter().map(|(p, _)| p).collect();
        assert!(g_paths.contains(&"/a".to_string()) && g_paths.contains(&"/secret-b".to_string()));
        let g_series = m.series(Granularity::Minute, 60, 1_000_000, None, None);
        assert_eq!(g_series.iter().map(|b| b.requests).sum::<u64>(), 3);
    }

    /// Same tenant-isolation guarantee, at the two NEW resolutions the Weekly
    /// (hour) and Monthly (day) views read from — a regression here would leak
    /// team-b's traffic into team-a's Weekly/Monthly chart even though the
    /// existing minute-resolution test above stays green.
    #[test]
    fn hour_and_day_resolutions_never_leak_across_tenants() {
        let m = MetricsStore::new();
        m.record(&ev("a-proj", "/a", 200), "team-a");
        m.record(&ev("a-proj", "/a", 500), "team-a");
        m.record(&ev("b-proj", "/secret-b", 200), "team-b");

        for gran in [Granularity::Hour, Granularity::Day] {
            let a_series = m.series(gran, gran.max_span_minutes(), 1_000_000, Some("team-a"), None);
            assert_eq!(a_series.iter().map(|b| b.requests).sum::<u64>(), 2, "{gran:?} team-a total");
            assert_eq!(a_series.iter().map(|b| b.errors).sum::<u64>(), 1, "{gran:?} team-a errors");
            let a_projects: Vec<String> = m
                .project_totals(gran, gran.max_span_minutes(), 1_000_000, Some("team-a"))
                .into_iter()
                .map(|(p, _)| p)
                .collect();
            assert_eq!(a_projects, vec!["a-proj".to_string()], "{gran:?} team-a sees only its project");

            let g_series = m.series(gran, gran.max_span_minutes(), 1_000_000, None, None);
            assert_eq!(g_series.iter().map(|b| b.requests).sum::<u64>(), 3, "{gran:?} global total");
        }
    }

    /// A restart must not silently empty the Weekly/Monthly view: rollup_snapshot
    /// -> rollup_load on a FRESH store must reproduce the same hour/day reads
    /// (persist.rs wires this into PlatformSnapshot's capture/restore).
    #[test]
    fn rollup_snapshot_round_trips_hour_and_day_data() {
        let m = MetricsStore::new();
        m.record(&ev("a-proj", "/a", 200), "team-a");
        m.record(&ev("a-proj", "/a", 500), "team-a");

        let snap_bytes = serde_json::to_vec(&m.rollup_snapshot()).expect("snapshot serializes");

        let restored = MetricsStore::new();
        let snap: RollupSnapshot = serde_json::from_slice(&snap_bytes).expect("snapshot deserializes");
        restored.rollup_load(snap);

        for gran in [Granularity::Hour, Granularity::Day] {
            let before = m.series(gran, gran.max_span_minutes(), 1_000_000, Some("team-a"), None);
            let after = restored.series(gran, gran.max_span_minutes(), 1_000_000, Some("team-a"), None);
            assert_eq!(
                before.iter().map(|b| b.requests).sum::<u64>(),
                after.iter().map(|b| b.requests).sum::<u64>(),
                "{gran:?} requests survive a snapshot round-trip"
            );
            assert_eq!(
                before.iter().map(|b| b.errors).sum::<u64>(),
                after.iter().map(|b| b.errors).sum::<u64>(),
                "{gran:?} errors survive a snapshot round-trip"
            );
        }

        // Minute buckets are intentionally NOT part of the snapshot — restored
        // store must start with zero minute-resolution history.
        let restored_minute = restored.series(Granularity::Minute, MAX_BUCKETS, 1_000_000, Some("team-a"), None);
        assert_eq!(restored_minute.iter().map(|b| b.requests).sum::<u64>(), 0, "minute buckets are not persisted");
    }
}
