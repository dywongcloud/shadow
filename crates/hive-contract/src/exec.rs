//! The executor: one writer per contract, a pure `apply`, and a deterministic index.
//!
//! Vitra's executor is the only party allowed to publish to the index log; participants may propose
//! but cannot violate the contract (whitepaper 2.1-2.2). Same shape here: [`Executor::commit`] is
//! the single writer for a contract's index, and `apply` is a pure function of (index, op) so a
//! monitor can recompute it.
//!
//! `apply` gets no clock, no randomness and no I/O. Everything impure is stamped into the
//! [`Ack`](crate::oplog::Ack) around it, exactly as Vitra puts `ts` in the ack rather than in the
//! apply arguments (whitepaper 3.3, vm-runtime "Restricted Mode").

use crate::contract::{Contract, Effect};
use crate::oplog::{canonical_map, Ack, Op, OpLog, INDEX_DOMAIN};
use std::collections::BTreeMap;

/// The contract's current state: a sorted key -> value map, the Vitra "index".
pub type Index = BTreeMap<String, String>;

/// One queued change to the index. Vitra's `tx` is `put`/`delete` only (contract-apis); so is this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
    Put { key: String, value: String },
    Delete { key: String },
}

/// The deterministic state transition. Pure: no clock, no randomness, no I/O, no `HashMap`
/// iteration. Given the same index and the same op it returns byte-identical mutations, on any
/// thread, in any process -- that is the property the monitor's equality check rests on.
///
/// Keys are namespaced `{tenant}/{target}/...`, so one tenant's writes can never collide with
/// another's.
pub fn apply(index: &Index, op: &Op) -> Vec<Mutation> {
    let ns = format!("{}/{}", op.tenant, op.target);
    match op.kind.as_str() {
        "db.create" => vec![Mutation::Put {
            key: format!("{ns}/status"),
            value: "live".to_string(),
        }],
        "db.delete" => index
            .keys()
            .filter(|k| k.starts_with(&format!("{ns}/")))
            .cloned()
            .map(|key| Mutation::Delete { key })
            .collect(),
        "db.write" => {
            let prev = index
                .get(&format!("{ns}/writes"))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let mut out = vec![Mutation::Put {
                key: format!("{ns}/writes"),
                value: (prev + 1).to_string(),
            }];
            if let Some(v) = op.payload.get("bytes") {
                out.push(Mutation::Put {
                    key: format!("{ns}/last_bytes"),
                    value: v.clone(),
                });
            }
            out
        }
        // LogOnly: admitted to the log, produces no state change. Enforced again after the fact by
        // [`Executor::commit`], so a bug in this table cannot turn a proposal into a write.
        _ => Vec::new(),
    }
}

/// Content identity of an index. BLAKE3 over the canonical sorted map, so two nodes holding the
/// same state hold the same 64-char string regardless of insertion order or thread count.
pub fn index_hash(index: &Index) -> String {
    fluid_core::content_hash(INDEX_DOMAIN, &canonical_map(index))
}

/// Why the executor refused to commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// A LogOnly op produced mutations: the contract's effect declaration and `apply` disagree.
    EffectViolation { kind: String, produced: usize },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::EffectViolation { kind, produced } => write!(
                f,
                "op kind {kind} is declared log-only but apply produced {produced} mutation(s)"
            ),
        }
    }
}

/// One contract's runtime: its log, its index, and its ack history.
///
/// Not internally synchronized: the caller ([`crate::engine::Engine`]) holds the per-contract write
/// lock, so "single writer per surface" is a property of the engine's sharding, not of this struct.
#[derive(Clone, Debug)]
pub struct Executor {
    pub contract: Contract,
    pub log: OpLog,
    pub index: Index,
    acks: Vec<Ack>,
}

impl Executor {
    pub fn new(contract: Contract) -> Executor {
        Executor {
            contract,
            log: OpLog::new(),
            index: Index::new(),
            acks: Vec::new(),
        }
    }

    pub fn acks(&self) -> &[Ack] {
        &self.acks
    }

    pub fn index_hash(&self) -> String {
        index_hash(&self.index)
    }

    /// Execute one admitted op and commit it atomically: the ack is appended carrying the index hash
    /// produced BY this op, so a partially-applied transaction is not representable. Vitra prepends
    /// the ack to the tx and applies the batch atomically for the same reason (whitepaper 3.3).
    ///
    /// `ts_ms` and actor are stamped here, outside `apply`.
    pub fn commit(&mut self, op: Op, ts_ms: u64) -> Result<Ack, ExecError> {
        let spec = self
            .contract
            .spec(&op.kind)
            .expect("op validated against this contract before commit");
        let mutations = apply(&self.index, &op);
        if spec.effect == Effect::LogOnly && !mutations.is_empty() {
            return Err(ExecError::EffectViolation {
                kind: op.kind.clone(),
                produced: mutations.len(),
            });
        }
        for m in mutations {
            match m {
                Mutation::Put { key, value } => {
                    self.index.insert(key, value);
                }
                Mutation::Delete { key } => {
                    self.index.remove(&key);
                }
            }
        }
        let hash = index_hash(&self.index);
        // seq is assigned by the log, before the ack exists: it is the log's, not the caller's.
        let mut seq_op = op;
        seq_op.seq = self.log.next_seq();
        let log_head = self.log.head_after(&seq_op);
        let ack = Ack {
            seq: seq_op.seq,
            ts_ms,
            success: true,
            error: None,
            index_hash: hash,
            log_head,
        };
        self.log.append(seq_op, ack.clone());
        self.acks.push(ack.clone());
        Ok(ack)
    }

    /// Record a refused op as a failed ack: the log stays append-only and complete, and the refusal
    /// is itself auditable. Nothing here decides authorization -- the caller already did.
    pub fn commit_refusal(&mut self, mut op: Op, ts_ms: u64, error: &str) -> Ack {
        op.seq = self.log.next_seq();
        let log_head = self.log.head_after(&op);
        let ack = Ack {
            seq: op.seq,
            ts_ms,
            success: false,
            error: Some(error.to_string()),
            index_hash: index_hash(&self.index),
            log_head,
        };
        self.log.append(op, ack.clone());
        self.acks.push(ack.clone());
        ack
    }
}
