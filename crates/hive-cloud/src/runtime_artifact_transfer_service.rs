use anyhow::{bail, Context};
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use crate::runtime_artifact_transfer_fs::TransferFs;
use crate::runtime_artifact_transfer_store::{
    chunk_sha256, decode_record, encode_record, ChunkSeal, TransferRecord,
};
use crate::runtime_artifact_transfer_wire::{
    self as wire, BeginRequest, CommitRequest, ReplyCode, TransferKey, TransferReply,
    TransferRequest, TransferState, MAX_CHUNK_BYTES,
};

/// Domain separator for the canonical readiness-receipt digest a receiver
/// stamps into its durable Prepared record and reply. Sender and receiver
/// compare this exact value at commit time.
const READINESS_DOMAIN: &[u8] = b"hive-deployment-readiness-v1\0";

/// Durably persist the platform's deployment state after a publish. Bound
/// once from `CloudState::new` (the service is constructed before the state
/// `Arc` exists, so the hook arrives immediately after); a commit with no
/// bound hook is refused rather than publishing an unpersistable record.
pub type PersistFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// One canonical digest over a gateway readiness receipt.
pub fn readiness_receipt_sha256(
    receipt: &fluid_gateway::DeploymentReadinessReceipt,
) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(receipt).context("encode deployment readiness receipt")?;
    let mut hash = Sha256::new();
    hash.update(READINESS_DOMAIN);
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

const DEFAULT_QUEUE_CAPACITY: usize = 32;
const DEFAULT_QUEUE_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_ACTIVE_NODE: usize = 32;
const DEFAULT_ACTIVE_TENANT: usize = 4;
const DEFAULT_ACTIVE_COORDINATOR: usize = 8;
const DEFAULT_LEASE_MS: u64 = 2 * 60 * 60 * 1000;
const DEFAULT_DISK_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct TransferBinding {
    pub project: String,
    pub project_incarnation: fluid_core::ProjectIncarnation,
    pub tenant: String,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct TransferStats {
    pub enabled: bool,
    pub active: usize,
    pub receiving: usize,
    pub finalizing: usize,
    pub materialized: usize,
    pub prepared: usize,
    pub committed: usize,
    pub aborted: usize,
    pub failed: usize,
    pub queued_bytes: u64,
    pub accepted_jobs: u64,
    pub refused_jobs: u64,
    pub recovered_transactions: u64,
    pub completed_chunks: u64,
    pub duplicate_chunks: u64,
}

pub struct TransferService {
    sender: Option<mpsc::SyncSender<WorkerMessage>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    records: Arc<RwLock<BTreeMap<String, TransferRecord>>>,
    admission: tokio::sync::Mutex<()>,
    accepting: AtomicBool,
    queued_bytes: Arc<AtomicU64>,
    max_queue_bytes: u64,
    boot_nonce: String,
    disabled_reason: Option<String>,
    shutdown_error: Arc<Mutex<Option<String>>>,
    persist: Arc<Mutex<Option<PersistFn>>>,
    accepted_jobs: Arc<AtomicU64>,
    refused_jobs: Arc<AtomicU64>,
    recovered_transactions: u64,
    completed_chunks: Arc<AtomicU64>,
    duplicate_chunks: Arc<AtomicU64>,
}

struct Limits {
    active_node: usize,
    active_tenant: usize,
    active_coordinator: usize,
    lease_ms: u64,
    disk_reserve_bytes: u64,
}

struct Job {
    request: TransferRequest,
    _lifecycle: Option<tokio::sync::OwnedMutexGuard<()>>,
    charge: u64,
    persist: Option<PersistFn>,
    reply: tokio::sync::oneshot::Sender<TransferReply>,
}

enum WorkerMessage {
    Execute(Job),
    Shutdown,
}

struct WorkerCounters {
    accepted_jobs: Arc<AtomicU64>,
    refused_jobs: Arc<AtomicU64>,
    completed_chunks: Arc<AtomicU64>,
    duplicate_chunks: Arc<AtomicU64>,
}

impl TransferService {
    pub fn open(
        store_root: std::path::PathBuf,
        node_name: String,
        boot_nonce: String,
        gw: Arc<fluid_gateway::Gateway>,
    ) -> anyhow::Result<Arc<Self>> {
        validate_boot_nonce(&boot_nonce)?;
        let fs = TransferFs::open(&store_root)?;
        let records = Arc::new(RwLock::new(recover_records(&fs, &boot_nonce)?));
        let recovered_transactions = records.read().len() as u64;
        let queue_capacity = env_usize(
            "HIVE_ARTIFACT_TRANSFER_QUEUE",
            DEFAULT_QUEUE_CAPACITY,
            1,
            256,
        );
        let max_queue_bytes = env_u64(
            "HIVE_ARTIFACT_TRANSFER_QUEUE_BYTES",
            DEFAULT_QUEUE_BYTES,
            MAX_CHUNK_BYTES as u64,
            2 * 1024 * 1024 * 1024,
        );
        let limits = Limits {
            active_node: env_usize(
                "HIVE_ARTIFACT_TRANSFER_ACTIVE_NODE",
                DEFAULT_ACTIVE_NODE,
                1,
                1024,
            ),
            active_tenant: env_usize(
                "HIVE_ARTIFACT_TRANSFER_ACTIVE_TENANT",
                DEFAULT_ACTIVE_TENANT,
                1,
                128,
            ),
            active_coordinator: env_usize(
                "HIVE_ARTIFACT_TRANSFER_ACTIVE_COORDINATOR",
                DEFAULT_ACTIVE_COORDINATOR,
                1,
                256,
            ),
            lease_ms: env_u64(
                "HIVE_ARTIFACT_TRANSFER_LEASE_MS",
                DEFAULT_LEASE_MS,
                60_000,
                24 * 60 * 60 * 1000,
            ),
            disk_reserve_bytes: env_u64(
                "HIVE_ARTIFACT_TRANSFER_DISK_RESERVE_BYTES",
                DEFAULT_DISK_RESERVE_BYTES,
                64 * 1024 * 1024,
                64 * 1024 * 1024 * 1024,
            ),
        };
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let queued_bytes = Arc::new(AtomicU64::new(0));
        let accepted_jobs = Arc::new(AtomicU64::new(0));
        let refused_jobs = Arc::new(AtomicU64::new(0));
        let completed_chunks = Arc::new(AtomicU64::new(0));
        let duplicate_chunks = Arc::new(AtomicU64::new(0));
        let shutdown_error = Arc::new(Mutex::new(None));
        let counters = WorkerCounters {
            accepted_jobs: accepted_jobs.clone(),
            refused_jobs: refused_jobs.clone(),
            completed_chunks: completed_chunks.clone(),
            duplicate_chunks: duplicate_chunks.clone(),
        };
        let worker_records = records.clone();
        let worker_queue_bytes = queued_bytes.clone();
        let worker_boot_nonce = boot_nonce.clone();
        let worker_node_name = node_name.clone();
        let worker_shutdown_error = shutdown_error.clone();
        let worker = std::thread::Builder::new()
            .name("hive-artifact-transfer".into())
            .spawn(move || {
                worker_loop(
                    fs,
                    receiver,
                    worker_records,
                    worker_queue_bytes,
                    worker_node_name,
                    worker_boot_nonce,
                    gw,
                    limits,
                    counters,
                    worker_shutdown_error,
                )
            })
            .context("spawn runtime artifact transfer worker")?;
        Ok(Arc::new(Self {
            sender: Some(sender),
            worker: Mutex::new(Some(worker)),
            records,
            admission: tokio::sync::Mutex::new(()),
            accepting: AtomicBool::new(true),
            queued_bytes,
            max_queue_bytes,
            boot_nonce,
            disabled_reason: None,
            shutdown_error,
            persist: Arc::new(Mutex::new(None)),
            accepted_jobs,
            refused_jobs,
            recovered_transactions,
            completed_chunks,
            duplicate_chunks,
        }))
    }

    pub fn disabled(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            sender: None,
            worker: Mutex::new(None),
            records: Arc::new(RwLock::new(BTreeMap::new())),
            admission: tokio::sync::Mutex::new(()),
            accepting: AtomicBool::new(false),
            queued_bytes: Arc::new(AtomicU64::new(0)),
            max_queue_bytes: 0,
            boot_nonce: String::new(),
            disabled_reason: Some(reason.into()),
            shutdown_error: Arc::new(Mutex::new(None)),
            persist: Arc::new(Mutex::new(None)),
            accepted_jobs: Arc::new(AtomicU64::new(0)),
            refused_jobs: Arc::new(AtomicU64::new(0)),
            recovered_transactions: 0,
            completed_chunks: Arc::new(AtomicU64::new(0)),
            duplicate_chunks: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Bind the durable platform persistence hook. Called exactly once, right
    /// after the owning `CloudState` `Arc` exists; until then every Commit is
    /// refused rather than publishing a Ready record a restart would lose.
    pub fn bind_persist(&self, persist: PersistFn) {
        *self.persist.lock() = Some(persist);
    }

    pub fn binding(&self, key: &TransferKey) -> Option<TransferBinding> {
        let records = self.records.read();
        let record = records.get(&key.transaction_id)?;
        if record.begin.key != *key {
            return None;
        }
        Some(TransferBinding {
            project: record.begin.project.clone(),
            project_incarnation: record.begin.project_incarnation,
            tenant: record.begin.key.tenant.clone(),
        })
    }

    pub async fn dispatch(
        &self,
        request: TransferRequest,
        lifecycle: tokio::sync::OwnedMutexGuard<()>,
    ) -> TransferReply {
        let admission = self.admission.lock().await;
        let key = request.key().clone();
        if !self.accepting.load(Ordering::Acquire) {
            let reason = self
                .disabled_reason
                .as_deref()
                .unwrap_or("runtime artifact transfer service is shutting down");
            return TransferReply::error(ReplyCode::Failed, Some(&key), reason);
        }
        let charge = request_charge(&request);
        if !reserve(&self.queued_bytes, self.max_queue_bytes, charge) {
            self.refused_jobs.fetch_add(1, Ordering::Relaxed);
            return TransferReply::error(
                ReplyCode::QueueFull,
                Some(&key),
                "runtime artifact transfer byte queue is full",
            );
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let job = Job {
            request,
            _lifecycle: Some(lifecycle),
            charge,
            persist: self.persist.lock().clone(),
            reply: reply_tx,
        };
        let Some(sender) = &self.sender else {
            release(&self.queued_bytes, charge);
            return TransferReply::error(
                ReplyCode::Failed,
                Some(&key),
                self.disabled_reason
                    .as_deref()
                    .unwrap_or("runtime artifact transfer service is disabled"),
            );
        };
        match sender.try_send(WorkerMessage::Execute(job)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(message))
            | Err(mpsc::TrySendError::Disconnected(message)) => {
                if let WorkerMessage::Execute(job) = message {
                    release(&self.queued_bytes, job.charge);
                }
                self.refused_jobs.fetch_add(1, Ordering::Relaxed);
                return TransferReply::error(
                    ReplyCode::QueueFull,
                    Some(&key),
                    "runtime artifact transfer worker queue is unavailable",
                );
            }
        }
        drop(admission);
        match reply_rx.await {
            Ok(reply) => reply,
            Err(_) => TransferReply::error(
                ReplyCode::Internal,
                Some(&key),
                "runtime artifact transfer worker stopped before replying",
            ),
        }
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let _admission = self.admission.lock().await;
        self.accepting.store(false, Ordering::Release);
        if self.worker.lock().is_none() {
            return match self.shutdown_error.lock().clone() {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => Ok(()),
            };
        }

        let enqueue_error = if let Some(sender) = &self.sender {
            let sender = sender.clone();
            tokio::task::spawn_blocking(move || sender.send(WorkerMessage::Shutdown))
                .await
                .context("join transfer shutdown enqueue")?
                .err()
                .map(|error| format!("enqueue transfer worker shutdown: {error}"))
        } else {
            None
        };

        let handle = self.worker.lock().take();
        let join_error = if let Some(handle) = handle {
            match tokio::task::spawn_blocking(move || handle.join()).await {
                Ok(Ok(())) => None,
                Ok(Err(_)) => Some("runtime artifact transfer worker panicked".to_string()),
                Err(error) => Some(format!(
                    "join runtime artifact transfer worker task: {error}"
                )),
            }
        } else {
            None
        };
        let sync_error = self.shutdown_error.lock().clone();
        let error = enqueue_error.or(join_error).or(sync_error);
        *self.shutdown_error.lock() = error.clone();
        match error {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }

    pub fn stats(&self) -> TransferStats {
        let records = self.records.read();
        let mut stats = TransferStats {
            enabled: self.disabled_reason.is_none(),
            queued_bytes: self.queued_bytes.load(Ordering::Relaxed),
            accepted_jobs: self.accepted_jobs.load(Ordering::Relaxed),
            refused_jobs: self.refused_jobs.load(Ordering::Relaxed),
            recovered_transactions: self.recovered_transactions,
            completed_chunks: self.completed_chunks.load(Ordering::Relaxed),
            duplicate_chunks: self.duplicate_chunks.load(Ordering::Relaxed),
            ..TransferStats::default()
        };
        for record in records.values() {
            match record.state {
                TransferState::Receiving => stats.receiving += 1,
                TransferState::Finalizing => stats.finalizing += 1,
                TransferState::Materialized => stats.materialized += 1,
                TransferState::Prepared => stats.prepared += 1,
                TransferState::Committed => stats.committed += 1,
                TransferState::Aborted => stats.aborted += 1,
                TransferState::Failed => stats.failed += 1,
            }
            if !record.state.terminal() {
                stats.active += 1;
            }
        }
        stats
    }

    pub fn boot_nonce(&self) -> &str {
        &self.boot_nonce
    }

    /// Whether this receiver initialized and is serving transfer operations.
    pub fn enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    fs: TransferFs,
    receiver: mpsc::Receiver<WorkerMessage>,
    records: Arc<RwLock<BTreeMap<String, TransferRecord>>>,
    queued_bytes: Arc<AtomicU64>,
    node_name: String,
    boot_nonce: String,
    gw: Arc<fluid_gateway::Gateway>,
    limits: Limits,
    counters: WorkerCounters,
    shutdown_error: Arc<Mutex<Option<String>>>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime artifact transfer worker runtime");
    // Prepared hidden candidates are PROCESS-LOCAL by design: their armed Drop
    // is the rollback mechanism, so the handles live here — owned by the one
    // worker that serializes every transfer mutation — and never in durable
    // state. A restart recovers `Prepared` records as `Materialized`
    // (`recover_records`), so this map and the durable store can never
    // disagree across a boot.
    let mut prepared: BTreeMap<String, fluid_gateway::StagedDeployment> = BTreeMap::new();
    while let Ok(message) = receiver.recv() {
        match message {
            WorkerMessage::Execute(mut job) => {
                release(&queued_bytes, job.charge);
                counters.accepted_jobs.fetch_add(1, Ordering::Relaxed);
                let key = job.request.key().clone();
                let persist = job.persist.take();
                let reply = handle_request(
                    &runtime,
                    &fs,
                    &records,
                    &mut prepared,
                    &gw,
                    persist,
                    &node_name,
                    &boot_nonce,
                    &limits,
                    &counters,
                    job.request,
                )
                .unwrap_or_else(|error| {
                    counters.refused_jobs.fetch_add(1, Ordering::Relaxed);
                    TransferReply::error(ReplyCode::Internal, Some(&key), format!("{error:#}"))
                });
                let _ = job.reply.send(reply);
                job._lifecycle.take();
            }
            WorkerMessage::Shutdown => {
                drop_prepared_candidates(&runtime, &fs, &records, &boot_nonce, &mut prepared);
                if let Err(error) = fs.sync_store() {
                    *shutdown_error.lock() = Some(format!(
                        "final runtime artifact transfer store sync failed: {error:#}"
                    ));
                }
                break;
            }
        }
    }
    // A closed channel without a Shutdown message still rolls hidden
    // candidates back before the runtime is dropped.
    drop_prepared_candidates(&runtime, &fs, &records, &boot_nonce, &mut prepared);
}

/// Drop every retained staged handle inside a live runtime context (its Drop
/// spawns pool unregistration) and downgrade the matching durable records so
/// no `Prepared` claim outlives the process that proved it.
fn drop_prepared_candidates(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    boot_nonce: &str,
    prepared: &mut BTreeMap<String, fluid_gateway::StagedDeployment>,
) {
    for (transaction_id, staged) in std::mem::take(prepared) {
        runtime.block_on(async move { drop(staged) });
        let record = records.read().get(&transaction_id).cloned();
        if let Some(mut record) = record {
            if record.state == TransferState::Prepared {
                if let Err(error) = downgrade_prepared(fs, records, boot_nonce, &mut record) {
                    tracing::error!(%transaction_id, %error, "prepared transfer downgrade failed during rollback");
                }
            }
        }
    }
    // The armed Drop spawns async pool unregistration onto this runtime;
    // bounded yields let those tasks make progress before the runtime goes
    // away. The synchronous candidate removal already happened inside Drop.
    runtime.block_on(async {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    prepared: &mut BTreeMap<String, fluid_gateway::StagedDeployment>,
    gw: &Arc<fluid_gateway::Gateway>,
    persist: Option<PersistFn>,
    node_name: &str,
    boot_nonce: &str,
    limits: &Limits,
    counters: &WorkerCounters,
    request: TransferRequest,
) -> anyhow::Result<TransferReply> {
    if request.key().target_node != node_name {
        return Ok(TransferReply::error(
            ReplyCode::WrongTarget,
            Some(request.key()),
            "runtime artifact transfer target does not match this node",
        ));
    }
    match request {
        TransferRequest::Begin(begin) => begin_transaction(fs, records, limits, boot_nonce, begin),
        TransferRequest::Chunk(chunk) => {
            apply_chunk(fs, records, limits, counters, boot_nonce, chunk)
        }
        TransferRequest::Query(key) => query_transaction(fs, records, prepared, boot_nonce, &key),
        TransferRequest::Finalize(key) => {
            finalize_transaction(runtime, fs, records, limits, boot_nonce, &key)
        }
        TransferRequest::Abort(key) => {
            abort_transaction(runtime, fs, records, prepared, boot_nonce, &key)
        }
        TransferRequest::Prepare(key) => {
            prepare_transaction(runtime, fs, records, prepared, gw, limits, boot_nonce, &key)
        }
        TransferRequest::Commit(commit) => commit_transaction(
            runtime, fs, records, prepared, gw, persist, boot_nonce, &commit,
        ),
    }
}

fn begin_transaction(
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    limits: &Limits,
    boot_nonce: &str,
    begin: BeginRequest,
) -> anyhow::Result<TransferReply> {
    if let Some(existing) = records.read().get(&begin.key.transaction_id).cloned() {
        if existing.begin == begin {
            return Ok(existing.reply(ReplyCode::Ok, "runtime artifact transfer already began"));
        }
        return Ok(TransferReply::error(
            ReplyCode::Conflict,
            Some(&begin.key),
            "runtime artifact transfer transaction id is bound to another generation",
        ));
    }
    let now = hive_core::now_ms();
    let records_guard = records.read();
    let active = records_guard
        .values()
        .filter(|record| !record.state.terminal() && record.lease_expires_ms >= now)
        .collect::<Vec<_>>();
    if active.len() >= limits.active_node
        || active
            .iter()
            .filter(|record| record.begin.key.tenant == begin.key.tenant)
            .count()
            >= limits.active_tenant
        || active
            .iter()
            .filter(|record| record.begin.key.coordinator_node == begin.key.coordinator_node)
            .count()
            >= limits.active_coordinator
    {
        return Ok(TransferReply::error(
            ReplyCode::ResourceExhausted,
            Some(&begin.key),
            "runtime artifact transfer active-transaction admission is full",
        ));
    }
    drop(records_guard);
    let required = begin
        .package
        .package_bytes
        .checked_add(begin.package.materialized_bytes)
        .and_then(|bytes| bytes.checked_add(limits.disk_reserve_bytes))
        .context("runtime artifact transfer disk requirement overflow")?;
    if fs.available_bytes()? < required {
        return Ok(TransferReply::error(
            ReplyCode::ResourceExhausted,
            Some(&begin.key),
            "runtime artifact transfer requires more node disk headroom",
        ));
    }
    let directory = fs.create_transaction(&begin.key.transaction_id)?;
    let package = directory.create_package()?;
    package.sync_all()?;
    directory.sync()?;
    let record = TransferRecord::new(
        begin,
        now,
        now.saturating_add(limits.lease_ms),
        boot_nonce.to_string(),
    )?;
    directory.write_state(&encode_record(&record)?)?;
    records
        .write()
        .insert(record.begin.key.transaction_id.clone(), record.clone());
    Ok(record.reply(ReplyCode::Ok, "runtime artifact transfer began"))
}

fn apply_chunk(
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    limits: &Limits,
    counters: &WorkerCounters,
    boot_nonce: &str,
    chunk: crate::runtime_artifact_transfer_wire::ChunkRequest,
) -> anyhow::Result<TransferReply> {
    let Some(mut record) = exact_record(records, &chunk.key) else {
        return Ok(TransferReply::error(
            ReplyCode::NotFound,
            Some(&chunk.key),
            "runtime artifact transfer transaction is unavailable",
        ));
    };
    if record.state != TransferState::Receiving {
        return Ok(record.reply(
            ReplyCode::Conflict,
            "runtime artifact transfer is not accepting chunks in its current state",
        ));
    }
    let end = chunk
        .offset
        .checked_add(chunk.bytes.len() as u64)
        .context("runtime artifact transfer chunk offset overflow")?;
    if end > record.begin.package.package_bytes {
        return Ok(record.reply(
            ReplyCode::OutOfOrder,
            "runtime artifact transfer chunk exceeds declared package size",
        ));
    }
    let directory = fs.open_transaction(&chunk.key.transaction_id)?;
    if chunk.offset < record.next_offset {
        let duplicate = record.chunks.iter().find(|seal| {
            seal.offset == chunk.offset
                && seal.bytes as usize == chunk.bytes.len()
                && seal.sha256 == chunk.chunk_sha256
        });
        let Some(_) = duplicate else {
            return Ok(record.reply(
                ReplyCode::ChunkConflict,
                "runtime artifact transfer duplicate chunk conflicts with durable journal",
            ));
        };
        let durable = directory.read_package_range(chunk.offset, chunk.bytes.len())?;
        if durable != chunk.bytes || chunk_sha256(&durable) != chunk.chunk_sha256 {
            return Ok(record.reply(
                ReplyCode::ChunkConflict,
                "runtime artifact transfer duplicate chunk conflicts with durable bytes",
            ));
        }
        counters.duplicate_chunks.fetch_add(1, Ordering::Relaxed);
        record.participant_boot_nonce = boot_nonce.to_string();
        return Ok(record.reply(
            ReplyCode::Ok,
            "runtime artifact transfer chunk already durable",
        ));
    }
    if chunk.offset != record.next_offset {
        return Ok(record.reply(
            ReplyCode::OutOfOrder,
            "runtime artifact transfer chunk is not the exact next contiguous offset",
        ));
    }
    directory.append_package(record.next_offset, &chunk.bytes)?;
    record.chunks.push(ChunkSeal {
        offset: chunk.offset,
        bytes: u32::try_from(chunk.bytes.len()).expect("wire chunk bound fits u32"),
        sha256: chunk.chunk_sha256,
    });
    record.next_offset = end;
    record.updated_ms = hive_core::now_ms();
    record.lease_expires_ms = record.updated_ms.saturating_add(limits.lease_ms);
    record.participant_boot_nonce = boot_nonce.to_string();
    directory.write_state(&encode_record(&record)?)?;
    records
        .write()
        .insert(chunk.key.transaction_id.clone(), record.clone());
    counters.completed_chunks.fetch_add(1, Ordering::Relaxed);
    Ok(record.reply(ReplyCode::Ok, "runtime artifact transfer chunk persisted"))
}

fn query_transaction(
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    prepared: &BTreeMap<String, fluid_gateway::StagedDeployment>,
    boot_nonce: &str,
    key: &TransferKey,
) -> anyhow::Result<TransferReply> {
    let Some(mut record) = exact_record(records, key) else {
        return Ok(TransferReply::error(
            ReplyCode::NotFound,
            Some(key),
            "runtime artifact transfer transaction is unavailable",
        ));
    };
    // A durable Prepared claim is only truthful while THIS process still holds
    // the armed staged handle — the hidden candidate and its launched cells
    // are process-local. Query must never return stale prepared authority.
    if record.state == TransferState::Prepared && !prepared.contains_key(&key.transaction_id) {
        downgrade_prepared(fs, records, boot_nonce, &mut record)?;
        return Ok(record.reply(
            ReplyCode::Ok,
            "prepared candidate did not survive this process; re-prepare",
        ));
    }
    record.participant_boot_nonce = boot_nonce.to_string();
    let message = if record.terminal_error.is_empty() {
        "runtime artifact transfer state recovered"
    } else {
        &record.terminal_error
    };
    Ok(record.reply(
        if record.state == TransferState::Failed {
            ReplyCode::Failed
        } else {
            ReplyCode::Ok
        },
        message,
    ))
}

fn finalize_transaction(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    limits: &Limits,
    boot_nonce: &str,
    key: &TransferKey,
) -> anyhow::Result<TransferReply> {
    let Some(mut record) = exact_record(records, key) else {
        return Ok(TransferReply::error(
            ReplyCode::NotFound,
            Some(key),
            "runtime artifact transfer transaction is unavailable",
        ));
    };
    if matches!(
        record.state,
        TransferState::Materialized
            | TransferState::Prepared
            | TransferState::Committed
            | TransferState::Failed
    ) {
        let code = if record.state == TransferState::Failed {
            ReplyCode::Failed
        } else {
            ReplyCode::Ok
        };
        let message = if record.terminal_error.is_empty() {
            "runtime artifact transfer finalization already completed"
        } else {
            &record.terminal_error
        };
        return Ok(record.reply(code, message));
    }
    if record.state != TransferState::Receiving
        || record.next_offset != record.begin.package.package_bytes
    {
        return Ok(record.reply(
            ReplyCode::OutOfOrder,
            "runtime artifact transfer package is not complete",
        ));
    }
    let directory = fs.open_transaction(&key.transaction_id)?;
    if directory.package_len()? != record.begin.package.package_bytes {
        return Ok(record.reply(
            ReplyCode::ChunkConflict,
            "runtime artifact transfer package length differs from durable state",
        ));
    }
    record.state = TransferState::Finalizing;
    record.updated_ms = hive_core::now_ms();
    record.lease_expires_ms = record.updated_ms.saturating_add(limits.lease_ms);
    record.participant_boot_nonce = boot_nonce.to_string();
    directory.write_state(&encode_record(&record)?)?;
    records
        .write()
        .insert(key.transaction_id.clone(), record.clone());
    let package = directory.open_package(false)?;
    let result = runtime.block_on(hive_backend::materialize_runtime_artifact_package(
        package,
        record.begin.package.clone(),
        fs.store_path(),
    ));
    record.updated_ms = hive_core::now_ms();
    record.participant_boot_nonce = boot_nonce.to_string();
    match result {
        Ok(artifact) => {
            if artifact.content_sha256() != record.begin.package.semantic_tree_sha256 {
                record.state = TransferState::Failed;
                record.terminal_error =
                    "materialized runtime artifact semantic identity changed".to_string();
            } else {
                record.state = TransferState::Materialized;
                record.semantic_tree_sha256 = artifact.content_sha256().to_string();
                record.terminal_error.clear();
            }
        }
        Err(error) => {
            record.state = TransferState::Failed;
            record.terminal_error = bounded(format!(
                "runtime artifact package finalization failed: {error:#}"
            ));
        }
    }
    directory.write_state(&encode_record(&record)?)?;
    records
        .write()
        .insert(key.transaction_id.clone(), record.clone());
    let code = if record.state == TransferState::Failed {
        ReplyCode::Failed
    } else {
        ReplyCode::Ok
    };
    let message = if record.terminal_error.is_empty() {
        "runtime artifact package materialized and verified"
    } else {
        &record.terminal_error
    };
    Ok(record.reply(code, message))
}

fn abort_transaction(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    prepared: &mut BTreeMap<String, fluid_gateway::StagedDeployment>,
    boot_nonce: &str,
    key: &TransferKey,
) -> anyhow::Result<TransferReply> {
    let Some(mut record) = exact_record(records, key) else {
        return Ok(TransferReply::error(
            ReplyCode::NotFound,
            Some(key),
            "runtime artifact transfer transaction is unavailable",
        ));
    };
    if record.state == TransferState::Committed {
        return Ok(record.reply(
            ReplyCode::Conflict,
            "committed runtime artifact transfer cannot be aborted",
        ));
    }
    if record.state == TransferState::Aborted {
        return Ok(record.reply(ReplyCode::Ok, "runtime artifact transfer already aborted"));
    }
    // Roll a hidden prepared candidate back FIRST: dropping the armed handle
    // synchronously removes the staged record and detaches its pools.
    if let Some(staged) = prepared.remove(&key.transaction_id) {
        runtime.block_on(async move { drop(staged) });
    }
    record.state = TransferState::Aborted;
    record.updated_ms = hive_core::now_ms();
    record.participant_boot_nonce = boot_nonce.to_string();
    record.hidden_deployment_id.clear();
    record.readiness_sha256.clear();
    // The materialized package/tree authority dies with the abort — the
    // durable record must not keep claiming a semantic identity its own
    // package file no longer backs (and reply validation on the coordinator
    // treats semantic authority on a non-materialized state as a fault).
    record.semantic_tree_sha256.clear();
    record.terminal_error.clear();
    let directory = fs.open_transaction(&key.transaction_id)?;
    directory.write_state(&encode_record(&record)?)?;
    directory.remove_package()?;
    records
        .write()
        .insert(key.transaction_id.clone(), record.clone());
    Ok(record.reply(ReplyCode::Ok, "runtime artifact transfer aborted"))
}

/// Restore a record whose durable `Prepared` claim can no longer be backed by
/// a live staged handle to honest `Materialized` state.
fn downgrade_prepared(
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    boot_nonce: &str,
    record: &mut TransferRecord,
) -> anyhow::Result<()> {
    record.state = TransferState::Materialized;
    record.hidden_deployment_id.clear();
    record.readiness_sha256.clear();
    record.terminal_error.clear();
    record.updated_ms = hive_core::now_ms();
    record.participant_boot_nonce = boot_nonce.to_string();
    let directory = fs.open_transaction(&record.begin.key.transaction_id)?;
    directory.write_state(&encode_record(record)?)?;
    records
        .write()
        .insert(record.begin.key.transaction_id.clone(), record.clone());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_transaction(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    prepared: &mut BTreeMap<String, fluid_gateway::StagedDeployment>,
    gw: &Arc<fluid_gateway::Gateway>,
    limits: &Limits,
    boot_nonce: &str,
    key: &TransferKey,
) -> anyhow::Result<TransferReply> {
    let Some(mut record) = exact_record(records, key) else {
        return Ok(TransferReply::error(
            ReplyCode::NotFound,
            Some(key),
            "runtime artifact transfer transaction is unavailable",
        ));
    };
    match record.state {
        TransferState::Prepared if prepared.contains_key(&key.transaction_id) => {
            return Ok(record.reply(
                ReplyCode::Ok,
                "runtime artifact deployment candidate is already prepared",
            ));
        }
        // Durable Prepared without a live handle cannot happen in-process (the
        // worker owns both), but repairing it here keeps the invariant local.
        TransferState::Prepared => downgrade_prepared(fs, records, boot_nonce, &mut record)?,
        TransferState::Committed => {
            return Ok(record.reply(
                ReplyCode::Conflict,
                "committed runtime artifact generation cannot be re-prepared",
            ));
        }
        TransferState::Materialized => {}
        _ => {
            return Ok(record.reply(
                ReplyCode::OutOfOrder,
                "runtime artifact transfer package is not materialized",
            ));
        }
    }
    let (staged, readiness) = match stage_hidden_candidate(runtime, fs, gw, &record) {
        Ok(prepared) => prepared,
        // Retryable, not terminal: the durable record stays Materialized and
        // the staged handle (if any was created) already rolled back via Drop.
        Err(error) => {
            return Ok(record.reply(
                ReplyCode::Failed,
                bounded(format!(
                    "runtime artifact deployment prepare failed: {error:#}"
                )),
            ));
        }
    };
    record.state = TransferState::Prepared;
    record.hidden_deployment_id = staged.info().id.to_string();
    record.readiness_sha256 = readiness;
    record.terminal_error.clear();
    record.updated_ms = hive_core::now_ms();
    record.lease_expires_ms = record.updated_ms.saturating_add(limits.lease_ms);
    record.participant_boot_nonce = boot_nonce.to_string();
    let directory = fs.open_transaction(&key.transaction_id)?;
    if let Err(error) = directory.write_state(&encode_record(&record)?) {
        // Durable state refused the Prepared claim: roll the candidate back
        // rather than holding readiness the store cannot survive a restart of.
        runtime.block_on(async move { drop(staged) });
        return Err(error);
    }
    records
        .write()
        .insert(key.transaction_id.clone(), record.clone());
    prepared.insert(key.transaction_id.clone(), staged);
    Ok(record.reply(
        ReplyCode::Ok,
        "runtime artifact deployment candidate prepared hidden",
    ))
}

/// Deliver the exact materialized artifact to the serving backend, verify the
/// backend's committed identity, stage the generation's canonical manifest as
/// a HIDDEN candidate, and prove bounded readiness — everything a receiver
/// does between `Materialized` and `Prepared`, with no source access, no
/// build, and no locally-reconstructed configuration.
fn stage_hidden_candidate(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    gw: &Arc<fluid_gateway::Gateway>,
    record: &TransferRecord,
) -> anyhow::Result<(fluid_gateway::StagedDeployment, String)> {
    let begin = &record.begin;
    let manifest = wire::decode_verified_manifest(begin)?;
    let image = manifest
        .image
        .clone()
        .context("verified generation manifest names no runtime image")?;
    let sealed = hive_backend::reopen_sealed_runtime_artifact(fs.store_path(), &begin.package)?;
    anyhow::ensure!(
        sealed.content_sha256() == record.semantic_tree_sha256,
        "materialized runtime artifact semantic identity changed before prepare"
    );
    let expected = sealed.identity(&image)?;
    let paths = gw.runtime_artifact_paths(&sealed)?;
    anyhow::ensure!(
        paths.delivery_required,
        "backend {} did not require explicit sealed runtime delivery",
        gw.backend_name()
    );
    let runtime_workdir = paths
        .guest_workdir
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("validated guest runtime workdir is not UTF-8"))?;
    let host_static_root = paths
        .host_static_root
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("sealed host static root is not UTF-8"))?;
    // Display-only source metadata, derived from the sealed snapshot authority
    // rather than any receiver-local state.
    let contract: fluid_build::DeploymentBuildContract =
        serde_json::from_slice(&begin.snapshot_bytes)
            .context("decode sealed deployment build contract")?;
    let git = match &contract.source {
        fluid_build::SourceSnapshot::Git {
            repository,
            branch,
            commit,
            ..
        } => Some(fluid_core::GitSource {
            repo_url: repository.clone(),
            branch: branch.clone().unwrap_or_default(),
            commit: commit.chars().take(7).collect(),
            commit_message: String::new(),
        }),
        _ => None,
    };
    let creator = if begin.creator.trim().is_empty() {
        "you".to_string()
    } else {
        begin.creator.clone()
    };
    let gw = gw.clone();
    let tenant = begin.key.tenant.clone();
    let production = begin.production;
    let project_incarnation = begin.project_incarnation;
    runtime.block_on(async move {
        gw.deliver_build(&image, &sealed).await.map_err(|error| {
            anyhow::anyhow!(
                "could not deliver the sealed generation to backend {}: {error}",
                gw.backend_name()
            )
        })?;
        let committed = gw
            .runtime_artifact_identity(&image)
            .await?
            .with_context(|| {
                format!(
                    "backend {} accepted image {image:?} without a committed artifact receipt",
                    gw.backend_name()
                )
            })?;
        anyhow::ensure!(
            committed == expected,
            "backend {} committed artifact identity {:?}, expected {:?}",
            gw.backend_name(),
            committed,
            expected
        );
        let mut staged = gw.stage_full_with_runtime_exact(
            host_static_root,
            Some(runtime_workdir),
            manifest,
            creator,
            git,
            production,
            tenant,
            project_incarnation,
        )?;
        let receipt = staged.prove_ready().await?.clone();
        let readiness = readiness_receipt_sha256(&receipt)?;
        Ok((staged, readiness))
    })
}

#[allow(clippy::too_many_arguments)]
fn commit_transaction(
    runtime: &tokio::runtime::Runtime,
    fs: &TransferFs,
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    prepared: &mut BTreeMap<String, fluid_gateway::StagedDeployment>,
    gw: &Arc<fluid_gateway::Gateway>,
    persist: Option<PersistFn>,
    boot_nonce: &str,
    commit: &CommitRequest,
) -> anyhow::Result<TransferReply> {
    let Some(mut record) = exact_record(records, &commit.key) else {
        return Ok(TransferReply::error(
            ReplyCode::NotFound,
            Some(&commit.key),
            "runtime artifact transfer transaction is unavailable",
        ));
    };
    if record.state == TransferState::Committed {
        if record.hidden_deployment_id == commit.hidden_deployment_id
            && record.readiness_sha256 == commit.readiness_sha256
        {
            return Ok(record.reply(
                ReplyCode::Ok,
                "runtime artifact generation already committed",
            ));
        }
        return Ok(record.reply(
            ReplyCode::Conflict,
            "committed runtime artifact generation differs from the requested authority",
        ));
    }
    if record.state != TransferState::Prepared {
        return Ok(record.reply(
            ReplyCode::OutOfOrder,
            "runtime artifact commit requires a prepared candidate",
        ));
    }
    if record.hidden_deployment_id != commit.hidden_deployment_id
        || record.readiness_sha256 != commit.readiness_sha256
    {
        return Ok(record.reply(
            ReplyCode::Conflict,
            "prepared candidate authority differs from the commit request",
        ));
    }
    let Some(staged) = prepared.remove(&commit.key.transaction_id) else {
        downgrade_prepared(fs, records, boot_nonce, &mut record)?;
        return Ok(record.reply(
            ReplyCode::Conflict,
            "prepared candidate is no longer held by this process; re-prepare",
        ));
    };
    let Some(persist) = persist else {
        // Refuse rather than publish a Ready record a restart would lose; the
        // candidate stays prepared and the coordinator retries.
        prepared.insert(commit.key.transaction_id.clone(), staged);
        return Ok(record.reply(
            ReplyCode::Failed,
            "platform persistence is not bound on this node; commit refused",
        ));
    };
    match runtime.block_on(async { staged.publish_ready() }) {
        Ok((info, _receipt)) => {
            if !persist() {
                let removed = info.id.to_string();
                let gw = gw.clone();
                runtime.block_on(async move {
                    gw.remove(&removed).await;
                });
                let _ = persist();
                downgrade_prepared(fs, records, boot_nonce, &mut record)?;
                return Ok(record.reply(
                    ReplyCode::Failed,
                    "published deployment could not be durably persisted; candidate rolled back",
                ));
            }
            record.state = TransferState::Committed;
            record.terminal_error.clear();
            record.updated_ms = hive_core::now_ms();
            record.participant_boot_nonce = boot_nonce.to_string();
            let directory = fs.open_transaction(&commit.key.transaction_id)?;
            directory.write_state(&encode_record(&record)?)?;
            records
                .write()
                .insert(commit.key.transaction_id.clone(), record.clone());
            Ok(record.reply(ReplyCode::Ok, "runtime artifact generation committed"))
        }
        Err(error) => {
            // publish_ready consumed the handle; its armed Drop already rolled
            // the hidden candidate back on this failure path.
            downgrade_prepared(fs, records, boot_nonce, &mut record)?;
            Ok(record.reply(
                ReplyCode::Failed,
                bounded(format!(
                    "runtime artifact generation publish failed: {error:#}"
                )),
            ))
        }
    }
}

fn exact_record(
    records: &RwLock<BTreeMap<String, TransferRecord>>,
    key: &TransferKey,
) -> Option<TransferRecord> {
    records
        .read()
        .get(&key.transaction_id)
        .filter(|record| record.begin.key == *key)
        .cloned()
}

fn recover_records(
    fs: &TransferFs,
    boot_nonce: &str,
) -> anyhow::Result<BTreeMap<String, TransferRecord>> {
    let mut recovered = BTreeMap::new();
    for transaction_id in fs.transaction_ids()? {
        let directory = match fs.open_transaction(&transaction_id) {
            Ok(directory) => directory,
            Err(error) => {
                tracing::error!(%transaction_id, %error, "runtime artifact transfer transaction directory refused during recovery");
                continue;
            }
        };
        let mut record = match directory
            .read_state()
            .and_then(|bytes| decode_record(&bytes))
        {
            Ok(record) => record,
            Err(error) => {
                tracing::error!(%transaction_id, %error, "runtime artifact transfer durable state refused during recovery");
                continue;
            }
        };
        if record.begin.key.transaction_id != transaction_id {
            tracing::error!(%transaction_id, "runtime artifact transfer directory identity differs from durable state");
            continue;
        }
        if record.state != TransferState::Aborted {
            let package_len = match directory.package_len() {
                Ok(len) => len,
                Err(error) => {
                    tracing::error!(%transaction_id, %error, "runtime artifact transfer package refused during recovery");
                    continue;
                }
            };
            if package_len < record.next_offset {
                tracing::error!(%transaction_id, package_len, durable_prefix = record.next_offset, "runtime artifact transfer package is shorter than durable state");
                continue;
            }
            if package_len > record.next_offset
                && matches!(
                    record.state,
                    TransferState::Receiving | TransferState::Finalizing
                )
            {
                directory.truncate_package(record.next_offset)?;
            }
        }
        if record.state == TransferState::Finalizing {
            record.state = TransferState::Receiving;
            record.terminal_error.clear();
            record.updated_ms = hive_core::now_ms();
            record.participant_boot_nonce = boot_nonce.to_string();
            directory.write_state(&encode_record(&record)?)?;
        }
        // A `Prepared` claim names a hidden staged candidate and its launched
        // readiness proof — both strictly process-local (`StagedDeployment` is
        // an armed in-memory handle). A fresh boot therefore recovers the
        // durable record as `Materialized` and clears the dead authority; the
        // coordinator observes the new boot nonce and repeats Prepare.
        if record.state == TransferState::Prepared {
            record.state = TransferState::Materialized;
            record.hidden_deployment_id.clear();
            record.readiness_sha256.clear();
            record.terminal_error.clear();
            record.updated_ms = hive_core::now_ms();
            record.participant_boot_nonce = boot_nonce.to_string();
            directory.write_state(&encode_record(&record)?)?;
        }
        recovered.insert(transaction_id, record);
    }
    Ok(recovered)
}

fn request_charge(request: &TransferRequest) -> u64 {
    match request {
        TransferRequest::Chunk(chunk) => chunk.bytes.len() as u64 + 4096,
        TransferRequest::Begin(begin) => {
            begin.snapshot_bytes.len() as u64 + begin.manifest_bytes.len() as u64 + 4096
        }
        _ => 4096,
    }
}

fn reserve(counter: &AtomicU64, maximum: u64, amount: u64) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return false;
        };
        if next > maximum {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn release(counter: &AtomicU64, amount: u64) {
    let previous = counter.fetch_sub(amount, Ordering::AcqRel);
    debug_assert!(previous >= amount);
}

fn env_usize(name: &str, default: usize, minimum: usize, maximum: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(minimum, maximum))
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64, minimum: u64, maximum: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(minimum, maximum))
        .unwrap_or(default)
}

fn validate_boot_nonce(value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("runtime artifact transfer boot nonce must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn bounded(mut value: String) -> String {
    if value.len() > crate::runtime_artifact_transfer_wire::MAX_ERROR_BYTES {
        value.truncate(crate::runtime_artifact_transfer_wire::MAX_ERROR_BYTES);
    }
    value
}
