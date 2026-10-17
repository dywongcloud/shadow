//! The concurrency model: one writer per contract, sharded so unrelated contracts never contend.
//!
//! Vitra serializes at the executor -- many participants propose, one executor applies in log order
//! (whitepaper 2.1). The contended resource here is the same: a contract's (log, index) pair. Ops on
//! DIFFERENT contracts are independent and must not serialize, so executors live in `shards` and a
//! contract is addressed by hashing its id to a shard.
//!
//! Where contention actually lives, stated rather than assumed:
//!   1. The per-shard `RwLock`. Ops on two contracts that hash to the same shard serialize even
//!      though they are logically independent -- that is the real cost, and it is why `shards` is
//!      configurable and measured, not fixed.
//!   2. Log append + index apply inside the lock. Both are O(1)-ish per op (BTreeMap insert, Vec
//!      push), so the critical section is short; nothing in it does I/O, allocates unboundedly or
//!      awaits.
//!   3. NOT on reads: [`Engine::replay_contract`] takes the shard read lock, so monitors audit
//!      concurrently with writers. It is CPU-bound and long, so callers must run it off the serving
//!      runtime (`tokio::task::block_in_place` + `spawn_blocking`), per the repo's bulkhead rule.
//!
//! There is no global lock anywhere: no cross-contract ordering is claimed, so none is paid for.

use crate::audit::{replay, Breach};
use crate::contract::{validate, Caller, Contract, OpError};
use crate::exec::{index_hash, Executor, ExecError, Index};
use crate::oplog::{Op, OpLog};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default shard count. More shards = less false sharing between unrelated contracts.
pub const DEFAULT_SHARDS: usize = 16;

type Shard = RwLock<BTreeMap<String, Executor>>;

/// A set of contract executors. Clone is cheap and shares state (each shard is an `Arc`).
#[derive(Clone)]
pub struct Engine {
    shards: Arc<Vec<Shard>>,
}

impl Engine {
    pub fn new(shards: usize) -> Engine {
        let shards = shards.max(1);
        let mut v = Vec::with_capacity(shards);
        for _ in 0..shards {
            v.push(RwLock::new(BTreeMap::new()));
        }
        Engine {
            shards: Arc::new(v),
        }
    }

    pub fn shards(&self) -> usize {
        self.shards.len()
    }

    fn shard(&self, contract_id: &str) -> &Shard {
        // FNV-1a: stable across processes, which matters because the SHARDING must not change which
        // executor holds a contract between runs.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in contract_id.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        &self.shards[(h as usize) % self.shards.len()]
    }

    /// Ensure a contract has an executor. Idempotent; takes the shard write lock once.
    pub async fn register(&self, contract: Contract) {
        let id = contract.id.clone();
        let mut shard = self.shard(&id).write().await;
        shard.entry(id).or_insert_with(|| Executor::new(contract));
    }

    /// The one mutation entry point: validate against the contract, then commit under the
    /// contract's single writer.
    ///
    /// Authorization is NOT computed here. `caller` must already carry the tenant the platform bound
    /// and the verdict of its ownership check; [`validate`] refuses to proceed without them. A
    /// caller that has not been through the platform's gate cannot construct an admissible `Caller`
    /// with `owns_target: true` in any path this crate exposes.
    pub async fn submit(
        &self,
        contract: &Contract,
        caller: Caller,
        kind: &str,
        payload: BTreeMap<String, String>,
        target: &str,
        ts_ms: u64,
    ) -> Result<SubmitResult, OpError> {
        match validate(contract, &caller, kind, &payload) {
            Ok(_) => {}
            Err(e) => {
                // The refusal is recorded too: an append-only log that only holds successes is not
                // an audit trail.
                let mut shard = self.shard(&contract.id).write().await;
                let ex = shard
                    .entry(contract.id.clone())
                    .or_insert_with(|| Executor::new(contract.clone()));
                let op = Op {
                    seq: 0,
                    contract: contract.id.clone(),
                    tenant: caller.tenant.clone(),
                    actor: caller.actor.clone(),
                    target: target.to_string(),
                    kind: kind.to_string(),
                    payload,
                };
                let ack = ex.commit_refusal(op, ts_ms, &e.to_string());
                return Ok(SubmitResult {
                    committed: false,
                    error: Some(e.to_string()),
                    seq: ack.seq,
                    index_hash: ex.index_hash(),
                    log_head: ack.log_head,
                });
            }
        }
        let mut shard = self.shard(&contract.id).write().await;
        let ex = shard
            .entry(contract.id.clone())
            .or_insert_with(|| Executor::new(contract.clone()));
        let op = Op {
            seq: 0,
            contract: contract.id.clone(),
            tenant: caller.tenant.clone(),
            actor: caller.actor.clone(),
            target: target.to_string(),
            kind: kind.to_string(),
            payload,
        };
        match ex.commit(op, ts_ms) {
            Ok(ack) => Ok(SubmitResult {
                committed: true,
                error: None,
                seq: ack.seq,
                index_hash: ex.index_hash(),
                log_head: ack.log_head,
            }),
            Err(ExecError::EffectViolation { kind, produced }) => {
                let msg = ExecError::EffectViolation { kind, produced }.to_string();
                let seq = ex.log.next_seq();
                Ok(SubmitResult {
                    committed: false,
                    error: Some(msg),
                    seq,
                    index_hash: ex.index_hash(),
                    log_head: ex.log.head().to_string(),
                })
            }
        }
    }

    /// Snapshot one contract's log under the shard read lock: writers on OTHER contracts in the
    /// same shard still proceed, writers on THIS contract wait.
    pub async fn snapshot_log(&self, contract_id: &str) -> Option<OpLog> {
        let shard = self.shard(contract_id).read().await;
        shard.get(contract_id).map(|e| e.log.clone())
    }

    pub async fn index_hash(&self, contract_id: &str) -> Option<String> {
        let shard = self.shard(contract_id).read().await;
        shard.get(contract_id).map(|e| e.index_hash())
    }

    /// Monitor-side full audit of one contract. Returns the recomputed index hash or the first
    /// breach. CPU-bound over the whole log: call it from a blocking/bulkhead context, never from a
    /// request-serving worker.
    pub async fn audit(&self, contract: &Contract) -> Result<Index, Breach> {
        let log = self.snapshot_log(&contract.id).await.unwrap_or_default();
        replay(contract, &log).map_err(|b| b)
    }
}

/// What a submit produced.
#[derive(Clone, Debug)]
pub struct SubmitResult {
    pub committed: bool,
    pub error: Option<String>,
    pub seq: u64,
    pub index_hash: String,
    pub log_head: String,
}

/// Index hash of a freshly replayed log, used by the audit binary to compare two independent
/// replays of the same history.
pub fn replay_hash(contract: &Contract, log: &OpLog) -> Result<String, Breach> {
    Ok(index_hash(&replay(contract, log)?))
}
