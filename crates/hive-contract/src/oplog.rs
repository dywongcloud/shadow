//! The append-only operation log: one hash-chained, tamper-evident record per contract.
//!
//! Vitra's oplog is a signed, append-only Hypercore; every message can produce an inclusion proof
//! (whitepaper 2.7). The construction here is the same shape over a different primitive: each entry
//! commits to its predecessor, so the log's head hash binds the whole history and any rewrite,
//! truncation or fork is detectable by replay. Signing stays where the platform already does it
//! (ed25519 gossip envelopes, `IntegritySigner`); this log is the CONTENT commitment, not a
//! replacement for transport authentication.
//!
/// Entries are never mutated: [`OpLog::append`] and [`OpLog::append_entry`] are the only writers, and
/// neither can change an entry already stored.
///
/// They can be DROPPED, but only from the front, only by [`OpLog::truncate_front`], and only against
/// a head a signed checkpoint attests. That is compaction, and it is a different claim from "the log
/// is unbounded": the retained window shrinks to what is still needed to recompute the index, while
/// the seq space, the chain and the checkpoints over the dropped windows stay exactly as they were.

use crate::contract::{Caller, OpSpec};
use std::collections::BTreeMap;

/// Domain separators for the BLAKE3 content identities this module mints. New identities are BLAKE3
/// per the repo's hashing rule (`fluid_core::content_hash`); these are new, so BLAKE3 it is.
pub const OP_DOMAIN: &[u8] = b"hive-contract/oplog/v1";
pub const INDEX_DOMAIN: &[u8] = b"hive-contract/index/v1";
/// Domain separator for the bytes a stored entry's SIGNATURE covers. The signature is not part of the
/// entry hash -- the hash stays a pure function of `(prev, op)` so a monitor can recompute it without
/// any key -- so the signature binds `entry_hash` under its own domain instead.
///
/// The preimage covers the ACK as well as `entry_hash`. That is the difference between a signed entry
/// being EVIDENCE and being a FRAUD PROOF: `entry_hash` is a function of `(prev, op)` alone, so two
/// acks for one op produce the same hash and -- before this binding -- the same signature, and an
/// executor could hand two monitors two different accounts of one operation with nothing to tell
/// them apart. Verified in [`crate::persist::entry_sig_preimage`]; see [`crate::audit`].
pub const ENTRY_SIG_DOMAIN: &[u8] = b"hive-contract/oplog-entry/v1";

/// Canonical bytes for a value: a hand-rolled, feature-independent serialization.
///
/// `serde_json` map ordering depends on the `preserve_order` feature, which another workspace member
/// could turn on and silently change every digest this crate mints. Hashing therefore never goes
/// through a `HashMap`/`serde_json::Value`: payload and index are `BTreeMap` (sorted by
/// construction) and are written out here with explicit separators.
///
/// Every field is LENGTH-PREFIXED (`<len>:<bytes>`). A bare separator join is not injective: a
/// value that contains the separator (or an `=`) reproduces the encoding of a DIFFERENT field list,
/// so two genuinely different values mint one digest. That is not a cosmetic collision -- it lets a
/// hand-built payload travel under an entry hash the executor never signed, and an adjudication cut
/// from it then convicts an honest executor. The prefix makes the encoding parseable left to right,
/// so each field list has exactly one encoding.
pub fn canonical_fields(fields: &[&str]) -> Vec<u8> {
    let mut out = String::new();
    write_canonical_fields(&mut out, fields);
    out.into_bytes()
}

/// [`canonical_fields`] appending into an existing buffer, for a caller that is already building one
/// string (so the length-prefix rule lives in exactly one place).
fn write_canonical_fields(out: &mut String, fields: &[&str]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push('\x1f');
        }
        push_field(out, f);
    }
}

/// One length-prefixed field: `<len>:<bytes>`.
fn push_field(out: &mut String, f: &str) {
    out.push_str(&f.len().to_string());
    out.push(':');
    out.push_str(f);
}

/// Canonical bytes for a `BTreeMap`: length-prefixed `k=v` lines, keys already sorted.
///
/// Both the key and the value are length-prefixed for the same reason
/// [`canonical_fields`] prefixes: a value containing `\x1e` or `=` must not be able to stand in for
/// a field boundary.
pub fn canonical_map(m: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = String::new();
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push('\x1e');
        }
        push_field(&mut out, k);
        out.push('=');
        push_field(&mut out, v);
    }
    out.into_bytes()
}

/// One emitted operation: what Vitra calls the message published on a participant's oplog.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Op {
    /// Monotonic, assigned by the single writer for this contract. This is the replay order.
    pub seq: u64,
    pub contract: String,
    /// The verified tenant that authorized the operation (never a client-set header).
    pub tenant: String,
    pub actor: String,
    /// The database id or project the operation acts on.
    pub target: String,
    pub kind: String,
    pub payload: BTreeMap<String, String>,
}

impl Op {
    /// Bytes the entry hash commits to. `seq` is inside, so a reordering changes every later hash.
    pub fn canonical(&self) -> Vec<u8> {
        let mut head = canonical_fields(&[
            &self.seq.to_string(),
            &self.contract,
            &self.tenant,
            &self.actor,
            &self.target,
            &self.kind,
        ]);
        head.push(b'\x1d');
        head.extend_from_slice(&canonical_map(&self.payload));
        head
    }

    pub fn hash(&self) -> String {
        fluid_core::content_hash(OP_DOMAIN, &self.canonical())
    }
}

/// The executor's acknowledgement: the Vitra `ack` (whitepaper 3.3).
///
/// Non-deterministic facts live HERE and nowhere else -- `ts_ms` and `actor` are stamped by the
/// executor around `apply`, never passed into it. That is what keeps `apply` pure and the log
/// replayable: Vitra's own rule (whitepaper 2.5) is that purity and reproducible ordering are the
/// two properties a deterministic mapping needs, and the ack is where the impurity is parked.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Ack {
    pub seq: u64,
    pub ts_ms: u64,
    pub success: bool,
    /// Populated only on failure; a failed op leaves the index untouched.
    pub error: Option<String>,
    /// The index hash AT THIS SEQ, so a monitor can verify one transaction without replaying to
    /// the head. Vitra stores acks in the output log for exactly this atomicity (3.1).
    pub index_hash: String,
    /// The oplog head hash after this entry was appended.
    pub log_head: String,
}

impl Ack {
    /// Bytes the entry SIGNATURE commits to, alongside the entry hash.
    ///
    /// Why the ack is in the signed preimage at all: `entry_hash` is a function of `(prev, op)` and
    /// CANNOT cover the ack, because `ack.log_head` is the entry hash itself. So an entry's ack is
    /// the one field of a stored line that a signature over `entry_hash` alone does not bind -- and
    /// it is the field that says what the executor claims happened. Left unbound, anybody who has
    /// ever seen one signed line can copy its signature onto a second, conflicting ack and accuse
    /// an honest executor of equivocation; the accusation would verify, and the proof would convict
    /// the innocent. Binding the ack makes the accusation sound: two conflicting acks at one seq
    /// now require two different signatures from the node, so only the executor could have
    /// published both (see [`crate::fraud::Equivocation`]).
    ///
    /// `error` is written as a PRESENCE TAG plus the value, not as `error.unwrap_or_default()`.
    /// Flattening an `Option<String>` to a bare string is not injective: `None` and `Some("")` both
    /// encode to the empty field, so one signature covers both, while
    /// [`crate::fraud::conflict`] compares the typed field and sees them as different. That gap is
    /// exactly the forgery this function exists to close -- flipping `None` to `Some("")` on a
    /// genuine signed entry kept the signature valid and produced an artefact that convicted a node
    /// that had done nothing wrong. `0`/`1` can never collide with a value's own bytes.
    pub fn canonical(&self) -> Vec<u8> {
        let (present, value) = match &self.error {
            None => ("0", ""),
            Some(e) => ("1", e.as_str()),
        };
        canonical_fields(&[
            &self.seq.to_string(),
            &self.ts_ms.to_string(),
            &self.success.to_string(),
            present,
            value,
            &self.index_hash,
            &self.log_head,
        ])
    }
}

/// One log entry: the operation, the ack the executor produced for it, and the chain commitment.
///
/// `entry_hash` is a stored fact AND a recomputable one: [`LogEntry::expected_hash`] derives it from
/// `prev_hash` and `op`, which is what lets a verifier catch an entry whose OPERATION was rewritten
/// under a hash copied from the original. Comparing the stored field to itself proves nothing, so
/// nothing here does.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub op: Op,
    pub ack: Ack,
    /// `BLAKE3(OP_DOMAIN || prev_hash || op.canonical())`.
    pub entry_hash: String,
    pub prev_hash: String,
}

impl LogEntry {
    /// The entry hash its own contents produce: `chain_hash(prev_hash, op)`.
    ///
    /// This is the binding a verifier actually checks. An entry is offered as evidence, and the hash
    /// it carries is only meaningful if it is the hash of THIS op over THIS prev -- otherwise a
    /// rewritten payload travels under the original entry's hash and every proof cut for it still
    /// "verifies".
    pub fn expected_hash(&self) -> String {
        OpLog::chain_hash(&self.prev_hash, &self.op)
    }

    /// Whether the entry's stored hash is the one its own contents produce.
    pub fn hash_is_self_consistent(&self) -> bool {
        self.expected_hash() == self.entry_hash
    }
}

/// Append-only operation log for one contract.
///
/// `entries` is the RETAINED window, not the whole history: compaction drops the entries a signed
/// checkpoint has already attested, and [`OpLog::base`] is the seq of the first entry that is still
/// here. Sequence numbers are never renumbered by a compaction -- an entry's `seq` is its identity
/// for as long as any checkpoint or replica cites it -- so `base` is carried instead: the log is a
/// half-open range `[base, base + entries.len())` over the contract's seq space.
///
/// `base_prev` is the head the first retained entry links to. It is what keeps the chain verifiable
/// across a cut: without it, dropping the entries before `base` would leave `entries[0].prev_hash`
/// pointing at a hash nobody can check. It comes from the signed checkpoint that authorized the cut,
/// so the value is attested rather than remembered.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct OpLog {
    entries: Vec<LogEntry>,
    head: String,
    /// Seq of `entries[0]`. Zero for a log that has never been compacted.
    #[serde(default)]
    base: u64,
    /// The head `entries[0].prev_hash` must equal: the log head just before `base`.
    #[serde(default)]
    base_prev: String,
}

impl OpLog {
    pub fn new() -> OpLog {
        OpLog::default()
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    /// Seq of the first RETAINED entry. Everything before this was compacted away under a signed
    /// checkpoint; it is not "gone from history", it is no longer needed to recompute the index.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// The log head immediately before [`OpLog::base`]: what the first retained entry links to.
    pub fn base_prev(&self) -> &str {
        &self.base_prev
    }

    /// How many entries are retained here. NOT the contract's seq count -- that is
    /// [`OpLog::next_seq`]. Comparing two logs by `len()` compares two different windows and means
    /// nothing; compare by seq.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// The entry at `seq`, wherever the retained window starts. `None` for a seq that has been
    /// compacted away or that has not been written yet -- and those two are indistinguishable ON
    /// PURPOSE: a log that has dropped a prefix must not claim to know what was in it.
    pub fn entry_at(&self, seq: u64) -> Option<&LogEntry> {
        let i = seq.checked_sub(self.base)? as usize;
        self.entries.get(i)
    }

    /// The next sequence number. Assigned by the single writer, which is the only caller of
    /// [`OpLog::append`], so concurrent submitters can never race for the same seq.
    ///
    /// `base + len`, never `len`: after a compaction the retained window does not start at zero, and
    /// numbering a new entry as if it did would give it a seq an earlier entry already holds.
    pub fn next_seq(&self) -> u64 {
        self.base + self.entries.len() as u64
    }

    /// The hash an entry linking `prev` to `op` must carry. One construction, used by
    /// [`OpLog::head_after`], [`OpLog::append`], [`OpLog::append_entry`] and
    /// [`OpLog::verify_chain`], so an entry cannot be valid on one path and invalid on another.
    pub(crate) fn chain_hash(prev: &str, op: &Op) -> String {
        let mut preimage = prev.as_bytes().to_vec();
        preimage.push(b'\x1c');
        preimage.extend_from_slice(&op.canonical());
        fluid_core::content_hash(OP_DOMAIN, &preimage)
    }

    /// The head the log WOULD have after appending `op`, without mutating. Lets the executor stamp
    /// the resulting head into the ack it appends alongside the op, so the entry is self-consistent
    /// on the first write.
    pub fn head_after(&self, op: &Op) -> String {
        OpLog::chain_hash(&self.head, op)
    }

    /// Append one op + its ack. The only mutation the log admits; there is no remove, no truncate
    /// and no update, so a fork (Vitra's `LogForkFraudProof` case) cannot be produced here.
    pub fn append(&mut self, op: Op, ack: Ack) -> String {
        let entry_hash = OpLog::chain_hash(&self.head, &op);
        self.entries.push(LogEntry {
            op,
            ack,
            prev_hash: std::mem::take(&mut self.head),
            entry_hash: entry_hash.clone(),
        });
        self.head = entry_hash;
        self.head.clone()
    }

    /// Entries at or after `seq`: the unit a replica offers a peer that is behind.
    ///
    /// Keyed on SEQ, not position: a compacted peer's first retained entry is not at position 0, and
    /// slicing by position would hand out the wrong entries.
    pub fn entries_from(&self, seq: u64) -> &[LogEntry] {
        let start = seq
            .saturating_sub(self.base)
            .min(self.entries.len() as u64) as usize;
        &self.entries[start..]
    }

    /// Rebuild a log from a retained window: the load path after a restart and after a compaction.
    ///
    /// The window is validated, not trusted -- the chain is recomputed from `base_prev` and the head
    /// is re-derived -- so a segment file that was truncated, reordered or edited does not become a
    /// log that quietly verifies.
    pub fn restore(base: u64, base_prev: &str, entries: Vec<LogEntry>) -> Result<OpLog, ChainBreak> {
        // With no retained entries the log's head IS the head before `base`: a store that was
        // checkpointed and compacted down to an empty tail (`checkpoint_now` on shutdown does exactly
        // this) has nothing left to replay, and defaulting the head to "" made `verify_chain` report a
        // break at the one moment the log was most provably consistent -- so an honest restart was
        // refused, and the refusal masked every check that ran after it.
        let head = entries
            .last()
            .map(|e| e.entry_hash.clone())
            .unwrap_or_else(|| base_prev.to_string());
        let log = OpLog {
            head,
            entries,
            base,
            base_prev: base_prev.to_string(),
        };
        log.verify_chain()?;
        Ok(log)
    }

    /// Drop every entry before `keep_from`: the compaction primitive.
    ///
    /// Refused unless a checkpoint actually attests the cut, which is what `prev_at` is: the head the
    /// log had at `keep_from - 1`, taken from the signed checkpoint, not remembered by this process.
    /// Compaction without that value would leave `entries[0].prev_hash` unverifiable, so the log's
    /// chain would have a hole in it that [`OpLog::verify_chain`] could no longer detect -- the one
    /// thing an append-only log must never have.
    pub fn truncate_front(
        &mut self,
        keep_from: u64,
        prev_at: &str,
    ) -> Result<usize, TruncateError> {
        if keep_from < self.base {
            return Err(TruncateError::BelowBase {
                keep_from,
                base: self.base,
            });
        }
        let next = self.next_seq();
        if keep_from > next {
            return Err(TruncateError::BeyondHead { keep_from, next });
        }
        let drop = (keep_from - self.base) as usize;
        if drop == 0 {
            return Ok(0);
        }
        self.entries.drain(..drop);
        self.base = keep_from;
        self.base_prev = prev_at.to_string();
        Ok(drop)
    }

    /// Whether the retained part of this log is a byte-identical head of `self`.
    ///
    /// Two logs whose retained windows differ can still be compared, over the range they both hold:
    /// that is the only evidence either of them has. A peer that is merely behind, or that is behind
    /// a compaction the other side has already performed, still compares equal over the overlap,
    /// which is what keeps "merge a peer that is behind" working after a compaction.
    ///
    /// `false` when there is no overlap at all: an offer that begins past our head is not a prefix of
    /// anything we hold, it is a gap.
    pub fn starts_with(&self, prefix: &OpLog) -> bool {
        if prefix.next_seq() > self.next_seq() {
            return false;
        }
        let from = self.base.max(prefix.base);
        // A prefix that retains NO entries (a fresh log, or one compacted down to nothing) has
        // nothing to compare -- and it is still a prefix of everything that reaches at least as far.
        // Without this the empty case fell through to "no seq in common", which made a brand-new
        // replica unable to merge the head it was offered and reported it as a fork.
        if prefix.base >= prefix.next_seq() {
            return prefix.next_seq() <= self.next_seq();
        }
        if prefix.next_seq() <= from {
            return false;
        }
        (from..prefix.next_seq()).all(|seq| match (self.entry_at(seq), prefix.entry_at(seq)) {
            (Some(a), Some(b)) => {
                a.entry_hash == b.entry_hash && a.op.canonical() == b.op.canonical()
            }
            _ => false,
        })
    }

    /// Append one entry a peer offered. The only remote-fed writer, and it RE-DERIVES the entry hash
    /// instead of trusting the peer's copy, so a fabricated, reordered or truncated entry cannot be
    /// merged. The entry's ack is not verified here -- [`crate::replicate::Replica::merge`] checks it,
    /// because that check needs the contract and the index.
    pub fn append_entry(&mut self, entry: LogEntry) -> Result<u64, AppendEntryError> {
        let expect = self.next_seq();
        if entry.op.seq != expect {
            return Err(AppendEntryError::Gap {
                expected: expect,
                offered: entry.op.seq,
            });
        }
        if entry.ack.seq != expect {
            return Err(AppendEntryError::Gap {
                expected: expect,
                offered: entry.ack.seq,
            });
        }
        if entry.prev_hash != self.head {
            return Err(AppendEntryError::Prev {
                seq: expect,
                expected: self.head.clone(),
                offered: entry.prev_hash,
            });
        }
        let computed = OpLog::chain_hash(&self.head, &entry.op);
        if computed != entry.entry_hash {
            return Err(AppendEntryError::Hash {
                seq: expect,
                expected: computed,
                offered: entry.entry_hash,
            });
        }
        self.entries.push(entry);
        self.head = self.entries[self.entries.len() - 1].entry_hash.clone();
        Ok(expect)
    }

    /// Walk the chain and recompute every entry hash. Any rewrite, reorder or truncation of a
    /// stored entry shows up as a `ChainBreak` at the first diverging seq.
    ///
    /// Starts from `base_prev`, not from the empty string: after a compaction the first retained
    /// entry links to a head this log no longer holds, and walking from `""` would report a break at
    /// every entry of every compacted log.
    pub fn verify_chain(&self) -> Result<(), ChainBreak> {
        self.verify_chain_from(self.base, &self.base_prev)
    }

    /// Walk the chain from an attested starting point: the replay-from-a-checkpoint path, where the
    /// head at `start_seq - 1` comes from the signed checkpoint rather than from this log's own
    /// `base_prev`. Every retained entry is checked either way; only the anchor differs.
    pub fn verify_chain_from(&self, start_seq: u64, start_prev: &str) -> Result<(), ChainBreak> {
        let start = match start_seq.checked_sub(self.base) {
            Some(i) => i as usize,
            None => {
                return Err(ChainBreak {
                    seq: start_seq,
                    expected: start_seq.to_string(),
                    stored: format!("log window starts at {}", self.base),
                })
            }
        };
        let mut prev = start_prev.to_string();
        for (i, e) in self.entries.iter().enumerate().skip(start) {
            let seq = self.base + i as u64;
            let expect = OpLog::chain_hash(&prev, &e.op);
            if expect != e.entry_hash || e.prev_hash != prev {
                return Err(ChainBreak {
                    seq,
                    expected: expect,
                    stored: e.entry_hash.clone(),
                });
            }
            prev = e.entry_hash.clone();
        }
        if prev != self.head {
            return Err(ChainBreak {
                seq: self.next_seq(),
                expected: prev,
                stored: self.head.clone(),
            });
        }
        Ok(())
    }
}

/// The log's append-only invariant was violated at `seq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainBreak {
    pub seq: u64,
    pub expected: String,
    pub stored: String,
}

/// Why a compaction could not be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncateError {
    /// `keep_from` is before the window's own start: those entries are already gone, so this is a
    /// second compaction of the same range, not a new one.
    BelowBase { keep_from: u64, base: u64 },
    /// `keep_from` is past the head: it would drop entries that do not exist yet.
    BeyondHead { keep_from: u64, next: u64 },
}

impl std::fmt::Display for TruncateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TruncateError::BelowBase { keep_from, base } => write!(
                f,
                "cannot compact to seq {keep_from}: window already starts at {base}"
            ),
            TruncateError::BeyondHead { keep_from, next } => write!(
                f,
                "cannot compact to seq {keep_from}: log head is {next}"
            ),
        }
    }
}

/// Why an offered entry could not be appended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppendEntryError {
    /// The offered entry's seq is not the one the log's single writer would assign next: a gap or a
    /// replay. The log is dense, so a hole is never filled out of order.
    Gap { expected: u64, offered: u64 },
    /// The offered entry does not link to our head.
    Prev {
        seq: u64,
        expected: String,
        offered: String,
    },
    /// The offered entry's hash is not the one its own op produces: fabricated or rewritten.
    Hash {
        seq: u64,
        expected: String,
        offered: String,
    },
}

impl std::fmt::Display for AppendEntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppendEntryError::Gap { expected, offered } => write!(
                f,
                "offered entry at seq {offered}, log expects {expected}"
            ),
            AppendEntryError::Prev {
                seq,
                expected,
                offered,
            } => write!(
                f,
                "offered entry at seq {seq} links to {offered}, log head is {expected}"
            ),
            AppendEntryError::Hash {
                seq,
                expected,
                offered,
            } => write!(
                f,
                "offered entry at seq {seq} carries hash {offered}, computed {expected}"
            ),
        }
    }
}

/// A validated, not-yet-executed operation: the output of `contract::validate`, carrying the spec it
/// was validated against so downstream code cannot drift from the decision.
pub struct Admission<'a> {
    pub op: Op,
    pub spec: &'a OpSpec,
    pub caller: Caller,
}
