//! Runtime stall detector: the process names its own freezes.
//!
//! The control-plane leader once froze its whole tokio runtime (both listeners dark, every timer
//! stopped) for minutes at a time, and the only trace was a silence gap in the journal found a day
//! later. A capture taken during a freeze showed a single `tokio-rt-worker` burning CPU while every
//! other worker sat parked: a long synchronous computation on a worker that every other task was
//! waiting on. Nothing inside the process noticed, because nothing inside the process ever asked
//! whether the runtime still ran tasks.
//!
//! This watcher asks, from a dedicated OS thread that no tokio task can block: every
//! `HIVE_RUNTIME_STALL_PROBE_MS` (1000) it spawns a trivial task onto the runtime and waits for it
//! to be polled. If the probe is not polled within `HIVE_RUNTIME_STALL_WARN_MS` (2000) the runtime
//! is stalled (every worker blocked, or the scheduler starved) and the watcher WARNs at once, then
//! samples `/proc/self/task/*/stat` while the stall lasts to name the threads that burn CPU through
//! it, and ERRORs the episode length when the probe finally lands. With `HIVE_RUNTIME_STALL_DUMP=1`
//! it also runs `gdb -batch -p <tid> -ex bt` on the hottest thread (the release profile keeps the
//! symbol table for exactly this) so the function is in the journal. `stats()` serves the counters
//! on the operator endpoint `/v1/admin/runtime`.
//!
//! A probe landing late is the user's experience of a freeze; a single busy worker with idle
//! siblings is NOT a stall (the probe lands on a sibling), so this measures what matters and
//! nothing else.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

static PROBES: AtomicU64 = AtomicU64::new(0);
static EPISODES: AtomicU64 = AtomicU64::new(0);
static LAST_STALL_MS: AtomicU64 = AtomicU64::new(0);
static MAX_STALL_MS: AtomicU64 = AtomicU64::new(0);
static LAST_STALL_WALL_MS: AtomicU64 = AtomicU64::new(0);
static IN_STALL: AtomicBool = AtomicBool::new(false);
static LAST_HOT: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());

/// Longest a single episode is waited out before it is recorded as
/// unrecovered and the watcher resumes probing (the process may be dying).
const MAX_EPISODE: Duration = Duration::from_secs(900);

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// The counters, for `GET /v1/admin/runtime`.
pub fn stats() -> Value {
    json!({
        "probes": PROBES.load(Ordering::Relaxed),
        "stall_episodes": EPISODES.load(Ordering::Relaxed),
        "in_stall": IN_STALL.load(Ordering::Relaxed),
        "last_stall_ms": LAST_STALL_MS.load(Ordering::Relaxed),
        "max_stall_ms": MAX_STALL_MS.load(Ordering::Relaxed),
        "last_stall_wall_ms": LAST_STALL_WALL_MS.load(Ordering::Relaxed),
        "last_hot_threads": LAST_HOT.lock().clone(),
        "warn_ms": env_u64("HIVE_RUNTIME_STALL_WARN_MS", 2000),
    })
}

/// Start the watcher thread. Call once, from inside the runtime.
pub fn spawn(handle: tokio::runtime::Handle) {
    let probe_every = Duration::from_millis(env_u64("HIVE_RUNTIME_STALL_PROBE_MS", 1000));
    let warn = Duration::from_millis(env_u64("HIVE_RUNTIME_STALL_WARN_MS", 2000));
    let dump = std::env::var("HIVE_RUNTIME_STALL_DUMP").map(|v| v == "1").unwrap_or(false);
    if let Err(e) = std::thread::Builder::new()
        .name("hive-runtime-watch".into())
        .spawn(move || loop {
            std::thread::sleep(probe_every);
            let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
            let sent = Instant::now();
            handle.spawn(async move {
                let _ = tx.try_send(());
            });
            PROBES.fetch_add(1, Ordering::Relaxed);
            match rx.recv_timeout(warn) {
                Ok(()) => {}
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => stall(&rx, sent, warn, dump),
            }
        })
    {
        tracing::error!(error = %e, "runtime watch: could not start the watcher thread");
    }
}

/// One stall episode: attribute while it lasts, record when it ends.
fn stall(rx: &std::sync::mpsc::Receiver<()>, sent: Instant, warn: Duration, dump: bool) {
    IN_STALL.store(true, Ordering::Relaxed);
    LAST_STALL_WALL_MS.store(hive_core::now_ms(), Ordering::Relaxed);
    let before = cpu_snapshot();
    tracing::warn!(
        waited_ms = warn.as_millis() as u64,
        "runtime stall: a task spawned onto the tokio runtime has not been polled -- every \
         worker is blocked or the scheduler is starved; sampling the threads that burn CPU \
         through it (see /v1/admin/runtime)"
    );
    let mut hot = String::new();
    let mut dumped = false;
    let recovered = loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => break true,
            Err(RecvTimeoutError::Disconnected) => break false,
            Err(RecvTimeoutError::Timeout) => {
                let after = cpu_snapshot();
                hot = top_cpu(&before, &after, 3);
                if dump && !dumped {
                    dumped = true;
                    if let Some(tid) = hottest_tid(&before, &after) {
                        tracing::warn!(tid, frames = %gdb_frames(tid), "runtime stall: hottest thread's user stack");
                    }
                }
                if sent.elapsed() > MAX_EPISODE {
                    break false;
                }
            }
        }
    };
    let ms = sent.elapsed().as_millis() as u64;
    EPISODES.fetch_add(1, Ordering::Relaxed);
    LAST_STALL_MS.store(ms, Ordering::Relaxed);
    MAX_STALL_MS.fetch_max(ms, Ordering::Relaxed);
    *LAST_HOT.lock() = hot.clone();
    IN_STALL.store(false, Ordering::Relaxed);
    tracing::error!(
        stall_ms = ms,
        recovered,
        hot_threads = %hot,
        "runtime stall ENDED -- the threads that burned CPU through it are named; a stall is a \
         synchronous computation or blocking call on a tokio worker that must move to \
         spawn_blocking or a dedicated thread"
    );
}

/// `(tid, comm, utime+stime ticks)` for every thread of this process (Linux).
///
/// `comm` may contain spaces, so the stat line is split after the LAST `)`; `utime` and `stime` are
/// stat fields 14 and 15, indices 11 and 12 once `pid (comm)` and the state are consumed.
fn cpu_snapshot() -> Vec<(u64, String, u64)> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return out;
    };
    for entry in dir.flatten() {
        let Some(tid) = entry.file_name().to_str().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else { continue };
        let comm = stat[..close]
            .split_once('(')
            .map(|(_, c)| c.to_string())
            .unwrap_or_default();
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let utime: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
        let stime: u64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
        out.push((tid, comm, utime + stime));
    }
    out
}

fn deltas(before: &[(u64, String, u64)], after: &[(u64, String, u64)]) -> Vec<(u64, String, u64)> {
    let mut d: Vec<(u64, String, u64)> = after
        .iter()
        .map(|(tid, comm, now)| {
            let was = before.iter().find(|(t, _, _)| t == tid).map(|(_, _, v)| *v).unwrap_or(*now);
            (*tid, comm.clone(), now.saturating_sub(was))
        })
        .filter(|(_, _, d)| *d > 0)
        .collect();
    d.sort_by(|a, b| b.2.cmp(&a.2));
    d
}

/// `tid:comm:+<ms>` for the `n` threads that consumed the most CPU between the
/// two snapshots (10 ms ticks).
fn top_cpu(before: &[(u64, String, u64)], after: &[(u64, String, u64)], n: usize) -> String {
    deltas(before, after)
        .into_iter()
        .take(n)
        .map(|(tid, comm, d)| format!("{tid}:{comm}:+{}ms", d * 10))
        .collect::<Vec<_>>()
        .join(" ")
}

fn hottest_tid(before: &[(u64, String, u64)], after: &[(u64, String, u64)]) -> Option<u64> {
    deltas(before, after).first().map(|(tid, _, _)| *tid)
}

/// User-space frames of `tid` via gdb (symbols are kept in the release
/// profile), one line, bounded to 20 s. Empty when gdb is absent or refused.
fn gdb_frames(tid: u64) -> String {
    let out = std::process::Command::new("timeout")
        .args(["20", "gdb", "-batch", "-p", &tid.to_string(), "-ex", "bt 25"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.starts_with('#'))
            .map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join(" | "),
        Err(e) => format!("gdb unavailable: {e}"),
    }
}
