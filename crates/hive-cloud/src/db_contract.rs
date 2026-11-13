//! The managed-database contract's ONE wiring seam into the platform.
//!
//! `crates/hive-contract` is the contract + execution-transparency layer; every guarantee it makes
//! is unexercised until a real mutation path calls it. This module is where that call lives, and it
//! is deliberately the ONLY place in `hive-cloud` that touches the engine: one seam means the
//! coverage question ("which mutation paths are wired?") is answerable by looking at one file, and
//! `hive_contract::reconcile::DECLARED_SCOPE` records the answer per path.
//!
//! What this module does NOT do:
//!   * It does not authorize. `tenant` is the cryptographically-bound tenant the platform already
//!     resolved (`admin::tenant`, `admin::write_scope` for a mirrored write) and `owns_target` is
//!     the verdict of the platform's own ownership check for this database. Both are handed to
//!     `contract::validate`, which fails CLOSED when either is missing: an untagged or non-owning
//!     caller produces a REFUSED entry, never a committed one. So adding a call here cannot open a
//!     path that was closed before it.
//!   * It does not perform the mutation, and it does not run BEFORE it. Every caller here mutates
//!     first and records afterwards: `blob_put` has already written to disk by the time `wrote` is
//!     called, and `queue_push`/`vector_upsert`/`publish` have already moved their store. A REFUSED
//!     entry is therefore a RECORD that the platform's write was not attested -- never a prevention
//!     of it. The bytes have already moved. Do not describe this layer as refusing an unauthorized
//!     write: the platform's authorization is `auth::require_auth` plus the roles matrix and the
//!     ownership check, and this module only reports what those already decided, after the fact.
//!     What the refusal buys is visibility -- canonical state holds a write the contract refuses to
//!     attest, which is exactly the divergence `hive_contract::reconcile` exists to surface.
//!   * It does not perform the mutation. The caller has already done it and passes the result it
//!     MEASURED afterwards as `outcome`; an `Err` is recorded with `success: false` and applies NO
//!     state change, so a database write that never landed is not asserted as one that did. Passing
//!     `Ok(())` unconditionally would leave that guarantee unexercised on every live path, so a
//!     caller's `outcome` is a measurement (an object that reads back, a counter that moved), never
//!     a constant.
//!   * It does not fail the request. A contract recording problem is reported and dropped: the
//!     platform's own mutation has already happened, and losing an audit entry is strictly better
//!     than answering an error for work that succeeded.

use crate::state::CloudState;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What recording one operation produced.
#[derive(Clone, Debug)]
pub struct Receipt {
    /// The log sequence the operation occupies: 0 for the first op after boot.
    pub seq: u64,
    /// True only when the op was admitted AND the platform's own mutation succeeded.
    pub committed: bool,
    /// The contract index hash AFTER this op.
    pub index_hash: String,
    /// The oplog head hash AFTER this op: the commitment a monitor re-derives by replaying the log
    /// against `apply`, and the thing a rewritten or truncated history cannot reproduce.
    pub log_head: String,
    /// Why the op was refused or its execution failed; `None` on a clean commit.
    pub error: Option<String>,
    /// The caller's copy of the publication: the entry plus the executor's signature over its hash
    /// and its ack (whitepaper 2.7, and the "oplog root proof" + "index root proof" the
    /// participant's transaction flow returns in 3.4).
    ///
    /// This is what makes a recorded write provable to somebody who was not watching. The platform
    /// has always had the log; without this the caller -- and therefore the tenant -- has only the
    /// platform's word that the write was published, and any dispute about whether it happened is
    /// settled by asking the executor that recorded it. With it, the receipt is an artefact the
    /// executor cannot repudiate and a third party can check against the node's public key alone.
    ///
    /// `None` on an engine with no store: there is no signing identity to publish with, and a proof
    /// nobody signed asserts nothing.
    ///
    /// Freshness is not part of it, and that is the whitepaper's own caveat: the proof says the entry
    /// WAS published, never that the log still ends there.
    pub proof: Option<hive_contract::InclusionProof>,
}

/// The actor string recorded for a request: the JWT subject, or a marker that no subject was bound.
///
/// The actor is a LABEL, never a gate -- `contract::Caller::owns_target` is what the contract
/// enforces, and it comes from the platform's own check. An unbound request is recorded under
/// `"unknown"` rather than left blank so an entry is never ambiguous with a truncated field.
pub(crate) fn actor(claims: Option<&crate::auth::Claims>) -> String {
    claims
        .map(|c| c.sub.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether a resolved tenant is one the platform actually bound an identity to.
///
/// `admin::ANON_TENANT` is the namespace an unauthenticated request lands in under JWT enforcement;
/// it owns no data. Handing it to the contract as an owning caller would mint an entry asserting a
/// tenant-scoped mutation for a caller with no tenant, so it is refused here (the contract then
/// records the refusal, which is the honest outcome).
fn owns(tenant: &str) -> bool {
    !tenant.is_empty() && tenant != crate::admin::ANON_TENANT
}

/// Record one managed-database mutation on the contract's oplog.
///
/// `kind` is one of the contract's declared ops (`db.create`, `db.write`, `db.delete`); `extra` carries the
/// payload keys beyond `db_id` and `project` that the declared key set demands exactly -- a missing
/// or unexpected key is refused by `contract::validate` and recorded as a refusal, so a payload
/// built by hand cannot silently widen the contract's schema. `target` is always the database id,
/// which is also the value the payload carries at `db_id`: the identity the contract authorized and
/// the identity whose index keys move are the same string by construction.
///
/// Returns `None` only when the engine could not record at all; every contract-level refusal is
/// recorded as an entry and comes back as a `Receipt` carrying the reason.
pub(crate) async fn record(
    c: &Arc<CloudState>,
    tenant: &str,
    actor: &str,
    kind: &str,
    db_id: &str,
    project: &str,
    extra: &[(&str, &str)],
    owns_target: bool,
    outcome: Result<(), String>,
) -> Option<Receipt> {
    let mut payload = BTreeMap::new();
    payload.insert("db_id".to_string(), db_id.to_string());
    payload.insert("project".to_string(), project.to_string());
    for (k, v) in extra {
        payload.insert((*k).to_string(), (*v).to_string());
    }
    let caller = hive_contract::Caller {
        tenant: tenant.to_string(),
        actor: actor.to_string(),
        owns_target: owns(tenant) && owns_target, platform_attested: false,
    };
    let result = c
        .contract_engine
        .submit(
            &c.managed_db_contract,
            caller,
            kind,
            payload,
            db_id,
            hive_core::now_ms(),
            outcome,
        )
        .await;
    match result {
        Ok(r) => {
            let receipt = Receipt {
                seq: r.seq,
                committed: r.committed,
                index_hash: r.index_hash,
                log_head: r.log_head,
                error: r.error,
                proof: r.proof,
            };
            // Carried through rather than recomputed: the receipt is the one artefact a caller can
            // hand to a third party, and it has to be the bytes the executor published.
            let proof_seq = receipt.proof.as_ref().map(|p| p.seq());
            match &receipt.error {
                Some(e) => tracing::warn!(
                    op = kind,
                    db = %db_id,
                    tenant = %tenant,
                    seq = receipt.seq,
                    error = %e,
                    "contract: managed-db op refused"
                ),
                None => tracing::info!(
                    op = kind,
                    db = %db_id,
                    tenant = %tenant,
                    seq = receipt.seq,
                    committed = receipt.committed,
                    index_hash = %receipt.index_hash,
                    log_head = %receipt.log_head,
                    proof_seq = proof_seq.unwrap_or(0),
                    proof = receipt.proof.is_some(),
                    "contract: managed-db op recorded"
                ),
            }
            Some(receipt)
        }
        Err(e) => {
            // Not a contract refusal: the engine could not produce an entry at all. Loud, and the
            // platform's mutation stands -- it already happened and is not undone here.
            tracing::error!(op = kind, db = %db_id, error = %e, "contract: managed-db op NOT recorded");
            None
        }
    }
}

/// [`record`] for a `db.create`: the database was provisioned (locally or on a peer that adopted
/// the record) and its catalog identity now exists.
pub(crate) async fn created(
    c: &Arc<CloudState>,
    tenant: &str,
    actor: &str,
    db: &crate::databases::Database,
    outcome: Result<(), String>,
) -> Option<Receipt> {
    record(
        c,
        tenant,
        actor,
        "db.create",
        &db.id,
        &db.project,
        &[("kind", db.kind.label())],
        // The record's stored tag is the tenant the platform resolved for this request
        // (`admin::database_create` overwrites `req.team` with it), so the caller owns what it just
        // created -- except when the resolved tenant is anonymous, which `owns` refuses.
        crate::admin::record_tenant(&db.team) == tenant,
        outcome,
    )
    .await
}

/// The actor recorded for a removal with no request in flight: a project purge, an incarnation
/// discard, or an operator console row delete.
pub(crate) const PLATFORM_ACTOR: &str = "platform";

/// [`record`] for a `db.write` that landed through a platform-native storage lane
/// (`/v1/storage/blob|queue|vector|pubsub`, and the realtime room lane at `/v1/ws/realtime/:room`).
///
/// Realtime is the fifth lane. ALL FIVE lanes now reach the seam from two places: the handler that
/// serves a local request, and the arm of `apply_mirrored_write` that replays the same write on a
/// peer. The second half is what made a replicated write visible: a mirrored write is a real
/// mutation of the receiving node's store, so an arm that applied it and recorded nothing left the
/// write on every node except the origin -- coverage the audit trail claimed and did not have.
/// A mirrored replay has no request in flight, so it is recorded under [`PLATFORM_ACTOR`] with the
/// tenant the signed mirror proof bound; fan-out excludes this node by name, so a write is never
/// recorded twice.
///
/// Those lanes are addressed by PAYLOAD NAME, never by database id, so the database the write landed
/// in is resolved here (`DatabaseStore::native_db`) from the same `(kind, team, name)` triple the
/// lane itself writes under. That lookup is also the platform's ownership verdict for this write: a
/// record found this way is one whose stored tenant IS the tenant the request resolved to, so
/// `owns_target` below is that comparison made explicitly rather than assumed -- the same expression
/// [`created`] uses, and an untagged or foreign row therefore records a REFUSAL instead of an
/// attested write.
///
/// A lane no database record owns is NOT silently dropped: [`unattributed`] records it, refused,
/// against the storage address the platform actually wrote under. The platform really did move those
/// bytes (under `<tenant>::<name>`), and an audit trail that loses the write cannot answer "did this
/// data land?" -- while a refusal applies NO index mutation, so the entry says "the platform wrote
/// bytes no managed database attests" without minting a database that owns them.
///
/// `bytes` is the size of the payload that moved, and `outcome` is the platform's own result: an
/// `Err` is recorded with `success: false` and applies NO state change, so a write that never landed
/// is never asserted as one that did.
pub(crate) async fn wrote(
    c: &Arc<CloudState>,
    tenant: &str,
    actor: &str,
    kind: crate::databases::DbKind,
    name: &str,
    bytes: u64,
    outcome: Result<(), String>,
) -> Option<Receipt> {
    let bytes = bytes.to_string();
    match c.databases.native_db(kind, tenant, name) {
        Some(db) => {
            let tenant_owns = crate::admin::record_tenant(&db.team) == tenant;
            // Canonical state records the write plane for a write that actually landed, so the count
            // and the last-bytes value the contract's index claims are claims canonical state can
            // attest. Only on `Ok`: a write that never landed changed nothing, and moving the
            // counter for it would make the two accounts disagree for a reason that is not a
            // divergence.
            //
            // It counts the write even when the CONTRACT refuses it, and that is deliberate: canonical
            // state accounts for what the platform actually DID, while the contract accounts for what
            // it can attest. A write the contract refuses therefore shows up as a divergence --
            // canonical has it, the contract does not -- which is exactly the finding. Suppressing the
            // count here would leave the unattested write with nothing to be compared against, which
            // is the silence this layer exists to end.
            //
            // Recording it HERE, next to the contract entry, is what keeps the two sides of a
            // reconciliation measured over the same event: they cannot drift because one path forgot
            // the other.
            if outcome.is_ok() {
                c.databases.record_write(&db.id, &bytes);
            }
            record(
                c,
                tenant,
                actor,
                "db.write",
                &db.id,
                &db.project,
                &[("bytes", &bytes)],
                tenant_owns,
                outcome,
            )
            .await
        }
        // No managed database owns this payload name. The write still happened, so it is recorded as
        // what it is instead of vanishing between the platform's data plane and its audit trail.
        None => unattributed(c, tenant, actor, kind, name, &bytes, outcome).await,
    }
}

/// Marker prefix of an [`unattributed`] write's target: a target carrying it is a STORAGE ADDRESS,
/// never a database id, so an entry can never be mistaken for an attestation about a database.
pub(crate) const UNATTRIBUTED: &str = "unattributed";

/// [`record`] for a write the platform performed that NO managed database record owns.
///
/// The seam's three rules hold exactly as they do for an attributed write: it does not authorize
/// (`owns_target` is `false` by construction -- there is no target database for this tenant to own),
/// it does not perform the mutation (the caller already did and passes its `outcome`), and it does
/// not fail the request (a recording problem is reported and dropped).
///
/// The entry is therefore always a REFUSAL -- `contract::validate` answers `OpError::TenantMismatch`,
/// or `OpError::UntaggedTenant` for a caller with no bound tenant -- and `Executor::commit_refusal`
/// appends it WITHOUT applying a mutation. So the log carries the write (its address, its size, its
/// actor, whether it landed) and the index keeps no database it cannot vouch for: honest about what
/// happened, silent about nothing, and never an invented attestation.
///
/// Canonical state's write plane is deliberately NOT counted here: it is the per-DATABASE account a
/// reconciler compares against the contract's index, and a key for a database that does not exist
/// would make that comparison a claim about a phantom. The refusal entry is the record.
async fn unattributed(
    c: &Arc<CloudState>,
    tenant: &str,
    actor: &str,
    kind: crate::databases::DbKind,
    name: &str,
    bytes: &str,
    outcome: Result<(), String>,
) -> Option<Receipt> {
    // The address the lane wrote under, which is also the address a reader of the log must look for.
    let target = format!("{UNATTRIBUTED}:{}:{}::{}", kind.label(), tenant, name);
    let receipt = record(
        c,
        tenant,
        actor,
        "db.write",
        &target,
        // No project: the write did not land in a project's managed database, and naming one would
        // assert an ownership the platform never established.
        "",
        &[("bytes", bytes)],
        false,
        outcome,
    )
    .await;
    let seq = receipt.as_ref().map(|r| r.seq).unwrap_or(0);
    tracing::warn!(
        op = "db.write",
        kind = %kind.label(),
        tenant = %tenant,
        name = %name,
        bytes = %bytes,
        seq = seq,
        recorded = receipt.is_some(),
        "contract: managed-db write is UNATTRIBUTED (no database record owns this payload name)"
    );
    receipt
}

/// [`record`] for a `db.delete` the PLATFORM performs on its own authority, with no request in
/// flight.
///
/// Why the caller is the record's own tenant, and why `owns_target` is true: both decide the INDEX,
/// not access. The platform is removing a row whose only bound tenant identity is the one stored on
/// it (`admin::record_tenant(&db.team)`), and a project purge or an incarnation discard is done
/// under that tenant's authority. A `db.delete` recorded as a refusal goes through
/// `Executor::commit_refusal`, which applies NO mutation -- so the index would keep a database that
/// canonical state no longer has. That is exactly the divergence the contract exists to surface,
/// and it is what every unwired removal path silently produced.
pub(crate) async fn purged(c: &Arc<CloudState>, db: &crate::databases::Database) -> Option<Receipt> {
    record(
        c,
        crate::admin::record_tenant(&db.team),
        PLATFORM_ACTOR,
        "db.delete",
        &db.id,
        &db.project,
        &[],
        true,
        Ok(()),
    )
    .await
}

/// [`record`] for a `db.delete`: the catalog entry was removed and its payload data purged.
pub(crate) async fn deleted(
    c: &Arc<CloudState>,
    tenant: &str,
    actor: &str,
    db_id: &str,
    project: &str,
    owns_target: bool,
) -> Option<Receipt> {
    record(
        c,
        tenant,
        actor,
        "db.delete",
        db_id,
        project,
        &[],
        owns_target,
        Ok(()),
    )
    .await
}


// ---- metering / billing contract mirror -----------------------------------------------------------
// The ONLY seam also owns the metering and billing ledger mirror. Every charge, credit, plan
// change, renewal and resource-meter sample becomes a signed, attributed, seq-checked entry in
// hive.billing-meter.v1: the same hash-chained, Ed25519-signed, replay-verifiable oplog the
// managed-database contract uses. Nothing here authorizes, charges or fails a request: the
// platform has ALREADY moved the meter or written the ledger row before enqueue() is called, and
// enqueue() is a RECORD, never a prevention. Charging stays behind
// HIVE_BILLING_METER_RESOURCES and the single leadership::may_act(BillingMeter) writer.
//
// Why a bounded queue plus an async drain: ledger_push and meter_usage are SYNCHRONOUS and hold
// BillingStore locks; Engine::submit is async and does durable fsync. Taking a runtime handle and
// spawning one task per charge would reorder the log and lose entries on shutdown. A bounded,
// process-wide queue drained in order from the bulkhead metering loop gives one writer, one order,
// and an explicit, counted drop when the queue is full -- never a silent one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The actor label on a platform-measured event. A LABEL, never a gate: the gate is
/// `Caller::platform_attested` plus the server-derived tenant check in `metering_owns`.
pub const BILLING_METER_ACTOR: &str = "platform:meter";

/// Bounded so a stalled drain can never turn billing into unbounded memory. Overflow is COUNTED
/// and WARNed, and the count is reported by `drain_metering` -- an unattested event is visible.
const MAX_PENDING_METER_OPS: usize = 4096;
/// Submissions per drain tick, so one tick never monopolises the bulkhead runtime.
const MAX_DRAIN_PER_TICK: usize = 256;

static BILLING_CONTRACT: OnceLock<hive_contract::Contract> = OnceLock::new();
static PENDING: parking_lot::Mutex<VecDeque<MeterOp>> =
    parking_lot::Mutex::new(VecDeque::new());
static METER_OPS_DROPPED: AtomicU64 = AtomicU64::new(0);

/// One measured platform event, with the exact payload the contract declared.
#[derive(Clone)]
pub struct MeterOp {
    pub kind: &'static str,
    pub tenant: String,
    pub target: String,
    pub payload: BTreeMap<String, String>,
    /// The platform's own measurement: did the meter move / did the ledger row read back?
    /// NEVER a literal `Ok(())`; an Err is recorded as a failed entry with no index mutation.
    pub outcome: Result<(), String>,
}

/// Enqueue one measured event. Called from `billing::ledger_push`, `billing::meter_usage` and
/// the invoice rollover; never from a request-path authorization decision.
pub fn enqueue_meter_op(op: MeterOp) {
    let mut q = PENDING.lock();
    if q.len() >= MAX_PENDING_METER_OPS {
        METER_OPS_DROPPED.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            kind = op.kind,
            tenant = %op.tenant,
            entry_id = op.payload.get("entry_id").map(String::as_str).unwrap_or(""),
            "contract: metering event queue full; event DROPPED (NOT attested)"
        );
        return;
    }
    q.push_back(op);
}

/// Whether a tenant tag is one the platform actually derived from a stored record. An anonymous,
/// untagged or `_unattributed` tag is recorded as a REFUSED entry with no index mutation: it is
/// measured but never attested, and never collapsed into a billable namespace.
fn metering_owns(tenant: &str) -> bool {
    let t = tenant.trim();
    !t.is_empty()
        && t != crate::admin::ANON_TENANT
        && t != crate::admin::UNTAGGED_TENANT
        && t != crate::usage_metering::UNATTRIBUTED
        && crate::admin::record_tenant(t) == t
}

#[derive(Debug, Default)]
pub struct DrainReport {
    pub submitted: usize,
    pub refused: usize,
    pub persist_errors: usize,
    pub dropped: u64,
}

/// Submit queued events through the ONE seam, in queue order, one writer. Runs on the bulkhead
/// metering loop (never a serving tokio worker). Returns a report the caller logs, so a dropped or
/// unpersisted charge reaches the platform's log instead of disappearing.
pub async fn drain_metering(c: &Arc<CloudState>) -> DrainReport {
    let ops: Vec<MeterOp> = {
        let mut q = PENDING.lock();
        (0..MAX_DRAIN_PER_TICK)
            .filter_map(|_| q.pop_front())
            .collect()
    };
    let mut report = DrainReport {
        dropped: METER_OPS_DROPPED.swap(0, Ordering::Relaxed),
        ..DrainReport::default()
    };
    if ops.is_empty() {
        return report;
    }
    let contract = BILLING_CONTRACT.get_or_init(hive_contract::billing_meter_contract);
    // Completeness comes from CANONICAL STATE, not from the in-memory queue. A restart, a crashed
    // drain or a full queue can drop a queued event, but it cannot drop a ledger row that still
    // exists: this pull-based mirror behind a durable cursor re-attests every row it has not yet
    // seen, so an omitted charge is recoverable rather than silently lost.
    let (mirrored, mirror_failures) = mirror_ledger_rows(c, contract).await;
    report.submitted += mirrored;
    report.persist_errors += mirror_failures;
    for op in ops {
        let retry = op.clone();
        let caller = hive_contract::Caller {
            tenant: op.tenant.clone(),
            actor: BILLING_METER_ACTOR.to_string(),
            owns_target: metering_owns(&op.tenant),
            platform_attested: true,
        };
        match c
            .contract_engine
            .submit(
                contract,
                caller,
                op.kind,
                op.payload,
                &op.target,
                hive_core::now_ms(),
                op.outcome,
            )
            .await
        {
            Ok(r) => {
                report.submitted += 1;
                if !r.committed {
                    report.refused += 1;
                }
                if r.persist_error.is_some() {
                    report.persist_errors += 1;
                }
                tracing::info!(
                    op = op.kind,
                    tenant = %op.tenant,
                    seq = r.seq,
                    committed = r.committed,
                    log_head = %r.log_head,
                    proof = r.proof.is_some(),
                    persist_error = r.persist_error.is_some(),
                    error = r.error.as_deref().unwrap_or(""),
                    "contract: metering/billing event recorded"
                );
            }
            Err(e) => {
                // Requeue at the FRONT so order is preserved and a transient store failure does
                // not become a permanent omission. Bounded by the same cap as enqueue.
                let mut q = PENDING.lock();
                if q.len() < MAX_PENDING_METER_OPS {
                    q.push_front(retry);
                } else {
                    METER_OPS_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
                drop(q);
                // The store could not open: NO entry was produced. Loud, and the platform's own
                // ledger row still stands -- a contract recording problem never fails billing.
                report.persist_errors += 1;
                tracing::error!(
                    op = op.kind,
                    tenant = %op.tenant,
                    error = %e,
                    "contract: metering/billing event NOT recorded"
                );
            }
        }
    }
    report
}

/// BLAKE3 digest of a ledger row's free-text note. A NEW content identity, so BLAKE3 per the
/// repo's hashing rule; it keeps an operator-supplied note from growing an append-only log
/// without bound while still binding the note's bytes into the signed entry.
pub fn note_hash(note: &str) -> String {
    fluid_core::content_hash(b"hive-contract/billing-note/v1", note.as_bytes())
}


/// Node-local durable cursor of the ledger row ids already attested into the contract log.
/// Plain text, one id per line: a corrupt or missing file degrades to an empty set, which
/// re-attests rows (an idempotent index PUT) rather than losing one.
fn cursor_path() -> std::path::PathBuf {
    crate::persist::data_dir().join("contract-billing-cursor.txt")
}

fn load_cursor() -> std::collections::BTreeSet<String> {
    std::fs::read_to_string(cursor_path())
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty() && l.len() <= 128)
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn save_cursor(ids: &std::collections::BTreeSet<String>) {
    let path = cursor_path();
    let tmp = path.with_extension("txt.tmp");
    let body = ids.iter().map(|id| id.as_str()).collect::<Vec<_>>().join("\n");
    let body = format!("{body}\n");
    if let Ok(()) = std::fs::create_dir_all(crate::persist::data_dir()) {
        if std::fs::write(&tmp, body.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
            return;
        }
    }
    tracing::warn!(path = %path.display(), "contract: billing mirror cursor could not be saved");
}

/// The exact declared payload of one canonical ledger row. The raw note is bounded: it is
/// evidence, not a place to grow an append-only log without limit.
fn ledger_row_payload(e: &crate::billing::LedgerEntry) -> BTreeMap<String, String> {
    let note: String = e.note.chars().take(256).collect();
    BTreeMap::from([
        ("tenant_tag".to_string(), e.tenant.clone()),
        ("entry_id".to_string(), e.id.clone()),
        ("entry_ts_ms".to_string(), e.ts_ms.to_string()),
        ("entry_kind".to_string(), e.kind.clone()),
        ("amount_cents".to_string(), e.amount_cents.to_string()),
        (
            "balance_after_cents".to_string(),
            e.balance_after_cents.to_string(),
        ),
        ("note_hash".to_string(), note_hash(&e.note)),
        ("note".to_string(), note),
    ])
}

/// Pull-based completeness mirror: every raw `LedgerEntry` in canonical billing state that this
/// node has not yet attested becomes one signed `billing.ledger` entry through this seam. The
/// outcome is a MEASUREMENT -- the row is in the snapshot read from BillingStore -- and the id is
/// added to the durable cursor only after a submitted entry has no persist error.
///
/// This is the answer to a lost queue: an event enqueue is volatile, canonical state is not.
async fn mirror_ledger_rows(
    c: &Arc<CloudState>,
    contract: &hive_contract::Contract,
) -> (usize, usize) {
    let (_, rows) = c.billing.snapshot();
    let present: std::collections::BTreeSet<String> =
        rows.iter().map(|e| e.id.clone()).collect();
    let mut cursor = load_cursor();
    // A pruned row no longer needs a cursor entry; the period remains anchored by the signed
    // billing.checkpoint entry. This keeps the cursor bounded by the live raw ledger.
    cursor.retain(|id| present.contains(id));
    let mut pending: Vec<crate::billing::LedgerEntry> = rows
        .into_iter()
        .filter(|e| !cursor.contains(&e.id))
        .collect();
    pending.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.id.cmp(&b.id)));
    pending.truncate(MAX_DRAIN_PER_TICK);
    let mut ok = 0usize;
    let mut bad = 0usize;
    for e in pending {
        let caller = hive_contract::Caller {
            tenant: e.tenant.clone(),
            actor: BILLING_METER_ACTOR.to_string(),
            owns_target: metering_owns(&e.tenant),
            platform_attested: true,
        };
        let payload = ledger_row_payload(&e);
        let outcome = if present.contains(&e.id) {
            Ok(())
        } else {
            Err(format!("ledger row {} is no longer in canonical state", e.id))
        };
        match c
            .contract_engine
            .submit(
                contract,
                caller,
                hive_contract::BILLING_LEDGER,
                payload,
                &e.tenant,
                hive_core::now_ms(),
                outcome,
            )
            .await
        {
            Ok(r) if r.persist_error.is_none() => {
                cursor.insert(e.id.clone());
                ok += 1;
                tracing::info!(
                    op = "billing.ledger",
                    tenant = %e.tenant,
                    entry_id = %e.id,
                    seq = r.seq,
                    committed = r.committed,
                    proof = r.proof.is_some(),
                    "contract: canonical ledger row attested"
                );
            }
            other => {
                bad += 1;
                let detail = match other {
                    Ok(r) => format!("persist error on seq {}: {:?}", r.seq, r.persist_error),
                    Err(e) => e.to_string(),
                };
                tracing::error!(
                    op = "billing.ledger",
                    entry_id = %e.id,
                    detail = %detail,
                    "contract: canonical ledger row NOT attested (will retry)"
                );
            }
        }
    }
    save_cursor(&cursor);
    (ok, bad)
}
