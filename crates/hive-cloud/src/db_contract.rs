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
        owns_target: owns(tenant) && owns_target,
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
            };
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
