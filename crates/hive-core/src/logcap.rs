//! Bounded capture of subprocess output. Every build/exec log line comes from a
//! TENANT-CONTROLLED child, and `BufReader::lines()` has no length limit, so a child that never
//! emits a newline (`\r` progress bars, one-line source maps) grows one `String` until the process
//! dies. [`read_capped_line`] / [`read_capped_line_blocking`] keep DRAINING the pipe while
//! RETAINING at most [`MAX_LOG_LINE_BYTES`]; every truncation is counted in [`LOG_CAP_STATS`] and
//! marked with [`TRUNCATION_SUFFIX`].

use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum bytes RETAINED from a single log line; anything beyond is drained and counted, never
/// buffered (16 KiB is far above any legitimate line).
pub const MAX_LOG_LINE_BYTES: usize = 16 * 1024;

/// Appended to a line that hit the cap, so truncation is visible in the log
/// itself and not only in a counter nobody reads.
pub const TRUNCATION_SUFFIX: &str = "…[hive: line truncated, ";

/// Process-global counters for the line cap, exposed by hive-cloud's `GET /v1/debug/memory` so a
/// tenant hosing stdout is visible from outside while the node is alive.
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

/// Blocking variant for the in-guest agent (plain OS threads, no runtime). Returns `Ok(None)` at
/// EOF; never retains more than `cap` bytes.
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

/// Async variant over any [`tokio::io::AsyncBufRead`]. Returns `Ok(None)` at EOF; never retains
/// more than `cap` bytes and always drains the pipe.
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
