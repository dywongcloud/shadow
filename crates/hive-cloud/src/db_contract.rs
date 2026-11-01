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
//!     resolved (`admin::tenant`) and `owns_target` is the verdict of the platform's own ownership
//!     check for this database. Both are handed to `contract::validate`, which fails CLOSED when
//!     either is missing: an untagged or non-owning caller produces a REFUSED entry, never a
//!     committed one. So adding a call here cannot open a path that was closed before it.
//!   * It does not perform the mutation. The caller has already done it and passes its result as
//!     `outcome`; an `Err` is recorded with `success: false` and applies NO state change, so a
//!     database write that never landed is not asserted as one that did.
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
/// `kind` is one of the contract's declared ops (`db.create`, `db.delete`); `extra` carries the
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
                    seq = receipt.seq,
                    error = %e,
                    "contract: managed-db op refused"
                ),
                None => tracing::info!(
                    op = kind,
                    db = %db_id,
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
