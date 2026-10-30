//! Append-only **audit log**: the durable, historical record of every state mutation, for
//! compliance and ACID durability.
//!
//! ACID model for the platform store:
//! * **Atomicity**: each mutation is followed by an atomic snapshot write (temp file + `rename`,
//! see [`crate::persist::save`]).
//! * **Consistency**: tenant scoping and cardinality are enforced at the API layer; the audit log
//! records the resulting transition.
//! * **Isolation**: every store guards its data with a `RwLock`, so concurrent readers and writers
//! never observe a partial update.
//! * **Durability**: the snapshot is `fsync`-ed and every audit entry is appended (and `fsync`-ed)
//! to `audit.jsonl`, an immutable history that is never rewritten in place. The append happens on
//! ONE writer thread that group-commits (everything queued since the last pass goes out under a
//! single `fsync`), never on the serving worker that recorded the mutation: a `sync_all` on a
//! loaded disk takes milliseconds to seconds, and a mutation handler that waits for it parks a
//! tokio worker. [`flush_blocking`] drains the queue at graceful shutdown, beside
//! `persist::flush_blocking`.

use hive_core::now_ms;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

/// Entries kept in memory for `recent`, the operator audit view and the follower sync.
const RING_CAP: usize = 2000;
/// Lines read back from the end of `audit.jsonl` at boot to seed the ring and the sequence.
const BOOT_TAIL_LINES: usize = 1000;
/// Read size of the backwards tail scan.
const TAIL_CHUNK_BYTES: usize = 64 * 1024;
/// The tail scan never reads more than this, however long the lines are: a history of hundreds of
/// MB must not be read (or held) whole at boot.
const TAIL_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Default bound on lines queued for the writer (`HIVE_AUDIT_QUEUE_MAX`). A full queue means the
/// disk has stalled for far longer than one group commit; a further entry then stays in the ring
/// only and is counted and logged, instead of parking the worker that recorded it.
const DEFAULT_QUEUE_MAX: usize = 65_536;
/// Most queued lines one `fsync` covers.
const MAX_BATCH: usize = 4096;
/// How long `flush_blocking` waits for the writer, per log.
const FLUSH_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonic sequence number (transaction ordering).
    pub seq: u64,
    pub ts_ms: u64,
    pub tenant: String,
    pub actor: String,
    /// e.g. "create" | "update" | "delete" | "charge".
    pub action: String,
    /// e.g. "deployment" | "database" | "env" | "team" | "billing".
    pub entity: String,
    pub entity_id: String,
    #[serde(default)]
    pub detail: String,
}

enum Job {
    Line(String),
    /// Acknowledged once every line queued before it is on disk.
    Flush(SyncSender<()>),
}

/// What one group commit carries: the lines to append and the flush barriers to acknowledge once
/// they are on disk.
#[derive(Default)]
struct Batch {
    text: String,
    lines: usize,
    acks: Vec<SyncSender<()>>,
}

impl Batch {
    fn take(&mut self, job: Job) {
        match job {
            Job::Line(line) => {
                self.text.push_str(&line);
                self.text.push('\n');
                self.lines += 1;
            }
            Job::Flush(ack) => self.acks.push(ack),
        }
    }
}

/// The append side of one log: a bounded queue into one writer thread.
struct Writer {
    tx: SyncSender<Job>,
    /// Entries that never reached `audit.jsonl` because the queue was full or the writer is gone.
    dropped: AtomicU64,
}

/// Every live writer, so shutdown can drain them without a handle on the `CloudState`.
fn writers() -> &'static Mutex<Vec<Weak<Writer>>> {
    static W: OnceLock<Mutex<Vec<Weak<Writer>>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

fn queue_max() -> usize {
    std::env::var("HIVE_AUDIT_QUEUE_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_QUEUE_MAX)
}

impl Writer {
    fn spawn(path: PathBuf) -> Option<Arc<Writer>> {
        let (tx, rx) = sync_channel::<Job>(queue_max());
        let spawned = std::thread::Builder::new()
            .name("audit-writer".into())
            .spawn(move || write_loop(path, rx));
        if let Err(error) = spawned {
            tracing::error!(%error, "audit: could not start the writer thread; entries are appended synchronously");
            return None;
        }
        let writer = Arc::new(Writer {
            tx,
            dropped: AtomicU64::new(0),
        });
        let mut all = writers().lock();
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&writer));
        Some(writer)
    }

    /// Queues one line without ever waiting.
    fn enqueue(&self, line: String) {
        match self.tx.try_send(Job::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped % 4096 == 0 {
                    tracing::error!(
                        dropped,
                        "audit: the writer queue is full or gone; entries are kept in memory only and are NOT in audit.jsonl"
                    );
                }
            }
        }
    }

    /// Waits (bounded) until everything queued so far is on disk.
    fn flush(&self, deadline: Duration) {
        let started = Instant::now();
        let (ack_tx, ack_rx) = sync_channel::<()>(1);
        let mut job = Job::Flush(ack_tx);
        loop {
            match self.tx.try_send(job) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) if started.elapsed() < deadline => {
                    job = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
        let _ = ack_rx.recv_timeout(deadline.saturating_sub(started.elapsed()));
    }
}

/// The writer thread: block for a job, take everything else already queued (up to `MAX_BATCH`
/// lines), append the lines under ONE `fsync`, then acknowledge any flush barriers in the batch.
fn write_loop(path: PathBuf, rx: Receiver<Job>) {
    let mut last_error_log: Option<Instant> = None;
    while let Ok(first) = rx.recv() {
        let mut batch = Batch::default();
        batch.take(first);
        while batch.lines < MAX_BATCH {
            match rx.try_recv() {
                Ok(job) => batch.take(job),
                Err(_) => break,
            }
        }
        let Batch {
            text: batch,
            acks,
            lines: _,
        } = batch;
        if !batch.is_empty() {
            if let Err(error) = append_durably(&path, batch.as_bytes()) {
                let quiet = last_error_log.is_some_and(|at| at.elapsed() < Duration::from_secs(60));
                if !quiet {
                    last_error_log = Some(Instant::now());
                    tracing::error!(path = %path.display(), %error, "audit: appending to the audit log failed");
                }
            }
        }
        for ack in acks {
            let _ = ack.send(());
        }
    }
}

/// Appends `bytes` to the log (created with its directory when absent) and `fsync`s it. The file
/// is opened per batch, so an operator moving or truncating `audit.jsonl` is picked up on the next
/// write.
fn append_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    f.sync_all()
}

/// Drains every audit writer: returns once everything recorded so far is on disk, or after
/// [`FLUSH_DEADLINE`] per log. The graceful-shutdown path calls it (`persist::flush_blocking`).
pub fn flush_blocking() {
    let live: Vec<Arc<Writer>> = writers().lock().iter().filter_map(Weak::upgrade).collect();
    for writer in live {
        writer.flush(FLUSH_DEADLINE);
    }
}

/// The last `want` complete lines of `path`, oldest first. Reads backwards in
/// [`TAIL_CHUNK_BYTES`] chunks and stops once it holds `want` lines, the start of the file or
/// [`TAIL_MAX_BYTES`], so boot costs the size of the tail, not of the history.
fn tail_lines(path: &Path, want: usize) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let floor = len.saturating_sub(TAIL_MAX_BYTES);
    let mut pos = len;
    let mut newlines = 0usize;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    while pos > floor && newlines <= want {
        let n = (pos - floor).min(TAIL_CHUNK_BYTES as u64) as usize;
        pos -= n as u64;
        let mut chunk = vec![0u8; n];
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut chunk).is_err() {
            break;
        }
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        chunks.push(chunk);
    }
    let bytes: Vec<u8> = chunks.into_iter().rev().flatten().collect();
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if pos > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(want);
    lines[skip..]
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

pub struct AuditLog {
    entries: Mutex<VecDeque<AuditEntry>>,
    seq: AtomicU64,
    path: PathBuf,
    /// `None` only when the writer thread could not start: [`Self::record`] then appends inline.
    writer: Option<Arc<Writer>>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        let mut entries = VecDeque::with_capacity(1024);
        let mut max_seq = 0u64;
        for line in tail_lines(&path, BOOT_TAIL_LINES) {
            if let Ok(e) = serde_json::from_str::<AuditEntry>(&line) {
                max_seq = max_seq.max(e.seq);
                entries.push_back(e);
            }
        }
        AuditLog {
            entries: Mutex::new(entries),
            seq: AtomicU64::new(max_seq + 1),
            writer: Writer::spawn(path.clone()),
            path,
        }
    }

    /// Record a mutation: queue it for the append-only history (durable once the writer's next
    /// group commit lands) + ring. Never waits on the disk. The ring lock covers sequence
    /// assignment AND queueing, so the file's line order is the sequence order.
    pub fn record(
        &self,
        tenant: &str,
        actor: &str,
        action: &str,
        entity: &str,
        entity_id: &str,
        detail: &str,
    ) {
        let mut q = self.entries.lock();
        let entry = AuditEntry {
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            ts_ms: now_ms(),
            tenant: if tenant.is_empty() {
                "personal".into()
            } else {
                tenant.into()
            },
            actor: actor.to_string(),
            action: action.to_string(),
            entity: entity.to_string(),
            entity_id: entity_id.to_string(),
            detail: detail.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            match &self.writer {
                Some(writer) => writer.enqueue(line),
                None => {
                    if let Err(error) = append_durably(&self.path, format!("{line}\n").as_bytes()) {
                        tracing::error!(path = %self.path.display(), %error, "audit: appending to the audit log failed");
                    }
                }
            }
        }
        crate::enterprise::siem_emit(&entry);
        if q.len() >= RING_CAP {
            q.pop_front();
        }
        q.push_back(entry);
    }

    /// Recent entries (newest first), optionally filtered to one tenant.
    pub fn recent(&self, limit: usize, tenant: Option<&str>) -> Vec<AuditEntry> {
        let q = self.entries.lock();
        q.iter()
            .rev()
            .filter(|e| tenant.map(|t| e.tenant == t).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Snapshot (oldest to newest) for the fleet follower sync. Audit records are written on the
    /// control-plane leader during every mutation (mutations forward there through
    /// `admin_ingress`), so the leader's buffer is the authoritative fleet-wide audit; a follower
    /// otherwise shows only its own local events. Bounded by the buffer's own capacity (recent
    /// history is all the operator audit view renders).
    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.entries.lock().iter().cloned().collect()
    }

    pub fn load(&self, data: Vec<AuditEntry>) {
        let mut q = self.entries.lock();
        q.clear();
        q.extend(data);
    }
}
