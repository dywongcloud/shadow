//! Per-job log fan-out: a replay ring bounded on BOTH line count and total bytes (either alone is
//! escapable) plus a broadcast channel for live subscribers. Eviction is never silent: dropped
//! lines are counted, a `warn!` fires when a bus first drops, and every reader gets an explicit
//! marker line.

use hive_core::LogLine;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;
use tracing::warn;

/// Retained replay lines per job; only the REPLAY tail is bounded, the live broadcast is
/// unaffected.
const MAX_LINES: usize = 5_000;
/// Retained replay bytes per job. The binding constraint in practice: 5k lines
/// of a `--verbose` build is small, 5k lines of base64 is not.
const MAX_BYTES: usize = 4 * 1024 * 1024;

fn max_lines() -> usize {
    env_usize("HIVE_LOGBUS_MAX_LINES", MAX_LINES)
}

fn max_bytes() -> usize {
    env_usize("HIVE_LOGBUS_MAX_BYTES", MAX_BYTES)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[derive(Default)]
struct Ring {
    lines: VecDeque<LogLine>,
    bytes: usize,
    dropped_lines: u64,
    dropped_bytes: u64,
}

pub struct LogBus {
    ring: Mutex<Ring>,
    tx: broadcast::Sender<LogLine>,
    done: AtomicBool,
    /// One `warn!` per bus, not one per evicted line — the eviction path runs
    /// once per line for the rest of a chatty build.
    warned: AtomicBool,
}

impl LogBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        LogBus {
            ring: Mutex::new(Ring::default()),
            tx,
            done: AtomicBool::new(false),
            warned: AtomicBool::new(false),
        }
    }

    pub fn push(&self, line: LogLine) {
        let (max_l, max_b) = (max_lines(), max_bytes());
        let first_drop = {
            let mut r = self.ring.lock();
            r.bytes += line.line.len();
            r.lines.push_back(line.clone());
            let mut evicted_any = false;
            while r.lines.len() > max_l || (r.bytes > max_b && r.lines.len() > 1) {
                match r.lines.pop_front() {
                    Some(old) => {
                        r.bytes = r.bytes.saturating_sub(old.line.len());
                        r.dropped_lines += 1;
                        r.dropped_bytes += old.line.len() as u64;
                        evicted_any = true;
                    }
                    None => break,
                }
            }
            evicted_any && !self.warned.swap(true, Ordering::Relaxed)
        };
        if first_drop {
            warn!(
                max_lines = max_l,
                max_bytes = max_b,
                "build log exceeded the replay buffer bound; oldest lines are being dropped \
                 (live streaming is unaffected)"
            );
        }
        let _ = self.tx.send(line);
    }

    pub fn mark_done(&self) {
        self.done.store(true, Ordering::SeqCst);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// `(dropped_lines, dropped_bytes)` — what the bound cost this job.
    pub fn dropped(&self) -> (u64, u64) {
        let r = self.ring.lock();
        (r.dropped_lines, r.dropped_bytes)
    }

    /// Bytes currently retained for replay — the gauge that says whether the
    /// bound is doing anything.
    pub fn retained_bytes(&self) -> usize {
        self.ring.lock().bytes
    }

    /// Snapshot of the retained replay tail, plus a receiver for future lines.
    pub fn subscribe(&self) -> (Vec<LogLine>, broadcast::Receiver<LogLine>) {
        let r = self.ring.lock();
        let rx = self.tx.subscribe();
        (materialize(&r), rx)
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        materialize(&self.ring.lock())
    }
}

/// Copy the ring out for a reader, prefixed with an honest marker when the bound dropped anything
/// (a silently-shortened log reads like a build that started later).
fn materialize(r: &Ring) -> Vec<LogLine> {
    let mut out: Vec<LogLine> = Vec::with_capacity(r.lines.len() + 1);
    if r.dropped_lines > 0 {
        out.push(LogLine {
            ts_ms: r.lines.front().map(|l| l.ts_ms).unwrap_or(0),
            stream: hive_core::LogStream::System,
            line: format!(
                "[hive: {} earlier line(s) / {} bytes dropped — replay buffer bound reached]",
                r.dropped_lines, r.dropped_bytes
            ),
        });
    }
    out.extend(r.lines.iter().cloned());
    out
}

impl Default for LogBus {
    fn default() -> Self {
        Self::new()
    }
}
