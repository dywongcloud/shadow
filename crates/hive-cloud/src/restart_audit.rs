//! Restart audit: makes an OOM-kill restart SELF-EVIDENT instead of silent.
//!
//! The gap it closes: a node killed by the cgroup OOM killer comes back a second later under
//! `Restart=always`, healthz answers and the mesh reconverges, and nothing at the platform level
//! records that it died. The SIGKILLed process cannot log its own death, the new one starts with a
//! clean slate, and the only evidence is a kernel line in `dmesg` on that host, so every downstream
//! symptom (probe failures, dropped trunks, a placement gap, a DNS withdrawal) reads as its own
//! mystery.
//!
//! `memwatch` is the other half and is deliberately not this: it watches RSS climb and tries to
//! capture a profile BEFORE the kill. This module runs AFTER, on the next boot, and answers 'did we
//! just die, and why?', which stays answerable even for a burst too fast for any sampler to catch.
//!
//! ## How the verdict is reached
//!
//! A marker file (`$HIVE_DATA/run_marker.json`) is written at boot and refreshed by a heartbeat,
//! carrying this process's pid, start time, the host's boot id, its last observed RSS and memory
//! ceiling, and the cgroup's OOM counter. The SIGTERM path stamps `clean_exit: true`. At boot the
//! previous process's marker is a witness statement, and the verdict is:
//!
//! * no marker → **first_boot**.
//! * `clean_exit` set → **clean_restart** (a deploy, a `systemctl restart`).
//! * the kernel log names the previous PID in an OOM kill → **oom_kill**: direct evidence, and the
//!   pid match is required, because an older kill's line still in the ring buffer must never be
//!   re-reported as this restart's cause.
//! * this cgroup's `memory.events` `oom_kill` advanced past the previous heartbeat's reading →
//!   **oom_kill**.
//! * the host boot id changed → **host_reboot** (the host went down, not us).
//! * the last heartbeat's RSS was within `HIVE_OOM_SUSPECT_PCT` (default 90) of the memory ceiling
//!   → **oom_suspected**: named as a suspicion, never as proof, because on a host whose kernel
//!   buffer has rolled or is unreadable this is all the evidence there is, and calling it
//!   `oom_kill` would be a lie the operator cannot audit.
//! * otherwise → **unclean_exit** (a panic, a SIGKILL from something else, power loss with no
//!   boot-id change): also worth knowing, and otherwise invisible.
//!
//! Every verdict is appended to a bounded history file (`HISTORY_MAX` records), so 'this node
//! cycles every 2-3 hours' is a readable list of timestamps and uptimes rather than an inference.
//! The counters ride gossip (`NodeInfo::oom_restarts_24h` / `last_oom_ms` / `started_ms`), so the
//! fleet view shows it without logging into anything.
//!
//! Nothing here can fail a boot: every host reading is best-effort (`None` means UNKNOWN and is
//! never coerced to a number, because a fabricated reading produces a fabricated verdict), an
//! unreadable or corrupt marker is treated as absent (with a WARN), and no verdict is ever
//! fabricated from a missing source. An unreadable history is set aside and treated as absent too,
//! but its restarts are UNKNOWN, not zero, so the per-reason restart caps counted from it fail
//! closed ([`controlled_restarts_within`]).

use hive_core::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Set by `mark_clean_exit`; read by the heartbeat before every marker write. The graceful tail
/// runs tens of seconds past the clean-exit stamp, so a 20 s heartbeat tick lands after it and,
/// without this latch, rewrites the marker with `clean_exit=false`, reporting every self-exiting
/// restart as UNCLEAN on the next boot.
static CLEAN_EXIT_MARKED: AtomicBool = AtomicBool::new(false);

/// Bounded — this is a diagnostic tail, not a log.
const HISTORY_MAX: usize = 64;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn marker_path() -> std::path::PathBuf {
    crate::persist::data_dir().join("run_marker.json")
}

fn history_path() -> std::path::PathBuf {
    crate::persist::data_dir().join("restart_history.json")
}

/// The previous process's witness statement. Every field is `serde(default)` so a marker written by
/// an older build still deserializes: a missing field must degrade the verdict's confidence, never
/// discard the whole record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Marker {
    #[serde(default)]
    node: String,
    #[serde(default)]
    pid: u32,
    /// `/proc/sys/kernel/random/boot_id` — changes only when the HOST reboots,
    /// which is what separates "the host went down" from "we were killed".
    #[serde(default)]
    boot_id: String,
    #[serde(default)]
    started_ms: u64,
    #[serde(default)]
    heartbeat_ms: u64,
    #[serde(default)]
    rss_bytes: u64,
    /// cgroup `memory.max`, else host MemTotal. The denominator the RSS
    /// suspicion is measured against.
    #[serde(default)]
    mem_limit_bytes: u64,
    /// `memory.events: oom_kill` as of the last heartbeat.
    #[serde(default)]
    cgroup_oom_kills: u64,
    #[serde(default)]
    clean_exit: bool,
    #[serde(default)]
    version: String,
    /// The controlled-restart reason this process requested (`exit(17)` paths: memory pressure,
    /// mesh watchdog triggers), empty when none. Written the moment the restart is REQUESTED, not
    /// at exit, so a shutdown that dies on its hard deadline still leaves the reason behind for the
    /// next boot's record.
    #[serde(default)]
    restart_reason: String,
}

/// One boot's verdict. Serialized into the history file and the endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestartRecord {
    pub ts_ms: u64,
    /// `first_boot` | `clean_restart` | `oom_kill` | `oom_suspected` |
    /// `host_reboot` | `unclean_exit`
    pub verdict: String,
    /// Human-readable statement of WHAT was observed, never a restatement of the verdict: an
    /// operator must be able to disagree with the conclusion by reading the evidence.
    pub evidence: String,
    #[serde(default)]
    pub prev_pid: u32,
    #[serde(default)]
    pub prev_started_ms: u64,
    /// How long the previous process lived. THE number for "is this node
    /// cycling?".
    #[serde(default)]
    pub prev_uptime_ms: u64,
    #[serde(default)]
    pub prev_rss_bytes: u64,
    #[serde(default)]
    pub mem_limit_bytes: u64,
    /// Previous RSS as a percentage of the memory ceiling, when both are known.
    #[serde(default)]
    pub prev_rss_pct: Option<u64>,
    /// The controlled-restart reason the previous process requested, if it asked for its own
    /// restart (`memory_pressure`, `mesh_isolation`, `mesh_degradation`, `establishment_wedge`).
    /// What restart rate caps count ([`controlled_restarts_within`]), so a cap survives the very
    /// restarts it limits.
    #[serde(default)]
    pub reason: Option<String>,
}

impl RestartRecord {
    fn is_oom(&self) -> bool {
        self.verdict == "oom_kill" || self.verdict == "oom_suspected"
    }
}

static STARTED_MS: AtomicU64 = AtomicU64::new(0);
static LAST_OOM_MS: AtomicU64 = AtomicU64::new(0);
/// This node's name, for marker writes outside the boot/heartbeat paths.
static NODE: OnceLock<String> = OnceLock::new();
/// The controlled-restart reason this process requested (first wins, like
/// `ControlledRestart`'s own latch).
static REQUESTED_RESTART: OnceLock<String> = OnceLock::new();
/// Why this process's persisted restart history cannot be trusted to COUNT
/// restarts (`None` = trusted); set at boot, never cleared.
static HISTORY_FAULT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// The latest run-marker write failure (`None` once a write succeeds again).
static MARKER_FAULT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn boot_record() -> &'static OnceLock<RestartRecord> {
    static R: OnceLock<RestartRecord> = OnceLock::new();
    &R
}

fn history() -> &'static parking_lot::RwLock<Vec<RestartRecord>> {
    static H: OnceLock<parking_lot::RwLock<Vec<RestartRecord>>> = OnceLock::new();
    H.get_or_init(|| parking_lot::RwLock::new(Vec::new()))
}

fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// This process's cgroup path as an absolute `/sys/fs/cgroup/...` dir (cgroup v2 only: v1's split
/// hierarchy has no single `memory.events`, and the fleet is v2).
fn cgroup_dir() -> Option<std::path::PathBuf> {
    let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = raw
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim()
        .trim_start_matches('/')
        .to_string();
    let dir = std::path::Path::new("/sys/fs/cgroup").join(rel);
    dir.is_dir().then_some(dir)
}

/// `memory.events: oom_kill`, the kernel's own count of processes this cgroup has had killed. The
/// counter lives with the CGROUP: a recreated cgroup starts again at 0 and a surviving one keeps
/// counting, which is why `classify` compares it with the previous heartbeat's reading instead of
/// treating any non-zero value as new.
fn cgroup_oom_kills() -> Option<u64> {
    let dir = cgroup_dir()?;
    let text = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// The ceiling RSS is actually measured against: the cgroup's `memory.max` when it is a real
/// number, else the host's MemTotal. `memory.max` reading `max` (no limit) is not a limit and must
/// fall through; treating the string as 0 would make every node look permanently 100% of its
/// ceiling.
fn mem_limit_bytes() -> Option<u64> {
    if let Some(dir) = cgroup_dir() {
        if let Ok(text) = std::fs::read_to_string(dir.join("memory.max")) {
            if let Ok(v) = text.trim().parse::<u64>() {
                return Some(v);
            }
        }
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo.lines().find_map(|l| {
        let rest = l.strip_prefix("MemTotal:")?;
        rest.trim()
            .trim_end_matches("kB")
            .trim()
            .parse::<u64>()
            .ok()
            .map(|kb| kb.saturating_mul(1024))
    })
}

/// Scans the kernel ring buffer for an OOM kill naming `prev_pid`, reading `/dev/kmsg` rather than
/// shelling out to `dmesg`/`journalctl` (no subprocess, no PATH assumption, works on a node whose
/// journal is volatile). It is opened NON-BLOCKING (`O_NONBLOCK`, written as its Linux literal
/// `0o4000`) because a blocking read waits forever for the NEXT message once the buffer is drained,
/// which would hang boot. `/dev/kmsg` exists only on Linux and may be unreadable (no permission, a
/// container without it): that is UNKNOWN, and the caller falls back to the RSS suspicion instead
/// of inventing a verdict. Each `read()` returns exactly one `prio,seq,ts,flag;message` record, at
/// most 8192 are read, and `EPIPE` (32) means the buffer wrapped past our position, so it is a
/// continue, not a stop.
///
/// Returns `(direct_evidence, all_oom_lines_seen)`. The direct verdict requires the previous pid to
/// appear in the line: the ring buffer routinely still holds OOM lines from EARLIER kills, and
/// reporting one as this restart's cause would manufacture a fresh incident from old history. The
/// other lines (at most 16) are context for an operator.
#[cfg(unix)]
fn kmsg_oom_evidence(prev_pid: u32) -> (Option<String>, Vec<String>) {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    const O_NONBLOCK: i32 = 0o4000;
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/kmsg")
    {
        Ok(f) => f,
        Err(_) => return (None, Vec::new()),
    };
    let mut direct = None;
    let mut lines: Vec<String> = Vec::new();
    let mut buf = [0u8; 8192];
    for _ in 0..8192 {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let rec = String::from_utf8_lossy(&buf[..n]);
                let msg = match rec.split_once(';') {
                    Some((_, m)) => m.trim(),
                    None => rec.trim(),
                };
                let low = msg.to_ascii_lowercase();
                let is_oom = low.contains("out of memory")
                    || low.contains("oom-kill")
                    || low.contains("oom_reaper");
                if !is_oom {
                    continue;
                }
                let line = msg.lines().next().unwrap_or(msg).to_string();
                if prev_pid > 0
                    && (line.contains(&format!("Killed process {prev_pid} "))
                        || line.contains(&format!("pid={prev_pid},")))
                {
                    direct = Some(line.clone());
                }
                if lines.len() < 16 {
                    lines.push(line);
                }
            }
            Err(e) => {
                if e.raw_os_error() == Some(32) {
                    continue;
                }
                break;
            }
        }
    }
    (direct, lines)
}

#[cfg(not(unix))]
fn kmsg_oom_evidence(_prev_pid: u32) -> (Option<String>, Vec<String>) {
    (None, Vec::new())
}

/// Writes through a temp file and `rename`, so the marker and the history are never torn: a torn
/// marker read at boot would be indistinguishable from a crash.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn read_marker() -> Option<Marker> {
    let path = marker_path();
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Marker>(&text) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(),
                "restart audit: unreadable run marker — treating this boot as unaudited");
            None
        }
    }
}

fn current_marker(node: &str, clean_exit: bool) -> Marker {
    Marker {
        node: node.to_string(),
        pid: std::process::id(),
        boot_id: boot_id().unwrap_or_default(),
        started_ms: STARTED_MS.load(Ordering::Relaxed),
        heartbeat_ms: now_ms(),
        rss_bytes: crate::memwatch::rss_bytes().unwrap_or(0),
        mem_limit_bytes: mem_limit_bytes().unwrap_or(0),
        cgroup_oom_kills: cgroup_oom_kills().unwrap_or(0),
        clean_exit,
        version: env!("CARGO_PKG_VERSION").to_string(),
        restart_reason: REQUESTED_RESTART.get().cloned().unwrap_or_default(),
    }
}

/// Write the marker; a failure is recorded as a marker fault until the next
/// write succeeds (see [`controlled_restarts_within`]).
fn save_marker(m: &Marker) {
    let result = serde_json::to_vec(m)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_atomic(&marker_path(), &bytes));
    let mut fault = lock(&MARKER_FAULT);
    match result {
        Ok(()) => *fault = None,
        Err(e) => {
            if fault.is_none() {
                tracing::warn!(error = %e, path = %marker_path().display(),
                    "restart audit: run marker write FAILED — restart rate caps fail closed \
                     until a marker write succeeds");
            }
            *fault = Some(format!("run marker write failed: {e}"));
        }
    }
}

/// The persisted history. `Err` when a history file EXISTS but cannot be read or parsed: the
/// restarts it recorded are unknown, which is not the same as none (an absent file is a genuine
/// empty history). The cap is enforced on LOAD as well as on write, so no on-disk file can reload
/// past `HISTORY_MAX`.
fn load_history() -> Result<Vec<RestartRecord>, String> {
    let path = history_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("restart history unreadable: {e}")),
    };
    let mut v = serde_json::from_str::<Vec<RestartRecord>>(&text)
        .map_err(|e| format!("restart history unparseable: {e}"))?;
    if v.len() > HISTORY_MAX {
        let start = v.len() - HISTORY_MAX;
        v.drain(..start);
    }
    Ok(v)
}

fn save_history(records: &[RestartRecord]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(records).map_err(std::io::Error::other)?;
    write_atomic(&history_path(), &bytes)
}

/// Classify the previous process's exit from its marker plus live host
/// evidence. Pure given its inputs; the I/O is all in [`audit_boot`].
fn classify(prev: &Marker, now_boot_id: Option<&str>, suspect_pct: u64) -> RestartRecord {
    let now = now_ms();
    let uptime = prev.heartbeat_ms.saturating_sub(prev.started_ms);
    let limit = if prev.mem_limit_bytes > 0 {
        prev.mem_limit_bytes
    } else {
        mem_limit_bytes().unwrap_or(0)
    };
    let pct = (limit > 0 && prev.rss_bytes > 0).then(|| prev.rss_bytes.saturating_mul(100) / limit);
    let mut rec = RestartRecord {
        ts_ms: now,
        verdict: "unclean_exit".into(),
        evidence: String::new(),
        prev_pid: prev.pid,
        prev_started_ms: prev.started_ms,
        prev_uptime_ms: uptime,
        prev_rss_bytes: prev.rss_bytes,
        mem_limit_bytes: limit,
        prev_rss_pct: pct,
        reason: (!prev.restart_reason.is_empty()).then(|| prev.restart_reason.clone()),
    };

    if prev.clean_exit {
        rec.verdict = "clean_restart".into();
        rec.evidence = "previous process recorded a graceful shutdown (SIGTERM path ran)".into();
        return rec;
    }

    let (direct, seen) = kmsg_oom_evidence(prev.pid);
    if let Some(line) = direct {
        rec.verdict = "oom_kill".into();
        rec.evidence = format!("kernel log names pid {}: {line}", prev.pid);
        return rec;
    }

    if let Some(kills) = cgroup_oom_kills() {
        if kills > prev.cgroup_oom_kills {
            rec.verdict = "oom_kill".into();
            rec.evidence = format!(
                "cgroup memory.events oom_kill advanced {} -> {kills} since the previous \
                 heartbeat",
                prev.cgroup_oom_kills
            );
            return rec;
        }
    }

    let host_rebooted = match (now_boot_id, prev.boot_id.as_str()) {
        (Some(now_id), prev_id) if !prev_id.is_empty() => now_id != prev_id,
        _ => false,
    };
    if host_rebooted {
        rec.verdict = "host_reboot".into();
        rec.evidence = format!(
            "host boot id changed ({} -> {}) — the HOST went down, not just this process",
            prev.boot_id,
            now_boot_id.unwrap_or("?")
        );
        return rec;
    }

    if let Some(p) = pct {
        if p >= suspect_pct {
            rec.verdict = "oom_suspected".into();
            rec.evidence = format!(
                "no kernel evidence available, but the previous process's last heartbeat had \
                 RSS {} MiB = {p}% of its {} MiB ceiling (>= HIVE_OOM_SUSPECT_PCT {suspect_pct})",
                prev.rss_bytes / 1024 / 1024,
                limit / 1024 / 1024
            );
            return rec;
        }
    }

    rec.evidence = format!(
        "no graceful-shutdown record, no kernel OOM line for pid {}, host boot id unchanged, \
         last RSS {} MiB — the process died without exiting cleanly (panic, external SIGKILL, \
         or a kill whose kernel line is no longer in the ring buffer){}",
        prev.pid,
        prev.rss_bytes / 1024 / 1024,
        if seen.is_empty() {
            String::new()
        } else {
            format!("; {} unrelated OOM line(s) are in the buffer", seen.len())
        }
    );
    rec
}

/// Reads the previous run's marker, reaches a verdict, logs it LOUDLY, appends it to the bounded
/// history and stamps a fresh marker for this process at once: a node that OOMs before the first
/// heartbeat tick must still leave a witness behind. Call once, early in boot, before anything that
/// can wedge; it is local file I/O and cannot fail the boot.
///
/// An unreadable history is set aside (`restart_history.corrupt-<ms>`) for forensics rather than
/// overwritten, and this process's restart rate caps fail closed because the restarts the file held
/// are unknown, not zero. A failed history write does the same: this boot's record is not on disk,
/// so the next process cannot count it, and this one cannot trust what it counts.
pub fn audit_boot(node: &str) -> Value {
    STARTED_MS.store(now_ms(), Ordering::Relaxed);
    let _ = NODE.set(node.to_string());
    let suspect_pct = env_u64("HIVE_OOM_SUSPECT_PCT", 90).min(100);
    let now_boot = boot_id();

    let rec = match read_marker() {
        Some(prev) => classify(&prev, now_boot.as_deref(), suspect_pct),
        None => RestartRecord {
            ts_ms: now_ms(),
            verdict: "first_boot".into(),
            evidence: "no previous run marker on this node (first boot, or a wiped data dir)"
                .into(),
            prev_pid: 0,
            prev_started_ms: 0,
            prev_uptime_ms: 0,
            prev_rss_bytes: 0,
            mem_limit_bytes: mem_limit_bytes().unwrap_or(0),
            prev_rss_pct: None,
            reason: None,
        },
    };

    let mut hist = match load_history() {
        Ok(hist) => hist,
        Err(why) => {
            let aside = history_path().with_extension(format!("corrupt-{}", now_ms()));
            let _ = std::fs::rename(history_path(), &aside);
            tracing::warn!(reason = %why, moved_to = %aside.display(),
                "restart audit: restart history could not be loaded — restart rate caps fail \
                 closed for this process");
            *lock(&HISTORY_FAULT) = Some(why);
            Vec::new()
        }
    };
    hist.push(rec.clone());
    if hist.len() > HISTORY_MAX {
        let start = hist.len() - HISTORY_MAX;
        hist.drain(..start);
    }
    if let Err(e) = save_history(&hist) {
        tracing::warn!(error = %e, path = %history_path().display(),
            "restart audit: restart history write FAILED — restart rate caps fail closed for \
             this process");
        lock(&HISTORY_FAULT).get_or_insert_with(|| format!("restart history write failed: {e}"));
    }
    let w = window_stats(&hist);
    let restarts_24h = w.restarts;
    let oom_24h = w.oom;
    let min_uptime = w.min_uptime_ms;
    if let Some(ms) = hist.iter().filter(|r| r.is_oom()).map(|r| r.ts_ms).max() {
        LAST_OOM_MS.store(ms, Ordering::Relaxed);
    }
    *history().write() = hist;
    let _ = boot_record().set(rec.clone());

    save_marker(&current_marker(node, false));

    let summary = json!({
        "node": node,
        "verdict": rec.verdict,
        "evidence": rec.evidence,
        "prev_pid": rec.prev_pid,
        "prev_uptime_ms": rec.prev_uptime_ms,
        "prev_rss_bytes": rec.prev_rss_bytes,
        "prev_rss_pct": rec.prev_rss_pct,
        "mem_limit_bytes": rec.mem_limit_bytes,
        "restarts_24h": restarts_24h,
        "oom_restarts_24h": oom_24h,
        "unclean_restarts_24h": w.unclean,
        "min_uptime_ms_24h": min_uptime,
    });

    match rec.verdict.as_str() {
        "oom_kill" | "oom_suspected" => tracing::error!(
            node,
            verdict = %rec.verdict,
            evidence = %rec.evidence,
            prev_pid = rec.prev_pid,
            prev_uptime_secs = rec.prev_uptime_ms / 1000,
            prev_rss_mb = rec.prev_rss_bytes / 1024 / 1024,
            prev_rss_pct = rec.prev_rss_pct.unwrap_or(0),
            mem_limit_mb = rec.mem_limit_bytes / 1024 / 1024,
            restarts_24h,
            oom_restarts_24h = oom_24h,
            min_uptime_secs_24h = min_uptime.map(|m| m / 1000).unwrap_or(0),
            "OOM RESTART: this node was killed for memory and restarted — every \
             health/probe/placement symptom on it since then is downstream of THIS. \
             Full history: GET /v1/node/restarts (node-local)"
        ),
        "unclean_exit" => tracing::error!(
            node,
            evidence = %rec.evidence,
            prev_pid = rec.prev_pid,
            prev_uptime_secs = rec.prev_uptime_ms / 1000,
            restarts_24h,
            unclean_restarts_24h = w.unclean,
            crash_cycling = w.crashy() >= CRASH_CYCLE_MIN,
            "UNCLEAN RESTART: the previous process died without a graceful shutdown"
        ),
        "host_reboot" => tracing::warn!(
            node,
            evidence = %rec.evidence,
            prev_uptime_secs = rec.prev_uptime_ms / 1000,
            "host reboot detected"
        ),
        _ => tracing::info!(
            node,
            verdict = %rec.verdict,
            restarts_24h,
            "restart audit"
        ),
    }
    summary
}

/// One 24h window's restart counts. `first_boot` is excluded from the restart count: a wiped data
/// dir is not a restart, and counting it as one would make every fresh node look unstable.
///
/// `unclean` counts the `unclean_exit` verdicts: restarts where the process DIED without a graceful
/// shutdown and without kernel OOM evidence (a SIGSEGV, a panic, a SIGKILL it did not ask for). It
/// is the only counter that sees an involuntary crash: [`crate::ControlledRestart`] is the
/// chokepoint for restarts hive CHOOSES, and a process that segfaulted is already gone, so no rate
/// cap, reason tally or admission check ever observes it, and an OOM-only view (`oom_restarts_24h`)
/// reads perfect health through a whole crash loop.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowStats {
    pub restarts: u32,
    pub oom: u32,
    pub unclean: u32,
    pub min_uptime_ms: Option<u64>,
}

impl WindowStats {
    /// Deaths that were neither chosen nor a host reboot: OOM kills plus every
    /// other involuntary exit.
    pub fn crashy(&self) -> u32 {
        self.oom.saturating_add(self.unclean)
    }
}

/// How many involuntary deaths in 24h make a node crash-cycling. Three, not one: a single unclean
/// exit is a deploy SIGKILLed at the shutdown deadline or a one-off panic, and calling that a loop
/// would make this signal worthless.
const CRASH_CYCLE_MIN: u32 = 3;

fn window_stats(hist: &[RestartRecord]) -> WindowStats {
    let cutoff = now_ms().saturating_sub(DAY_MS);
    let recent: Vec<&RestartRecord> = hist
        .iter()
        .filter(|r| r.ts_ms >= cutoff && r.verdict != "first_boot")
        .collect();
    let oom = recent.iter().filter(|r| r.is_oom()).count() as u32;
    let unclean = recent
        .iter()
        .filter(|r| r.verdict == "unclean_exit")
        .count() as u32;
    let min_uptime = recent
        .iter()
        .filter(|r| r.prev_uptime_ms > 0)
        .map(|r| r.prev_uptime_ms)
        .min();
    WindowStats {
        restarts: recent.len() as u32,
        oom,
        unclean,
        min_uptime_ms: min_uptime,
    }
}

/// Gossiped counters (see `NodeInfo`), so a cycling node is visible from the
/// dashboard and `/v1/nodes` without reading any host's logs.
pub fn oom_restarts_24h() -> u32 {
    window_stats(&history().read()).oom
}

/// Involuntary deaths in the last 24h. Gossiped on `NodeInfo` next to `oom_restarts_24h` so
/// `/v1/nodes` shows a crash loop rather than a node that merely looks freshly started.
pub fn unclean_restarts_24h() -> u32 {
    window_stats(&history().read()).unclean
}

pub fn last_oom_ms() -> Option<u64> {
    match LAST_OOM_MS.load(Ordering::Relaxed) {
        0 => None,
        v => Some(v),
    }
}

/// Epoch-ms this process started. Gossiped: a peer's uptime resetting is the
/// single cheapest fleet-visible signal that it is cycling.
pub fn started_ms() -> u64 {
    STARTED_MS.load(Ordering::Relaxed)
}

/// Records that this process REQUESTED a controlled restart for `reason`, and stamps it into the
/// run marker at once: the next boot's history record then carries it whatever happens to the
/// graceful tail (a clean exit, the hard deadline's forced exit, a SIGKILL). Called by the shared
/// `ControlledRestart::request`, so every automatic restart path records its reason in one place
/// and a rate cap can count them across restarts.
pub fn note_controlled_restart(reason: &str) {
    if REQUESTED_RESTART.set(reason.to_string()).is_err() {
        return;
    }
    if CLEAN_EXIT_MARKED.load(Ordering::SeqCst) {
        return;
    }
    let node = NODE.get().cloned().unwrap_or_default();
    save_marker(&current_marker(&node, false));
}

/// How many restarts this node took for controlled-restart `reason` within the last `window_ms`,
/// from the persisted history (so it counts restarts of EARLIER processes, the one thing an
/// in-memory counter cannot).
///
/// FAILS CLOSED: `Err(why)` when the count cannot be trusted: the history file existed but could
/// not be read or parsed at boot, this boot's record could not be written (a full or read-only data
/// dir: the next process would count zero), or the latest run-marker write failed (a restart
/// requested now would leave no reason behind). A cap that fails open is a restart loop on exactly
/// the node whose disk is already in trouble. What it cannot see: a rollback to a binary predating
/// `RestartRecord::reason` rewrites the history without it, so after the roll-forward earlier
/// restarts count as none, at most one extra capped restart, once.
pub fn controlled_restarts_within(reason: &str, window_ms: u64) -> Result<usize, String> {
    if let Some(why) = lock(&HISTORY_FAULT).clone() {
        return Err(why);
    }
    if let Some(why) = lock(&MARKER_FAULT).clone() {
        return Err(why);
    }
    let cutoff = now_ms().saturating_sub(window_ms);
    Ok(history()
        .read()
        .iter()
        .filter(|r| r.ts_ms >= cutoff && r.reason.as_deref() == Some(reason))
        .count())
}

/// Stamps the marker as a GRACEFUL shutdown. Called from the SIGTERM/SIGINT path next to
/// `persist::flush_blocking`; without it every deploy restart would be classified `unclean_exit`
/// and the signal would be worthless. The latch is set BEFORE the write, so a heartbeat tick racing
/// this call observes the stamp as final and skips its own write.
pub fn mark_clean_exit(node: &str) {
    CLEAN_EXIT_MARKED.store(true, Ordering::SeqCst);
    let mut m = current_marker(node, true);
    m.heartbeat_ms = now_ms();
    save_marker(&m);
}

/// Heartbeat + reminder loop. The heartbeat carries the RSS and cgroup-OOM readings the NEXT boot
/// classifies against (a SIGKILLed process cannot write them on the way out), so its interval
/// trades freshness against load: too long and the recorded RSS predates the burst that killed us.
/// Once the clean-exit stamp is set the loop returns, because a later heartbeat would overwrite it
/// with `clean_exit=false`.
///
/// The reminder exists because a single boot-time log line scrolls away: while this node is
/// crash-cycling it re-states the fact periodically, so a node dying every few minutes is loud in a
/// log tail taken at ANY moment. Cycling means ANY involuntary death, OOM or not (gating on `oom >
/// 0` alone would stay silent for a SIGSEGV/panic loop, whose verdict is `unclean_exit` with
/// `oom_restarts_24h = 0`). The incident is level-triggered like every automated incident
/// (`incidents.rs`): re-asserted every pass with a TTL of three passes, so it lapses and the
/// leader-only reconciler resolves it once the node stops dying.
pub fn spawn(node: String, cloud: Arc<crate::CloudState>) {
    let interval = std::time::Duration::from_secs(env_u64("HIVE_RESTART_MARKER_SECS", 20));
    let remind_ms = env_u64("HIVE_RESTART_REMIND_SECS", 900) * 1000;
    let ttl_ms = interval.as_millis() as u64 * 3;
    tokio::spawn(async move {
        let mut last_remind = 0u64;
        loop {
            tokio::time::sleep(interval).await;
            if CLEAN_EXIT_MARKED.load(Ordering::SeqCst) {
                return;
            }
            save_marker(&current_marker(&node, false));
            let w = window_stats(&history().read());
            let now = now_ms();

            if w.crashy() >= CRASH_CYCLE_MIN {
                cloud.incidents.open(crate::incidents::OpenReq {
                    condition: format!("crash-cycle:{node}"),
                    ttl_ms,
                    title: format!("{node} is crash-cycling"),
                    severity: crate::incidents::Severity::Critical,
                    affected: vec![node.clone()],
                    message: format!(
                        "{unclean} unclean exits and {oom} OOM kills in the last 24h \
                         ({restarts} restarts total, shortest uptime {min}s). These are \
                         involuntary: the process died, so none of the controlled-restart \
                         caps ever saw it. Every health, probe, placement and \
                         FUNCTION_NO_RESPONSE symptom on this node is downstream of this. \
                         Detail: GET /v1/node/restarts on the node.",
                        unclean = w.unclean,
                        oom = w.oom,
                        restarts = w.restarts,
                        min = w.min_uptime_ms.map(|m| m / 1000).unwrap_or(0),
                    ),
                });
            } else {
                cloud.incidents.clear(&format!("crash-cycle:{node}"), "no involuntary deaths in the last 24h");
            }

            if w.crashy() >= CRASH_CYCLE_MIN && now.saturating_sub(last_remind) >= remind_ms {
                last_remind = now;
                tracing::error!(
                    node = %node,
                    oom_restarts_24h = w.oom,
                    unclean_restarts_24h = w.unclean,
                    restarts_24h = w.restarts,
                    shortest_uptime_secs = w.min_uptime_ms.map(|m| m / 1000).unwrap_or(0),
                    uptime_secs = now.saturating_sub(started_ms()) / 1000,
                    "node is CRASH-CYCLING: {n} processes died without a graceful shutdown in \
                     the last 24h. Treat unhealthy/probe/placement anomalies on this node as \
                     symptoms, and a chain-head owner that keeps dying as a fleet-wide event \
                     (leader-only jobs pause on every crash). \
                     Detail: GET /v1/node/restarts",
                    n = w.crashy(),
                );
            }
        }
    });
}

/// Operator view. NODE-LOCAL, like `/v1/dns/stats`: the marker, the history file and `/proc` all
/// belong to the node serving the request, so the gossiped `NodeInfo` counters are the fleet view.
pub fn snapshot(node: &str) -> Value {
    let hist = history().read().clone();
    let w = window_stats(&hist);
    let started = started_ms();
    json!({
        "node": node,
        "pid": std::process::id(),
        "started_ms": started,
        "uptime_ms": now_ms().saturating_sub(started),
        "boot_verdict": boot_record().get(),
        "restarts_24h": w.restarts,
        "oom_restarts_24h": w.oom,
        "unclean_restarts_24h": w.unclean,
        "last_oom_ms": last_oom_ms(),
        "shortest_uptime_ms_24h": w.min_uptime_ms,
        "rss_bytes": crate::memwatch::rss_bytes(),
        "mem_limit_bytes": mem_limit_bytes(),
        "cgroup_oom_kills": cgroup_oom_kills(),
        "boot_id": boot_id(),
        "history": hist,
    })
}
