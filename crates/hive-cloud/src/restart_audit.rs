//! Restart audit — makes an OOM-kill restart SELF-EVIDENT instead of silent.
//!
//! The gap this closes, stated as what actually happened. Nodes on this fleet
//! are killed by the cgroup OOM killer and systemd restarts them a second
//! later. `Restart=always` does its job perfectly, and that is the problem:
//! the process comes back, healthz answers, the mesh reconverges, and NOTHING
//! at the platform level records that the node died. No log line (the old
//! process was SIGKILLed — it cannot log its own death, and the new one starts
//! with a clean slate), no metric, no gossip field, no incident. The only
//! evidence is a kernel line in `dmesg` on that specific host.
//!
//! Consequence, measured: a node cycling every 2–3 hours presented to
//! operators as "random unhealthy nodes" for a whole session. Every downstream
//! symptom — probe failures, dropped trunks, a placement gap, a DNS withdrawal
//! — was investigated as its own mystery, because the one fact that explains
//! all of them (the process is being killed) was only visible to whoever
//! thought to SSH in and read `dmesg` on the right host at the right time.
//!
//! `memwatch` is the other half of this and is deliberately not this: it
//! watches RSS climb and tries to capture a profile BEFORE the kill. This
//! module runs AFTER, on the next boot, and answers "did we just die, and
//! why?" — which is the question that was unanswerable, and which stays
//! answerable even for a burst too fast for any sampler to catch.
//!
//! ## How the verdict is reached
//!
//! A marker file (`$HIVE_DATA/run_marker.json`) is written at boot and
//! refreshed by a heartbeat, carrying this process's pid, start time, the
//! host's boot id, its last observed RSS and memory ceiling, and the cgroup's
//! OOM counter. The SIGTERM path stamps `clean_exit: true`. So at boot the
//! previous process's marker is a witness statement, and the verdict is:
//!
//! * `clean_exit` set → **clean_restart** (a deploy, a `systemctl restart`).
//! * kernel log names the previous PID in an OOM kill → **oom_kill**. Direct
//!   evidence, and the pid match is required — an older kill's line still in
//!   the ring buffer must never be re-reported as this restart's cause.
//! * this cgroup's `memory.events` already shows `oom_kill > 0` → **oom_kill**.
//! * host boot id changed → **host_reboot** (the host went down, not us).
//! * last heartbeat RSS was within `HIVE_OOM_SUSPECT_PCT` of the memory
//!   ceiling → **oom_suspected**. Named as a suspicion, never as proof: on a
//!   host whose kernel buffer has rolled or is unreadable this is all the
//!   evidence there is, and calling it `oom_kill` would be a lie the operator
//!   cannot audit.
//! * a panic hook record naming the prior process → **panic_abort** (including
//!   the original panic that a later destructor panic would otherwise bury).
//! * otherwise → **unclean_exit** (SIGKILL from something else, power loss
//!   with no boot-id change... also worth knowing, also invisible today).
//!
//! Every verdict is appended to a bounded history file, so "this node cycles
//! every 2–3 hours" is a readable list of timestamps and uptimes rather than
//! an inference someone has to make. The counters ride gossip
//! (`NodeInfo::oom_restarts_24h` / `last_oom_ms` / `started_ms`), so the fleet
//! view shows it without logging into anything.
//!
//! Nothing here can fail a boot: every read is best-effort, an unreadable or
//! corrupt marker/history is treated as absent (with a WARN), and no verdict
//! is ever fabricated from a missing source.

use hive_core::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

/// Set by `mark_clean_exit`; read by the heartbeat before every marker write.
/// The graceful tail runs tens of seconds past the clean-exit stamp, so a
/// 20 s heartbeat tick lands after it and — without this latch — rewrites the
/// marker with `clean_exit=false`, reporting every self-exiting restart as
/// UNCLEAN on the next boot (measured on every graceful stop of 75).
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

/// The first panic observed in a process. Persisting this separately matters:
/// a second panic in a destructor aborts Rust before the original panic's
/// useful context can survive in the usual log tail.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PanicRecord {
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    ts_ms: u64,
    #[serde(default)]
    thread: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    location: String,
}

fn last_panic_path() -> std::path::PathBuf {
    crate::persist::data_dir().join("last_panic.json")
}

/// The previous process's witness statement. Every field `serde(default)` so a
/// marker written by an older build still deserializes — a missing field must
/// degrade the verdict's confidence, never discard the whole record.
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
}

/// One boot's verdict. Serialized into the history file and the endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestartRecord {
    pub ts_ms: u64,
    /// `first_boot` | `clean_restart` | `oom_kill` | `oom_suspected` |
    /// `host_reboot` | `panic_abort` | `unclean_exit`
    pub verdict: String,
    /// Human-readable statement of WHAT was observed, never a restatement of
    /// the verdict — an operator must be able to disagree with the conclusion
    /// by reading the evidence.
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
}

impl RestartRecord {
    fn is_oom(&self) -> bool {
        self.verdict == "oom_kill" || self.verdict == "oom_suspected"
    }
}

static STARTED_MS: AtomicU64 = AtomicU64::new(0);
static LAST_OOM_MS: AtomicU64 = AtomicU64::new(0);

fn boot_record() -> &'static OnceLock<RestartRecord> {
    static R: OnceLock<RestartRecord> = OnceLock::new();
    &R
}

fn history() -> &'static parking_lot::RwLock<Vec<RestartRecord>> {
    static H: OnceLock<parking_lot::RwLock<Vec<RestartRecord>>> = OnceLock::new();
    H.get_or_init(|| parking_lot::RwLock::new(Vec::new()))
}

// ---------------------------------------------------------------------------
// Host readings. All best-effort; `None` means UNKNOWN and is never coerced to
// a number, because a fabricated reading here produces a fabricated verdict.
// ---------------------------------------------------------------------------

fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// This process's cgroup path as an absolute `/sys/fs/cgroup/...` dir (cgroup
/// v2 only — v1's split hierarchy has no single `memory.events` and the fleet
/// is v2).
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

/// `memory.events: oom_kill` — the kernel's own count of processes this cgroup
/// has had killed. Note the counter lives with the CGROUP: systemd recreates a
/// unit's cgroup on restart, so this normally reads 0 on a fresh boot and a
/// NON-zero value is itself strong evidence (something in this cgroup was
/// killed since it was created, and this process was just started into it).
fn cgroup_oom_kills() -> Option<u64> {
    let dir = cgroup_dir()?;
    let text = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// The ceiling RSS is actually measured against: the cgroup's `memory.max`
/// when it is a real number, else the host's MemTotal. `memory.max` reading
/// `max` (no limit) is not a limit and must fall through — treating the string
/// as 0 would make every node look permanently 100% of its ceiling.
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

/// Scan the kernel ring buffer for an OOM kill naming `prev_pid`.
///
/// `/dev/kmsg` rather than shelling out to `dmesg`/`journalctl`: no subprocess,
/// no PATH assumption, and it works on a node whose journal is volatile. Opened
/// NON-BLOCKING because a blocking read on `/dev/kmsg` waits forever for the
/// NEXT message once the buffer is drained — that would hang boot.
///
/// Returns `(direct_evidence, all_oom_lines_seen)`. The direct verdict requires
/// the previous pid to appear in the line: the ring buffer routinely still
/// holds OOM lines from EARLIER kills, and reporting one of those as this
/// restart's cause would manufacture a fresh incident out of old history. The
/// other lines are still returned, as context an operator can read.
#[cfg(unix)]
fn kmsg_oom_evidence(prev_pid: u32) -> (Option<String>, Vec<String>) {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    // O_NONBLOCK (0o4000 on Linux). A literal rather than a whole `libc`
    // dependency for one constant; `/dev/kmsg` only exists on Linux, so on any
    // other unix the open below simply fails and the caller falls back.
    const O_NONBLOCK: i32 = 0o4000;
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/kmsg")
    {
        Ok(f) => f,
        // Unreadable (no permission, a container without it) — UNKNOWN, and
        // the caller falls back to the RSS suspicion rather than inventing a
        // verdict.
        Err(_) => return (None, Vec::new()),
    };
    let mut direct = None;
    let mut lines: Vec<String> = Vec::new();
    // Each read() returns exactly one record. Bounded so a huge ring buffer
    // cannot stretch boot.
    let mut buf = [0u8; 8192];
    for _ in 0..8192 {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let rec = String::from_utf8_lossy(&buf[..n]);
                // "prio,seq,ts,flag;message"
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
                // EPIPE (32) = the buffer wrapped past our read position; the
                // kernel repositions us and the next read succeeds, so this is
                // a continue, not a stop.
                if e.raw_os_error() == Some(32) {
                    continue;
                }
                break; // EAGAIN (drained) or anything else
            }
        }
    }
    (direct, lines)
}

#[cfg(not(unix))]
fn kmsg_oom_evidence(_prev_pid: u32) -> (Option<String>, Vec<String>) {
    (None, Vec::new())
}

// ---------------------------------------------------------------------------
// Marker + history I/O. Atomic temp+rename everywhere (a torn marker read at
// boot would be indistinguishable from a crash).
// ---------------------------------------------------------------------------

fn write_atomic(path: &std::path::Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
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
    }
}

fn save_marker(m: &Marker) {
    if let Ok(bytes) = serde_json::to_vec(m) {
        write_atomic(&marker_path(), &bytes);
    }
}

fn load_history() -> Vec<RestartRecord> {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<RestartRecord>>(&t).ok())
        .map(|mut v| {
            if v.len() > HISTORY_MAX {
                // Enforce the cap on LOAD as well as on write, so no on-disk
                // file can reload past it (the `dns_geo` MAX_ENTRIES rule).
                let start = v.len() - HISTORY_MAX;
                v.drain(..start);
            }
            v
        })
        .unwrap_or_default()
}

fn save_history(records: &[RestartRecord]) {
    if let Ok(bytes) = serde_json::to_vec(records) {
        write_atomic(&history_path(), &bytes);
    }
}

fn read_last_panic() -> Option<PanicRecord> {
    std::fs::read_to_string(last_panic_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// Preserve the first panic for the next boot, then delegate to Rust's default
/// hook so stderr/backtraces remain unchanged. The hook deliberately avoids
/// tracing and locks: it can run while another thread is unwinding.
pub fn install_panic_hook() {
    static PANIC_RECORDED: AtomicBool = AtomicBool::new(false);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        if PANIC_RECORDED.swap(true, Ordering::Relaxed) {
            return;
        }
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into());
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown".into());
        let record = PanicRecord {
            pid: std::process::id(),
            ts_ms: now_ms(),
            thread: std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_string(),
            message,
            location,
        };
        if let Ok(bytes) = serde_json::to_vec(&record) {
            write_atomic(&last_panic_path(), &bytes);
        }
    }));
}

// ---------------------------------------------------------------------------
// The audit itself.
// ---------------------------------------------------------------------------

/// Classify the previous process's exit from its marker plus live host
/// evidence. Pure given its inputs; the I/O is all in [`audit_boot`].
fn classify(
    prev: &Marker,
    now_boot_id: Option<&str>,
    suspect_pct: u64,
    panic: Option<&PanicRecord>,
) -> RestartRecord {
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
    };

    if prev.clean_exit {
        rec.verdict = "clean_restart".into();
        rec.evidence = "previous process recorded a graceful shutdown (SIGTERM path ran)".into();
        return rec;
    }

    if let Some(panic) = panic.filter(|panic| panic.pid == prev.pid) {
        rec.verdict = "panic_abort".into();
        rec.evidence = format!(
            "panic hook captured {} on thread {:?} at {} (epoch-ms {}) before the process terminated",
            panic.message, panic.thread, panic.location, panic.ts_ms
        );
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

/// Read the previous run's marker, reach a verdict, log it LOUDLY, append it
/// to the bounded history, and stamp a fresh marker for this process.
///
/// Call once, early in boot, before anything that can wedge — it is pure local
/// file I/O and cannot fail the boot.
pub fn audit_boot(node: &str) -> Value {
    STARTED_MS.store(now_ms(), Ordering::Relaxed);
    let suspect_pct = env_u64("HIVE_OOM_SUSPECT_PCT", 90).min(100);
    let now_boot = boot_id();
    let last_panic = read_last_panic();

    let rec = match read_marker() {
        Some(prev) => classify(&prev, now_boot.as_deref(), suspect_pct, last_panic.as_ref()),
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
        },
    };

    let mut hist = load_history();
    hist.push(rec.clone());
    if hist.len() > HISTORY_MAX {
        let start = hist.len() - HISTORY_MAX;
        hist.drain(..start);
    }
    save_history(&hist);
    let (restarts_24h, oom_24h, min_uptime) = window_stats(&hist);
    if let Some(ms) = hist.iter().filter(|r| r.is_oom()).map(|r| r.ts_ms).max() {
        LAST_OOM_MS.store(ms, Ordering::Relaxed);
    }
    *history().write() = hist;
    let _ = boot_record().set(rec.clone());

    // Stamp this process's marker immediately: a node that OOMs before the
    // first heartbeat tick must still leave a witness behind.
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
            "UNCLEAN RESTART: the previous process died without a graceful shutdown"
        ),
        "panic_abort" => tracing::error!(
            node,
            evidence = %rec.evidence,
            prev_pid = rec.prev_pid,
            prev_uptime_secs = rec.prev_uptime_ms / 1000,
            restarts_24h,
            "PANIC RESTART: the previous process panicked before aborting"
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

/// `(restarts_24h, oom_restarts_24h, shortest_uptime_ms_24h)`.
///
/// `first_boot` is excluded from the restart count — a wiped data dir is not a
/// restart, and counting it as one would make every fresh node look unstable.
fn window_stats(hist: &[RestartRecord]) -> (u32, u32, Option<u64>) {
    let cutoff = now_ms().saturating_sub(DAY_MS);
    let recent: Vec<&RestartRecord> = hist
        .iter()
        .filter(|r| r.ts_ms >= cutoff && r.verdict != "first_boot")
        .collect();
    let oom = recent.iter().filter(|r| r.is_oom()).count() as u32;
    let min_uptime = recent
        .iter()
        .filter(|r| r.prev_uptime_ms > 0)
        .map(|r| r.prev_uptime_ms)
        .min();
    (recent.len() as u32, oom, min_uptime)
}

/// Gossiped counters (see `NodeInfo`), so a cycling node is visible from the
/// dashboard and `/v1/nodes` without reading any host's logs.
pub fn oom_restarts_24h() -> u32 {
    window_stats(&history().read()).1
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

/// Stamp the marker as a GRACEFUL shutdown. Called from the SIGTERM/SIGINT
/// path next to `persist::flush_blocking` — without it every deploy restart
/// would be classified `unclean_exit` and the signal would be worthless.
pub fn mark_clean_exit(node: &str) {
    // Latched BEFORE the write so a heartbeat tick racing this call observes
    // the stamp as final and skips its own write.
    CLEAN_EXIT_MARKED.store(true, Ordering::SeqCst);
    let mut m = current_marker(node, true);
    m.heartbeat_ms = now_ms();
    save_marker(&m);
}

/// Heartbeat + reminder loop.
///
/// The heartbeat is what makes the verdict possible at all: it carries the
/// RSS and cgroup-OOM readings the NEXT boot classifies against, and a
/// SIGKILLed process obviously cannot write them on the way out. Interval is a
/// tradeoff — too long and the recorded RSS predates the burst that killed us.
///
/// The reminder exists because a single boot-time log line scrolls away. While
/// this node has OOM restarts inside the 24h window it re-states the fact
/// periodically, so a node cycling every 2–3 hours is loud in a log tail taken
/// at ANY moment, not just the one right after a kill.
pub fn spawn(node: String) {
    let interval = std::time::Duration::from_secs(env_u64("HIVE_RESTART_MARKER_SECS", 20));
    let remind_ms = env_u64("HIVE_RESTART_REMIND_SECS", 900) * 1000;
    tokio::spawn(async move {
        let mut last_remind = 0u64;
        loop {
            tokio::time::sleep(interval).await;
            if CLEAN_EXIT_MARKED.load(Ordering::SeqCst) {
                // The clean-exit stamp is the marker's final word; a heartbeat
                // after it would overwrite it with clean_exit=false.
                return;
            }
            save_marker(&current_marker(&node, false));
            let (restarts, oom, min_uptime) = window_stats(&history().read());
            let now = now_ms();
            if oom > 0 && now.saturating_sub(last_remind) >= remind_ms {
                last_remind = now;
                tracing::error!(
                    node = %node,
                    oom_restarts_24h = oom,
                    restarts_24h = restarts,
                    shortest_uptime_secs = min_uptime.map(|m| m / 1000).unwrap_or(0),
                    uptime_secs = now.saturating_sub(started_ms()) / 1000,
                    "node is OOM-CYCLING: it has been killed for memory within the last 24h. \
                     Treat unhealthy/probe/placement anomalies on this node as symptoms. \
                     Detail: GET /v1/node/restarts"
                );
            }
        }
    });
}

/// Operator view. NODE-LOCAL, like `/v1/dns/stats`: the marker, the history
/// file and `/proc` all belong to the node serving the request. Through the
/// dashboard's `/ops/*` proxy this reads the LEADER's restarts, not the
/// page-serving node's — the gossiped `NodeInfo` counters are the fleet view.
pub fn snapshot(node: &str) -> Value {
    let hist = history().read().clone();
    let (restarts, oom, min_uptime) = window_stats(&hist);
    let started = started_ms();
    json!({
        "node": node,
        "pid": std::process::id(),
        "started_ms": started,
        "uptime_ms": now_ms().saturating_sub(started),
        "boot_verdict": boot_record().get(),
        "restarts_24h": restarts,
        "oom_restarts_24h": oom,
        "last_oom_ms": last_oom_ms(),
        "shortest_uptime_ms_24h": min_uptime,
        "rss_bytes": crate::memwatch::rss_bytes(),
        "mem_limit_bytes": mem_limit_bytes(),
        "cgroup_oom_kills": cgroup_oom_kills(),
        "boot_id": boot_id(),
        "history": hist,
    })
}
