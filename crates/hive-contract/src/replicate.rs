//! Replication: how a second node comes to hold the same `(log, index)` pair the executor holds.
//!
//! Vitra has no consensus: a participant's oplog is synced by peers, and honesty is enforced by
//! replay, not by agreement (whitepaper 2.6-2.7). This module is that sync, and it inherits the one
//! property the whole design rests on: **a replica never believes a peer's index.** It accepts the
//! LOG, re-derives the index itself through the same pure `apply` the executor used, and rejects the
//! batch the moment its own recomputation disagrees with the ack the publisher stamped. A peer can
//! therefore withhold entries or offer a fork; it cannot offer a history whose consequences are
//! different from the ones it claims.
//!
//! The merge is PER-ROW, keyed `(contract, seq)`, because that is what the substrate demands:
//! `store_sync` adoption is wholesale-replace and is therefore allowed only from an owner every node
//! agrees on (AGENTS.md; `cluster.rs:49` `OwnerSource::may_adopt_wholesale_from`), and from a backup
//! owner or a `Fallback` target only per-row `MERGE_STORES` merge. [`Replica::merge`] is the per-row
//! path and is always legal; [`Replica::adopt_wholesale`] is the replace path and carries the
//! authority predicate, mirrored from the real one rather than re-invented.
//!
//! Four invariants, each of which the merge enforces structurally:
//!
//! 1. **A merge never moves the log backwards.** A peer that is merely behind is a no-op; a peer
//!    holding a SHORTER, different history is refused as [`MergeError::Regress`]. This is the fleet's
//!    own rule ("one stale reading never withdraws one") stated at the replication seam: since ops
//!    replicate rather than rows, rolling the log back is the only way a deleted key could come back.
//! 2. **Tombstones are permanent because the log is.** There is no erase: a `db.delete` is an
//!    appended op, and a replica that has not synced it has simply not seen it yet.
//! 3. **Deletion is learned from an op, never from an absence.** A peer offering only an index has
//!    nothing admissible -- [`ReplicaSnapshot`] cannot be built without a log, so that shape is
//!    unrepresentable, not merely rejected.
//! 4. **Authorization is decided once, by the owner, and carried in the op.** A replica re-checks
//!    everything it CAN check locally (the kind is declared, the payload matches, a `LogOnly` op
//!    still produces no mutation, the ack agrees with its own replay) and does not re-decide
//!    `owns_target`, which only the owner can attest against canonical state. Monitors close that
//!    gap against canonical state instead (`crate::reconcile`).
//!
//! Permissionless participation lives here too: a `LogOnly` proposal (`db.propose`,
//! `contract.rs:267`) may be relayed by any node with no allowlisting, and [`Replica::merge`]
//! accepts it. A `StateWrite` op arriving the same way is still held to the contract, and
//! `Contract::declare` (`contract.rs:108`) makes `OpenParticipation + StateWrite` unrepresentable, so
//! relaying cannot widen an authorization decision.

use crate::audit::{replay, Breach};
use crate::contract::{Contract, Effect};
use crate::exec::{apply, index_hash, Index};
use crate::oplog::{AppendEntryError, ChainBreak, LogEntry, OpLog};

/// Domain separator for a snapshot's content identity (BLAKE3, per the repo's new-identity rule).
pub const SNAPSHOT_DOMAIN: &[u8] = b"hive-contract/snapshot/v1";

/// One contract's copy on a node that is not the writer. Same `(log, index)` shape as
/// [`crate::exec::Executor`], fed by [`Replica::merge`] instead of by `Engine::submit`.
#[derive(Clone, Debug)]
pub struct Replica {
    pub contract: Contract,
    pub log: OpLog,
    pub index: Index,
}

impl Replica {
    pub fn new(contract: Contract) -> Replica {
        Replica {
            contract,
            log: OpLog::new(),
            index: Index::new(),
        }
    }

    pub fn index_hash(&self) -> String {
        index_hash(&self.index)
    }

    pub fn snapshot(&self) -> ReplicaSnapshot {
        ReplicaSnapshot {
            contract_id: self.contract.id.clone(),
            contract_version: self.contract.version,
            log: self.log.clone(),
        }
    }

    /// Merge everything in `offered` that this replica does not already hold.
    ///
    /// Idempotent, order-insensitive over the set of logs merged, and atomic: the whole tail is
    /// verified against a shadow copy first, so a batch that fails at entry N leaves this replica
    /// exactly as it was. Order-insensitivity is what makes convergence hold -- two replicas that
    /// merge the same set of entries in different orders land on the same index hash, because order
    /// is the log's `seq`, not the order the batches arrived in.
    pub fn merge(&mut self, offered: &OpLog) -> Result<SyncReport, MergeError> {
        if offered.is_empty() {
            return Err(MergeError::Empty);
        }
        // Lengths are SEQ counts, never retained-entry counts: after a compaction a replica's window
        // is shorter than its history, and comparing two windows by `len()` would call a compacted
        // peer "behind" and refuse a history it already agrees with.
        let before = self.log.next_seq();
        let theirs = offered.next_seq();

        // A peer that is behind, or holding a different history of the same or shorter length.
        if theirs <= before {
            if self.log.starts_with(offered) {
                return Ok(SyncReport {
                    appended: 0,
                    duplicates: offered.len(),
                    skipped_stale: theirs < before,
                    len_before: before as usize,
                    len_after: before as usize,
                });
            }
            let seq = first_divergence(&self.log, offered);
            return Err(if theirs < before {
                MergeError::Regress {
                    held: before as usize,
                    offered: theirs as usize,
                }
            } else {
                MergeError::Fork {
                    seq,
                    held: hash_at(&self.log, seq),
                    offered: hash_at(offered, seq),
                }
            });
        }

        if !offered.starts_with(&self.log) {
            let seq = first_divergence(&self.log, offered);
            return Err(MergeError::Fork {
                seq,
                held: hash_at(&self.log, seq),
                offered: hash_at(offered, seq),
            });
        }

        // Verify the whole tail against a shadow copy: entry linkage, contract shape, and the ack.
        // The tail starts at OUR next seq, by seq: `before` is a seq, and `entries_from` is keyed on
        // one, so a compacted publisher hands over exactly the entries we lack.
        let mut shadow_log = self.log.clone();
        let mut shadow_index = self.index.clone();
        let mut appended = 0usize;
        for entry in offered.entries_from(before) {
            shadow_log.append_entry(entry.clone())?;
            self.verify_against(&mut shadow_index, entry)?;
            appended += 1;
        }
        self.log = shadow_log;
        self.index = shadow_index;
        Ok(SyncReport {
            appended,
            duplicates: 0,
            skipped_stale: false,
            len_before: before as usize,
            len_after: self.log.next_seq() as usize,
        })
    }

    /// Re-derive what `entry` does to `index` and hold the publisher's ack to it. The one place
    /// replication decides whether a peer told the truth: it never copies an index, it recomputes
    /// one. Mirrors [`crate::exec::Executor::commit`] exactly, so the two cannot drift -- same
    /// `apply`, same effect check, same ack comparison.
    fn verify_against(&self, index: &mut Index, entry: &LogEntry) -> Result<(), MergeError> {
        let seq = entry.op.seq;
        if !entry.ack.success {
            let recomputed = index_hash(index);
            if recomputed != entry.ack.index_hash {
                return Err(MergeError::DivergentAck {
                    seq,
                    published: entry.ack.index_hash.clone(),
                    recomputed,
                });
            }
            return Ok(());
        }
        let spec = self
            .contract
            .spec(&entry.op.kind)
            .ok_or_else(|| MergeError::UndeclaredOp {
                seq,
                kind: entry.op.kind.clone(),
            })?;
        let mutations = apply(index, &entry.op);
        if spec.effect == Effect::LogOnly && !mutations.is_empty() {
            return Err(MergeError::EffectViolation {
                seq,
                kind: entry.op.kind.clone(),
                produced: mutations.len(),
            });
        }
        for m in mutations {
            match m {
                crate::exec::Mutation::Put { key, value } => {
                    index.insert(key, value);
                }
                crate::exec::Mutation::Delete { key } => {
                    index.remove(&key);
                }
            }
        }
        let recomputed = index_hash(index);
        if recomputed != entry.ack.index_hash {
            return Err(MergeError::DivergentAck {
                seq,
                published: entry.ack.index_hash.clone(),
                recomputed,
            });
        }
        Ok(())
    }

    /// Rebuild the index from the log we hold, discarding whatever index we had. Used after a
    /// wholesale adoption, and after any local repair: the log is the record, the index is derived.
    fn rebuild(&mut self) -> Result<SyncReport, MergeError> {
        let len_before = self.log.len();
        let index = replay(&self.contract, &self.log)?;
        self.index = index;
        Ok(SyncReport {
            appended: 0,
            duplicates: len_before,
            skipped_stale: false,
            len_before,
            len_after: self.log.len(),
        })
    }

    /// Replace the whole log with an authority's snapshot.
    ///
    /// The gate is the fleet's own, mirrored from `crates/hive-cloud/src/cluster.rs:49`
    /// (`may_adopt_wholesale_from`): only the strict chain head, or the identity election on a
    /// chain-less mesh. A backup owner differs per observer and would hand out a stale fork, so from
    /// it -- and from every non-authority -- replication degrades to [`Replica::merge`], never to a
    /// replace. Also refused: an empty or unparsable payload, and anything that would move the log
    /// backwards.
    pub fn adopt_wholesale(
        &mut self,
        snap: &ReplicaSnapshot,
        source: &AdoptSource,
    ) -> Result<SyncReport, MergeError> {
        if snap.log.is_empty() {
            return Err(MergeError::Empty);
        }
        if !source.may_adopt_wholesale() {
            return Err(MergeError::NotAuthorizedWholesale {
                owner: source.owner.clone(),
                reason: source.reason(),
            });
        }
        if snap.contract_id != self.contract.id {
            return Err(MergeError::ContractMismatch {
                expected: self.contract.id.clone(),
                offered: snap.contract_id.clone(),
            });
        }
        snap.log.verify_chain().map_err(MergeError::BrokenChain)?;

        // Seq counts again: an authority's snapshot may be compacted, and `len()` would make a
        // compacted authority look like a peer that is behind.
        let before = self.log.next_seq();
        let theirs = snap.log.next_seq();
        if theirs < before {
            if self.log.starts_with(&snap.log) {
                return Ok(SyncReport {
                    appended: 0,
                    duplicates: snap.log.len(),
                    skipped_stale: true,
                    len_before: before as usize,
                    len_after: before as usize,
                });
            }
            return Err(MergeError::Regress {
                held: before as usize,
                offered: theirs as usize,
            });
        }
        if theirs == before && !self.log.starts_with(&snap.log) {
            let seq = first_divergence(&self.log, &snap.log);
            return Err(MergeError::Fork {
                seq,
                held: hash_at(&self.log, seq),
                offered: hash_at(&snap.log, seq),
            });
        }
        if theirs == before {
            return Ok(SyncReport {
                appended: 0,
                duplicates: before as usize,
                skipped_stale: false,
                len_before: before as usize,
                len_after: before as usize,
            });
        }
        self.log = snap.log.clone();
        let mut report = self.rebuild()?;
        report.appended = (self.log.next_seq() - before) as usize;
        report.duplicates = before as usize;
        report.len_before = before as usize;
        report.len_after = self.log.next_seq() as usize;
        Ok(report)
    }
}

/// What a replica sends a peer. The log is mandatory: there is no snapshot of the index alone, so an
/// index-only sync -- which would let a deletion be inferred from an absence -- is unrepresentable.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ReplicaSnapshot {
    pub contract_id: String,
    pub contract_version: u32,
    pub log: OpLog,
}

impl ReplicaSnapshot {
    /// Content identity of the snapshot: contract, schema version and log head. A peer can compare
    /// this before pulling, and a monitor can cite it.
    pub fn digest(&self) -> String {
        let mut pre = self.contract_id.clone().into_bytes();
        pre.push(b'\x1c');
        pre.extend_from_slice(self.contract_version.to_string().as_bytes());
        pre.push(b'\x1c');
        pre.extend_from_slice(self.log.head().as_bytes());
        fluid_core::content_hash(SNAPSHOT_DOMAIN, &pre)
    }

    /// Refuse an unparsable payload rather than adopting an empty default. `serde` can hand back a
    /// snapshot whose log failed to deserialize in a lenient field; that is worse than no snapshot.
    pub fn parse(bytes: &[u8]) -> Result<ReplicaSnapshot, MergeError> {
        if bytes.is_empty() {
            return Err(MergeError::Empty);
        }
        serde_json::from_slice(bytes).map_err(|_| MergeError::Unparsable)
    }
}

/// Where a wholesale offer came from. Mirrors `crates/hive-cloud/src/cluster.rs:29-55`
/// (`OwnerSource` + `may_adopt_wholesale_from`) so the two predicates cannot disagree about who is
/// allowed to replace a store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptSource {
    pub owner: String,
    /// `chain.first() == owner` -- the strict owner of a configured `HIVE_CP_OWNER_CHAIN`.
    pub owner_is_chain_head: bool,
    /// The identity election. Only ever an authority when no chain is configured.
    pub from_election: bool,
    pub chain_configured: bool,
}

impl AdoptSource {
    /// Whether a wholesale replace is admissible from this source.
    pub fn may_adopt_wholesale(&self) -> bool {
        if !self.chain_configured {
            // No chain: the election is the authority. Quorum-free meshes only.
            self.from_election
        } else {
            // Chain configured: only the head. A backup owner or a Fallback guess is merge-only.
            self.owner_is_chain_head
        }
    }

    /// Why a wholesale replace was refused, for the report.
    pub fn reason(&self) -> &'static str {
        if !self.chain_configured && !self.from_election {
            "no owner chain and no election: source is not a write authority"
        } else if self.chain_configured && !self.owner_is_chain_head {
            "backup owner or fallback target: wholesale replace is merge-only"
        } else {
            "not a write authority"
        }
    }
}

/// What one merge did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub appended: usize,
    /// Entries the peer offered that we already held. Merge is idempotent, so this is normal.
    pub duplicates: usize,
    /// The offer was a strict prefix of what we hold: a peer that is behind. Never an error, never
    /// a rollback.
    pub skipped_stale: bool,
    pub len_before: usize,
    pub len_after: usize,
}

/// Why a merge or an adoption was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeError {
    /// The offered log is shorter than ours and not a prefix of it: adopting it would erase entries
    /// we hold, and a deleted key would come back.
    Regress { held: usize, offered: usize },
    /// Two histories disagree at one seq. Never merged; the operator has to pick one.
    Fork {
        seq: u64,
        held: String,
        offered: String,
    },
    /// The offered entry could not be linked: a gap, a wrong prev, or a hash that does not match its
    /// own op.
    Append(AppendEntryError),
    /// The publisher's ack does not match what our own `apply` produced: the peer lied about the
    /// consequences of its own log.
    DivergentAck {
        seq: u64,
        published: String,
        recomputed: String,
    },
    /// A successful op whose kind the contract does not declare.
    UndeclaredOp { seq: u64, kind: String },
    /// A `LogOnly` op produced mutations.
    EffectViolation {
        seq: u64,
        kind: String,
        produced: usize,
    },
    /// The offered log's own chain does not verify.
    BrokenChain(ChainBreak),
    /// Wholesale replace from a source that is not an authority. Degrades to merge, never to replace.
    NotAuthorizedWholesale {
        owner: String,
        reason: &'static str,
    },
    /// An offer for a different contract.
    ContractMismatch {
        expected: String,
        offered: String,
    },
    /// Empty or unparsable payload: never adopted, since an empty snapshot erases by looking like a
    /// legitimate short history.
    Empty,
    Unparsable,
    /// A checkpoint the publisher's log fails to honour: its index does not hash to what it names,
    /// its head is not the head its own entries reach, it resumes where this log has already
    /// compacted past, or a compaction was attempted with no head attesting the cut. Carried whole,
    /// because each of those is a different refusal and a merge that swallowed the difference would
    /// leave an operator nothing to act on.
    Checkpoint(Breach),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Checkpoint(b) => write!(f, "{b}"),
            MergeError::Regress { held, offered } => write!(
                f,
                "offered log is shorter ({offered}) than held ({held}) and not a prefix: refusing to regress"
            ),
            MergeError::Fork { seq, held, offered } => {
                write!(f, "histories fork at seq {seq}: held {held}, offered {offered}")
            }
            MergeError::Append(e) => write!(f, "{e}"),
            MergeError::DivergentAck {
                seq,
                published,
                recomputed,
            } => write!(
                f,
                "ack at seq {seq} claims index {published}, replay computes {recomputed}"
            ),
            MergeError::UndeclaredOp { seq, kind } => {
                write!(f, "op at seq {seq} has undeclared kind {kind}")
            }
            MergeError::EffectViolation {
                seq,
                kind,
                produced,
            } => write!(
                f,
                "op {kind} at seq {seq} is log-only but produced {produced} mutation(s)"
            ),
            MergeError::BrokenChain(c) => {
                write!(f, "offered log chain broken at seq {}: {}", c.seq, c.stored)
            }
            MergeError::NotAuthorizedWholesale { owner, reason } => {
                write!(f, "wholesale replace refused from {owner}: {reason}")
            }
            MergeError::ContractMismatch { expected, offered } => {
                write!(f, "offer is for contract {offered}, replica holds {expected}")
            }
            MergeError::Empty => f.write_str("empty snapshot is never adopted"),
            MergeError::Unparsable => f.write_str("unparsable snapshot is never adopted"),
        }
    }
}

impl From<AppendEntryError> for MergeError {
    fn from(e: AppendEntryError) -> MergeError {
        MergeError::Append(e)
    }
}

impl From<Breach> for MergeError {
    fn from(b: Breach) -> MergeError {
        match b {
            Breach::BrokenChain(c) => MergeError::BrokenChain(c),
            Breach::DivergentIndex {
                seq,
                published,
                recomputed,
            } => MergeError::DivergentAck {
                seq,
                published,
                recomputed,
            },
            Breach::SeqMismatch { position, ack_seq } => MergeError::Append(AppendEntryError::Gap {
                expected: position as u64,
                offered: ack_seq,
            }),
            Breach::UndeclaredOp { seq, kind } => MergeError::UndeclaredOp { seq, kind },
            // The checkpoint cases, which only exist once a log can be compacted: a publisher whose
            // attestation does not survive its own entries offers nothing admissible.
            Breach::BadCheckpoint { .. }
            | Breach::CheckpointHeadMismatch { .. }
            | Breach::CheckpointGap { .. }
            | Breach::BadCompaction(_) => MergeError::Checkpoint(b),
        }
    }
}

/// The first seq at which two logs disagree, compared BY SEQ.
///
/// Positional comparison is wrong the moment either log has been compacted: `entries[0]` on one is
/// not `entries[0]` on the other, and zipping the two windows would call every seq after the cut a
/// divergence. Both logs are asked for the same seq instead, and a seq only one of them still holds
/// is reported at face value rather than compared -- a peer that compacted a prefix is not thereby
/// forked.
fn first_divergence(held: &OpLog, offered: &OpLog) -> u64 {
    let from = held.base().max(offered.base());
    let to = held.next_seq().min(offered.next_seq());
    for seq in from..to {
        match (held.entry_at(seq), offered.entry_at(seq)) {
            (Some(a), Some(b)) => {
                if a.entry_hash != b.entry_hash || a.op.canonical() != b.op.canonical() {
                    return seq;
                }
            }
            _ => return seq,
        }
    }
    to
}

/// The entry hash one log holds at `seq`, empty when it does not hold it.
fn hash_at(log: &OpLog, seq: u64) -> String {
    log.entry_at(seq)
        .map(|e| e.entry_hash.clone())
        .unwrap_or_default()
}
