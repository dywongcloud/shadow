//! The THIRD contract instance: the balanced double-entry journal.
//!
//! DATA, not code. Every row is `PlatformAttested` (the platform measured the billing event) and
//! `StateWrite`, so `apply` produces an index row replay can compare against. The forbidden
//! `OpenParticipation` + `StateWrite` pair is refused by `Contract::declare` and rechecked by
//! `assert_admissible` on EVERY validate/commit/submit.
//!
//! A `journal.refusal` records a typed journal invariant violation. It is a RECORD, never a
//! prevention: the platform's single-entry ledger row already exists by the time this is submitted.

use crate::contract::{AuthPolicy, Contract, Effect};
use crate::journal::{
    JournalEntry, JournalError, JOURNAL_CHECKPOINT, JOURNAL_CHECKPOINT_KEYS, JOURNAL_ENTRY,
    JOURNAL_ENTRY_KEYS, JOURNAL_RECONCILIATION, JOURNAL_RECON_KEYS, JOURNAL_REFUSAL,
    JOURNAL_REFUSAL_KEYS, JOURNAL_RETENTION, JOURNAL_RETENTION_KEYS,
};

/// A separate log directory and seq space from the metering contract and the managed-db contract.
pub const CONTRACT_ID: &str = "hive.billing-journal.v1";
pub const CONTRACT_VERSION: u32 = 1;
pub const TENANT_TAG: &str = "tenant_tag";

pub const DECLARED_OPS: &[(&str, &[&str])] = &[
    (JOURNAL_ENTRY, JOURNAL_ENTRY_KEYS),
    (JOURNAL_REFUSAL, JOURNAL_REFUSAL_KEYS),
    (JOURNAL_RECONCILIATION, JOURNAL_RECON_KEYS),
    (JOURNAL_RETENTION, JOURNAL_RETENTION_KEYS),
    (JOURNAL_CHECKPOINT, JOURNAL_CHECKPOINT_KEYS),
];

pub fn billing_journal_contract() -> Contract {
    let mut c = Contract::new(CONTRACT_ID);
    c.version = CONTRACT_VERSION;
    for (kind, keys) in DECLARED_OPS {
        c.declare(
            kind,
            AuthPolicy::PlatformAttested,
            Effect::StateWrite,
            keys,
            TENANT_TAG,
        )
        .unwrap_or_else(|e| panic!("billing-journal contract row '{kind}' is invalid: {e}"));
    }
    c
}

/// Payload of one typed invariant refusal. Counted as a write even though the journal refused it.
pub fn refusal_payload(
    tenant_tag: &str,
    refusal_id: &str,
    at_ms: u64,
    e: &JournalError,
    idempotency_key: &str,
    writes_attempted: u64,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (TENANT_TAG.to_string(), tenant_tag.to_string()),
        ("refusal_id".to_string(), refusal_id.to_string()),
        ("at_ms".to_string(), at_ms.to_string()),
        ("invariant_code".to_string(), e.code().to_string()),
        ("detail".to_string(), e.to_string()),
        ("idempotency_key".to_string(), idempotency_key.to_string()),
        (
            "writes_attempted".to_string(),
            writes_attempted.to_string(),
        ),
    ])
}

/// Payload of one immutable reconciliation record.
pub fn reconciliation_payload(
    tenant_tag: &str,
    r: &crate::journal::ReconciliationRecord,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (TENANT_TAG.to_string(), tenant_tag.to_string()),
        ("run_id".to_string(), r.run_id.clone()),
        ("at_ms".to_string(), r.at_ms.to_string()),
        ("from_seq".to_string(), r.from_seq.to_string()),
        ("to_seq".to_string(), r.to_seq.to_string()),
        (
            "entries_scanned".to_string(),
            r.entries_scanned.to_string(),
        ),
        (
            "discrepancy_count".to_string(),
            r.discrepancy_count.to_string(),
        ),
        ("chain_tip".to_string(), r.chain_tip.clone()),
        ("worm_tip".to_string(), r.worm_tip.clone()),
        ("record_hash".to_string(), r.record_hash.clone()),
    ])
}

/// Payload of one retention-evaluation audit event.
pub fn retention_payload(
    tenant_tag: &str,
    e: &crate::journal::RetentionEvent,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (TENANT_TAG.to_string(), tenant_tag.to_string()),
        ("event_id".to_string(), e.event_id.clone()),
        ("at_ms".to_string(), e.at_ms.to_string()),
        ("from_seq".to_string(), e.from_seq.to_string()),
        ("to_seq".to_string(), e.to_seq.to_string()),
        (
            "entries_retained".to_string(),
            e.entries_retained.to_string(),
        ),
        ("entries_pruned".to_string(), e.entries_pruned.to_string()),
        ("decision".to_string(), e.decision.clone()),
        ("audit_tip".to_string(), e.audit_tip.clone()),
    ])
}

/// Payload of one Ed25519-signed Merkle range commitment.
pub fn checkpoint_payload(
    tenant_tag: &str,
    c: &crate::journal::RangeCommitment,
    contract_id: &str,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (TENANT_TAG.to_string(), tenant_tag.to_string()),
        ("from_seq".to_string(), c.from_seq.to_string()),
        ("to_seq".to_string(), c.to_seq.to_string()),
        (
            "entry_count".to_string(),
            c.entry_hashes.len().to_string(),
        ),
        (
            "merkle_root".to_string(),
            c.checkpoint.checkpoint.merkle_root.clone(),
        ),
        (
            "journal_tip".to_string(),
            c.checkpoint.checkpoint.log_head.clone(),
        ),
        (
            "worm_tip".to_string(),
            c.checkpoint.checkpoint.prev_head.clone(),
        ),
        ("signature_hex".to_string(), c.signature_hex.clone()),
        ("public_key_hex".to_string(), c.public_key_hex.clone()),
        (
            "signer_id".to_string(),
            c.checkpoint.signer_id.clone(),
        ),
        ("contract_id".to_string(), contract_id.to_string()),
    ])
}

/// Verify that a journal payload names exactly the keys its contract declared.
pub fn payload_is_closed(
    contract: &Contract,
    kind: &str,
    payload: &std::collections::BTreeMap<String, String>,
) -> Result<(), JournalError> {
    let spec = contract
        .spec(kind)
        .ok_or_else(|| JournalError::Refusal(format!("undeclared kind {kind}")))?;
    let declared: std::collections::BTreeSet<&str> =
        spec.payload_keys.iter().map(String::as_str).collect();
    let given: std::collections::BTreeSet<&str> =
        payload.keys().map(String::as_str).collect();
    if declared != given {
        return Err(JournalError::Refusal(format!(
            "payload for {kind} does not match its declared key set"
        )));
    }
    Ok(())
}

/// Convenience: an entry's payload with its tenant tag checked against the journal contract.
pub fn entry_payload(
    contract: &Contract,
    e: &JournalEntry,
    tenant_tag: &str,
) -> Result<std::collections::BTreeMap<String, String>, JournalError> {
    let p = crate::journal::entry_payload(e, tenant_tag);
    payload_is_closed(contract, JOURNAL_ENTRY, &p)?;
    Ok(p)
}
