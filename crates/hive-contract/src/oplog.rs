//! The append-only operation log: one hash-chained, tamper-evident record per contract.
//!
//! Vitra's oplog is a signed, append-only Hypercore; every message can produce an inclusion proof
//! (whitepaper 2.7). The construction here is the same shape over a different primitive: each entry
//! commits to its predecessor, so the log's head hash binds the whole history and any rewrite,
//! truncation or fork is detectable by replay. Signing stays where the platform already does it
//! (ed25519 gossip envelopes, `IntegritySigner`); this log is the CONTENT commitment, not a
//! replacement for transport authentication.
//!
//! Entries are never mutated and never removed. [`OpLog::append`] is the only writer.

use crate::contract::{Caller, OpSpec};
use std::collections::BTreeMap;

/// Domain separators for the BLAKE3 content identities this module mints. New identities are BLAKE3
/// per the repo's hashing rule (`fluid_core::content_hash`); these are new, so BLAKE3 it is.
pub const OP_DOMAIN: &[u8] = b"hive-contract/oplog/v1";
pub const INDEX_DOMAIN: &[u8] = b"hive-contract/index/v1";

/// Canonical bytes for a value: a hand-rolled, feature-independent serialization.
///
/// `serde_json` map ordering depends on the `preserve_order` feature, which another workspace member
/// could turn on and silently change every digest this crate mints. Hashing therefore never goes
/// through a `HashMap`/`serde_json::Value`: payload and index are `BTreeMap` (sorted by
/// construction) and are written out here with explicit separators.
pub fn canonical_fields(fields: &[&str]) -> Vec<u8> {
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push('\x1f');
        }
        out.push_str(f);
    }
    out.into_bytes()
}

/// Canonical bytes for a `BTreeMap`: `k=v` lines, keys already sorted.
pub fn canonical_map(m: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = String::new();
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push('\x1e');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
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

/// One log entry: the operation, the ack the executor produced for it, and the chain commitment.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub op: Op,
    pub ack: Ack,
    /// `BLAKE3(OP_DOMAIN || prev_hash || op.canonical())`.
    pub entry_hash: String,
    pub prev_hash: String,
}

/// Append-only operation log for one contract.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct OpLog {
    entries: Vec<LogEntry>,
    head: String,
}

impl OpLog {
    pub fn new() -> OpLog {
        OpLog::default()
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// The next sequence number. Assigned by the single writer, which is the only caller of
    /// [`OpLog::append`], so concurrent submitters can never race for the same seq.
    pub fn next_seq(&self) -> u64 {
        self.entries.len() as u64
    }

    /// The head the log WOULD have after appending `op`, without mutating. Lets the executor stamp
    /// the resulting head into the ack it appends alongside the op, so the entry is self-consistent
    /// on the first write.
    pub fn head_after(&self, op: &Op) -> String {
        let mut preimage = self.head.clone().into_bytes();
        preimage.push(b'\x1c');
        preimage.extend_from_slice(&op.canonical());
        fluid_core::content_hash(OP_DOMAIN, &preimage)
    }

    /// Append one op + its ack. The only mutation the log admits; there is no remove, no truncate
    /// and no update, so a fork (Vitra's `LogForkFraudProof` case) cannot be produced here.
    pub fn append(&mut self, op: Op, ack: Ack) -> String {
        let mut preimage = self.head.clone().into_bytes();
        preimage.push(b'\x1c');
        preimage.extend_from_slice(&op.canonical());
        let entry_hash = fluid_core::content_hash(OP_DOMAIN, &preimage);
        self.entries.push(LogEntry {
            op,
            ack,
            prev_hash: std::mem::take(&mut self.head),
            entry_hash: entry_hash.clone(),
        });
        self.head = entry_hash;
        self.head.clone()
    }

    /// Walk the chain and recompute every entry hash. Any rewrite, reorder or truncation of a
    /// stored entry shows up as a `ChainBreak` at the first diverging seq.
    pub fn verify_chain(&self) -> Result<(), ChainBreak> {
        let mut prev = String::new();
        for (i, e) in self.entries.iter().enumerate() {
            let mut preimage = prev.clone().into_bytes();
            preimage.push(b'\x1c');
            preimage.extend_from_slice(&e.op.canonical());
            let expect = fluid_core::content_hash(OP_DOMAIN, &preimage);
            if expect != e.entry_hash || e.prev_hash != prev {
                return Err(ChainBreak {
                    seq: i as u64,
                    expected: expect,
                    stored: e.entry_hash.clone(),
                });
            }
            prev = e.entry_hash.clone();
        }
        if prev != self.head {
            return Err(ChainBreak {
                seq: self.entries.len() as u64,
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

/// A validated, not-yet-executed operation: the output of `contract::validate`, carrying the spec it
/// was validated against so downstream code cannot drift from the decision.
pub struct Admission<'a> {
    pub op: Op,
    pub spec: &'a OpSpec,
    pub caller: Caller,
}
