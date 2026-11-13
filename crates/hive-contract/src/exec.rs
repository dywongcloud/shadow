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

use crate::contract::{AuthPolicy, Contract, Effect};
use crate::contracts::billing_meter::{
    BILLING_CHECKPOINT, BILLING_LEDGER, METER_SAMPLE, METER_SKUS,
};
use crate::oplog::{canonical_fields, canonical_map, Ack, LogEntry, Op, OpLog, INDEX_DOMAIN};
use std::collections::BTreeMap;

/// The contract's current state: a sorted key -> value map, the Vitra "index".
pub type Index = BTreeMap<String, String>;

/// One queued change to the index. Vitra's `tx` is `put`/`delete` only (contract-apis); so is this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
    Put { key: String, value: String },
    Delete { key: String },
}

/// Escape one component of an index key's namespace.
///
/// `/` separates the two components, so a raw tenant or target that carries one makes the PAIR
/// ambiguous: ("alice/b", "c") and ("alice", "b/c") both build `alice/b/c`, and `db.delete` deletes
/// by that prefix, so one tenant's delete reaches another's keys. `%` is escaped first and then `/`,
/// so the encoding is injective and the separator cannot occur inside a component. A component with
/// neither character -- every real tenant tag and database id -- encodes to itself.
pub fn escape_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            c => out.push(c),
        }
    }
    out
}

/// The namespace every key of one `(tenant, target)` pair lives under: injective in the PAIR, so no
/// two distinct pairs can address one key. `apply` and `reconcile::project` build keys through this
/// one function so the two cannot disagree about where a database's state lives.
pub fn namespace(tenant: &str, target: &str) -> String {
    format!("{}/{}", escape_component(tenant), escape_component(target))
}

/// The deterministic state transition. Pure: no clock, no randomness, no I/O, no `HashMap`
/// iteration. Given the same index and the same op it returns byte-identical mutations, on any
/// thread, in any process -- that is the property the monitor's equality check rests on.
///
/// Keys are namespaced `namespace(tenant, target)`, so one tenant's writes can never collide with
/// another's.
pub fn apply(index: &Index, op: &Op) -> Vec<Mutation> {
    let ns = namespace(&op.tenant, &op.target);
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
        METER_SAMPLE => meter_sample_mutations(&ns, op),
        BILLING_LEDGER => vec![Mutation::Put {
            key: format!("{ns}/ledger/{}", escape_component(&field(op, "entry_id"))),
            value: ledger_row_value(op),
        }],
        BILLING_CHECKPOINT => vec![Mutation::Put {
            key: format!(
                "{ns}/checkpoint/{}",
                escape_component(&field(op, "period_start_ms"))
            ),
            value: ledger_checkpoint_value(op),
        }],
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

/// The value one metering/billing index key holds: LENGTH-PREFIXED canonical fields, so a
/// replaying monitor can rebuild the exact bytes from the log's payload and compare them against
/// the row it sees in the billing ledger. A bare join is not injective and would let a rewritten
/// charge travel under the original encoding.
pub fn canonical_index_value(fields: &[&str]) -> String {
    String::from_utf8_lossy(&canonical_fields(fields)).into_owned()
}

fn field(op: &Op, key: &str) -> String {
    op.payload.get(key).cloned().unwrap_or_default()
}

/// One meter sample: the cumulative value of each of the twelve SKUs plus the window it was
/// measured at. Cumulative values, not deltas, so replay can compare the contract index against
/// the meter's own persisted row without re-deriving a baseline.
fn meter_sample_mutations(ns: &str, op: &Op) -> Vec<Mutation> {
    let mut out = vec![Mutation::Put {
        key: format!("{ns}/usage/window_end_ms"),
        value: field(op, "window_end_ms"),
    }];
    for sku in METER_SKUS {
        out.push(Mutation::Put {
            key: format!("{ns}/usage/{sku}"),
            value: field(op, sku),
        });
    }
    out
}

/// Canonical bytes of ONE ledger row. The charge's amount, balance, kind, timestamp, tenant and a
/// BLAKE3 digest of its note are all in the signed log payload, so a rewritten charge is detected
/// by recomputing this value from the log and comparing it with the row canonical state holds.
pub fn ledger_row_value(op: &Op) -> String {
    canonical_index_value(&[
        &field(op, "tenant_tag"),
        &field(op, "entry_id"),
        &field(op, "entry_ts_ms"),
        &field(op, "entry_kind"),
        &field(op, "amount_cents"),
        &field(op, "balance_after_cents"),
        &field(op, "note_hash"),
        &field(op, "note"),
    ])
}

/// Canonical bytes of one finalized `LedgerCheckpoint`. Anchors the platform's own SHA-256
/// ledger hash-chain under this log's Ed25519 signature, including the period and the raw row
/// count, so omitting a whole period's charges is detectable too.
pub fn ledger_checkpoint_value(op: &Op) -> String {
    canonical_index_value(&[
        &field(op, "tenant_tag"),
        &field(op, "period_start_ms"),
        &field(op, "period_end_ms"),
        &field(op, "entry_count"),
        &field(op, "commitment"),
    ])
}

/// Why the executor refused to commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// A contract that skipped `declare` (public fields or `Deserialize`) carries an inadmissible
    /// row. Refused before ANY index mutation, not merely logged afterwards.
    InadmissibleContract(String),
    /// A LogOnly op produced mutations: the contract's effect declaration and `apply` disagree.
    EffectViolation { kind: String, produced: usize },
    /// The op's kind is not one this contract declares. Returned, never panicked: `Executor` is
    /// public and `Engine::submit` is not the only caller, so a panic here would take down whatever
    /// task reached it (contract-commit-panics-on-undeclared-kind).
    UndeclaredKind { kind: String },
    /// The op's `target` is not the value its payload carries at the kind's declared `target_key`.
    /// `apply` namespaces every key on `target`, so committing such an op would move the state of a
    /// database the payload never named. Checked here as well as in `contract::validate`, because
    /// `Executor` is public: `Engine::submit` is not the only caller, and a binding that holds at
    /// one entry point only is not a binding (contract-target-not-bound-to-payload-db-id).
    TargetMismatch { target: String, expected: String },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::InadmissibleContract(d) => {
                write!(f, "contract is inadmissible: {d}")
            }
            ExecError::EffectViolation { kind, produced } => write!(
                f,
                "op kind {kind} is declared log-only but apply produced {produced} mutation(s)"
            ),
            ExecError::UndeclaredKind { kind } => {
                write!(f, "op kind {kind} is not declared by this contract")
            }
            ExecError::TargetMismatch { target, expected } => {
                write!(f, "target {target} is not the {expected} the payload names")
            }
        }
    }
}

/// Bound on a failure string recorded in an ack.
///
/// The failure text comes from the backend, and every byte of it is written to an append-only log, so
/// an unbounded string is an unbounded growth path. A longer message is cut rather than stored whole.
///
/// This caps ONE entry, not the log: the log is bounded by checkpointing and compaction
/// ([`crate::persist`]), which is a separate mechanism and the one that actually stops growth.
pub const MAX_ERROR_CHARS: usize = 256;

fn bounded(msg: &str) -> String {
    if msg.chars().count() <= MAX_ERROR_CHARS {
        return msg.to_string();
    }
    let mut out: String = msg.chars().take(MAX_ERROR_CHARS).collect();
    out.push_str("...(truncated)");
    out
}

/// One contract's runtime: its log, its index, and its ack history.
///
/// Not internally synchronized: the caller ([`crate::engine::Engine`]) holds the per-contract write
/// lock, so "single writer per surface" is a property of the engine's sharding, not of this struct.
///
/// Serializable, so a snapshot of the whole runtime is a value that can be written out and compared.
/// The durable copy is not this struct, though -- it is the segment files plus signed checkpoints in
/// [`crate::persist`], which is what survives a restart and what bounds the log. Serializing an
/// `Executor` is for inspection and reconciliation, never for durability.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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

    /// The entry most recently appended: what a caller persists after a commit. `None` only for a
    /// fresh executor that has never committed.
    pub fn last_entry(&self) -> Option<&LogEntry> {
        self.log.entries().last()
    }

    /// Rebuild from parts: the inverse of serialization, and the shape a restored log+index arrives
    /// in. `acks` is re-derived from the log rather than taken from the caller, so a restored
    /// executor's ack history can never disagree with the entries it holds.
    pub fn from_parts(contract: Contract, log: OpLog, index: Index) -> Executor {
        let acks = log.entries().iter().map(|e| e.ack.clone()).collect();
        Executor {
            contract,
            log,
            index,
            acks,
        }
    }

    pub fn index_hash(&self) -> String {
        index_hash(&self.index)
    }

    /// Execute one admitted op and commit it atomically: the ack is appended carrying the index hash
    /// produced BY this op, so a partially-applied transaction is not representable. Vitra prepends
    /// the ack to the tx and applies the batch atomically for the same reason (whitepaper 3.3).
    ///
    /// `ts_ms` and actor are stamped here, outside `apply`.
    ///
    /// `outcome` is the result of EXECUTING the op against the real backend, supplied by the caller
    /// that performed it, and it is what an ack's `success` records. `Err` means the op was admitted
    /// and executed and the EXECUTION FAILED: no mutation is applied -- a write that never landed
    /// did not change the database -- and the entry still goes into the log with `success: false`
    /// and the bounded reason. A refusal at the gate is a different thing and goes through
    /// [`Executor::commit_refusal`]. An ack that can only ever say `true` records ADMISSION, and a
    /// log like that asserts success for every write a wedged database never performed.
    pub fn commit(
        &mut self,
        op: Op,
        ts_ms: u64,
        outcome: Result<(), String>,
    ) -> Result<Ack, ExecError> {
        // Recheck the whole contract at the point of use: `Contract` has public fields and
        // derives `Deserialize`, so `declare` is not the only construction route.
        self.contract
            .assert_admissible()
            .map_err(|e| ExecError::InadmissibleContract(e.to_string()))?;
        let spec = self
            .contract
            .spec(&op.kind)
            .ok_or_else(|| ExecError::UndeclaredKind {
                kind: op.kind.clone(),
            })?;
        // The exact pair, again, on the row being committed.
        if spec.auth == AuthPolicy::OpenParticipation && spec.effect == Effect::StateWrite {
            return Err(ExecError::InadmissibleContract(format!(
                "op kind {} lets open participation write state",
                op.kind
            )));
        }
        // Every key this op writes is namespaced on `op.target`, so the target must be the identity
        // the payload itself names. `contract::validate` refuses this at the gate; refusing it here
        // too keeps the binding true for a caller that reaches the executor directly.
        if op.payload.get(&spec.target_key).map(|v| v.as_str()) != Some(op.target.as_str()) {
            return Err(ExecError::TargetMismatch {
                target: op.target.clone(),
                expected: op
                    .payload
                    .get(&spec.target_key)
                    .cloned()
                    .unwrap_or_else(|| spec.target_key.clone()),
            });
        }
        let mutations = apply(&self.index, &op);
        if spec.effect == Effect::LogOnly && !mutations.is_empty() {
            return Err(ExecError::EffectViolation {
                kind: op.kind.clone(),
                produced: mutations.len(),
            });
        }
        match outcome {
            Err(e) => Ok(self.record(op, ts_ms, false, Some(bounded(&e)))),
            Ok(()) => {
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
                Ok(self.record(op, ts_ms, true, None))
            }
        }
    }

    /// Record a refused op as a failed ack: the log stays append-only and complete, and the refusal
    /// is itself auditable. Nothing here decides authorization -- the caller already did.
    ///
    /// This is the GATE's refusal path (undeclared kind, bad payload, no owning tenant, and an
    /// executor-level refusal such as [`ExecError::EffectViolation`]). A failure of the execution
    /// itself goes through [`Executor::commit`] with `Err`, so the two never collapse into one.
    pub fn commit_refusal(&mut self, op: Op, ts_ms: u64, error: &str) -> Ack {
        self.record(op, ts_ms, false, Some(bounded(error)))
    }

    /// Append one op and the ack the executor produced for it. The single writer of log entries, so
    /// the seq, the index hash and the chain head an ack carries always describe THIS entry.
    fn record(&mut self, mut op: Op, ts_ms: u64, success: bool, error: Option<String>) -> Ack {
        // seq is assigned by the log, before the ack exists: it is the log's, not the caller's.
        op.seq = self.log.next_seq();
        let log_head = self.log.head_after(&op);
        let ack = Ack {
            seq: op.seq,
            ts_ms,
            success,
            error,
            index_hash: index_hash(&self.index),
            log_head,
        };
        self.log.append(op, ack.clone());
        self.acks.push(ack.clone());
        ack
    }
}
