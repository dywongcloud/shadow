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
//!
//! A second, ADDITIVE sample kind `meter.catalog.sample` records the nineteen cumulative
//! counters of the ONE canonical `fluid_core::usage` catalog. It is a DIFFERENT op with a
//! DIFFERENT closed payload schema and a DIFFERENT index namespace: the twelve-key
//! `meter.sample` entries already signed on live nodes keep replaying byte-for-byte unchanged.
//! Both kinds are MEASUREMENT only; neither authorizes a charge, and charging stays behind the
//! operator opt-in.

use crate::contract::{AuthPolicy, Contract, Effect};

/// Stable contract id. A separate id from `hive.managed-db.v1` means a separate executor, shard,
/// durable log directory and signing stream: billing evidence never competes with a database
/// write for the same seq space.
pub const CONTRACT_ID: &str = "hive.billing-meter.v1";
/// Kept at 1 on purpose: adding an op kind is ADDITIVE. A signed checkpoint already on a live
/// node carries this contract_version, and the legacy `meter.sample` payload schema is
/// unchanged, so raising the version would break the replay of entries the nodes already hold.
pub const CONTRACT_VERSION: u32 = 1;

pub const METER_SAMPLE: &str = "meter.sample";
/// Second, additive sample kind. FROZEN legacy `meter.sample` entries keep their own op name,
/// payload schema and `usage/` index namespace; this kind never rewrites them.
pub const METER_CATALOG_SAMPLE: &str = "meter.catalog.sample";
/// Third, additive sample kind: the six AI Gateway meters. Its own closed payload schema
/// and its own `ai/` index namespace, so neither of the two older kinds is rewritten.
pub const METER_AI_SAMPLE: &str = "meter.ai.sample";
pub const BILLING_LEDGER: &str = "billing.ledger";
pub const BILLING_CHECKPOINT: &str = "billing.checkpoint";

/// The payload key that names the target. Its value is the server-derived tenant tag, so the
/// index namespace and the identity the contract authorized can never be two different strings.
pub const TENANT_TAG: &str = "tenant_tag";

/// The twelve LEGACY cumulative usage dimensions, named exactly as `billing::UsageTotals`
/// names them. FROZEN: signed `meter.sample` entries on live nodes replay against this exact
/// closed set, so no key is added, removed or renamed here. A meter sample carries ALL of
/// them: a sample that silently omitted a SKU would be a different op with a different
/// payload, and `validate` refuses it. The canonical catalog counters live in `CATALOG_SKUS`.
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

/// The nineteen cumulative counters of the ONE canonical `fluid_core::usage` catalog.
/// Order MUST equal `fluid_core::usage::COUNTER_KEYS`, which is the fixed order
/// `fluid_core::usage::counter_keys()` returns: the signed payload key set is part of the
/// replay contract, so reordering it changes what a historical entry means. This crate cannot
/// depend on a runtime Vec for a `const` payload schema: `counter_keys()` builds a `Vec` at
/// runtime, so the literals are repeated here in that exact order and the witness binary checks
/// the equality live. Never reorder without a version bump.
pub const CATALOG_SKUS: &[&str] = &[
    "cdn_requests",
    "fast_data_transfer_bytes",
    "workflow_storage_byte_ms",
    "fast_origin_transfer_bytes",
    "workflow_storage_writes_bytes",
    "isr_reads",
    "image_transformations",
    "image_cache_writes",
    "observability_events",
    "web_analytics_events",
    "build_cpu_ms",
    "deployment_storage_byte_ms",
    "function_storage_byte_ms",
    "sandbox_memory_mb_ms",
    "sandbox_active_cpu_ms",
    "sandbox_creations",
    "queue_message_sends",
    "queue_message_receives",
    "queue_message_deletes",
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

/// Closed payload schema of the additive catalog sample: the target, the window, and ALL
/// nineteen catalog keys. A missing or extra key is refused by `validate`, so a sample cannot
/// silently drop a measured counter or smuggle an undeclared one into the signed log.
const CATALOG_SAMPLE_KEYS: &[&str] = &[
    "tenant_tag",
    "window_end_ms",
    "cdn_requests",
    "fast_data_transfer_bytes",
    "workflow_storage_byte_ms",
    "fast_origin_transfer_bytes",
    "workflow_storage_writes_bytes",
    "isr_reads",
    "image_transformations",
    "image_cache_writes",
    "observability_events",
    "web_analytics_events",
    "build_cpu_ms",
    "deployment_storage_byte_ms",
    "function_storage_byte_ms",
    "sandbox_memory_mb_ms",
    "sandbox_active_cpu_ms",
    "sandbox_creations",
    "queue_message_sends",
    "queue_message_receives",
    "queue_message_deletes",
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

/// Closed payload schema of the additive AI sample: the target, the window, and ALL
/// six AI meters. The `ai/` index segment is separate from `usage/` and `catalog/`,
/// so a signed legacy or catalog sample replay stays byte-identical.
pub const AI_SAMPLE_KEYS: &[&str] = &[
    "tenant_tag",
    "window_end_ms",
    "ai_public_micro_cents",
    "ai_cogs_micro_cents",
    "ai_input_tokens",
    "ai_output_tokens",
    "ai_requests",
    "ai_web_search_calls",
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
    // Recording is MEASUREMENT, never authorization: this row only attests counters the platform
    // already measured, and a REFUSED entry is a record, not a prevention. Charging stays behind
    // the operator opt-in `HIVE_BILLING_METER_RESOURCES` and the single
    // `leadership::may_act(BillingMeter)` writer; declaring a second sample kind enables neither.
    // `PlatformAttested` + `StateWrite` is admissible (only OpenParticipation + StateWrite is
    // forbidden), and the new kind writes under its own `catalog/` index segment, so a signed
    // legacy `meter.sample` replay stays byte-identical. CONTRACT_VERSION stays 1: additive.
    DeclaredOp {
        kind: METER_CATALOG_SAMPLE,
        auth: AuthPolicy::PlatformAttested,
        effect: Effect::StateWrite,
        payload_keys: CATALOG_SAMPLE_KEYS,
        target_key: TENANT_TAG,
    },
    // Third ADDITIVE sample kind: the six AI Gateway meters. Recording is MEASUREMENT,
    // never authorization and never a charge: it attests the node-local meter even when
    // AI charging is off and the chargeable delta is zero. PlatformAttested + StateWrite
    // is admissible; the `ai/` index segment keeps both older kinds byte-identical on
    // replay. CONTRACT_VERSION stays 1: no signed payload schema is changed.
    DeclaredOp {
        kind: METER_AI_SAMPLE,
        auth: AuthPolicy::PlatformAttested,
        effect: Effect::StateWrite,
        payload_keys: AI_SAMPLE_KEYS,
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
