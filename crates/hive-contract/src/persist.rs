//! Durable storage for one contract's log: append-only segments, periodic signed checkpoints, and
//! the compaction those checkpoints make safe.
//!
//! Three properties, and they are the reason this module exists rather than a `serde_json::to_writer`
//! at shutdown:
//!
//! 1. **Restart-safe.** Every entry is written to disk before the caller is told it was committed,
//!    and `sync_data()` is called on it. A process restart therefore replays to the same index hash
//!    it had before -- proved by [`Store::open`] rebuilding from the last checkpoint plus the tail,
//!    not by asserting that serialization round-trips.
//! 2. **Bounded.** Entries are dropped from the front once a signed checkpoint has attested them, so
//!    retained entries stay under one checkpoint window instead of growing with history. What grows
//!    is the checkpoint count, and that is capped too.
//! 3. **Checkable by a stranger.** Each checkpoint is signed by the node's existing Ed25519 identity
//!    and commits to the index hash, the log head and a Merkle root over the window, so a third party
//!    handed a checkpoint plus a tail can verify the whole claim without holding the history and
//!    without trusting the writer.
//!
//! On disk, per contract directory:
//!
//! ```text
//! seg-000000000000.jsonl   entries [0, W)
//! seg-000000000256.jsonl   entries [256, 512)      W = checkpoint_every
//! cp-000000000256.json     signed checkpoint for [0, 256)
//! ```
//!
//! Names are zero-padded so lexicographic order is seq order -- no parsing of directory listings to
//! find "the latest". Checkpoints are written to a `.tmp` and renamed, so a crash mid-write leaves
//! the previous checkpoint intact rather than a half-written one.
//!
//! Compaction deletes WHOLE files, never rewrites one: segments are cut on the same boundaries
//! checkpoints are, so a segment is either entirely covered by retained checkpoints or entirely
//! droppable. Nothing has to read-modify-write a log file.

use crate::checkpoint::{Checkpoint, SignedCheckpoint, TrustedCheckpoint};
use crate::exec::{index_hash, Index};
use crate::merkle;
use crate::oplog::{LogEntry, OpLog};
use crate::signing::{verify_ed25519, CheckpointSigner};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Entries per checkpoint window. One window is the unit of commitment, of compaction and of an
/// inclusion proof, so this number is simultaneously: how much audit work a checkpoint saves, how
/// many entries are retained in memory, and how long an inclusion path is.
pub const DEFAULT_CHECKPOINT_EVERY: usize = 256;

/// How many most-recent checkpoints are kept. Older ones are deleted along with their segments: they
/// are superseded by the newer ones, which attest the same state plus more. Keeping a few is what
/// lets a verifier replay from an older point after a compaction.
pub const DEFAULT_RETAIN_CHECKPOINTS: usize = 4;

/// How a contract's log is persisted.
#[derive(Clone)]
pub struct PersistConfig {
    /// Entries per window. Clamped to at least 1.
    pub checkpoint_every: usize,
    /// Checkpoints retained on disk. Clamped to at least 1.
    pub retain_checkpoints: usize,
    /// The node's signing identity. `None` is refused at [`Store::open`], never downgraded to an
    /// unsigned checkpoint: an unsigned checkpoint asserts nothing, and an artefact shaped like a
    /// checkpoint but carrying no signature is how a forged history would be smuggled in.
    pub signer: Option<Arc<dyn CheckpointSigner>>,
}

impl Default for PersistConfig {
    fn default() -> PersistConfig {
        PersistConfig {
            checkpoint_every: DEFAULT_CHECKPOINT_EVERY,
            retain_checkpoints: DEFAULT_RETAIN_CHECKPOINTS,
            signer: None,
        }
    }
}

/// What appending one entry did.
#[derive(Clone, Debug, Default)]
pub struct AppendStep {
    /// A window closed and was attested.
    pub checkpoint: Option<SignedCheckpoint>,
    /// The in-memory log can be compacted to this seq. `None` when the window is still open.
    pub truncate_to: Option<u64>,
    /// The head the first retained entry will link to: the checkpoint's `log_head`. Compaction
    /// without it would leave the chain unverifiable, so it is handed over with the instruction.
    pub truncate_prev: Option<String>,
    pub reclaimed_segments: usize,
    pub reclaimed_checkpoints: usize,
}

/// Why persistence failed. Every variant is a refusal to continue silently.
#[derive(Clone, Debug)]
pub enum StoreError {
    /// No signing identity was configured.
    NoSigner,
    /// The signer refused or failed.
    SignFailed(String),
    /// A segment line is not a `LogEntry`, or a checkpoint file is not a `SignedCheckpoint`.
    Corrupt { file: String, detail: String },
    /// A stored checkpoint did not verify: forged, tampered with, or signed by a key that is not the
    /// node's. Refused at load, so a bad checkpoint can never become the basis of a replay.
    BadCheckpoint { file: String, detail: String },
    /// A stored entry's signature does not verify under the node's key, or the entry carries none:
    /// it was not written by this node, so it is not part of the history.
    BadEntry { file: String, seq: u64 },
    /// The rebuilt log does not chain.
    BrokenChain { detail: String },
    /// An I/O failure. Carried as text so this type stays `Clone` and `Send` across await points.
    Io { op: String, detail: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NoSigner => f.write_str(
                "no checkpoint signer configured: refusing to persist unsigned checkpoints",
            ),
            StoreError::SignFailed(d) => write!(f, "checkpoint signing failed: {d}"),
            StoreError::Corrupt { file, detail } => write!(f, "{file} is corrupt: {detail}"),
            StoreError::BadCheckpoint { file, detail } => {
                write!(f, "{file} does not verify: {detail}")
            }
            StoreError::BadEntry { file, seq } => write!(
                f,
                "{file}: entry at seq {seq} does not carry a signature this node made"
            ),
            StoreError::BrokenChain { detail } => write!(f, "rebuilt log does not chain: {detail}"),
            StoreError::Io { op, detail } => write!(f, "{op} failed: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {}

fn io(op: &str) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |e| StoreError::Io {
        op: op.to_string(),
        detail: e.to_string(),
    }
}

/// One line of a segment: the entry, plus the signature that binds it to the node's identity.
///
/// The chain alone does not stop an entry that was never submitted: `chain_hash` is computable by
/// anybody, so a hand-written line that links to the head and carries the hash its own op produces is
/// indistinguishable from a real one on replay -- it simply becomes part of the history the next
/// checkpoint attests. The signature is what makes "the log says this happened" a statement the node
/// can be held to, and it is checked on every read that will be adopted.
///
/// The signature is NOT inside [`LogEntry`], and it is not inside `entry_hash`: both are pure
/// functions of the operation and its predecessor, so a monitor without any key can still recompute
/// every hash and every Merkle root. What is signed is `entry_hash`, under
/// [`crate::oplog::ENTRY_SIG_DOMAIN`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SegmentLine {
    pub entry: LogEntry,
    /// Hex Ed25519 signature over `ENTRY_SIG_DOMAIN || entry.entry_hash`.
    pub sig: String,
}

/// The bytes a stored entry's signature covers.
pub fn entry_sig_preimage(entry_hash: &str) -> Vec<u8> {
    let mut out = crate::oplog::ENTRY_SIG_DOMAIN.to_vec();
    out.extend_from_slice(entry_hash.as_bytes());
    out
}

/// Segment file for the window starting at `base`. Zero-padded: sorted file order is seq order.
pub fn segment_name(base: u64) -> String {
    format!("seg-{base:012}.jsonl")
}

/// Checkpoint file for the window ending at `end`.
pub fn checkpoint_name(end: u64) -> String {
    format!("cp-{end:012}.json")
}

fn parse_checkpoint_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("cp-")?.strip_suffix(".json")?;
    rest.parse::<u64>().ok()
}

fn parse_segment_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(".jsonl")?;
    rest.parse::<u64>().ok()
}

/// The most recent signed checkpoint in `dir`, verified before it is handed out.
///
/// `trust` is what turns "this artefact is internally consistent" into "this artefact is MY
/// history". Pass `Some(..)` on every path that will adopt the checkpoint -- a restart rebuilding an
/// index from disk -- and `None` only when the caller is a stranger that has not been told whose key
/// to expect and will decide that itself.
pub async fn read_latest_checkpoint(
    dir: &Path,
    trust: Option<&TrustedCheckpoint<'_>>,
) -> Result<Option<SignedCheckpoint>, StoreError> {
    let ends = list_checkpoint_ends(dir).await?;
    let end = match ends.into_iter().max() {
        Some(e) => e,
        None => return Ok(None),
    };
    read_checkpoint(dir, end, trust).await.map(Some)
}

/// Read and verify the checkpoint for the window ending at `end`.
///
/// Beyond the artefact's own checks, two bindings come from WHERE it was found rather than from what
/// it says:
///   * the file that carries it names the window, so a checkpoint stored under another window's name
///     (a rollback by rename) is refused -- the name is not decoration, it is the index the reader
///     used to find it;
///   * `trust`, when given, pins the signing identity and the contract.
pub async fn read_checkpoint(
    dir: &Path,
    end: u64,
    trust: Option<&TrustedCheckpoint<'_>>,
) -> Result<SignedCheckpoint, StoreError> {
    let path = dir.join(checkpoint_name(end));
    let name = path.display().to_string();
    let bytes = tokio::fs::read(&path).await.map_err(io("read checkpoint"))?;
    let cp: SignedCheckpoint = serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt {
        file: name.clone(),
        detail: e.to_string(),
    })?;
    if cp.checkpoint.window_end != end {
        return Err(StoreError::BadCheckpoint {
            file: name,
            detail: format!(
                "file is named for window {end}, artefact attests window {}",
                cp.checkpoint.window_end
            ),
        });
    }
    cp.verify(trust)
        .map_err(|e| StoreError::BadCheckpoint {
            file: name,
            detail: e.to_string(),
        })
        .map(|()| cp)
}

async fn list_checkpoint_ends(dir: &Path) -> Result<Vec<u64>, StoreError> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await.map_err(io("read_dir"))?;
    while let Some(e) = rd.next_entry().await.map_err(io("read_dir"))? {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(v) = parse_checkpoint_name(&name) {
            out.push(v);
        }
    }
    out.sort_unstable();
    Ok(out)
}

async fn list_segment_bases(dir: &Path) -> Result<Vec<u64>, StoreError> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await.map_err(io("read_dir"))?;
    while let Some(e) = rd.next_entry().await.map_err(io("read_dir"))? {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(v) = parse_segment_name(&name) {
            out.push(v);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Every entry at or after `from`, in seq order, read from the segment files.
///
/// Used to rebuild after a restart and to materialise a window an inclusion proof needs after the
/// in-memory log has moved on.
pub async fn read_entries_from(
    dir: &Path,
    from: u64,
    public_key: Option<&[u8]>,
) -> Result<Vec<LogEntry>, StoreError> {
    let bases = list_segment_bases(dir).await?;
    // Every segment is read and filtered by seq: a segment's window size is not knowable from its
    // name, so the only safe filter is the one applied per line below.
    let mut out = Vec::new();
    for b in bases {
        let path = dir.join(segment_name(b));
        let name = path.display().to_string();
        let text = tokio::fs::read_to_string(&path).await.map_err(io("read segment"))?;
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let l: SegmentLine =
                serde_json::from_str(line).map_err(|err| StoreError::Corrupt {
                    file: name.clone(),
                    detail: format!("line {}: {err}", i + 1),
                })?;
            verify_line(&l, &name, public_key)?;
            if l.entry.op.seq >= from {
                out.push(l.entry);
            }
        }
    }
    out.sort_by_key(|e| e.op.seq);
    Ok(out)
}

/// Check one stored line's signature against the node's Ed25519 public key.
///
/// `None` skips the check, for a caller that has not been told whose key to expect -- a stranger
/// reading a log it was handed verifies the entries only as content, because it cannot tell a real
/// signature from one made by a key it has never heard of. Every path that ADOPTS entries passes the
/// key.
fn verify_line(l: &SegmentLine, file: &str, public_key: Option<&[u8]>) -> Result<(), StoreError> {
    let Some(pk) = public_key else {
        return Ok(());
    };
    let sig = match hex::decode(&l.sig) {
        Ok(s) if !s.is_empty() => s,
        // An entry with no signature is not "an old entry", it is an entry this node did not write.
        // Nothing in this format ever wrote one, so there is no legacy case to accommodate.
        _ => {
            return Err(StoreError::BadEntry {
                file: file.to_string(),
                seq: l.entry.op.seq,
            })
        }
    };
    if !verify_ed25519(pk, &entry_sig_preimage(&l.entry.entry_hash), &sig) {
        return Err(StoreError::BadEntry {
            file: file.to_string(),
            seq: l.entry.op.seq,
        });
    }
    Ok(())
}

/// The entries of one window `[base, end)`: what an inclusion proof is cut against after compaction
/// has dropped them from memory. Filtered by seq rather than by file, because a window that was
/// closed early shares a segment file with the one that follows it.
pub async fn read_window(
    dir: &Path,
    base: u64,
    end: u64,
    window: usize,
    public_key: Option<&[u8]>,
) -> Result<Vec<LogEntry>, StoreError> {
    let window = window.max(1) as u64;
    let path = dir.join(segment_name((base / window) * window));
    let name = path.display().to_string();
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(io("read segment"))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let l: SegmentLine = serde_json::from_str(line).map_err(|err| StoreError::Corrupt {
            file: name.clone(),
            detail: format!("line {}: {err}", i + 1),
        })?;
        verify_line(&l, &name, public_key)?;
        if l.entry.op.seq >= base && l.entry.op.seq < end {
            out.push(l.entry);
        }
    }
    out.sort_by_key(|e| e.op.seq);
    Ok(out)
}

/// Write `bytes` to `path` so a crash cannot leave a partial file where the real one was: write a
/// sibling `.tmp`, sync it, then rename over the target and sync the directory.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(io("create temp checkpoint"))?;
        f.write_all(bytes).await.map_err(io("write temp checkpoint"))?;
        f.sync_data().await.map_err(io("sync temp checkpoint"))?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(io("rename checkpoint"))?;
    // The rename is only durable once the directory entry is: without this, a crash can leave the
    // old file and no new one even though the write itself completed.
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| StoreError::Io {
            op: "sync dir".to_string(),
            detail: format!("{}: {e}", dir.display()),
        })
}

/// What [`Store::open`] returns: the writer plus the state it rebuilt from disk.
///
/// `index` is the checkpoint's index and `log` is the tail AFTER it -- the two do NOT meet yet. The
/// caller closes the gap by replaying ([`crate::audit::replay_from`]), which is also what verifies
/// the tail. Handing back an unchecked pair would let a restart publish an index the entries do not
/// support.
pub struct Opened {
    pub store: Store,
    pub log: OpLog,
    pub index: Index,
    /// The checkpoint `index` came from, when there was one. `None` on a fresh directory.
    pub checkpoint: Option<SignedCheckpoint>,
}

/// The append-only writer for one contract's log.
pub struct Store {
    dir: PathBuf,
    contract_id: String,
    contract_version: u32,
    window: usize,
    retain: usize,
    signer: Option<Arc<dyn CheckpointSigner>>,
    /// The segment currently open for append, and the window index it belongs to.
    seg: Option<tokio::fs::File>,
    seg_base: Option<u64>,
    /// Entry hashes of the window being filled, in seq order: what the Merkle root is taken over.
    pending: Vec<String>,
    /// Seq of the first pending entry. A window starts wherever the previous one ended -- which is
    /// not always a multiple of `checkpoint_every`, because a window can be closed early.
    pending_start: Option<u64>,
    /// `prev_hash` of the first pending entry: the `prev_head` a checkpoint over this window carries.
    pending_prev: String,
}

impl Store {
    /// Open (and create if absent) a contract's log directory, and rebuild the state it holds.
    ///
    /// The rebuild starts from the newest VERIFIED checkpoint and applies the tail, so a restart
    /// costs O(tail) rather than O(history) -- and so a tampered checkpoint is refused here, at load,
    /// instead of being discovered later as a wrong index.
    pub async fn open(
        dir: PathBuf,
        contract_id: &str,
        contract_version: u32,
        cfg: PersistConfig,
    ) -> Result<Opened, StoreError> {
        if cfg.signer.is_none() {
            return Err(StoreError::NoSigner);
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(io("create log dir"))?;
        let window = cfg.checkpoint_every.max(1);
        let retain = cfg.retain_checkpoints.max(1);

        // The identity this log is allowed to have been written by. Taken from the CONFIGURED
        // signer, never from the artefact: a checkpoint that names its own key and verifies against
        // it proves only that somebody signed something. Rotating the signing key therefore makes
        // every stored checkpoint untrusted, and this refuses rather than re-attesting history under
        // a new key -- the honest failure for a log whose whole job is non-repudiation.
        let signer = cfg.signer.expect("checked above");
        let pinned_key = signer.public_key();
        let trust = TrustedCheckpoint {
            public_key_hex: &hex::encode(signer.public_key()),
            contract_id,
            contract_version,
        };

        let latest = read_latest_checkpoint(&dir, Some(&trust)).await?;
        // A checkpoint is also a commitment to the ENTRIES of its window, and the tail after it is
        // never replayed over them -- so the only thing standing between a rewrite inside an attested
        // window and an adopted forgery is the signed Merkle root. Recompute it whenever the window's
        // entries are still on disk (they are, for every window whose segment has not been reclaimed);
        // when they are gone there is nothing to compare, and the signature is all that is left.
        if let Some(cp) = &latest {
            let c = &cp.checkpoint;
            let entries = read_window(&dir, c.window_base, c.window_end, window, Some(&pinned_key))
                .await
                .unwrap_or_default();
            if !entries.is_empty() {
                let computed = merkle::merkle_root(
                    &entries.iter().map(|e| e.entry_hash.clone()).collect::<Vec<_>>(),
                );
                if computed != c.merkle_root {
                    return Err(StoreError::BadCheckpoint {
                        file: checkpoint_name(c.window_end),
                        detail: format!(
                            "window [{}, {}) on disk commits to {computed}, checkpoint names {}",
                            c.window_base, c.window_end, c.merkle_root
                        ),
                    });
                }
            }
        }
        let (index, base, base_prev) = match &latest {
            Some(cp) => {
                let c = &cp.checkpoint;
                (c.index.clone(), c.window_end, c.log_head.clone())
            }
            None => (Index::new(), 0u64, String::new()),
        };
        let entries = read_entries_from(&dir, base, Some(&pinned_key)).await?;
        let log = OpLog::restore(base, &base_prev, entries).map_err(|e| StoreError::BrokenChain {
            detail: format!("at seq {}: stored {} expected {}", e.seq, e.stored, e.expected),
        })?;

        let pending = log
            .entries()
            .iter()
            .map(|e| e.entry_hash.clone())
            .collect::<Vec<_>>();
        let store = Store {
            dir,
            contract_id: contract_id.to_string(),
            contract_version,
            window,
            retain,
            signer: Some(signer),
            seg: None,
            seg_base: None,
            pending_start: if log.is_empty() { None } else { Some(log.base()) },
            pending,
            pending_prev: log.base_prev().to_string(),
        };
        Ok(Opened {
            store,
            log,
            index,
            checkpoint: latest,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn window(&self) -> usize {
        self.window
    }

    /// The entry hashes of the window currently being filled. Empty right after a checkpoint.
    pub fn pending(&self) -> &[String] {
        &self.pending
    }

    /// The seq the window being filled starts at. `0` when nothing is pending.
    pub fn pending_base(&self) -> u64 {
        self.pending_start.unwrap_or(0)
    }

    /// The seq at which the window being filled will close: the next multiple of
    /// [`Store::window`] at or after the window's first entry.
    pub fn pending_boundary(&self) -> u64 {
        let window = self.window.max(1) as u64;
        ((self.pending_base() / window) + 1) * window
    }

    /// Append one entry, and close the window if this entry completes it.
    ///
    /// The write is flushed and synced before this returns, so "committed" means "on disk". One
    /// `write_all` of one JSON line is the whole cost per op; the heavier work -- taking a Merkle
    /// root, signing, writing a checkpoint, deleting superseded files -- happens once per window.
    ///
    /// `index` is the index as of THIS entry, which is the value a closing checkpoint attests; it is
    /// only read when the window closes.
    pub async fn append(
        &mut self,
        entry: &LogEntry,
        index: &Index,
        ts_ms: u64,
    ) -> Result<AppendStep, StoreError> {
        let window = self.window as u64;
        let seq = entry.op.seq;
        let wbase = (seq / window) * window;

        if self.pending.is_empty() {
            // Starting a window wherever the last one ended, which after an early close or a restart
            // is NOT a window boundary. The checkpoint therefore describes the range it actually
            // covers ([pending_start, end)) rather than a range it is assumed to cover.
            self.pending_start = Some(seq);
            self.pending_prev = entry.prev_hash.clone();
        }
        if self.seg_base != Some(wbase) {
            let path = self.dir.join(segment_name(wbase));
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(io("open segment"))?;
            self.seg = Some(f);
            self.seg_base = Some(wbase);
        }
        // Sign the entry as it is stored. The signature is over `entry_hash`, which is already a
        // function of `(prev, op)`, so it binds the operation, its position and its predecessor in
        // one go -- and it is the only thing on the line that a writer without the node's key cannot
        // produce.
        let signer = self.signer.as_ref().ok_or(StoreError::NoSigner)?;
        let sig = hex::encode(
            signer
                .sign(&entry_sig_preimage(&entry.entry_hash))
                .map_err(StoreError::SignFailed)?,
        );
        let mut line = serde_json::to_string(&SegmentLine {
            entry: entry.clone(),
            sig,
        })
        .map_err(|e| StoreError::Corrupt {
            file: segment_name(wbase),
            detail: format!("entry {seq} does not serialize: {e}"),
        })?;
        line.push('\n');
        let f = self.seg.as_mut().expect("segment opened above");
        f.write_all(line.as_bytes())
            .await
            .map_err(io("append entry"))?;
        f.sync_data().await.map_err(io("sync entry"))?;

        self.pending.push(entry.entry_hash.clone());
        let mut step = AppendStep::default();
        if seq + 1 < self.pending_boundary() {
            return Ok(step);
        }
        self.close_window(entry, index, ts_ms, &mut step).await?;
        Ok(step)
    }

    /// Attest the window that just closed, then reclaim what it supersedes.
    async fn close_window(
        &mut self,
        last: &LogEntry,
        index: &Index,
        ts_ms: u64,
        step: &mut AppendStep,
    ) -> Result<(), StoreError> {
        let window = self.window.max(1) as u64;
        let wbase = self.pending_base();
        // The range actually covered, not an assumed one: a window closed early ends at `last.seq+1`.
        let end = last.op.seq + 1;
        let merkle_root = merkle::merkle_root(&self.pending);
        let cp = Checkpoint {
            contract_id: self.contract_id.clone(),
            contract_version: self.contract_version,
            window_base: wbase,
            window_end: end,
            prev_head: self.pending_prev.clone(),
            log_head: last.entry_hash.clone(),
            merkle_root,
            index_hash: index_hash(index),
            index: index.clone(),
            ts_ms,
        };
        let signer = self.signer.as_ref().ok_or(StoreError::NoSigner)?;
        let signed = SignedCheckpoint::sign(cp, signer.as_ref(), ts_ms)
            .map_err(|e| StoreError::SignFailed(e.to_string()))?;
        let bytes = serde_json::to_vec_pretty(&signed).map_err(|e| StoreError::Corrupt {
            file: checkpoint_name(end),
            detail: e.to_string(),
        })?;
        write_atomic(&self.dir.join(checkpoint_name(end)), &bytes).await?;

        // Reclaim: keep the newest `retain` checkpoints and every segment they still need. Segments
        // are cut on window boundaries, so a segment is either fully covered or fully droppable --
        // nothing here rewrites a log file.
        let ends = list_checkpoint_ends(&self.dir).await?;
        if ends.len() > self.retain {
            let drop = &ends[..ends.len() - self.retain];
            for e in drop {
                let p = self.dir.join(checkpoint_name(*e));
                let _ = tokio::fs::remove_file(&p).await;
                step.reclaimed_checkpoints += 1;
            }
        }
        let kept = &ends[ends.len().saturating_sub(self.retain)..];
        // The oldest retained checkpoint's own window is the oldest one still needed: an inclusion
        // proof can be cut for it, and a replay can resume from it.
        let keep_from = kept.first().copied().unwrap_or(wbase);
        for b in list_segment_bases(&self.dir).await? {
            if b + window <= keep_from {
                let p = self.dir.join(segment_name(b));
                let _ = tokio::fs::remove_file(&p).await;
                step.reclaimed_segments += 1;
            }
        }
        // Also drop the open segment handle if its file was reclaimed, so the next append reopens the
        // right one instead of appending to an unlinked inode.
        if let Some(sb) = self.seg_base {
            if sb + window <= keep_from {
                self.seg = None;
                self.seg_base = None;
            }
        }

        step.checkpoint = Some(signed);
        step.truncate_to = Some(end);
        step.truncate_prev = Some(last.entry_hash.clone());
        self.pending.clear();
        self.pending_start = None;
        Ok(())
    }

    /// Force a checkpoint over whatever is pending, even though the window is not full: the shutdown
    /// and pre-roll path, where the tail matters more than the window being tidy.
    pub async fn checkpoint_now(
        &mut self,
        last: &LogEntry,
        index: &Index,
        ts_ms: u64,
    ) -> Result<Option<SignedCheckpoint>, StoreError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let mut step = AppendStep::default();
        self.close_window(last, index, ts_ms, &mut step).await?;
        Ok(step.checkpoint)
    }
}
