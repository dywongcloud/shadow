//! Supervision for long-lived background loops.
//!
//! A panic inside a `tokio::spawn`ed loop kills THAT task silently and forever
//! while `/healthz` stays green — the node keeps serving requests with a dead
//! subsystem, and the only symptom is log lines that stop appearing (which is
//! also what "nothing to report" looks like, so nobody notices). This is not
//! hypothetical: guardian's init panic ran that way for weeks, and
//! `spawn_world_reconcile`'s total log silence was indistinguishable from
//! "queue is clean" until the queue was inspected by hand.
//!
//! [`spawn_supervised`] gives a loop two properties:
//!  * **It restarts.** A panic (or an unexpected return — these loops are
//!    `loop {}`s and must never return) is logged loudly and the body is
//!    re-spawned after a backoff that doubles up to a cap, so a deterministic
//!    crash can't spin hot.
//!  * **It is visible.** Every supervised loop registers in a node-local table
//!    with its restart count and a heartbeat the body bumps each iteration via
//!    [`beat`]. [`snapshot`] serves that table (see `admin::tasks_health`), so
//!    "is the reconciler actually running?" is one GET instead of a journal
//!    archaeology session. This is deliberately node-local state behind a GET —
//!    each node reports its OWN loops; asking node X about node Y's tasks is
//!    meaningless, so the round-robin read split does not apply.

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
    last_beat_ms: AtomicU64, // 0 = never
}

fn registry() -> &'static parking_lot::Mutex<std::collections::BTreeMap<&'static str, Arc<Entry>>> {
    static R: std::sync::OnceLock<parking_lot::Mutex<std::collections::BTreeMap<&'static str, Arc<Entry>>>> =
        std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

fn entry(name: &'static str) -> Arc<Entry> {
    registry()
        .lock()
        .entry(name)
        .or_insert_with(|| Arc::new(Entry { restarts: AtomicU64::new(0), last_beat_ms: AtomicU64::new(0) }))
        .clone()
}

/// Mark the named loop alive. Call once per iteration, at the TOP of the loop
/// body — a beat after the work would stall for the whole duration of a slow
/// pass and read as dead.
pub fn beat(name: &'static str) {
    entry(name).last_beat_ms.store(hive_core::now_ms(), Ordering::Relaxed);
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

/// Spawn `make()`'s future and keep it alive: on panic OR return, log, back
/// off (5s doubling to 5min — a deterministic crash must not spin hot), and
/// re-spawn. `make` is a factory because the future is consumed per attempt.
///
/// The body should call `beat(name)` each iteration; supervision without the
/// heartbeat still restarts crashes but cannot distinguish "healthy and quiet"
/// from "alive but wedged mid-await", which is exactly the sj2 shape.
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
                    // Cancelled — the runtime is shutting down; do not respawn.
                    return;
                }
            }
            e.restarts.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(300);
        }
    });
}
