//! Per-job log fan-out: a buffer for replay plus a broadcast channel for live
//! subscribers (the API's log-streaming endpoint).

use hive_core::LogLine;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;

pub struct LogBus {
    buffer: Mutex<Vec<LogLine>>,
    tx: broadcast::Sender<LogLine>,
    done: AtomicBool,
}

impl LogBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        LogBus {
            buffer: Mutex::new(Vec::new()),
            tx,
            done: AtomicBool::new(false),
        }
    }

    pub fn push(&self, line: LogLine) {
        self.buffer.lock().push(line.clone());
        // Err just means no live subscribers; the buffer still has it.
        let _ = self.tx.send(line);
    }

    pub fn mark_done(&self) {
        self.done.store(true, Ordering::SeqCst);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// Snapshot of everything so far, plus a receiver for future lines.
    pub fn subscribe(&self) -> (Vec<LogLine>, broadcast::Receiver<LogLine>) {
        // Lock the buffer while creating the receiver so we don't miss or dup
        // a line that arrives mid-subscribe.
        let buf = self.buffer.lock();
        let rx = self.tx.subscribe();
        (buf.clone(), rx)
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.buffer.lock().clone()
    }
}

impl Default for LogBus {
    fn default() -> Self {
        Self::new()
    }
}
