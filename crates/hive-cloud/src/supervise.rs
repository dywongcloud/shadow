//! Supervision for long-lived background loops.
//!
//! A panic inside a `tokio::spawn`ed loop kills THAT task silently and forever while `/healthz`
//! stays green: the node keeps serving with a dead subsystem, and the only symptom is log lines
//! that stop appearing (which is also what "nothing to report" looks like, so nobody notices).
//!
//! [`spawn_supervised`] gives a loop two properties:
//! * **It restarts.** A panic (or an unexpected return: these loops are `loop {}`s and must never
//! return) is logged loudly and the body is re-spawned after a backoff that doubles up to a cap, so
//! a deterministic crash cannot spin hot.
//! * **It is visible.** Every supervised loop registers in a node-local table with its restart
//! count and a heartbeat the body bumps each iteration via [`beat`]. [`snapshot`] serves that table
//! (`admin::tasks_health`, `GET /v1/tasks/health`), so "is the reconciler actually running?" is one
//! GET. This is deliberately node-local state behind a GET: each node reports its OWN loops (asking
//! node X about node Y's tasks is meaningless), so the round-robin read split does not apply.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;

/// One supervised loop's public state.
#[derive(Serialize)]
pub struct TaskHealth {
    pub name: &'static str,
    /// How many times the body has been re-spawned after a panic/return.
    pub restarts: u64,
    /// ms since the body last called [`beat`] — the liveness signal. `null`
    /// until the first beat (a loop may sleep before its first iteration).
    pub last_beat_ms_ago: Option<u64>,
}

struct Entry {
    restarts: AtomicU64,
    /// Epoch ms of the last [`beat`]; 0 = never.
    last_beat_ms: AtomicU64,
}

fn registry() -> &'static parking_lot::Mutex<std::collections::BTreeMap<&'static str, Arc<Entry>>> {
    static R: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::BTreeMap<&'static str, Arc<Entry>>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

fn entry(name: &'static str) -> Arc<Entry> {
    registry()
        .lock()
        .entry(name)
        .or_insert_with(|| {
            Arc::new(Entry {
                restarts: AtomicU64::new(0),
                last_beat_ms: AtomicU64::new(0),
            })
        })
        .clone()
}

/// Marks the named loop alive. Call it once per iteration, at the TOP of the loop body: a beat
/// after the work would stall for the whole duration of a slow pass and read as dead.
pub fn beat(name: &'static str) {
    entry(name)
        .last_beat_ms
        .store(hive_core::now_ms(), Ordering::Relaxed);
}

/// Every supervised loop's health, for the admin surface.
pub fn snapshot() -> Vec<TaskHealth> {
    let now = hive_core::now_ms();
    registry()
        .lock()
        .iter()
        .map(|(name, e)| {
            let beat = e.last_beat_ms.load(Ordering::Relaxed);
            TaskHealth {
                name,
                restarts: e.restarts.load(Ordering::Relaxed),
                last_beat_ms_ago: (beat > 0).then(|| now.saturating_sub(beat)),
            }
        })
        .collect()
}

/// This process's own memory pressure, the direct signal for a PARTIAL wedge: the customer-facing
/// port keeps serving (what DNS/mesh health already covers, through the peer-observed gossip probe
/// in `spawn_health_loop`, a real `GOSSIP_GET` dispatch rather than a bare connect) while the
/// process quietly climbs toward its cgroup memory ceiling, and systemd `is-active` and the admin
/// `/healthz` stay green throughout, since a process that answers can still be minutes from an OOM
/// kill. Reading the number directly, rather than inferring RSS from behavior after the fact, lets
/// an operator or an alarm catch the NEXT climb before it reaches the cap.
#[derive(Serialize)]
pub struct MemoryPressure {
    /// This process's resident set (MB), from `/proc/self/status` on Linux; `None` off Linux (macOS
    /// dev nodes), where no portable equivalent is worth the complexity for a fleet that is Linux
    /// in production.
    pub rss_mb: Option<u64>,
    /// The cgroup v2 memory ceiling (MB) this process is confined to, from
    /// `/sys/fs/cgroup/memory.max`: the number that matters (the host's total RAM is not the
    /// constraint, the systemd `MemoryMax=` setting is). `None` when unreadable (no cgroup, cgroup
    /// v1, or a unit with no limit: `max` is left unparsed rather than reported as an artificial
    /// number).
    pub cgroup_limit_mb: Option<u64>,
    /// rss_mb / cgroup_limit_mb, when both are known — the number worth
    /// alerting on directly rather than computing at every call site.
    pub pct_of_limit: Option<f64>,
}

pub fn memory_pressure() -> MemoryPressure {
    let rss_mb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
                .map(|kb| kb / 1024)
        });
    let cgroup_limit_mb = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024));
    let pct_of_limit = match (rss_mb, cgroup_limit_mb) {
        (Some(r), Some(l)) if l > 0 => Some((r as f64 / l as f64) * 100.0),
        _ => None,
    };
    MemoryPressure {
        rss_mb,
        cgroup_limit_mb,
        pct_of_limit,
    }
}

/// Spawns `make()`'s future and keeps it alive: on panic OR return it logs, backs off (5 s doubling
/// to 5 min: a deterministic crash must not spin hot) and re-spawns. `make` is a factory because
/// the future is consumed per attempt. A cancelled task (the runtime shutting down) is not
/// respawned.
///
/// The body should call `beat(name)` each iteration; supervision without the heartbeat still
/// restarts crashes but cannot tell "healthy and quiet" from "alive but wedged mid-await".
pub fn spawn_supervised<F, Fut>(name: &'static str, make: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let e = entry(name);
    tokio::spawn(async move {
        let mut backoff_secs = 5u64;
        loop {
            let attempt = tokio::spawn(make());
            match attempt.await {
                Ok(()) => {
                    tracing::error!(task = name, "supervised background loop RETURNED (these loops must never exit) — restarting");
                }
                Err(join) if join.is_panic() => {
                    let what = {
                        let p = join.into_panic();
                        p.downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| p.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic payload".into())
                    };
                    tracing::error!(task = name, panic = %what, "supervised background loop PANICKED — restarting after backoff");
                }
                Err(_) => {
                    return;
                }
            }
            e.restarts.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(300);
        }
    });
}
