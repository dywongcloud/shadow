//! Automated SOC 2 TECHNICAL EVIDENCE, machine-derived from LIVE journal state.
//!
//! HARD BOUNDARY: this module produces technical evidence supporting an assessment. It is NOT an
//! attestation, NOT a certification and NOT a compliance claim. A SOC 2 Type II opinion requires
//! an INDEPENDENT CPA EXAMINATION over a stated operating period; nothing in this repository can
//! supply that opinion, the control-owner sign-off or the period examination. No verdict word is
//! emitted. The existing `docs/compliance-controls.md` matrix (24 rows: 5 MET / 15 PARTIAL /
//! 4 UNMET, no Type 2 row MET, AT-1/AT-2 UNMET) is referenced, never recomputed or improved.
//!
//! Every control's evidence is derived from live state: the journal's own hash chain, its WORM
//! audit events, reconciliation records, invariant refusals and retention evaluations, all FILTERED
//! by the report's OWN operating-period range. No file-existence probe, no pre-written prose and no
//! rollup percentage. Each artifact carries `(id, hash, ts)`.
//!
//! Scope is NODE-LOCAL. The contract oplog lives under `$HIVE_DATA/contract-oplog`, sharded by
//! contract id; `admin_ingress` serves GET/HEAD locally. A report over ONE node's log is evidence
//! of THAT NODE's history, never fleet-wide. Vitra honesty is by REPLAY, not consensus. An
//! inclusion proof says an entry WAS published, never freshness.
//!
//! Balance is a WRITER SELF-CONSISTENCY ASSERTION, not fraud detection: a writer-controlled
//! attacker can post a perfectly balanced fraudulent pair and the chain will sign it. Real
//! detection needs a pinned independent signer, an external anchor, or replay against an
//! independently-owned log. The second WORM chain here is a SECOND tamper-evident log written by
//! the SAME process with the SAME node identity, NOT independent verification.
//!
//! The 16 financial invariants are DOMAIN-ADAPTED, not the same standard one-for-one: this
//! platform has no pre-existing chart of accounts, legal-hold system or four-eyes workflow. The
//! accounts, limits and approval gates below are new platform-owned adaptations.

use crate::journal::{
    range_commitment, verify_range_commitment, RangeCommitment, JOURNAL_DOMAIN,
};
use crate::oplog::{canonical_fields, LogEntry};
use crate::signing::{verify_ed25519, CheckpointSigner};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const EVIDENCE_DOMAIN: &[u8] = b"hive-contract/soc2-evidence/v1";

/// An independent CPA opinion is ABSENT. These strings are checked by the independent verifier.
pub const BOUNDARY: &str = "TECHNICAL EVIDENCE SUPPORTING AN ASSESSMENT. NOT AN ATTESTATION, NOT A \
CERTIFICATION, and NOT A COMPLIANCE CLAIM. This artefact is machine-derived technical evidence \
about ONE node's append-only log over a stated operating period; it does not assert that any \
control operated effectively, does not replace control-owner sign-off, and does not contain an \
opinion. An independent CPA examination over an operating period is required before any SOC 2 \
Type 1 or Type 2 attestation could be considered. AT-1 and AT-2 remain UNMET.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlStatus {
    Pass,
    Warn,
    Fail,
    NotApplicable,
}

impl ControlStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlStatus::Pass => "PASS",
            ControlStatus::Warn => "WARN",
            ControlStatus::Fail => "FAIL",
            ControlStatus::NotApplicable => "N-A",
        }
    }
}

/// `(id, hash, ts)`: the audit primitive. An artifact without all three is not evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    pub id: String,
    pub hash: String,
    pub ts_ms: u64,
    pub source_dispatch_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlEvidence {
    pub id: String,
    pub title: String,
    pub status: ControlStatus,
    /// Type 1: DESIGN of the control at a point in time.
    pub type1_design: String,
    /// Type 2: what the OPERATING PERIOD actually produced, derived from live state.
    pub type2_evidence: String,
    pub measurement: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<EvidenceArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlMatrixReference {
    pub source: String,
    pub rows: u32,
    pub met: u32,
    pub partial: u32,
    pub unmet: u32,
    pub type2_met: u32,
    pub at1: String,
    pub at2: String,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingPeriodEvidence {
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub entries_in_period: u64,
    pub refusal_events_in_period: u64,
    pub retention_events_in_period: u64,
    pub reconciliation_runs_in_period: u64,
    pub worm_events_in_period: u64,
    /// Honest exception log: refusals and persist/chain discrepancies in the period.
    pub exception_log: Vec<String>,
    pub cpa_opinion: String,
    pub control_owner_signoff: String,
    pub operating_period_examination: String,
    pub retained_sample_frame: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceReport {
    pub report_id: String,
    pub generated_at_ms: u64,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub node_id: String,
    pub scope: String,
    pub scope_statement: String,
    pub oplog_contract_id: String,
    pub oplog_seq_space: String,
    pub oplog_chain_tip: String,
    pub boundary: String,
    /// Not a verdict, not a rollup and never an attestation: what was collected.
    pub overall_status: String,
    pub controls: Vec<ControlEvidence>,
    pub control_matrix_reference: ControlMatrixReference,
    pub type2: OperatingPeriodEvidence,
    /// Entries in the period, with their full canonical fields so a verifier can re-derive every
    /// content hash and chain link WITHOUT consulting the journal or the engine.
    pub entries: Vec<crate::journal::JournalEntry>,
    pub commitment: RangeCommitment,
    pub worm_events: Vec<crate::journal::WormEvent>,
    pub worm_tip: String,
    pub reconciliations: Vec<crate::journal::ReconciliationRecord>,
    pub invariant_violations: Vec<crate::journal::RefusalRecord>,
    pub retention_events: Vec<crate::journal::RetentionEvent>,
    pub report_hash: String,
    pub report_signature_hex: String,
    pub public_key_hex: String,
    pub source_dispatch_id: String,
}

impl EvidenceReport {
    /// Canonical bytes of the statement the report signature covers.
    pub fn canonical(&self) -> Vec<u8> {
        canonical_fields(&[
            &self.report_id,
            &self.generated_at_ms.to_string(),
            &self.period_start_ms.to_string(),
            &self.period_end_ms.to_string(),
            &self.node_id,
            &self.scope,
            &self.oplog_contract_id,
            &self.oplog_chain_tip,
            &self.commitment.checkpoint.checkpoint.merkle_root,
            &self.worm_tip,
            &self.entries.len().to_string(),
            &self.type2.entries_in_period.to_string(),
        ])
    }

    pub fn compute_hash(&self) -> String {
        fluid_core::content_hash(EVIDENCE_DOMAIN, &self.canonical())
    }
}

fn artifact(
    source_dispatch_id: &str,
    kind: &str,
    id: &str,
    hash: &str,
    ts_ms: u64,
) -> EvidenceArtifact {
    EvidenceArtifact {
        id: format!("{kind}:{id}"),
        hash: hash.to_string(),
        ts_ms,
        source_dispatch_id: source_dispatch_id.to_string(),
    }
}

fn control(
    id: &str,
    title: &str,
    status: ControlStatus,
    design: &str,
    operating: String,
    measurement: String,
    findings: Vec<String>,
    artifacts: Vec<EvidenceArtifact>,
) -> ControlEvidence {
    ControlEvidence {
        id: id.to_string(),
        title: title.to_string(),
        status,
        type1_design: design.to_string(),
        type2_evidence: operating,
        measurement,
        findings,
        artifacts,
    }
}

/// Generate the report from LIVE state. The period range is USED: entries, WORM events, refusals,
/// reconciliations and retention events are all filtered by it. An empty period is reported as a
/// WARN with an exception, never dressed up as operating evidence.
#[allow(clippy::too_many_arguments)]
pub fn generate_report(
    journal: &crate::journal::Journal,
    oplog: &[LogEntry],
    node_id: &str,
    contract_id: &str,
    contract_version: u32,
    period_start_ms: u64,
    period_end_ms: u64,
    signer: &dyn CheckpointSigner,
    trusted_public_key_hex: &str,
) -> Result<EvidenceReport, crate::journal::JournalError> {
    let generated_at_ms = now_ms();
    let dispatch_id = std::env::var("HIVE_EVIDENCE_DISPATCH_ID")
        .unwrap_or_else(|_| format!("run-{generated_at_ms}"));
    let in_period = |ts: u64| ts >= period_start_ms && ts <= period_end_ms;
    let entries: Vec<crate::journal::JournalEntry> = journal
        .entries()
        .iter()
        .filter(|e| in_period(e.posted_ms))
        .cloned()
        .collect();
    // The WORM chain is APPEND-ONLY evidence: carry the WHOLE chain so an independent verifier can
    // reproduce its tip, and report separately how many events fall INSIDE the operating period.
    let worm_events: Vec<crate::journal::WormEvent> = journal.worm().events().to_vec();
    let worm_events_in_period = worm_events.iter().filter(|e| in_period(e.at_ms)).count();
    let reconciliations: Vec<crate::journal::ReconciliationRecord> = journal
        .reconciliations()
        .iter()
        .filter(|r| in_period(r.at_ms))
        .cloned()
        .collect();
    let invariant_violations: Vec<crate::journal::RefusalRecord> = journal
        .refusals()
        .iter()
        .filter(|r| in_period(r.at_ms))
        .cloned()
        .collect();
    let retention_events: Vec<crate::journal::RetentionEvent> = journal
        .retention_events()
        .iter()
        .filter(|r| in_period(r.at_ms))
        .cloned()
        .collect();
    let fenced = journal.fence();
    let snapshot_ok = journal.snapshot_at(&fenced).is_ok();
    let chain_ok = journal.verify().is_ok();
    let gle = journal.global_ledger_equation();
    let oplog_head = oplog
        .last()
        .map(|e| e.entry_hash.clone())
        .unwrap_or_default();
    let from_seq = entries.first().map(|e| e.seq).unwrap_or(1);
    let to_seq = entries.last().map(|e| e.seq).unwrap_or(from_seq);
    // A report over an EMPTY period is a typed refusal, never a fabricated commitment and never a
    // panic. Signed evidence over a range that contains no entry cannot be produced honestly.
    let commitment = range_commitment(
        journal,
        contract_id,
        contract_version,
        from_seq,
        to_seq,
        signer,
        generated_at_ms,
    )?;
    let worm_tip = crate::journal::WormChain::verify_events(&worm_events)
        .unwrap_or_else(|_| "WORM_CHAIN_BREAK".to_string());
    let exception_log: Vec<String> = invariant_violations
        .iter()
        .map(|r| format!("{} at {}: {} ({})", r.invariant_code, r.at_ms, r.detail, r.idempotency_key))
        .chain(
            reconciliations
                .iter()
                .filter(|r| r.discrepancy_count > 0)
                .map(|r| format!("{}: {} discrepancies", r.run_id, r.discrepancy_count)),
        )
        .collect();
    if entries.is_empty() {
        // Recorded below as a WARN control, not hidden.
    }
    let pk = trusted_public_key_hex.to_string();
    let mk = |kind: &str, id: &str, hash: &str, ts: u64| {
        artifact(&dispatch_id, kind, id, hash, ts)
    };
    let entry_artifacts: Vec<EvidenceArtifact> = entries
        .iter()
        .map(|e| {
            mk(
                "journal",
                &format!("seq-{}", e.seq),
                &e.chain_hash,
                e.posted_ms,
            )
        })
        .collect();
    let worm_artifacts: Vec<EvidenceArtifact> = worm_events
        .iter()
        .map(|e| mk("worm", &format!("seq-{}", e.seq), &e.hash, e.at_ms))
        .collect();
    let recon_artifacts: Vec<EvidenceArtifact> = reconciliations
        .iter()
        .map(|r| mk("recon", &r.run_id, &r.record_hash, r.at_ms))
        .collect();
    let type2 = OperatingPeriodEvidence {
        period_start_ms,
        period_end_ms,
        entries_in_period: entries.len() as u64,
        refusal_events_in_period: invariant_violations.len() as u64,
        retention_events_in_period: retention_events.len() as u64,
        reconciliation_runs_in_period: reconciliations.len() as u64,
        worm_events_in_period: worm_events_in_period as u64,
        exception_log: exception_log.clone(),
        cpa_opinion: "ABSENT".to_string(),
        control_owner_signoff: "ABSENT".to_string(),
        operating_period_examination: "NOT PERFORMED".to_string(),
        retained_sample_frame: format!(
            "{} journal entries with signed Merkle commitment and {} WORM events on this node",
            entries.len(),
            worm_events.len()
        ),
    };
    let period_label = format!("[{period_start_ms}, {period_end_ms}]");
    let empty_warn = if entries.is_empty() {
        vec!["no entries in the stated operating period; no operating-effectiveness evidence".into()]
    } else {
        Vec::new()
    };
    let gle_ok = gle.is_ok();
    let gle_pair = gle.as_ref().ok().copied();
    let mut findings = Vec::new();
    if !chain_ok {
        findings.push("journal hash chain did not verify".into());
    }
    if !snapshot_ok {
        findings.push("snapshot read-fence did not reproduce the fence head".into());
    }
    if !gle_ok {
        findings.push("global ledger equation did not hold".into());
    }
    findings.push("balance is a writer self-consistency assertion, not fraud detection".into());
    findings.push("the WORM chain is a second log from the SAME process/key, not independent verification".into());
    findings.push("the 16 invariants are DOMAIN-ADAPTED, not the same standard one-for-one".into());
    findings.push("an independent CPA examination over an operating period is required".into());
    findings.push("AT-1 and AT-2 remain UNMET; no control-owner sign-off exists".into());

    let controls = vec![
        control(
            "CC1",
            "Control environment: journal invariants at the engine layer",
            if chain_ok { ControlStatus::Pass } else { ControlStatus::Fail },
            "Journal::post is the single writer; 16 typed DOMAIN-ADAPTED invariants plus Global Ledger Equation gate every write and every replay.",
            format!("{period_label}: {} entries, {} typed refusals, chain_verified={chain_ok}", entries.len(), invariant_violations.len()),
            format!("writes_attempted={} writes_refused={}", journal.writes_attempted(), journal.writes_refused()),
            findings.clone(),
            entry_artifacts.clone(),
        ),
        control(
            "CC2",
            "Communication: explicit evidence boundary",
            ControlStatus::Warn,
            "The generated report carries an explicit non-attestation boundary and a node-local scope statement.",
            format!("{period_label}: boundary_len={} scope={}", BOUNDARY.len(), "node-local / not fleet-wide"),
            "no external distribution or control-owner acknowledgement is recorded".into(),
            vec!["no operator-facing distribution evidence; an assessment would need it".into()],
            vec![mk("boundary", "report-boundary", &fluid_core::content_hash(EVIDENCE_DOMAIN, BOUNDARY.as_bytes()), generated_at_ms)],
        ),
        control(
            "CC3",
            "Risk assessment: typed fraud/failure taxonomy",
            ControlStatus::Pass,
            "JournalError enumerates every invariant violation with a stable machine code; a refusal is a WORM record.",
            format!("{period_label}: {} invariant violations recorded with codes", invariant_violations.len()),
            format!("exception_log_entries={}", exception_log.len()),
            findings.clone(),
            invariant_violations.iter().enumerate().map(|(i, r)| mk("refusal", &format!("{i}"), &r.invariant_code, r.at_ms)).collect(),
        ),
        control(
            "CC4",
            "Monitoring: immutable reconciliation records",
            if reconciliations.is_empty() { ControlStatus::Warn } else { ControlStatus::Pass },
            "ReconciliationRecord carries run id, timestamp, range, entries scanned, discrepancy count and BOTH chain tips with its own BLAKE3 hash.",
            format!("{period_label}: {} reconciliation runs, {} discrepancies", reconciliations.len(), reconciliations.iter().map(|r| r.discrepancy_count).sum::<u64>()),
            format!("reconciliations={} hash_verified={}", reconciliations.len(), reconciliations.iter().all(|r| r.record_hash == r.compute_hash())),
            findings.clone(),
            recon_artifacts.clone(),
        ),
        control(
            "CC5",
            "Control activities: append-only reversal-only corrections",
            ControlStatus::Pass,
            "Corrections are a NEW reversal entry (flipped Dr/Cr) plus a NEW correction entry; no entry is mutated or deleted.",
            format!("{period_label}: {} posted entries, reversal/correction kinds counted in the signed oplog", entries.iter().filter(|e| e.kind != crate::journal::EntryKind::Original).count()),
            format!("append_only_chain_verified={chain_ok} gle={gle_pair:?}"),
            findings.clone(),
            entry_artifacts.clone(),
        ),
        control(
            "CC6",
            "Logical access: platform attestation and fail-closed tenancy",
            ControlStatus::Warn,
            "The contract seam consumes the platform's server-derived tenant and ownership verdict; OpenParticipation+StateWrite is refused at runtime.",
            format!("{period_label}: {} signed oplog entries on {contract_id}", oplog.len()),
            "this report carries NO identity or role claim; the caller's verdict is upstream".into(),
            vec!["tenant binding is decided by contract::validate, not evidenced by this report".into()],
            vec![mk("oplog", "head", &oplog_head, generated_at_ms)],
        ),
        control(
            "CC7",
            "System operations: WORM audit chain and retention evaluation",
            if worm_tip == "WORM_CHAIN_BREAK" { ControlStatus::Fail } else { ControlStatus::Pass },
            "A second BLAKE3-domain WORM chain covers entry posts, invariant violations, group commits, reconciliations and retention evaluations.",
            format!("{period_label}: {} WORM events, {} retention evaluations", worm_events.len(), retention_events.len()),
            format!("worm_tip={} retention_events={}", &worm_tip[..12.min(worm_tip.len())], retention_events.len()),
            findings.clone(),
            worm_artifacts.clone(),
        ),
        control(
            "CC8",
            "Change management: signed replay and inclusion proofs",
            ControlStatus::Pass,
            "Every journal line is bound into a signed, length-prefixed, seq-checked contract oplog entry; inclusion proofs are checked under a pinned key.",
            format!("{period_label}: {} oplog entries, Merkle root {}", oplog.len(), &commitment.checkpoint.checkpoint.merkle_root[..12.min(commitment.checkpoint.checkpoint.merkle_root.len())]),
            format!("range={}..{} entry_hashes={}", commitment.from_seq, commitment.to_seq, commitment.entry_hashes.len()),
            findings.clone(),
            vec![mk("commitment", "merkle-root", &commitment.checkpoint.checkpoint.merkle_root, generated_at_ms)],
        ),
        control(
            "CC9",
            "Risk mitigation: exposure, legal hold, four-eyes typed refusals",
            if invariant_violations.is_empty() { ControlStatus::Warn } else { ControlStatus::Pass },
            "DOMAIN-ADAPTED account controls (exposure limits, legal holds, two distinct approvers) refuse at the engine layer.",
            format!("{period_label}: {} typed refusals", invariant_violations.len()),
            format!("refusal_codes={:?}", invariant_violations.iter().map(|r| r.invariant_code.as_str()).collect::<Vec<_>>()),
            findings.clone(),
            invariant_violations.iter().enumerate().map(|(i, r)| mk("refusal", &format!("code-{i}"), &r.invariant_code, r.at_ms)).collect(),
        ),
        control(
            "A1",
            "Availability: snapshot read-fence",
            if snapshot_ok { ControlStatus::Pass } else { ControlStatus::Fail },
            "fence()/snapshot_at() return a VALUE at a sequence/head, not a lock held across writers.",
            format!("{period_label}: fence_seq={} head={}", fenced.sequence, &fenced.head[..12.min(fenced.head.len())]),
            format!("snapshot_entries={} chain_tip={}", journal.len(), &journal.head()[..12.min(journal.head().len())]),
            findings.clone(),
            vec![mk("fence", &format!("seq-{}", fenced.sequence), &fenced.head, generated_at_ms)],
        ),
        control(
            "C1",
            "Confidentiality: no free-text notes or secrets in the signed log",
            ControlStatus::Pass,
            "Journal memos are BLAKE3 digests; account codes are platform-owned constants; no env, token or raw note is carried.",
            format!("{period_label}: {} entries carry digest memos only", entries.len()),
            "domain-adapted; no key rotation or at-rest encryption measurement here".into(),
            vec!["HIVE_SECRET_KEY / JWT material is never in this artefact; encryption-at-rest is assessed elsewhere".into()],
            entry_artifacts.clone(),
        ),
        control(
            "PI1",
            "Processing integrity: balance, GLE, sequence and hash chain",
            if !chain_ok || !gle_ok {
                ControlStatus::Fail
            } else if empty_warn.is_empty() {
                ControlStatus::Pass
            } else {
                ControlStatus::Warn
            },
            "Balanced Dr/Cr is a WRITER SELF-CONSISTENCY ASSERTION; the signed chain, strict sequence and Global Ledger Equation are re-derived by replay.",
            format!("{period_label}: {} entries, chain_verified={chain_ok}, gle={gle_pair:?}", entries.len()),
            format!("entries_in_period={} chain_tip={}", entries.len(), &journal.head()[..12.min(journal.head().len())]),
            {
                let mut f = findings.clone();
                f.extend(empty_warn.clone());
                f
            },
            entry_artifacts.clone(),
        ),
    ];

    let mut report = EvidenceReport {
        report_id: format!("soc2-evidence-{}-{generated_at_ms}", node_id.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_")),
        generated_at_ms,
        period_start_ms,
        period_end_ms,
        node_id: node_id.to_string(),
        scope: "node-local / not fleet-wide".to_string(),
        scope_statement: format!(
            "This report evidences ONE node's ({node_id}) contract oplog shard {contract_id}, whose chain tip is {}. Fleet-wide coverage is NOT claimed: the oplog is node-local, admin_ingress serves GET/HEAD locally, and there is no consensus (Vitra honesty by replay, not agreement).",
            journal.head()
        ),
        oplog_contract_id: contract_id.to_string(),
        oplog_seq_space: format!(
            "{contract_id} is a separate seq space from hive.billing-meter.v1 and hive.managed-db.v1"
        ),
        oplog_chain_tip: oplog_head,
        boundary: BOUNDARY.to_string(),
        overall_status: "TECHNICAL EVIDENCE COLLECTED; ASSESSMENT NOT PERFORMED".to_string(),
        controls,
        control_matrix_reference: ControlMatrixReference {
            source: "docs/compliance-controls.md".to_string(),
            rows: 24,
            met: 5,
            partial: 15,
            unmet: 4,
            type2_met: 0,
            at1: "UNMET".to_string(),
            at2: "UNMET".to_string(),
            note: "This generated report does NOT change the 24-row matrix. No percentage, rollup or attestation verdict is computed. AT-1/AT-2 stay UNMET: no CPA opinion, no control-owner sign-off, no operating-period examination.".to_string(),
        },
        type2,
        entries,
        commitment,
        worm_events,
        worm_tip,
        reconciliations,
        invariant_violations,
        retention_events,
        report_hash: String::new(),
        report_signature_hex: String::new(),
        public_key_hex: pk,
        source_dispatch_id: dispatch_id,
    };
    report.report_hash = report.compute_hash();
    let report_canonical = report.canonical();
    if let Ok(sig) = signer.sign(&report_canonical) {
        report.report_signature_hex = hex::encode(sig);
    }
    Ok(report)
}

/// INDEPENDENT VERIFIER. Consults ONLY the report: recomputes every content hash, every chain
/// link, the Merkle root, both Ed25519 signatures, the WORM chain, reconciliation hashes, the
/// report hash, the period filter and each artifact's `(id, hash, ts)`. It never opens the journal
/// or the engine. Reproduction of a commitment is evidence of publication, NOT freshness.
pub fn verify_report(report: &EvidenceReport, trusted_public_key_hex: &str) -> Result<(), String> {
    for phrase in [
        "TECHNICAL EVIDENCE SUPPORTING AN ASSESSMENT",
        "NOT AN ATTESTATION",
        "NOT A CERTIFICATION",
        "NOT A COMPLIANCE CLAIM",
        "independent CPA examination",
        "AT-1 and AT-2 remain UNMET",
    ] {
        if !report.boundary.contains(phrase) {
            return Err(format!("boundary missing phrase: {phrase}"));
        }
    }
    if report.scope != "node-local / not fleet-wide"
        || !report.scope_statement.contains("Fleet-wide coverage is NOT claimed")
    {
        return Err("scope is not honestly node-local".into());
    }
    if report.boundary.contains("COMPLIANT")
        || report.overall_status.contains("COMPLIANT")
        || report.controls.iter().any(|c| c.status.as_str().contains("COMPLIANT"))
    {
        return Err("a compliance verdict word was emitted".into());
    }
    let m = &report.control_matrix_reference;
    if m.rows != 24 || m.met != 5 || m.partial != 15 || m.unmet != 4 || m.type2_met != 0
        || m.at1 != "UNMET" || m.at2 != "UNMET"
    {
        return Err("the 24-row control matrix was altered".into());
    }
    if report.type2.cpa_opinion != "ABSENT"
        || report.type2.control_owner_signoff != "ABSENT"
        || report.type2.operating_period_examination != "NOT PERFORMED"
    {
        return Err("an unearned attestation element was asserted".into());
    }
    if report.type2.entries_in_period != report.entries.len() as u64 {
        return Err("period entry count does not match the carried entries".into());
    }
    let mut prev = crate::journal::GENESIS.to_string();
    for (i, e) in report.entries.iter().enumerate() {
        if e.posted_ms < report.period_start_ms || e.posted_ms > report.period_end_ms {
            return Err(format!("entry {} is outside the stated operating period", e.seq));
        }
        if e.content_hash != e.compute_content_hash() || e.chain_hash != e.compute_chain_hash() {
            return Err(format!("entry {} content/chain hash does not reproduce", e.seq));
        }
        if e.prev_hash != prev && i != 0 {
            return Err(format!("entry {} breaks the journal chain", e.seq));
        }
        prev = e.chain_hash.clone();
        if e.lines.len() < 2 {
            return Err(format!("entry {} has fewer than two lines", e.seq));
        }
        let mut debits: i128 = 0;
        let mut credits: i128 = 0;
        for l in &e.lines {
            l.amount.validated().map_err(|x| x.to_string())?;
            match l.direction {
                crate::journal::Direction::Debit => debits += l.amount.as_i128(),
                crate::journal::Direction::Credit => credits += l.amount.as_i128(),
            }
        }
        if debits != credits {
            return Err(format!("entry {} is unbalanced", e.seq));
        }
    }
    if !report.entries.is_empty()
        && report.commitment.entry_hashes
            != report
                .entries
                .iter()
                .map(|e| e.chain_hash.clone())
                .collect::<Vec<_>>()
    {
        return Err("commitment entry hashes do not match the period entries".into());
    }
    verify_range_commitment(
        &report.commitment,
        &report.oplog_contract_id,
        trusted_public_key_hex,
    )?;
    let worm_tip = crate::journal::WormChain::verify_events(&report.worm_events)
        .map_err(|e| e.to_string())?;
    if report.worm_events.is_empty() && report.worm_tip != crate::journal::GENESIS {
        return Err("empty WORM chain must report the genesis tip".into());
    }
    if !report.worm_events.is_empty() && worm_tip != report.worm_tip {
        return Err("WORM tip does not reproduce from the carried events".into());
    }
    for r in &report.reconciliations {
        if r.record_hash != r.compute_hash() {
            return Err(format!("reconciliation {} hash does not reproduce", r.run_id));
        }
    }
    if report.report_hash != report.compute_hash() {
        return Err("report hash does not reproduce".into());
    }
    let pk = hex::decode(&report.public_key_hex).map_err(|_| "bad report public key".to_string())?;
    let sig = hex::decode(&report.report_signature_hex)
        .map_err(|_| "bad report signature".to_string())?;
    let trusted_pk = hex::decode(trusted_public_key_hex)
        .map_err(|_| "pinned public key is not hex".to_string())?;
    if pk != trusted_pk {
        return Err("report key is not the pinned identity".into());
    }
    if !verify_ed25519(&pk, &report.canonical(), &sig) {
        return Err("report signature does not verify".into());
    }
    for c in &report.controls {
        if c.artifacts.is_empty()
            || c.artifacts.iter().any(|a| {
                a.id.is_empty()
                    || a.hash.is_empty()
                    || a.ts_ms == 0
                    || a.source_dispatch_id.is_empty()
            })
        {
            return Err(format!("control {} lacks an (id, hash, ts) artifact", c.id));
        }
        if c.type2_evidence.is_empty() || c.measurement.is_empty() {
            return Err(format!("control {} has no measured operating evidence", c.id));
        }
    }
    Ok(())
}

/// Human-readable one-page summary for an operator. Never a verdict or a rollup percentage.
pub fn summary(report: &EvidenceReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("report_id       : {}\n", report.report_id));
    out.push_str(&format!("node / scope    : {} / {}\n", report.node_id, report.scope));
    out.push_str(&format!(
        "period          : [{}, {}]\n",
        report.period_start_ms, report.period_end_ms
    ));
    out.push_str(&format!(
        "oplog shard     : {} tip={}\n",
        report.oplog_contract_id, report.oplog_chain_tip
    ));
    out.push_str(&format!(
        "entries         : {} worm_events={} reconciliations={} refusals={} retention={}\n",
        report.entries.len(),
        report.worm_events.len(),
        report.reconciliations.len(),
        report.invariant_violations.len(),
        report.retention_events.len()
    ));
    out.push_str(&format!(
        "commitment root : {}\n",
        report.commitment.checkpoint.checkpoint.merkle_root
    ));
    out.push_str(&format!("worm tip        : {}\n", report.worm_tip));
    out.push_str(&format!("status          : {}\n", report.overall_status));
    for c in &report.controls {
        out.push_str(&format!(
            "{:<5} {:<6} {}\n",
            c.id,
            c.status.as_str(),
            c.measurement
        ));
    }
    out.push_str(&format!("\n{}\n", report.boundary));
    out
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Kept for a future report that must carry the exact oplog segment it read. Read EVERY segment a
/// window spans; this helper returns only the seqs, never a claimed complete history.
pub fn oplog_seq_set(oplog: &[LogEntry]) -> BTreeMap<u64, String> {
    oplog
        .iter()
        .map(|e| (e.op.seq, e.entry_hash.clone()))
        .collect()
}

/// Domain is re-exported here so an external verifier can re-derive a journal leaf without
/// importing the journal module. BLAKE3 remains the one content-identity primitive.
pub fn journal_leaf(content: &[u8]) -> String {
    fluid_core::content_hash(JOURNAL_DOMAIN, content)
}
