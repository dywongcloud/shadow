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
//!   2. Log append + index apply inside the lock, and -- when the engine has a store -- one
//!      `write_all` plus one `sync_data` of a single JSON line. The I/O is deliberately INSIDE the
//!      critical section: an entry has to be on disk before the caller is told it committed, so
//!      moving it out would trade durability for latency. It is one small append and one sync per
//!      op, never a rewrite; the expensive half of persistence (Merkle root, sign, checkpoint
//!      write, reclaim) happens once per window, and `checkpoint_every` is the knob for how often.
//!   3. NOT on reads: [`Engine::replay_contract`] takes the shard read lock, so monitors audit
//!      concurrently with writers. It is CPU-bound and long, so callers must run it off the serving
//!      runtime (`tokio::task::block_in_place` + `spawn_blocking`), per the repo's bulkhead rule.
//!
//! There is no global lock anywhere: no cross-contract ordering is claimed, so none is paid for.

use crate::audit::{replay, Breach};
use crate::checkpoint::SignedCheckpoint;
use crate::contract::{validate, Caller, Contract, OpError};
use crate::exec::{index_hash, Executor, Index};
use crate::oplog::{Op, OpLog};
use crate::persist::{PersistConfig, Store, StoreError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Default shard count. More shards = less false sharing between unrelated contracts.
pub const DEFAULT_SHARDS: usize = 16;

type Shard = RwLock<BTreeMap<String, Executor>>;

/// Where a contract's log is kept, and how. Shared by every shard: the store is per contract, but the
/// settings and the signer are the node's.
struct Persist {
    dir: PathBuf,
    cfg: PersistConfig,
}

impl Persist {
    /// The directory for one contract. A contract id is a platform constant, not tenant input, but it
    /// is still reduced to a safe single path component here: this crate never builds a filesystem
    /// path out of a string it has not sanitized.
    fn dir_for(&self, contract_id: &str) -> PathBuf {
        let mut name = String::with_capacity(contract_id.len());
        for c in contract_id.chars() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                name.push(c);
            } else {
                name.push('_');
            }
        }
        if name.is_empty() || name == "." || name == ".." {
            name = "contract".to_string();
        }
        self.dir.join(name)
    }
}

/// A set of contract executors. Clone is cheap and shares state (each shard is an `Arc`).
#[derive(Clone)]
pub struct Engine {
    shards: Arc<Vec<Shard>>,
    persist: Option<Arc<Persist>>,
    /// One writer per contract, held under that contract's shard lock. Kept apart from the executor
    /// so an `Executor` stays a plain serializable value.
    stores: Arc<RwLock<BTreeMap<String, Arc<Mutex<Store>>>>>,
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
            persist: None,
            stores: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// An engine whose logs are durable: every op is written to disk before it is reported, and every
    /// `checkpoint_every` entries are attested by a signed checkpoint that supersedes the entries
    /// before it.
    ///
    /// `cfg.signer` is required. `None` is refused at [`Engine::register`] rather than degraded to
    /// unsigned checkpoints: a checkpoint nobody signed asserts nothing.
    pub fn open(shards: usize, dir: PathBuf, cfg: PersistConfig) -> Engine {
        let mut e = Engine::new(shards);
        e.persist = Some(Arc::new(Persist { dir, cfg }));
        e
    }

    pub fn shards(&self) -> usize {
        self.shards.len()
    }

    /// The directory one contract's log lives in, when this engine persists.
    pub fn store_dir(&self, contract_id: &str) -> Option<PathBuf> {
        self.persist.as_ref().map(|p| p.dir_for(contract_id))
    }

    pub fn persists(&self) -> bool {
        self.persist.is_some()
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

    /// Make sure this contract has an executor, creating it (and, when this engine persists,
    /// rebuilding its log and index from disk) if it does not.
    ///
    /// The rebuild is the restart path: it resumes from the newest VERIFIED checkpoint and applies
    /// the tail, so a restart lands on the same index hash the process had before it stopped. It is
    /// I/O, and it happens once per contract -- under this contract's shard write lock, never on a
    /// path that already holds a lock for something else.
    async fn ensure(
        &self,
        contract: &Contract,
        shard: &mut BTreeMap<String, Executor>,
    ) -> Result<(), StoreError> {
        if shard.contains_key(&contract.id) {
            return Ok(());
        }
        let ex = match self.persist.as_ref() {
            None => Executor::new(contract.clone()),
            Some(p) => {
                let opened = Store::open(
                    p.dir_for(&contract.id),
                    &contract.id,
                    contract.version,
                    p.cfg.clone(),
                )
                .await?;
                // The checkpoint's index and the tail after it have to be replayed into one index --
                // and replaying is also what CHECKS the tail against the acks it carries. Restoring
                // the checkpoint's index alone would publish a state the stored entries do not
                // support, and a restart would then disagree with every later audit.
                let index = crate::audit::replay_from(
                    contract,
                    &opened.log,
                    opened.checkpoint.as_ref().map(|c| &c.checkpoint),
                )
                .map_err(|e| StoreError::BrokenChain {
                    detail: e.to_string(),
                })?;
                self.stores.write().await.insert(
                    contract.id.clone(),
                    Arc::new(Mutex::new(opened.store)),
                );
                Executor::from_parts(contract.clone(), opened.log, index)
            }
        };
        shard.insert(contract.id.clone(), ex);
        Ok(())
    }

    /// Ensure a contract has an executor. Idempotent; takes the shard write lock once.
    pub async fn register(&self, contract: Contract) -> Result<(), StoreError> {
        let id = contract.id.clone();
        let mut shard = self.shard(&id).write().await;
        self.ensure(&contract, &mut shard).await
    }

    /// Write one committed entry to disk and apply whatever the step says about compaction.
    ///
    /// A persistence failure is REPORTED, never swallowed and never allowed to look like a committed,
    /// recorded op: durability is the whole point of the log, so a result carrying `persist_error` is
    /// an op the log cannot vouch for.
    async fn record(
        &self,
        contract_id: &str,
        ex: &mut Executor,
        ts_ms: u64,
    ) -> Option<String> {
        let persist = match self.persist.as_ref() {
            Some(p) => p,
            None => return None,
        };
        let entry = match ex.last_entry() {
            Some(e) => e.clone(),
            None => return Some("commit produced no entry to persist".to_string()),
        };
        let handle = {
            let guard = self.stores.read().await;
            guard.get(contract_id).cloned()
        };
        let handle = match handle {
            Some(h) => h,
            None => {
                // The executor exists but its store does not: recreate it from the same settings
                // rather than silently dropping entries.
                match Store::open(
                    persist.dir_for(contract_id),
                    contract_id,
                    ex.contract.version,
                    persist.cfg.clone(),
                )
                .await
                {
                    Ok(o) => {
                        let h = Arc::new(Mutex::new(o.store));
                        self.stores
                            .write()
                            .await
                            .insert(contract_id.to_string(), h.clone());
                        h
                    }
                    Err(e) => return Some(format!("store reopen failed: {e}")),
                }
            }
        };
        let mut store = handle.lock().await;
        match store.append(&entry, &ex.index, ts_ms).await {
            Ok(step) => {
                if let (Some(to), Some(prev)) = (step.truncate_to, step.truncate_prev) {
                    if let Err(e) = ex.log.truncate_front(to, &prev) {
                        return Some(format!("compaction refused: {e}"));
                    }
                }
                None
            }
            Err(e) => Some(e.to_string()),
        }
    }

    /// Close the current window now, even though it is not full: the pre-roll and shutdown path,
    /// where the tail matters more than the window being tidy. Also the only way an in-memory tail
    /// becomes replayable-from-a-checkpoint before it is long enough to close on its own.
    pub async fn checkpoint_now(
        &self,
        contract_id: &str,
        ts_ms: u64,
    ) -> Result<Option<SignedCheckpoint>, StoreError> {
        let handle = {
            let guard = self.stores.read().await;
            guard.get(contract_id).cloned()
        };
        let handle = match handle {
            Some(h) => h,
            None => return Ok(None),
        };
        let mut shard = self.shard(contract_id).write().await;
        let ex = match shard.get_mut(contract_id) {
            Some(e) => e,
            None => return Ok(None),
        };
        let (entry, index) = match (ex.last_entry().cloned(), ex.index.clone()) {
            (Some(e), i) => (e, i),
            _ => return Ok(None),
        };
        let mut store = handle.lock().await;
        let cp = store.checkpoint_now(&entry, &index, ts_ms).await?;
        if let Some(c) = &cp {
            if let Err(e) = ex.log.truncate_front(c.checkpoint.window_end, &c.checkpoint.log_head) {
                return Err(StoreError::BrokenChain {
                    detail: format!("compaction after forced checkpoint refused: {e}"),
                });
            }
        }
        Ok(cp)
    }

    /// The one mutation entry point: validate against the contract, then commit under the
    /// contract's single writer.
    ///
    /// Authorization is NOT computed here. `caller` must already carry the tenant the platform bound
    /// and the verdict of its ownership check; [`validate`] refuses to proceed without them. A
    /// caller that has not been through the platform's gate cannot construct an admissible `Caller`
    /// with `owns_target: true` in any path this crate exposes.
    ///
    /// `outcome` is the result of EXECUTING the operation against the real backend, and it is
    /// required: an `Err` is recorded as an entry with `success: false` and no mutation applied, so
    /// the log says the write failed instead of asserting a success the database never produced.
    /// `SubmitResult::committed` is `true` only when the operation was admitted AND executed.
    ///
    /// Every outcome -- committed, refused at the gate, refused by the executor -- is written to the
    /// log before this returns, when the engine has a store. A log that only holds the successful ops
    /// is not an audit trail.
    pub async fn submit(
        &self,
        contract: &Contract,
        caller: Caller,
        kind: &str,
        payload: BTreeMap<String, String>,
        target: &str,
        ts_ms: u64,
        outcome: Result<(), String>,
    ) -> Result<SubmitResult, OpError> {
        let op = Op {
            seq: 0,
            contract: contract.id.clone(),
            tenant: caller.tenant.clone(),
            actor: caller.actor.clone(),
            target: target.to_string(),
            kind: kind.to_string(),
            payload,
        };
        let mut shard = self.shard(&contract.id).write().await;
        if let Err(e) = self.ensure(contract, &mut shard).await {
            // The log could not be opened (an unverifiable stored checkpoint, or an unwritable
            // directory), so nothing this op does could be recorded. Refusing is the only honest
            // answer: an unrecorded op is exactly the silence reconciliation exists to detect.
            return Err(OpError::Store(e.to_string()));
        }
        let ex = shard
            .get_mut(&contract.id)
            .expect("executor ensured above");
        match validate(contract, &caller, kind, &op.payload, target) {
            Ok(_) => {}
            Err(e) => {
                // The refusal is recorded too: an append-only log that only holds successes is not
                // an audit trail.
                let ack = ex.commit_refusal(op, ts_ms, &e.to_string());
                let persist_error = self.record(&contract.id, ex, ts_ms).await;
                return Ok(SubmitResult {
                    committed: false,
                    error: Some(e.to_string()),
                    seq: ack.seq,
                    index_hash: ex.index_hash(),
                    log_head: ack.log_head,
                    persist_error,
                });
            }
        }
        // Kept so an op the EXECUTOR refuses is still recorded: `commit` consumes the op it is
        // given, and an op that vanished here would leave the log silent about it.
        let refused = op.clone();
        match ex.commit(op, ts_ms, outcome) {
            Ok(ack) => {
                let persist_error = self.record(&contract.id, ex, ts_ms).await;
                Ok(SubmitResult {
                    committed: ack.success,
                    error: ack.error.clone(),
                    seq: ack.seq,
                    index_hash: ex.index_hash(),
                    log_head: ack.log_head,
                    persist_error,
                })
            }
            Err(e) => {
                // Same rule as the gate's refusal branch: an op that reached the single writer is
                // part of the history whether or not it was applied.
                let msg = e.to_string();
                let ack = ex.commit_refusal(refused, ts_ms, &msg);
                let persist_error = self.record(&contract.id, ex, ts_ms).await;
                Ok(SubmitResult {
                    committed: false,
                    error: Some(msg),
                    seq: ack.seq,
                    index_hash: ex.index_hash(),
                    log_head: ack.log_head,
                    persist_error,
                })
            }
        }
    }

    /// Flush every open segment without closing a window: the timer-driven half of durability when
    /// [`PersistConfig::sync`] is off.
    ///
    /// Returns the number of contracts whose segment was actually synced. An error is reported per
    /// contract and never stops the rest: one unwritable log directory must not keep another
    /// contract's entries unsynced. `Ok(0)` for an engine with no store -- there is nothing to
    /// flush, and that is the honest answer rather than a silent success.
    pub async fn flush(&self) -> Result<usize, Vec<(String, StoreError)>> {
        let handles: Vec<(String, Arc<Mutex<Store>>)> = {
            let guard = self.stores.read().await;
            guard
                .iter()
                .map(|(id, h)| (id.clone(), h.clone()))
                .collect()
        };
        let mut synced = 0usize;
        let mut failed = Vec::new();
        for (id, handle) in handles {
            let mut store = handle.lock().await;
            match store.sync().await {
                Ok(()) => synced += 1,
                Err(e) => failed.push((id, e)),
            }
        }
        if failed.is_empty() {
            Ok(synced)
        } else {
            Err(failed)
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
///
/// `persist_error` is separate from `error` on purpose. `error` is about the OPERATION; `persist_error`
/// is about the RECORD of it. An op that executed but could not be written to the log has
/// `committed: true` and a non-empty `persist_error`, and that combination is a durable-store fault a
/// caller must surface -- collapsing the two would either hide it or claim an op failed when it did
/// not.
#[derive(Clone, Debug)]
pub struct SubmitResult {
    pub committed: bool,
    pub error: Option<String>,
    pub seq: u64,
    pub index_hash: String,
    pub log_head: String,
    /// Non-empty when the entry could not be made durable. Never silently dropped.
    pub persist_error: Option<String>,
}

/// Index hash of a freshly replayed log, used by the audit binary to compare two independent
/// replays of the same history.
pub fn replay_hash(contract: &Contract, log: &OpLog) -> Result<String, Breach> {
    Ok(index_hash(&replay(contract, log)?))
}
