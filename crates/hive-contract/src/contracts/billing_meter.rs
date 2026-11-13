//! The metering/billing contract INSTANCE: the second smart contract of this crate.
//!
//! It is DATA, exactly like [`crate::contracts::managed_database`]: one declarative table of op
//! kinds, who may emit them, whether they change index state, and their closed payload schema.
//! It exists so compute, storage, bandwidth, workflow/resource and credits events are not merely
//! numbers in a node-local sidecar: each one becomes a signed, attributed, seq-checked,
//! replay-verifiable entry in the same append-only, hash-chained, Ed25519-signed oplog the
//! managed-database contract uses.
//!
//! Every row is `AuthPolicy::PlatformAttested`, NOT `OpenParticipation`. A charge or a meter
//! sample is a claim the PLATFORM makes about its own measurement; giving it open participation
//! would let any node mint a charge against any tenant. `PlatformAttested` + `StateWrite` is an
//! admissible pair (only `OpenParticipation` + `StateWrite` is forbidden), and
//! `crate::contract::validate` requires `Caller::platform_attested`, a server-derived tenant that
//! is not the fail-closed `__untagged__` sentinel, and an ownership verdict.
//!
//! Recording is MEASUREMENT, never authorization: the platform has already moved the meter or
//! written the ledger row before the seam records it. A REFUSED entry is a record that a charge
//! could not be attested, never a prevention of the charge. Charging itself stays behind the
//! operator opt-in `HIVE_BILLING_METER_RESOURCES` and the single
//! `leadership::may_act(BillingMeter)` writer; nothing here enables either.

use crate::contract::{AuthPolicy, Contract, Effect};

/// Stable contract id. A separate id from `hive.managed-db.v1` means a separate executor, shard,
/// durable log directory and signing stream: billing evidence never competes with a database
/// write for the same seq space.
pub const CONTRACT_ID: &str = "hive.billing-meter.v1";
pub const CONTRACT_VERSION: u32 = 1;

pub const METER_SAMPLE: &str = "meter.sample";
pub const BILLING_LEDGER: &str = "billing.ledger";
pub const BILLING_CHECKPOINT: &str = "billing.checkpoint";

/// The payload key that names the target. Its value is the server-derived tenant tag, so the
/// index namespace and the identity the contract authorized can never be two different strings.
pub const TENANT_TAG: &str = "tenant_tag";

/// The twelve cumulative usage dimensions, named exactly as `billing::UsageTotals` names them.
/// A meter sample carries ALL of them: a sample that silently omitted a SKU would be a different
/// op with a different payload, and `validate` refuses it.
pub const METER_SKUS: &[&str] = &[
    "active_cpu_ms",
    "mem_gb_hr_milli",
    "requests",
    "blocked",
    "gpu_ms",
    "browser_requests",
    "db_row_ops",
    "db_storage_byte_ms",
    "object_storage_byte_ms",
    "ingress_bytes",
    "egress_bytes",
    "workflow_steps",
];

/// One row of the contract. `target_key` is always `tenant_tag`, which is always in the payload.
#[derive(Clone, Copy, Debug)]
pub struct DeclaredOp {
    pub kind: &'static str,
    pub auth: AuthPolicy,
    pub effect: Effect,
    pub payload_keys: &'static [&'static str],
    pub target_key: &'static str,
}

const METER_SAMPLE_KEYS: &[&str] = &[
    "tenant_tag",
    "window_end_ms",
    "active_cpu_ms",
    "mem_gb_hr_milli",
    "requests",
    "blocked",
    "gpu_ms",
    "browser_requests",
    "db_row_ops",
    "db_storage_byte_ms",
    "object_storage_byte_ms",
    "ingress_bytes",
    "egress_bytes",
    "workflow_steps",
];

const LEDGER_KEYS: &[&str] = &[
    "tenant_tag",
    "entry_id",
    "entry_ts_ms",
    "entry_kind",
    "amount_cents",
    "balance_after_cents",
    "note_hash",
    // The RAW note (bounded) as well as its digest: compute_ledger_checkpoint hashes the raw
    // note into its SHA-256 leaf, so the signed log must carry it for the period commitment to
    // be recomputable after the raw ledger rows are pruned.
    "note",
];

const CHECKPOINT_KEYS: &[&str] = &[
    "tenant_tag",
    "period_start_ms",
    "period_end_ms",
    "entry_count",
    "commitment",
];

pub const DECLARED_OPS: &[DeclaredOp] = &[
    DeclaredOp {
        kind: METER_SAMPLE,
        auth: AuthPolicy::PlatformAttested,
        effect: Effect::StateWrite,
        payload_keys: METER_SAMPLE_KEYS,
        target_key: TENANT_TAG,
    },
    DeclaredOp {
        kind: BILLING_LEDGER,
        auth: AuthPolicy::PlatformAttested,
        effect: Effect::StateWrite,
        payload_keys: LEDGER_KEYS,
        target_key: TENANT_TAG,
    },
    DeclaredOp {
        kind: BILLING_CHECKPOINT,
        auth: AuthPolicy::PlatformAttested,
        effect: Effect::StateWrite,
        payload_keys: CHECKPOINT_KEYS,
        target_key: TENANT_TAG,
    },
];

/// Build the contract. Every row goes through `Contract::declare`, so the table is held to the
/// same runtime gates a deserialized table is held to by `Contract::assert_admissible`.
pub fn billing_meter_contract() -> Contract {
    let mut c = Contract::new(CONTRACT_ID);
    c.version = CONTRACT_VERSION;
    for op in DECLARED_OPS {
        c.declare(op.kind, op.auth, op.effect, op.payload_keys, op.target_key)
            .unwrap_or_else(|e| panic!("billing-meter contract row '{}' is invalid: {e}", op.kind));
    }
    c
}
