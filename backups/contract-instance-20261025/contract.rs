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
    /// proposer mutate tenant data. Unrepresentable by construction, never merely rejected later.
    OpenParticipationMayNotWrite,
    DuplicateOpKind(String),
    EmptyOpKind,
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContractError::OpenParticipationMayNotWrite => f.write_str(
                "open-participation ops are log-only: an unauthenticated proposer may never mutate state",
            ),
            ContractError::DuplicateOpKind(k) => write!(f, "duplicate op kind {k}"),
            ContractError::EmptyOpKind => f.write_str("op kind must be non-empty"),
        }
    }
}

/// One operation kind: its authorization requirement, its effect, and the exact payload keys it
/// accepts (a payload key set is a schema; an operation carrying anything else is invalid).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpSpec {
    pub auth: AuthPolicy,
    pub effect: Effect,
    pub payload_keys: BTreeSet<String>,
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
    /// contract that would weaken tenancy can never exist in the system to be called.
    pub fn declare(
        &mut self,
        kind: &str,
        auth: AuthPolicy,
        effect: Effect,
        payload_keys: &[&str],
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
        self.ops.insert(
            kind.to_string(),
            OpSpec {
                auth,
                effect,
                payload_keys: payload_keys.iter().map(|k| k.to_string()).collect(),
            },
        );
        Ok(())
    }

    pub fn spec(&self, kind: &str) -> Option<&OpSpec> {
        self.ops.get(kind)
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
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
}

/// Validate `op` against `contract` for `caller`.
///
/// Three gates, in order: the kind is declared, the payload matches the declared key set exactly,
/// and the caller satisfies the kind's [`AuthPolicy`]. The last gate is the one that keeps tenancy
/// intact: a `TenantAuthorized` op with an untagged tenant fails closed, and `owns_target` is
/// supplied by the platform's existing check rather than recomputed here.
pub fn validate<'a>(
    contract: &'a Contract,
    caller: &Caller,
    kind: &str,
    payload: &BTreeMap<String, String>,
) -> Result<&'a OpSpec, OpError> {
    let spec = contract.spec(kind).ok_or_else(|| OpError::UndeclaredKind(kind.to_string()))?;

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

    match spec.auth {
        AuthPolicy::TenantAuthorized => {
            if caller.tenant.trim().is_empty() || caller.tenant == UNTAGGED_TENANT {
                return Err(OpError::UntaggedTenant);
            }
            if !caller.owns_target {
                return Err(OpError::TenantMismatch);
            }
        }
        // Open participation is admitted on identity alone: it is only reachable for LogOnly ops,
        // which `Contract::declare` guarantees, so admitting it here cannot reach tenant state.
        AuthPolicy::OpenParticipation => {}
    }
    Ok(spec)
}

/// The managed-database contract: the default policy set every database mutation path in
/// `hive-cloud` already enforces, expressed as contract terms.
///
/// `db.propose` is the permissionless-participation op: any node may append it, and it is LogOnly,
/// so it cannot touch a tenant's data.
pub fn managed_db_contract() -> Contract {
    let mut c = Contract::new("hive.managed-db.v1");
    c.declare(
        "db.create",
        AuthPolicy::TenantAuthorized,
        Effect::StateWrite,
        &["db_id", "kind", "project"],
    )
    .expect("db.create");
    c.declare(
        "db.delete",
        AuthPolicy::TenantAuthorized,
        Effect::StateWrite,
        &["db_id", "project"],
    )
    .expect("db.delete");
    c.declare(
        "db.write",
        AuthPolicy::TenantAuthorized,
        Effect::StateWrite,
        &["bytes", "db_id", "project"],
    )
    .expect("db.write");
    c.declare(
        "db.propose",
        AuthPolicy::OpenParticipation,
        Effect::LogOnly,
        &["db_id", "note"],
    )
    .expect("db.propose");
    c
}
