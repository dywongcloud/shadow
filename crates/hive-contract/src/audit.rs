//! Monitor-side audit: replay the log and compare against the published index.
//!
//! This is Vitra's contract audit (whitepaper 2.6): a monitor syncs the oplog and index, replays the
//! ops through `apply`, and compares its generated state against the executor's. A divergence is a
//! breach and is reproducible, so the monitor can name the seq and show the inclusion proofs.
//!
//! Two things are checked, and they are different claims:
//!   * [`verify_chain`] -- the log is append-only and un-forked (the `LogForkFraudProof` case).
//!   * [`replay`] -- the executor applied the contract honestly (the contract-breach case).

use crate::checkpoint::{Checkpoint, CheckpointVerifyError, SignedCheckpoint};
use crate::contract::Contract;
use crate::exec::{apply, index_hash, Index};
use crate::oplog::{ChainBreak, LogEntry, OpLog, TruncateError};

/// A detected divergence. The monitor publishes this instead of trusting the executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Breach {
    /// The index the monitor recomputed at `seq` is not the index the executor published at `seq`.
    DivergentIndex {
        seq: u64,
        published: String,
        recomputed: String,
    },
    /// The executor acked an op at a seq the log does not hold at that position.
    SeqMismatch { position: usize, ack_seq: u64 },
    /// The log's own append-only chain is broken.
    BrokenChain(ChainBreak),
    /// The executor acked a SUCCESSFUL op whose kind the contract does not declare. A refused op is
    /// recorded with its original kind and is not a breach (its error field says why it was refused).
    UndeclaredOp { seq: u64, kind: String },
    /// A checkpoint's carried index does not hash to the `index_hash` it names, so nothing it
    /// attests is usable.
    BadCheckpoint {
        window_end: u64,
        published: String,
        computed: String,
    },
    /// The checkpoint's `log_head` is not the head this log's own chain reaches at `window_end`: the
    /// checkpoint is for a different history than the entries handed out with it.
    CheckpointHeadMismatch {
        window_end: u64,
        published: String,
        computed: String,
    },
    /// Replay was asked to resume at a checkpoint the log has already compacted past, so the entries
    /// between the two are gone and the resume cannot be checked.
    CheckpointGap { window_end: u64, log_base: u64 },
    /// A compaction was attempted without a head attesting the cut.
    BadCompaction(TruncateError),
}

impl std::fmt::Display for Breach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Breach::DivergentIndex {
                seq,
                published,
                recomputed,
            } => write!(
                f,
                "breach at seq {seq}: executor published {published}, replay computed {recomputed}"
            ),
            Breach::SeqMismatch { position, ack_seq } => write!(
                f,
                "breach at log position {position}: entry carries ack seq {ack_seq}"
            ),
            Breach::BrokenChain(c) => write!(f, "log chain broken at seq {}: {}", c.seq, c.stored),
            Breach::UndeclaredOp { seq, kind } => {
                write!(f, "breach at seq {seq}: executor applied undeclared op kind {kind}")
            }
            Breach::BadCheckpoint {
                window_end,
                published,
                computed,
            } => write!(
                f,
                "checkpoint at {window_end} names index {published}, carries one hashing to {computed}"
            ),
            Breach::CheckpointHeadMismatch {
                window_end,
                published,
                computed,
            } => write!(
                f,
                "checkpoint at {window_end} names log head {published}, log chain reaches {computed}"
            ),
            Breach::CheckpointGap {
                window_end,
                log_base,
            } => write!(
                f,
                "cannot resume at checkpoint {window_end}: log window starts at {log_base}"
            ),
            Breach::BadCompaction(e) => write!(f, "compaction refused: {e}"),
        }
    }
}

/// Replay `log` from genesis against `contract` and return the rebuilt index.
///
/// "Genesis" means the start of the RETAINED window: after a compaction the entries before
/// [`OpLog::base`] are gone, and this replays what is here. It is therefore O(retained), not
/// O(history) -- and it is exactly what [`replay_from`] with `None` does. Use [`replay_from`] with a
/// checkpoint when the entries before the window matter to the claim being checked.
///
/// Deterministic by construction: order is the log's seq order (not arrival order, not a HashMap
/// order), and `apply` is pure. Two honest nodes replaying this log -- different machines, different
/// thread counts -- produce the same [`index_hash`].
pub fn replay(contract: &Contract, log: &OpLog) -> Result<Index, Breach> {
    replay_from(contract, log, None)
}

/// Replay `log` against `contract`, resuming from `cp` when one is given.
///
/// This is the property that makes compaction admissible: replaying from a signed checkpoint and
/// applying the tail MUST land on the same index as replaying the whole history, or a checkpoint
/// would be a second, quietly different truth. Equivalence is not assumed -- it is checked, because
/// the checkpoint's own `index` is verified against its `index_hash` and its `log_head` is verified
/// against the head this log's chain actually reaches at `window_end`. A checkpoint that disagrees
/// with the entries beside it is a [`Breach`], not a starting point.
///
/// The whole retained chain is verified from [`OpLog::base`] on every call, not just the tail after
/// `window_end`: entries inside the checkpoint's window could otherwise be edited without detection,
/// since the ack they carry is the one thing not covered by an entry hash.
pub fn replay_from(
    contract: &Contract,
    log: &OpLog,
    cp: Option<&Checkpoint>,
) -> Result<Index, Breach> {
    let (mut index, start_seq) = match cp {
        None => (Index::new(), log.base()),
        Some(c) => {
            let computed = index_hash(&c.index);
            if computed != c.index_hash {
                return Err(Breach::BadCheckpoint {
                    window_end: c.window_end,
                    published: c.index_hash.clone(),
                    computed,
                });
            }
            if c.window_end < log.base() {
                return Err(Breach::CheckpointGap {
                    window_end: c.window_end,
                    log_base: log.base(),
                });
            }
            // The checkpoint's head must be where this log's own chain is at `window_end`, or the
            // checkpoint is being offered as the start of a history it did not precede.
            let reached = if c.window_end == log.base() {
                Some(log.base_prev().to_string())
            } else {
                log.entry_at(c.window_end - 1)
                    .map(|e| e.entry_hash.clone())
            };
            if reached.as_deref() != Some(c.log_head.as_str()) {
                return Err(Breach::CheckpointHeadMismatch {
                    window_end: c.window_end,
                    published: c.log_head.clone(),
                    computed: reached.unwrap_or_else(|| "<seq compacted away>".to_string()),
                });
            }
            (c.index.clone(), c.window_end)
        }
    };
    log.verify_chain().map_err(Breach::BrokenChain)?;

    let base = log.base();
    for (pos, e) in log.entries().iter().enumerate().skip(
        (start_seq.saturating_sub(base)) as usize,
    ) {
        if e.ack.seq != base + pos as u64 {
            return Err(Breach::SeqMismatch {
                position: pos,
                ack_seq: e.ack.seq,
            });
        }
        if e.ack.success && contract.spec(&e.op.kind).is_none() {
            return Err(Breach::UndeclaredOp {
                seq: e.ack.seq,
                kind: e.op.kind.clone(),
            });
        }
        if !e.ack.success {
            // A refused op changed nothing; the published index hash must be the hash we hold.
            if e.ack.index_hash != index_hash(&index) {
                return Err(Breach::DivergentIndex {
                    seq: e.ack.seq,
                    published: e.ack.index_hash.clone(),
                    recomputed: index_hash(&index),
                });
            }
            continue;
        }
        for m in apply(&index, &e.op) {
            match m {
                crate::exec::Mutation::Put { key, value } => {
                    index.insert(key, value);
                }
                crate::exec::Mutation::Delete { key } => {
                    index.remove(&key);
                }
            }
        }
        let recomputed = index_hash(&index);
        if recomputed != e.ack.index_hash {
            return Err(Breach::DivergentIndex {
                seq: e.ack.seq,
                published: e.ack.index_hash.clone(),
                recomputed,
            });
        }
    }
    Ok(index)
}

/// Full verification, Vitra's "full audit": replay from genesis and confirm the final index hash
/// equals the one the executor last published.
pub fn verify(
    contract: &Contract,
    log: &OpLog,
    published_head_index_hash: &str,
) -> Result<String, Breach> {
    let index = replay(contract, log)?;
    let h = index_hash(&index);
    if h != published_head_index_hash {
        return Err(Breach::DivergentIndex {
            seq: log.next_seq(),
            published: published_head_index_hash.to_string(),
            recomputed: h,
        });
    }
    Ok(h)
}

// ---- disputes -----------------------------------------------------------------------------------
// Detection is worthless if the accuser has to be believed. A dispute is the compact artefact that
// lets a THIRD party settle the accusation from what it was handed, with no trust in either side:
// a signed checkpoint (the state everyone agrees on), the entries from there to the disputed seq
// (each bound by its own hash and by the chain), and the disputed ack (inside the last entry).

/// One monitor's accusation, in the smallest form that can be settled by someone who was not there.
#[derive(Clone, Debug)]
pub struct Dispute {
    /// The last signed checkpoint at or before the disputed seq: the agreed starting state.
    pub checkpoint: SignedCheckpoint,
    /// The entries from `checkpoint.window_end` through `disputed_seq`, inclusive. Bounded by the
    /// checkpoint window plus one window of tail, so an accusation is never "here is my whole log".
    pub segment: Vec<LogEntry>,
    /// The seq whose ack is being disputed.
    pub disputed_seq: u64,
}

/// The verdict a third party reaches from a [`Dispute`] alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Replay reproduces the executor's published index hash at the disputed seq: no breach.
    Honest { seq: u64, index_hash: String },
    /// Replay contradicts the published ack. This is a reproducible breach -- anyone running the
    /// same artefact reaches the same two hashes.
    Dishonest {
        seq: u64,
        published: String,
        recomputed: String,
    },
    /// The artefact does not hold together, so it settles nothing. Distinct from `Honest`: a
    /// malformed accusation is not a vindication.
    Inadmissible { reason: String },
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Honest { .. } => "honest",
            Verdict::Dishonest { .. } => "dishonest",
            Verdict::Inadmissible { .. } => "inadmissible",
        }
    }
}

/// Settle a [`Dispute`].
///
/// The third party checks, in order: the checkpoint's signature and its own index (so the starting
/// state is attested, not asserted); that the segment chains onto the checkpoint's `log_head` (so the
/// entries are the ones that followed it); and then replays. It never asks the monitor or the
/// executor anything, and it never copies an index -- it recomputes one, exactly as replication does.
pub fn adjudicate(contract: &Contract, dispute: &Dispute) -> Verdict {
    if let Err(e) = dispute.checkpoint.verify() {
        return Verdict::Inadmissible {
            reason: format!("checkpoint does not verify: {e}"),
        };
    }
    let cp = &dispute.checkpoint.checkpoint;
    if dispute.segment.is_empty() {
        return Verdict::Inadmissible {
            reason: "dispute carries no entries".to_string(),
        };
    }
    let first = &dispute.segment[0];
    if first.op.seq != cp.window_end {
        return Verdict::Inadmissible {
            reason: format!(
                "segment starts at seq {}, checkpoint resumes at {}",
                first.op.seq, cp.window_end
            ),
        };
    }
    if first.prev_hash != cp.log_head {
        return Verdict::Inadmissible {
            reason: format!(
                "segment links to {}, checkpoint head is {}",
                first.prev_hash, cp.log_head
            ),
        };
    }
    let last = dispute
        .segment
        .last()
        .expect("segment is non-empty: checked above");
    if last.op.seq != dispute.disputed_seq {
        return Verdict::Inadmissible {
            reason: format!(
                "segment ends at seq {}, dispute names {}",
                last.op.seq, dispute.disputed_seq
            ),
        };
    }
    let log = match OpLog::restore(first.op.seq, &first.prev_hash, dispute.segment.clone()) {
        Ok(l) => l,
        Err(e) => {
            return Verdict::Inadmissible {
                reason: format!("segment chain is broken: {}", e.stored),
            }
        }
    };
    match replay_from(contract, &log, Some(cp)) {
        Ok(index) => Verdict::Honest {
            seq: dispute.disputed_seq,
            index_hash: index_hash(&index),
        },
        Err(Breach::DivergentIndex {
            seq,
            published,
            recomputed,
        }) => Verdict::Dishonest {
            seq,
            published,
            recomputed,
        },
        Err(other) => Verdict::Inadmissible {
            reason: format!("replay could not be completed: {other}"),
        },
    }
}

impl From<CheckpointVerifyError> for Breach {
    fn from(e: CheckpointVerifyError) -> Breach {
        match e {
            CheckpointVerifyError::IndexHashMismatch {
                window_end,
                published,
                computed,
            } => Breach::BadCheckpoint {
                window_end,
                published,
                computed,
            },
            other => Breach::BadCheckpoint {
                window_end: 0,
                published: other.to_string(),
                computed: String::new(),
            },
        }
    }
}
