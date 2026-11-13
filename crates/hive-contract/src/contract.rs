//! Contract definition + operation validation.
//!
//! A contract is DATA, not code: it declares, per operation kind, who may emit the operation
//! ([`AuthPolicy`]) and whether the operation is allowed to change contract state ([`Effect`]).
//! That is the whole of the "smart contract" surface this slice implements -- there is no VM, no
//! guest language, and nothing here can widen an authorization decision.
//!
//! The default policy is [`AuthPolicy::TenantAuthorized`]: an operation that writes tenant data may
//! only be emitted by a caller whose cryptographically-bound tenant identity owns the target. That
//! is the same decision `crates/hive-cloud/src/admin.rs:3368` (`require_project`) already makes;
//! this module records the decision as a contract term so a replaying monitor can PROVE it held,
//! and it refuses to construct a contract that does not.
//!
//! The managed-database contract INSTANCE is NOT in this file. It lives in
//! [`crate::contracts::managed_database`] as one declarative table; this module is the machinery
//! that table is built from and validated against.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// The tenant string a request carries when no tenant is bound. Mirrors
/// `crates/hive-cloud/src/admin.rs:3338` (`record_tenant`): an untagged caller is a fail-closed
/// sentinel and never collapses to "personal".
pub const UNTAGGED_TENANT: &str = "__untagged__";

/// The STORED tenant tag, mirroring `crates/hive-cloud/src/admin.rs:3338` (`record_tenant`): an
/// empty or missing tag is the fail-closed sentinel, never "personal".
///
/// Request-side normalization (`admin.rs:3357` `norm`, empty -> "personal") is deliberately NOT
/// mirrored here. This crate only ever sees the verdict the platform already bound, so there is no
/// path through it that invents a tenant from an unauthenticated request.
pub fn record_tenant(raw: Option<&str>) -> String {
    match raw.unwrap_or("").trim() {
        "" => UNTAGGED_TENANT.to_string(),
        t => t.to_string(),
    }
}

pub const CONTRACT_SCHEMA_VERSION: u32 = 1;

/// Who may emit an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthPolicy {
    /// DEFAULT. The emitter must present a tenant identity bound to a real credential (a JWT claim
    /// or an API key) that owns the operation's target. No other emitter is admissible.
    TenantAuthorized,
    /// Open PARTICIPATION: any node may propose, relay or attest without operator allowlisting.
    /// This is the only sense in which this layer is "permissionless" -- see [`Effect`].
    OpenParticipation,
    /// A PLATFORM-MEASURED event: metering samples and billing-ledger rows. The emitter is platform
    /// code that already performed the measurement, never a tenant request and never an anonymous
    /// proposer. `StateWrite` is admissible here (the forbidden pair is
    /// `OpenParticipation` + `StateWrite` only), but [`validate`] requires the caller to carry
    /// `Caller::platform_attested` AND a server-derived tenant. Extending AuthPolicy is the honest
    /// way to admit these ops: giving them OpenParticipation would let any node mint a charge.
    PlatformAttested,
}

/// Whether an operation may change contract state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Effect {
    /// The operation mutates contract-owned state and is therefore part of the replayable index.
    StateWrite,
    /// The operation is recorded in the log and produces NO state change: proposals, relay
    /// receipts, monitor attestations.
    LogOnly,
}

/// Why a contract definition was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum ContractError {
    /// `OpenParticipation` + `StateWrite` -- the one combination that would let an unauthenticated
    /// proposer mutate tenant data. Refused at RUNTIME by `declare` and rechecked on every USE.
    OpenParticipationMayNotWrite,
    /// A contract reached the executor through a route that skipped `declare` (public fields or
    /// `Deserialize`) and carries a row that would never have been admitted.
    Inadmissible { kind: String, reason: String },
    DuplicateOpKind(String),
    EmptyOpKind,
    /// `target_key` is not one of the kind's declared payload keys: the op could never be validated,
    /// because the value that has to equal the caller's target would never be present.
    TargetKeyNotDeclared { kind: String, key: String },
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContractError::OpenParticipationMayNotWrite => f.write_str(
                "open-participation ops are log-only: an unauthenticated proposer may never mutate state",
            ),
            ContractError::Inadmissible { kind, reason } => {
                write!(f, "contract op kind {kind} is inadmissible: {reason}")
            }
            ContractError::DuplicateOpKind(k) => write!(f, "duplicate op kind {k}"),
            ContractError::EmptyOpKind => f.write_str("op kind must be non-empty"),
            ContractError::TargetKeyNotDeclared { kind, key } => write!(
                f,
                "op kind {kind} names target key {key}, which is not one of its declared payload keys"
            ),
        }
    }
}

/// One operation kind: its authorization requirement, its effect, the exact payload keys it accepts
/// (a payload key set is a schema; an operation carrying anything else is invalid), and WHICH of
/// those keys names the operation's target.
///
/// `target_key` is what binds a validated payload to the namespace the operation writes: the value
/// at that key must equal the target the caller submitted, so the identity the contract authorized
/// and the identity whose state moves cannot be two different strings
/// (contract-target-not-bound-to-payload-db-id).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpSpec {
    pub auth: AuthPolicy,
    pub effect: Effect,
    pub payload_keys: BTreeSet<String>,
    pub target_key: String,
}

/// A contract: an id, a version and a closed set of declared operation kinds.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Contract {
    pub id: String,
    pub version: u32,
    pub ops: BTreeMap<String, OpSpec>,
}

impl Contract {
    pub fn new(id: &str) -> Contract {
        Contract {
            id: id.to_string(),
            version: CONTRACT_SCHEMA_VERSION,
            ops: BTreeMap::new(),
        }
    }

    /// Declare an operation kind. Returns `Err` for the open-participation-writes combination, so a
    /// contract that would weaken tenancy can never exist in the system to be called, and for a
    /// `target_key` outside the declared payload keys, so the target binding can never be vacuous.
    pub fn declare(
        &mut self,
        kind: &str,
        auth: AuthPolicy,
        effect: Effect,
        payload_keys: &[&str],
        target_key: &str,
    ) -> Result<(), ContractError> {
        if kind.trim().is_empty() {
            return Err(ContractError::EmptyOpKind);
        }
        if auth == AuthPolicy::OpenParticipation && effect == Effect::StateWrite {
            return Err(ContractError::OpenParticipationMayNotWrite);
        }
        if self.ops.contains_key(kind) {
            return Err(ContractError::DuplicateOpKind(kind.to_string()));
        }
        if !payload_keys.iter().any(|k| *k == target_key) {
            return Err(ContractError::TargetKeyNotDeclared {
                kind: kind.to_string(),
                key: target_key.to_string(),
            });
        }
        self.ops.insert(
            kind.to_string(),
            OpSpec {
                auth,
                effect,
                payload_keys: payload_keys.iter().map(|k| k.to_string()).collect(),
                target_key: target_key.to_string(),
            },
        );
        Ok(())
    }

    pub fn spec(&self, kind: &str) -> Option<&OpSpec> {
        self.ops.get(kind)
    }

    /// The first declared op kind that makes this contract INADMISSIBLE, if any.
    ///
    /// Two terms are checked, both enforced by `declare` and skippable by a deserialized or
    /// public-field-constructed `Contract`: an `OpenParticipation` op must not be a
    /// `StateWrite`, and a `target_key` must be one of the declared payload keys (otherwise the
    /// target binding in [`validate`] is vacuous).
    pub fn inadmissible_kind(&self) -> Option<(&str, &'static str)> {
        for (kind, spec) in &self.ops {
            if spec.auth == AuthPolicy::OpenParticipation && spec.effect == Effect::StateWrite {
                return Some((kind, "open participation may not write state"));
            }
            if !spec.payload_keys.contains(&spec.target_key) {
                return Some((kind, "target_key is not a declared payload key"));
            }
        }
        None
    }

    /// The ONE admissibility predicate. `declare` refuses an inadmissible ROW before inserting it;
    /// every USE of a contract calls this again, because `Contract` and `OpSpec` have public
    /// fields and derive `Deserialize`: `serde_json::from_str::<Contract>` never goes through
    /// `declare`. A contract reaching `validate`, `Executor::commit` or `Engine::submit` from
    /// another construction route is refused before any state mutation.
    pub fn assert_admissible(&self) -> Result<(), ContractError> {
        match self.inadmissible_kind() {
            Some((kind, reason)) => Err(ContractError::Inadmissible {
                kind: kind.to_string(),
                reason: reason.to_string(),
            }),
            None => Ok(()),
        }
    }
}

/// Why an operation was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum OpError {
    UndeclaredKind(String),
    BadPayload {
        kind: String,
        missing: Vec<String>,
        unexpected: Vec<String>,
    },
    /// A `TenantAuthorized` operation arrived with no bound tenant identity.
    UntaggedTenant,
    /// A caller whose bound tenant does not own the operation's target.
    TenantMismatch,
    /// The submitted target is not the identity the validated payload names. Refused rather than
    /// recorded under the submitted one: otherwise the op the contract authorized and the state the
    /// op moves are two different databases.
    TargetMismatch { target: String, expected: String },
    /// The contract itself is inadmissible (a crafted `Deserialize` or public-field build).
    InadmissibleContract(String),
    /// A `PlatformAttested` op arrived without the platform's measurement attestation.
    PlatformAttestationMissing,
    /// The operation could not be RECORDED: the durable log would not open, because a stored
    /// checkpoint failed to verify or the log directory is unwritable.
    ///
    /// This is a refusal to act, not a note. An op the log cannot hold is indistinguishable from an
    /// op that never happened, which is precisely the silence [`crate::reconcile`] exists to detect --
    /// so it is refused before the operation is applied rather than reported afterwards.
    Store(String),
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::InadmissibleContract(d) => write!(f, "contract is inadmissible: {d}"),
            OpError::PlatformAttestationMissing => f.write_str(
                "platform-attested op carries no platform measurement attestation",
            ),
            OpError::UndeclaredKind(k) => write!(f, "op kind {k} is not declared by this contract"),
            OpError::BadPayload {
                kind,
                missing,
                unexpected,
            } => write!(
                f,
                "op {kind} payload mismatch (missing={missing:?} unexpected={unexpected:?})"
            ),
            OpError::UntaggedTenant => f.write_str("tenant-authorized op carries no bound tenant"),
            OpError::TenantMismatch => f.write_str("caller's tenant does not own the target"),
            OpError::TargetMismatch { target, expected } => write!(
                f,
                "target {target} is not the {expected} the payload names"
            ),
            OpError::Store(d) => write!(f, "contract log is not recordable: {d}"),
        }
    }
}

/// The caller's identity as the platform established it BEFORE this layer is reached: `tenant` is
/// the cryptographically-bound claim (never a header a client set) and `owns_target` is the result
/// of the existing ownership check for this operation's target.
///
/// This struct is the seam. Nothing in this crate ever computes tenancy for itself; it consumes the
/// verdict the platform already produced (`admin::tenant` + `admin::require_project`) and refuses
/// to proceed without it.
#[derive(Clone, Debug)]
pub struct Caller {
    pub tenant: String,
    pub actor: String,
    pub owns_target: bool,
    /// TRUE only when platform code measured this event itself (a meter sample or a billing-ledger
    /// row), never for a request-derived caller. Required by `AuthPolicy::PlatformAttested`.
    /// This is a verdict supplied by the platform seam, exactly like `owns_target`: the contract
    /// consumes it and fails closed without it; it never computes either one.
    pub platform_attested: bool,
}

/// Validate `op` against `contract` for `caller`, acting on `target`.
///
/// Four gates, in order: the kind is declared, the payload matches the declared key set exactly, the
/// payload's declared target key names the target the caller submitted, and the caller satisfies the
/// kind's [`AuthPolicy`]. The last gate is the one that keeps tenancy intact: a `TenantAuthorized`
/// op with an untagged tenant fails closed, and `owns_target` is supplied by the platform's existing
/// check rather than recomputed here.
///
/// The target gate is what makes the other three mean anything: `target` is the string the index is
/// namespaced on, so an op validated against one database and applied to another would move state
/// the contract never authorized.
pub fn validate<'a>(
    contract: &'a Contract,
    caller: &Caller,
    kind: &str,
    payload: &BTreeMap<String, String>,
    target: &str,
) -> Result<&'a OpSpec, OpError> {
    // Recheck the WHOLE contract, not just this row: a deserialized Contract skipped declare.
    contract
        .assert_admissible()
        .map_err(|e| OpError::InadmissibleContract(e.to_string()))?;
    let spec = contract.spec(kind).ok_or_else(|| OpError::UndeclaredKind(kind.to_string()))?;
    // Defense in depth on the exact row being used.
    if spec.auth == AuthPolicy::OpenParticipation && spec.effect == Effect::StateWrite {
        return Err(OpError::InadmissibleContract(format!(
            "op kind {kind} lets open participation write state"
        )));
    }

    let mut missing = Vec::new();
    for k in &spec.payload_keys {
        if !payload.contains_key(k) {
            missing.push(k.clone());
        }
    }
    let mut unexpected = Vec::new();
    for k in payload.keys() {
        if !spec.payload_keys.contains(k) {
            unexpected.push(k.clone());
        }
    }
    if !missing.is_empty() || !unexpected.is_empty() {
        missing.sort();
        unexpected.sort();
        return Err(OpError::BadPayload {
            kind: kind.to_string(),
            missing,
            unexpected,
        });
    }

    // The declared target key is one of the payload keys (Contract::declare refuses it otherwise) and
    // the payload check above proved every declared key is present, so this is a comparison, never a
    // lookup that can come up empty.
    if payload.get(&spec.target_key).map(|v| v.as_str()) != Some(target) {
        return Err(OpError::TargetMismatch {
            target: target.to_string(),
            expected: payload
                .get(&spec.target_key)
                .cloned()
                .unwrap_or_else(|| spec.target_key.clone()),
        });
    }

    match spec.auth {
        AuthPolicy::TenantAuthorized => {
            if caller.tenant.trim().is_empty() || caller.tenant == UNTAGGED_TENANT {
                return Err(OpError::UntaggedTenant);
            }
            if !caller.owns_target {
                return Err(OpError::TenantMismatch);
            }
        }
        // Open participation admits LogOnly ops only. The pair was rechecked above and by
        // assert_admissible: a crafted Contract cannot reach tenant state through this arm.
        AuthPolicy::OpenParticipation => {}
        // Platform-measured metering and billing ops. NOT OpenParticipation + StateWrite: the
        // emitter must be platform code that already measured the event, and the tenant must be
        // server-derived. An untagged or unattributed tenant is refused, never collapsed to a
        // billable namespace.
        AuthPolicy::PlatformAttested => {
            if !caller.platform_attested {
                return Err(OpError::PlatformAttestationMissing);
            }
            if caller.tenant.trim().is_empty() || caller.tenant == UNTAGGED_TENANT {
                return Err(OpError::UntaggedTenant);
            }
            if !caller.owns_target {
                return Err(OpError::TenantMismatch);
            }
        }
    }
    Ok(spec)
}

