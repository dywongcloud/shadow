//! Durable request-event history — what makes `/v1/logs` reach further back
//! than the in-memory ring.
//!
//! `CloudState` used to hold exactly 500 events per node in memory
//! (`VecDeque`, count-capped, never persisted): on a busy node that is minutes
//! of traffic, which is why the dashboard "only supports ~30 minutes". Nothing
//! in the tree persisted a request event anywhere, so no query parameter could
//! widen a window that was never stored — the 30-minute symptom was an emergent
//! property of the ring, not a window.
//!
//! This module is the missing durable half:
//! * every recorded [`Event`] is ALSO appended to a day-rotated JSONL file
//!   under the node's data dir (`events-YYYY-MM-DD.jsonl`), newest last;
//! * a query with an explicit time window reads those files newest-first under
//!   a line/byte budget instead of the ring;
//! * a query with NO time bound keeps using the ring (the 1.5 s dashboard poll
//!   stays O(500) and never touches disk).
//!
//! Requests are recorded on the node that SERVES them, so this history is
//! per-node by construction — the coordinator fan-out in `admin::logs` merges
//! the peers' windows, which is why every new filter parameter has to ride the
//! fan-out query string.
//!
//! Retention is a day-file sweep (age + total bytes), so disk is bounded and a
//! query can never be more expensive than what is actually kept.

use crate::state::Event;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const DAY_MS: u64 = 86_400_000;

/// Newest `limit` matches are kept per day-file, so this bounds both the work
/// and the resident memory of one ranged query regardless of file size.
const FLUSH_BYTES: usize = 64 * 1024;
/// A single JSONL line longer than this is corrupt/foreign — skip, never panic.
const MAX_LINE_BYTES: usize = 8192;

// ---- config ---------------------------------------------------------------

/// `HIVE_EVENT_LOG=0` disables the durable half (ring-only, the old behaviour).
fn durable_enabled() -> bool {
    std::env::var("HIVE_EVENT_LOG")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}
/// Days of history kept (default 30 — the widest plan-gated window plus slack).
fn retention_days() -> u64 {
    std::env::var("HIVE_EVENT_LOG_DAYS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30)
}
/// Total budget for ALL day-files; oldest days are dropped first.
fn max_bytes() -> u64 {
    std::env::var("HIVE_EVENT_LOG_MAX_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|m| m.saturating_mul(1024 * 1024))
        .unwrap_or(1024 * 1024 * 1024)
}
/// Hard ceiling on JSONL lines examined by ONE ranged query. Bounds the worst
/// case of `range=30d&limit=1000` to a bounded scan instead of the whole file.
fn scan_max_lines() -> usize {
    std::env::var("HIVE_EVENT_LOG_SCAN_LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(500_000)
}

// ---- day-file naming ------------------------------------------------------

fn day_index(ts_ms: u64) -> u64 {
    ts_ms / DAY_MS
}

/// `days -> (y, m, d)` (Howard Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `(y, m, d) -> days` (days_from_civil). `None` when the label is not a date.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn day_label(day: u64) -> String {
    let (y, m, d) = civil_from_days(day as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn parse_day_label(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    days_from_civil(y, m, d).map(|v| v as u64)
}

// ---- derived dimensions ---------------------------------------------------
//
// `Event` carries no environment/resource/cache/level column — they are derived
// here from fields that DO exist, and the derivation is the single place the
// filter and the UI agree on. `env`/`resource` are a JOIN (event -> deployment),
// never a recorded field, and the UI must label them as such; `cache`/`level`
// derive from `action` + `status` alone.

/// Cache outcome of one event, from its `action`
/// (`hit | stale | store | revalidate | miss`), matching the vocabulary
/// `metrics.rs` already buckets on.
pub fn derive_cache(action: &str) -> &'static str {
    match action {
        "cache-hit" => "hit",
        "cache-stale" => "stale",
        "cache-store" => "store",
        "cache-revalidate" => "revalidate",
        _ => "miss",
    }
}

/// Severity of one event: `fatal | error | warning | info`.
///
/// `fatal` = 5xx (the request reached the deployment and it failed);
/// `error` = 4xx or an edge refusal that produced no response at all
/// (status 0 — a deny/block has no status to read, so the action decides);
/// `warning` = 3xx or a degraded-but-served outcome; everything else is `info`.
pub fn derive_level(status: u16, action: &str) -> &'static str {
    if status >= 500 {
        "fatal"
    } else if status >= 400 {
        "error"
    } else if matches!(
        action,
        "waf-deny"
            | "bot-block"
            | "ip-blocked"
            | "host-rejected"
            | "rate-limited"
            | "throttled"
            | "deployment-not-ready"
            | "mesh-ws-fail"
            | "mesh-route-fail"
            | "mesh-route-noretry"
    ) {
        "error"
    } else if status >= 300 {
        "warning"
    } else if matches!(
        action,
        "rewrite" | "redirect" | "cache-stale" | "bot-log" | "deploy-password" | "mfe-route"
    ) {
        "warning"
    } else {
        "info"
    }
}

fn norm_kind(kind: &str, functions: &[String]) -> String {
    let k = kind.trim().to_ascii_lowercase();
    if k == "static" || k == "function" || k == "fullstack" {
        return k;
    }
    // No recorded kind: fall back to what the deployment actually carries —
    // a deployment with functions is a function/fullstack workload, otherwise
    // it is a static asset tree.
    if functions.is_empty() {
        "static".to_string()
    } else {
        "function".to_string()
    }
}

/// Deployment facts the `env` / `resource` joins read. Built once per query
/// from the node's deployment table (never per event).
#[derive(Default, Clone)]
pub struct DeployFacts {
    kind_by_dep: HashMap<String, String>,
    target_by_dep: HashMap<String, String>,
    kind_by_host: HashMap<String, String>,
    target_by_host: HashMap<String, String>,
}

impl DeployFacts {
    pub fn build(deps: &[fluid_core::DeploymentInfo]) -> Self {
        let mut f = DeployFacts::default();
        for d in deps {
            let id = d.id.to_string().to_ascii_lowercase();
            let kind = norm_kind(&d.kind, &d.functions);
            // `target` is IMMUTABLE ("production"|"preview") while `production`
            // is the live promoted flag — prefer target, fall back to the flag.
            let target = if !d.target.is_empty() {
                d.target.to_ascii_lowercase()
            } else if d.production {
                "production".to_string()
            } else {
                "preview".to_string()
            };
            if !id.is_empty() {
                f.kind_by_dep.insert(id.clone(), kind.clone());
                f.target_by_dep.insert(id, target.clone());
            }
            // Host fallback: an event recorded before `Event.deployment` was
            // tagged still names the host it was served on, and every alias an
            // event could have arrived on maps to the same deployment.
            for h in [&d.alias, &d.commit_alias, &d.branch_alias, &d.id_alias] {
                let h = h.split(':').next().unwrap_or(h).trim().to_ascii_lowercase();
                if h.is_empty() {
                    continue;
                }
                f.kind_by_host.insert(h.clone(), kind.clone());
                f.target_by_host.insert(h, target.clone());
            }
        }
        f
    }

    /// Derived dimensions for one event. Empty strings mean UNKNOWN (the event
    /// has no deployment tag and its host matches no known deployment) — an
    /// UNKNOWN dimension is excluded by any filter naming a concrete value,
    /// which is the honest answer rather than a guess.
    pub fn derive(&self, e: &Event) -> Derived {
        let dep = e.deployment.to_ascii_lowercase();
        let host = e
            .host
            .split(':')
            .next()
            .unwrap_or(&e.host)
            .to_ascii_lowercase();
        let env = if !dep.is_empty() {
            self.target_by_dep.get(&dep).cloned()
        } else {
            None
        }
        .or_else(|| self.target_by_host.get(&host).cloned())
        .unwrap_or_default();
        let resource = if !dep.is_empty() {
            self.kind_by_dep.get(&dep).cloned()
        } else {
            None
        }
        .or_else(|| self.kind_by_host.get(&host).cloned())
        .unwrap_or_default();
        Derived {
            env,
            resource,
            cache: derive_cache(&e.action).to_string(),
            level: derive_level(e.status, &e.action).to_string(),
        }
    }
}

/// The dimensions an event does not record. Serialized onto every `/v1/logs`
/// row so the UI can filter and display them without re-deriving.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Derived {
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub cache: String,
    #[serde(default)]
    pub level: String,
}

// ---- filter ---------------------------------------------------------------

/// One `status=` term: an exact code or a class (`5xx`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusPred {
    Exact(u16),
    Class(u8),
}

impl StatusPred {
    /// Parse one comma-separated term. Returns `None` for junk, which the
    /// caller turns into a 400 rather than silently ignoring (a filter that
    /// silently does nothing is the bug this module exists to remove).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        if s.len() == 3 && (s.ends_with("xx") || s.ends_with("**")) {
            let d = s.chars().next()?.to_digit(10)?;
            return if (1..=5).contains(&d) {
                Some(StatusPred::Class(d as u8))
            } else {
                None
            };
        }
        let n: u16 = s.parse().ok()?;
        if (100..=599).contains(&n) {
            Some(StatusPred::Exact(n))
        } else {
            None
        }
    }

    pub fn matches(&self, status: u16) -> bool {
        match self {
            StatusPred::Exact(n) => *n == status,
            StatusPred::Class(c) => status / 100 == *c as u16,
        }
    }
}

/// The server-side `/v1/logs` filter. Every field is BOUNDED at parse time
/// (see `admin::logs`): an unbounded list or window on a fan-out endpoint is a
/// denial-of-service vector, not a feature.
#[derive(Clone, Default)]
pub struct EventFilter {
    /// Inclusive lower bound; `0` means "no bound" (the ring path).
    pub since_ms: u64,
    /// Inclusive upper bound; `u64::MAX` means "no bound".
    pub until_ms: u64,
    pub limit: usize,
    pub statuses: Vec<StatusPred>,
    /// Lowercased methods.
    pub methods: Vec<String>,
    /// Lowercased host substrings.
    pub hosts: Vec<String>,
    /// Lowercased path substrings.
    pub routes: Vec<String>,
    pub levels: Vec<String>,
    pub caches: Vec<String>,
    pub envs: Vec<String>,
    pub resources: Vec<String>,
    /// Free text, lowercased at parse time.
    pub text: Option<String>,
    /// Tenant/project/deployment scoping, applied BY THE SCAN so that `limit`
    /// counts rows the caller is actually allowed to see. Scoping after the
    /// scan would let a tenant filter eat the whole page and silently return
    /// fewer rows than requested with more still on disk.
    #[doc(hidden)]
    pub scope: Option<std::sync::Arc<dyn Fn(&Event) -> bool + Send + Sync>>,
}

impl EventFilter {
    /// Whether any filter reads a DERIVED dimension. When none does, the scan
    /// skips the (allocating) derivation entirely — a 30-day window can examine
    /// hundreds of thousands of lines and must not pay for a join nobody asked
    /// for. Derived values are still stamped onto the handful of rows returned.
    pub fn needs_derived(&self) -> bool {
        !self.levels.is_empty()
            || !self.caches.is_empty()
            || !self.envs.is_empty()
            || !self.resources.is_empty()
    }

    pub fn matches(&self, e: &Event, d: &Derived) -> bool {
        if let Some(s) = self.scope.as_ref() {
            if !s(e) {
                return false;
            }
        }
        if e.ts_ms < self.since_ms || e.ts_ms > self.until_ms {
            return false;
        }
        if !self.statuses.is_empty() && !self.statuses.iter().any(|p| p.matches(e.status)) {
            return false;
        }
        if !self.methods.is_empty() {
            let m = e.method.to_ascii_lowercase();
            if !self.methods.iter().any(|x| *x == m) {
                return false;
            }
        }
        if !self.hosts.is_empty() {
            let h = e.host.to_ascii_lowercase();
            if !self.hosts.iter().any(|x| h.contains(x.as_str())) {
                return false;
            }
        }
        if !self.routes.is_empty() {
            let p = e.path.to_ascii_lowercase();
            if !self.routes.iter().any(|x| p.contains(x.as_str())) {
                return false;
            }
        }
        if !self.levels.is_empty() && !self.levels.iter().any(|x| *x == d.level) {
            return false;
        }
        if !self.caches.is_empty() && !self.caches.iter().any(|x| *x == d.cache) {
            return false;
        }
        if !self.envs.is_empty() && !self.envs.iter().any(|x| *x == d.env) {
            return false;
        }
        if !self.resources.is_empty() && !self.resources.iter().any(|x| *x == d.resource) {
            return false;
        }
        if let Some(t) = self.text.as_ref() {
            if !(e.path.to_ascii_lowercase().contains(t.as_str())
                || e.host.to_ascii_lowercase().contains(t.as_str())
                || e.detail.to_ascii_lowercase().contains(t.as_str())
                || e.action.to_ascii_lowercase().contains(t.as_str()))
            {
                return false;
            }
        }
        true
    }
}

/// Split one comma-separated query value into a bounded, lowercased list.
/// `None` when the value is absent or empty (filter not applied).
pub fn parse_list(raw: Option<&str>, max_items: usize, max_len: usize) -> Option<Vec<String>> {
    let raw = raw?;
    let v: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .take(max_items)
        .map(|s| if s.len() > max_len { s[..max_len].to_string() } else { s })
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

// ---- the store ------------------------------------------------------------

struct DayWriter {
    day: u64,
    buffered: usize,
    file: BufWriter<File>,
}

pub struct EventLog {
    dir: PathBuf,
    durable: bool,
    writer: Mutex<Option<DayWriter>>,
    /// The hot path: newest <=500 events, newest last. Serves every unbounded
    /// query (the 1.5 s dashboard poll) without touching disk.
    ring: Mutex<VecDeque<Event>>,
}

impl EventLog {
    pub fn new(dir: PathBuf) -> Self {
        let durable = durable_enabled();
        let s = Self {
            dir,
            durable,
            writer: Mutex::new(None),
            ring: Mutex::new(VecDeque::with_capacity(512)),
        };
        if durable {
            s.sweep();
        }
        s
    }

    fn path_for(&self, day: u64) -> PathBuf {
        self.dir.join(format!("events-{}.jsonl", day_label(day)))
    }

    /// Record one event: ring (bounded, hot) + durable append (history).
    pub fn append(&self, ev: Event) {
        if self.durable {
            self.append_durable(&ev);
        }
        let mut q = self.ring.lock();
        if q.len() >= 500 {
            q.pop_front();
        }
        q.push_back(ev);
    }

    /// Push the buffered tail to the OS so a ranged read sees it. One `flush`
    /// per ranged query, never an `fsync`.
    fn flush(&self) {
        if let Some(w) = self.writer.lock().as_mut() {
            let _ = w.file.flush();
            w.buffered = 0;
        }
    }

    fn open_day(&self, day: u64) -> Option<DayWriter> {
        if let Err(e) = fs::create_dir_all(&self.dir) {
            tracing::warn!("event_log: cannot create {}: {e}", self.dir.display());
            return None;
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(day))
            .ok()?;
        Some(DayWriter {
            day,
            buffered: 0,
            file: BufWriter::with_capacity(FLUSH_BYTES, f),
        })
    }

    fn append_durable(&self, ev: &Event) {
        let Ok(line) = serde_json::to_string(ev) else {
            return;
        };
        let day = day_index(ev.ts_ms);
        let mut w = self.writer.lock();
        let needs_rotate = w.as_ref().map(|x| x.day) != Some(day);
        if needs_rotate {
            if let Some(mut old) = w.take() {
                let _ = old.file.flush();
            }
            // Sweep on rotation: once per calendar day, never per event.
            self.sweep();
            *w = self.open_day(day);
        }
        let Some(w) = w.as_mut() else { return };
        if writeln!(w.file, "{line}").is_ok() {
            w.buffered += line.len() + 1;
        }
        // Flush on a byte budget, never fsync: a per-request fsync on the
        // serving path would serialize every request behind a disk barrier.
        // Worst case is losing the tail of the current 64 KiB buffer, which is
        // exactly the window the in-memory ring still covers.
        if w.buffered >= FLUSH_BYTES {
            let _ = w.file.flush();
            w.buffered = 0;
        }
    }

    /// Newest-first, bounded by `limit`. No time bound → the ring only, which
    /// is what keeps the default poll O(500) and off the disk.
    pub fn recent(&self, limit: usize) -> Vec<Event> {
        let q = self.ring.lock();
        q.iter().rev().take(limit).cloned().collect()
    }

    /// Oldest event this node can still answer for (ring or disk), `0` when
    /// unknown/empty. Used to tell the UI the real floor of its history.
    pub fn oldest_ms(&self) -> u64 {
        let ring_oldest = self.ring.lock().front().map(|e| e.ts_ms).unwrap_or(u64::MAX);
        if !self.durable {
            return if ring_oldest == u64::MAX { 0 } else { ring_oldest };
        }
        let disk_oldest = self
            .day_files()
            .first()
            .map(|(day, _, _)| *day * DAY_MS)
            .unwrap_or(u64::MAX);
        let m = ring_oldest.min(disk_oldest);
        if m == u64::MAX { 0 } else { m }
    }

    pub fn query(&self, f: &EventFilter, facts: &DeployFacts) -> Vec<Event> {
        let mut out = Vec::with_capacity(f.limit.min(1024));
        if self.needs_disk(f) {
            // The durable writer is buffered (64 KiB), so the last seconds of
            // traffic are not on disk yet. Flush first — otherwise a ranged
            // query would MISS the very newest events, i.e. the ones the user
            // just generated, which reads as "the range filter is broken".
            self.flush();
            self.scan_disk(f, facts, &mut out);
        } else {
            let q = self.ring.lock();
            let derived = Derived::default();
            for e in q.iter().rev() {
                if out.len() >= f.limit {
                    break;
                }
                let d = if f.needs_derived() {
                    facts.derive(e)
                } else {
                    derived.clone()
                };
                if f.matches(e, &d) {
                    out.push(e.clone());
                }
            }
        }
        out
    }

    /// Disk is consulted only when the ring provably does not cover the window:
    /// `since_ms == 0` (unbounded) or the ring's oldest event is newer than the
    /// requested start. That is what keeps the default poll cheap.
    fn needs_disk(&self, f: &EventFilter) -> bool {
        if !self.durable || f.since_ms == 0 {
            return false;
        }
        match self.ring.lock().front() {
            Some(oldest) => oldest.ts_ms > f.since_ms,
            None => true,
        }
    }

    fn scan_disk(&self, f: &EventFilter, facts: &DeployFacts, out: &mut Vec<Event>) {
        let now = hive_core::now_ms();
        let last = day_index(f.until_ms.min(now));
        let first = day_index(f.since_ms);
        // Newest day first: the newest files are the ones that fill `limit`,
        // so older files are usually never opened at all.
        let mut day = last;
        let mut lines = 0usize;
        let max_lines = scan_max_lines();
        loop {
            if out.len() >= f.limit || lines >= max_lines {
                return;
            }
            if let Ok(fh) = File::open(self.path_for(day)) {
                // Keep only this day's NEWEST `limit` matches (files are
                // chronological), so the resident set is bounded by `limit`.
                let mut keep: VecDeque<Event> = VecDeque::with_capacity(f.limit.min(1024) + 1);
                let derived = Derived::default();
                for line in BufReader::with_capacity(1 << 16, fh).lines() {
                    lines += 1;
                    if lines >= max_lines {
                        break;
                    }
                    let Ok(line) = line else { break };
                    if line.len() > MAX_LINE_BYTES {
                        continue;
                    }
                    let Ok(e) = serde_json::from_str::<Event>(&line) else {
                        continue;
                    };
                    if e.ts_ms < f.since_ms || e.ts_ms > f.until_ms {
                        continue;
                    }
                    let d = if f.needs_derived() {
                        facts.derive(&e)
                    } else {
                        derived.clone()
                    };
                    if f.matches(&e, &d) {
                        keep.push_back(e);
                        if keep.len() > f.limit {
                            keep.pop_front();
                        }
                    }
                }
                while let Some(e) = keep.pop_back() {
                    if out.len() >= f.limit {
                        break;
                    }
                    out.push(e);
                }
            }
            // `<=` and not `==`: `first` is derived from `since_ms`, which is
            // caller-supplied. A `since_ms` past `now` (or past `until_ms`)
            // makes `first > last`, so an equality test would never match and
            // the decrement below would run off the bottom of u64 -- a panic
            // under overflow-checks, and under release (no overflow-checks) a
            // wrap to u64::MAX that spins this loop ~2^64 times inside
            // block_in_place, hanging a tokio worker permanently. Bailing out
            // as soon as `day` reaches (or is already below) `first` keeps the
            // walk bounded for every possible input.
            if day <= first {
                return;
            }
            day -= 1;
        }
    }

    /// Day-files currently on disk, oldest first: `(day, path, bytes)`.
    fn day_files(&self) -> Vec<(u64, PathBuf, u64)> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut v: Vec<(u64, PathBuf, u64)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let rest = name
                    .strip_prefix("events-")
                    .and_then(|s| s.strip_suffix(".jsonl"))?;
                let day = parse_day_label(rest)?;
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                Some((day, e.path(), len))
            })
            .collect();
        v.sort_by_key(|x| x.0);
        v
    }

    /// Drop day-files past the retention age, then past the byte budget
    /// (oldest first). Never touches today's file.
    pub fn sweep(&self) {
        let files = self.day_files();
        if files.is_empty() {
            return;
        }
        let today = day_index(hive_core::now_ms());
        let cutoff = today.saturating_sub(retention_days());
        let cap = max_bytes();
        let mut total: u64 = files.iter().map(|x| x.2).sum();
        for (day, path, len) in files {
            if day == today || (day >= cutoff && total <= cap) {
                break;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    total = total.saturating_sub(len);
                    tracing::info!(
                        "event_log: reaped {} (day {day}, {} bytes)",
                        path.display(),
                        len
                    );
                }
                Err(e) => tracing::warn!("event_log: cannot reap {}: {e}", path.display()),
            }
        }
    }
}

/// Re-exported so `admin.rs` can keep the ring/durable split invisible.
pub fn data_subdir(root: &Path) -> PathBuf {
    root.join("request-events")
}
