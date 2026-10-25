//! The managed-database contract INSTANCE: the actual smart contract of this crate.
//!
//! This file IS the contract. It is one declarative table: every operation kind the contract admits,
//! with, per kind, who may emit it ([`AuthPolicy`]), whether it may change contract state
//! ([`Effect`]), and the exact payload key set it accepts. Everything else in the crate is machinery
//! around this table -- [`crate::contract`] is the type system and the three gates,
//! [`crate::exec`] applies an admitted op, [`crate::audit`] replays, [`crate::replicate`] merges,
//! [`crate::reconcile`] diffs the result against canonical state.
//!
//! It is DATA, not code. There is no VM, no guest language and no scripting surface here, and
//! nothing in this table can widen an authorization decision: the one combination that could --
//! `OpenParticipation` + `StateWrite` -- is refused by [`Contract::declare`], so it cannot be
//! written below even by mistake, and it cannot load at runtime either.
//!
//! How to read it: one row per declared kind. A payload key set is a schema -- an operation carrying
//! a missing key or an unexpected key is refused by `contract::validate`. The set is closed: a kind
//! that is not a row here is not part of the contract. Keeping the table in a file of its own is what
//! makes a change to the contract a diff a reviewer can see.

use crate::contract::{AuthPolicy, Contract, Effect};

/// The contract's stable identifier. Logs, snapshots and replicas are bound to it: a snapshot
/// carrying another contract id is refused (`MergeError::ContractMismatch`). Bumping this renames
/// the contract, and every replica of the old one stops merging with it.
pub const CONTRACT_ID: &str = "hive.managed-db.v1";

/// The version of THIS contract instance, carried in [`Contract::version`] and therefore in every
/// index the contract produces.
///
/// A contract version is not the schema version: the schema ([`crate::contract::CONTRACT_SCHEMA_VERSION`])
/// is how a contract is encoded, this is which contract it is. They are asserted equal today because
/// this is the first version of both -- the assert is what forces a bump of one to be a decision
/// about the other, never a silent drift.
pub const CONTRACT_VERSION: u32 = 1;

const _: () = assert!(CONTRACT_VERSION == crate::contract::CONTRACT_SCHEMA_VERSION);

/// One row of the contract: an operation kind and the three terms that govern it.
#[derive(Clone, Copy, Debug)]
pub struct DeclaredOp {
    pub kind: &'static str,
    /// Who may emit the operation.
    pub auth: AuthPolicy,
    /// Whether the operation may change contract state.
    pub effect: Effect,
    /// The exact payload key set. Exact in both directions: a missing key and an unexpected key are
    /// both refusals.
    pub payload_keys: &'static [&'static str],
    /// Which payload key names the operation's target. `contract::validate` refuses an op whose
    /// submitted target is not this key's value, so the database the contract authorized is the
    /// database whose state moves.
    pub target_key: &'static str,
}

/// The contract: the closed set of declared operation kinds.
///
/// `db.propose` is the permissionless-participation op -- any node may append it, and it is
/// `LogOnly`, so it cannot touch a tenant's data. Every other kind is `TenantAuthorized`: the
/// emitter must present the tenant identity the platform already bound to the operation's target.
pub const DECLARED_OPS: &[DeclaredOp] = &[
    DeclaredOp {
        kind: "db.create",
        auth: AuthPolicy::TenantAuthorized,
        effect: Effect::StateWrite,
        payload_keys: &["db_id", "kind", "project"],
        target_key: "db_id",
    },
    DeclaredOp {
        kind: "db.delete",
        auth: AuthPolicy::TenantAuthorized,
        effect: Effect::StateWrite,
        payload_keys: &["db_id", "project"],
        target_key: "db_id",
    },
    DeclaredOp {
        kind: "db.write",
        auth: AuthPolicy::TenantAuthorized,
        effect: Effect::StateWrite,
        payload_keys: &["bytes", "db_id", "project"],
        target_key: "db_id",
    },
    DeclaredOp {
        kind: "db.propose",
        auth: AuthPolicy::OpenParticipation,
        effect: Effect::LogOnly,
        payload_keys: &["db_id", "note"],
        target_key: "db_id",
    },
];

/// Build the managed-database contract from [`DECLARED_OPS`].
///
/// Every row goes through [`Contract::declare`], so the table is held to the same gates as any other
/// contract: a row that would let an unauthenticated proposer mutate state, or that repeats a kind,
/// does not produce a contract -- it names the row that broke it.
pub fn managed_db_contract() -> Contract {
    let mut c = Contract::new(CONTRACT_ID);
    c.version = CONTRACT_VERSION;
    for op in DECLARED_OPS {
        c.declare(op.kind, op.auth, op.effect, op.payload_keys, op.target_key)
            .unwrap_or_else(|e| panic!("managed-db contract row '{}' is invalid: {e}", op.kind));
    }
    c
}
