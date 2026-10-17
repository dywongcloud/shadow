//! Monitor-side audit: replay the log and compare against the published index.
//!
//! This is Vitra's contract audit (whitepaper 2.6): a monitor syncs the oplog and index, replays the
//! ops through `apply`, and compares its generated state against the executor's. A divergence is a
//! breach and is reproducible, so the monitor can name the seq and show the inclusion proofs.
//!
//! Two things are checked, and they are different claims:
//!   * [`verify_chain`] -- the log is append-only and un-forked (the `LogForkFraudProof` case).
//!   * [`replay`] -- the executor applied the contract honestly (the contract-breach case).

use crate::contract::Contract;
use crate::exec::{apply, index_hash, Index};
use crate::oplog::{ChainBreak, OpLog};

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
        }
    }
}

/// Replay `log` from genesis against `contract` and return the rebuilt index.
///
/// Deterministic by construction: order is the log's seq order (not arrival order, not a HashMap
/// order), and `apply` is pure. Two honest nodes replaying this log -- different machines, different
/// thread counts -- produce the same [`index_hash`].
pub fn replay(contract: &Contract, log: &OpLog) -> Result<Index, Breach> {
    log.verify_chain().map_err(Breach::BrokenChain)?;
    let mut index = Index::new();
    for (pos, e) in log.entries().iter().enumerate() {
        if e.ack.seq != pos as u64 {
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
            seq: log.len() as u64,
            published: published_head_index_hash.to_string(),
            recomputed: h,
        });
    }
    Ok(h)
}
