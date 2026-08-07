//! Bounded capture of subprocess output.
//!
//! Every build/exec log line on this platform originates in a TENANT-CONTROLLED
//! child process, and until this module existed every one of those pipes was
//! drained with `BufReader::lines()` / `next_line()`:
//!
//! * `hive-cloud/src/git.rs`   — the deploy build (`npm install && npm run build`)
//! * `hive-backend/src/mock.rs`— the mock cell backend's build steps
//! * `hive-cell-agent`         — in-guest exec/build output
//!
//! `lines()` splits ONLY on `\n` and has NO length limit, so a child that
//! writes without ever emitting a newline grows ONE `String` until the process
//! dies. That is not a corner case: `\r`-only progress rendering (npm, pnpm,
//! pip, curl, `podman pull`, most bundler progress plugins), a single-line
//! source map, `tsc --listFiles` piped through a formatter, or a plain
//! `while true; do printf x; done` all produce it. On a multi-tenant node the
//! whole process — every other tenant's traffic with it — dies of one tenant's
//! stdout. The three fc-hongkong kills at 98,214,816 / 98,215,476 /
//! 98,189,292 kB anon-rss are within 0.03% of each other: that is a process
//! consuming everything the host has, not a leak converging on a number.
//!
//! [`read_capped_line`] (async) and [`read_capped_line_blocking`] (sync) keep
//! DRAINING the pipe — a child must never block on a full pipe because of us —
//! while RETAINING at most [`MAX_LOG_LINE_BYTES`]. Peak retained bytes per
//! reader is therefore the cap plus the `BufReader`'s own fixed buffer,
//! regardless of what the child writes.
//!
//! The bound is observable, never silent: every truncation increments
//! [`LOG_CAP_STATS`], the returned [`CappedLine`] carries `dropped_bytes`, and
//! the emitted text ends in [`TRUNCATION_SUFFIX`] so the log READER can see it
//! too.

use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum bytes RETAINED from a single log line. Anything beyond this is
/// drained and counted, never buffered.
///
/// 16 KiB is far above any legitimate log line (a 200-column terminal row is
/// ~200 bytes; the longest real lines here are stack traces and webpack module
/// paths, low kilobytes) and small enough that the worst case across every
/// concurrent build on a node is negligible.
pub const MAX_LOG_LINE_BYTES: usize = 16 * 1024;

/// Appended to a line that hit the cap, so truncation is visible in the log
/// itself and not only in a counter nobody reads.
pub const TRUNCATION_SUFFIX: &str = "…[hive: line truncated, ";

/// Process-global counters for the line cap. Exposed by hive-cloud's
/// `GET /v1/debug/memory` so "is a tenant hosing us through stdout?" is a
/// question the fleet can answer from the outside, on a node that is still
/// alive, instead of being reconstructed from an OOM kill line.
pub static LOG_CAP_STATS: LogCapStats = LogCapStats::new();

#[derive(Debug)]
pub struct LogCapStats {
    lines_truncated: AtomicU64,
    bytes_dropped: AtomicU64,
    lines_total: AtomicU64,
}

impl LogCapStats {
    const fn new() -> LogCapStats {
        LogCapStats {
            lines_truncated: AtomicU64::new(0),
            bytes_dropped: AtomicU64::new(0),
            lines_total: AtomicU64::new(0),
        }
    }

    fn record(&self, dropped: u64) {
        self.lines_total.fetch_add(1, Ordering::Relaxed);
        if dropped > 0 {
            self.lines_truncated.fetch_add(1, Ordering::Relaxed);
            self.bytes_dropped.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// `(lines_total, lines_truncated, bytes_dropped)`.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.lines_total.load(Ordering::Relaxed),
            self.lines_truncated.load(Ordering::Relaxed),
            self.bytes_dropped.load(Ordering::Relaxed),
        )
    }
}

/// One captured line plus what the cap cost.
#[derive(Debug, Clone)]
pub struct CappedLine {
    /// At most `MAX_LOG_LINE_BYTES` (+ the truncation suffix) of text.
    pub text: String,
    /// Bytes read off the pipe and deliberately discarded.
    pub dropped_bytes: u64,
}

impl CappedLine {
    pub fn truncated(&self) -> bool {
        self.dropped_bytes > 0
    }
}

/// Shared tail of both readers: turn the retained bytes into a `String`,
/// annotate truncation, and account for it.
fn finish(mut buf: Vec<u8>, dropped: u64) -> CappedLine {
    // `lines()` semantics: a `\r\n` terminator leaves no `\r` in the line.
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if dropped > 0 {
        text.push_str(TRUNCATION_SUFFIX);
        text.push_str(&dropped.to_string());
        text.push_str(" bytes dropped]");
    }
    LOG_CAP_STATS.record(dropped);
    CappedLine {
        text,
        dropped_bytes: dropped,
    }
}

/// Split `available` at the first `\n`: returns the segment to retain-or-drop,
/// how many bytes to consume from the reader, and whether the line ended here.
fn split_at_newline(available: &[u8]) -> (&[u8], usize, bool) {
    match available.iter().position(|&b| b == b'\n') {
        Some(i) => (&available[..i], i + 1, true),
        None => (available, available.len(), false),
    }
}

/// Blocking variant, for the in-guest agent (`hive-cell-agent` runs its readers
/// on plain OS threads with `std::io`, and deliberately depends on no runtime).
///
/// Returns `Ok(None)` at EOF. Never retains more than `cap` bytes.
pub fn read_capped_line_blocking<R: std::io::BufRead>(
    r: &mut R,
    cap: usize,
) -> std::io::Result<Option<CappedLine>> {
    let mut out: Vec<u8> = Vec::new();
    let mut dropped: u64 = 0;
    let mut saw_input = false;
    loop {
        let (consume_n, line_done) = {
            let available = r.fill_buf()?;
            if available.is_empty() {
                break; // EOF
            }
            saw_input = true;
            let (seg, consume_n, line_done) = split_at_newline(available);
            let room = cap.saturating_sub(out.len());
            let take = room.min(seg.len());
            out.extend_from_slice(&seg[..take]);
            dropped += (seg.len() - take) as u64;
            (consume_n, line_done)
        };
        r.consume(consume_n);
        if line_done {
            return Ok(Some(finish(out, dropped)));
        }
    }
    if !saw_input {
        return Ok(None);
    }
    Ok(Some(finish(out, dropped)))
}

/// Async variant over any [`tokio::io::AsyncBufRead`] — the build-output path
/// in `hive-cloud` and `hive-backend`.
///
/// Returns `Ok(None)` at EOF. Never retains more than `cap` bytes; the pipe is
/// drained either way so the child never blocks on a full pipe.
#[cfg(feature = "async-io")]
pub async fn read_capped_line<R>(r: &mut R, cap: usize) -> std::io::Result<Option<CappedLine>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut out: Vec<u8> = Vec::new();
    let mut dropped: u64 = 0;
    let mut saw_input = false;
    loop {
        let (consume_n, line_done) = {
            let available = r.fill_buf().await?;
            if available.is_empty() {
                break; // EOF
            }
            saw_input = true;
            let (seg, consume_n, line_done) = split_at_newline(available);
            let room = cap.saturating_sub(out.len());
            let take = room.min(seg.len());
            out.extend_from_slice(&seg[..take]);
            dropped += (seg.len() - take) as u64;
            (consume_n, line_done)
        };
        r.consume(consume_n);
        if line_done {
            return Ok(Some(finish(out, dropped)));
        }
    }
    if !saw_input {
        return Ok(None);
    }
    Ok(Some(finish(out, dropped)))
}
