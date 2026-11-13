//! Live witness for the balanced double-entry journal and its signed contract binding.
//!
//! NOT a test file and not a mock: a real `Engine`, real `Store`, real Ed25519 `KeyedSigner`, a real
//! filesystem, real `audit::verify` replay, real inclusion/signed-entry proofs, and a real
//! `state.json` round trip when `--state <path>` names one. It posts balanced entries through the
//! ONE seam (`Engine::submit`), REFUSES every invariant violation with its typed error, proves
//! reversal-only correction, detects rewritten/omitted/duplicated/reordered lines and entries by
//! replay, measures single-post vs group-commit, generates a SOC 2 technical-evidence report and
//! independently VERIFIES its signed Merkle commitment.
//!
//! Scope: NODE-LOCAL evidence over `hive.billing-journal.v1`, never fleet-wide. An inclusion proof
//! says an entry WAS published, never that the log still ends there. No consensus, no bond model.
//!
//! Usage: `contract-double-entry [--dir <path>] [--state <path>] [--report <path>]`; exit 0 on
//! RESULT PASS, 1 on FAIL.

use hive_contract::contract::Caller;
use hive_contract::contracts::journal::{
    billing_journal_contract, entry_payload as journal_entry_payload, refusal_payload,
    reconciliation_payload, retention_payload, CONTRACT_ID, CONTRACT_VERSION,
};
use hive_contract::engine::Engine;
use hive_contract::journal::{
    draft_from_legacy_row, range_commitment, verify_range_commitment, AccountStatus, Amount,
    Direction, Journal, JournalDraft, JournalEntry, JournalError, JournalLine, JOURNAL_ENTRY,
    JOURNAL_RECONCILIATION, JOURNAL_RETENTION, CURRENCY_USD,
};
use hive_contract::persist::{read_entries_from, read_signed_entries_from, PersistConfig};
use hive_contract::signing::{CheckpointSigner, KeyedSigner};
use hive_contract::{InclusionProof, LogEntry, OpLog, SignedEntry};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

fn arg(name: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == &format!("--{name}"))
        .and_then(|i| a.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn platform(tenant: &str) -> Caller {
    Caller {
        tenant: tenant.to_string(),
        actor: "platform:meter".to_string(),
        owns_target: true,
        platform_attested: true,
    }
}

fn line(account: &str, direction: Direction, cents: i64) -> JournalLine {
    JournalLine {
        account: account.to_string(),
        direction,
        amount: Amount::new(cents).expect("witness amount is non-zero and in range"),
        currency: CURRENCY_USD.to_string(),
        memo: "witness-note-blake3".to_string(),
    }
}

fn balanced(key: &str, cents: i64, ts: u64) -> JournalDraft {
    JournalDraft {
        lines: vec![
            line("cash", Direction::Debit, cents),
            line("revenue", Direction::Credit, cents),
        ],
        currency: CURRENCY_USD.to_string(),
        idempotency_key: key.to_string(),
        actor: "platform:meter".to_string(),
        second_actor: None,
        posted_ms: ts,
        memo: format!("memo-{key}"),
        kind: hive_contract::journal::EntryKind::Original,
        reverses: None,
    }
}

/// The ONE seam: mutate the journal FIRST, then record the outcome in the signed contract oplog.
/// A REFUSED invariant is a RECORD (a `journal.refusal` entry), never a prevention and never a
/// silent skip. Both outcomes carry a measured `outcome`, never a constant `Ok(())`.
async fn post_and_record(
    engine: &Engine,
    contract: &hive_contract::Contract,
    journal: &mut Journal,
    tenant: &str,
    draft: JournalDraft,
) -> Result<JournalEntry, JournalError> {
    let attempt = journal.writes_attempted().saturating_add(1);
    let key = draft.idempotency_key.clone();
    match journal.post(draft.clone()) {
        Ok(e) => {
            let payload = journal_entry_payload(contract, &e, tenant)
                .expect("journal entry payload matches its declared key set");
            let outcome = if journal.entry(e.seq).is_some() {
                Ok(())
            } else {
                Err(format!("journal entry {} did not read back", e.seq))
            };
            let r = engine
                .submit(
                    contract,
                    platform(tenant),
                    JOURNAL_ENTRY,
                    payload,
                    tenant,
                    e.posted_ms,
                    outcome,
                )
                .await
                .expect("journal entry is recordable");
            assert!(
                r.committed && r.persist_error.is_none(),
                "journal entry was not durably committed"
            );
            Ok(e)
        }
        Err(e) => {
            let payload = refusal_payload(
                tenant,
                &format!("refusal-{attempt}-{}", e.code()),
                draft.posted_ms,
                &e,
                &key,
                attempt,
            );
            let recorded = journal
                .refusals()
                .last()
                .is_some_and(|r| r.invariant_code == e.code());
            let outcome = if recorded {
                Ok(())
            } else {
                Err("typed refusal was not recorded in the WORM audit chain".to_string())
            };
            let r = engine
                .submit(
                    contract,
                    platform(tenant),
                    hive_contract::JOURNAL_REFUSAL,
                    payload,
                    tenant,
                    draft.posted_ms,
                    outcome,
                )
                .await
                .expect("journal refusal is recordable");
            assert!(r.committed, "typed refusal was not recorded as an entry");
            Err(e)
        }
    }
}

#[tokio::main]
async fn main() {
    let base = PathBuf::from(arg(
        "dir",
        &std::env::temp_dir()
            .join(format!("hive-contract-journal-{}", std::process::id()))
            .to_string_lossy(),
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create run dir");
    let state_path = arg("state", "");
    let report_path = arg("report", "");

    let mut probes: Vec<(&'static str, bool, String)> = Vec::new();
    let contract = billing_journal_contract();
    let pkcs8 = KeyedSigner::generate_pkcs8().expect("generate key");
    let node: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("journal-witness", &pkcs8).expect("load key"));
    let pk = hex::encode(node.public_key());
    let pk_bytes = node.public_key();
    let engine = Engine::open(
        4,
        base.join("log"),
        PersistConfig {
            checkpoint_every: 1024,
            retain_checkpoints: 4,
            signer: Some(node.clone()),
            sync: true,
        },
    );
    engine
        .register(contract.clone())
        .await
        .expect("register journal contract");

    // ---- typed invariant probes: each admits a valid entry, then REFUSES its violation ---------
    fn probe(
        name: &'static str,
        expected: &'static str,
        probes: &mut Vec<(&'static str, bool, String)>,
        violating: impl FnOnce(&mut Journal) -> Result<JournalEntry, JournalError>,
    ) {
        let mut j = Journal::default_chart();
        let admitted = j.post(balanced(&format!("valid-{name}"), 100, 1_000)).is_ok();
        let result = violating(&mut j);
        let ok = admitted
            && result.as_ref().is_err_and(|e| e.code() == expected)
            && j.entry(1).is_some();
        probes.push((
            name,
            ok,
            match result {
                Err(e) => format!("{}: {}", e.code(), e),
                Ok(_) => "NO REFUSAL".to_string(),
            },
        ));
    }

    probe(
        "I01 >=2 lines refused",
        "TooFewLines",
        &mut probes,
        |j| {
            let mut d = balanced("too-few", 100, 2_000);
            d.lines.truncate(1);
            j.post(d)
        },
    );
    probe(
        "I02 debits==credits refused",
        "UnbalancedEntry",
        &mut probes,
        |j| {
            let mut d = balanced("unbalanced", 100, 2_000);
            d.lines[1] = line("revenue", Direction::Credit, 90);
            j.post(d)
        },
    );
    probe(
        "I03 zero amount refused",
        "ZeroAmount",
        &mut probes,
        |j| {
            let zero: JournalLine = serde_json::from_str(
                r#"{"account":"cash","direction":"Debit","amount":0,"currency":"USD","memo":"probe"}"#,
            )
            .expect("Deserialize bypasses the Amount newtype");
            let mut d = balanced("zero", 100, 2_000);
            d.lines[0] = zero;
            j.post(d)
        },
    );
    probe(
        "I04 unknown account refused",
        "UnknownAccount",
        &mut probes,
        |j| {
            let mut d = balanced("unknown", 100, 2_000);
            d.lines[1] = line("ghost", Direction::Credit, 100);
            j.post(d)
        },
    );
    probe(
        "I05 closed account refused",
        "AccountClosed",
        &mut probes,
        |j| {
            j.set_status("revenue", AccountStatus::Closed)
                .expect("chart has revenue");
            j.post(balanced("closed", 100, 2_000))
        },
    );
    probe(
        "I06 currency mismatch refused",
        "CurrencyMismatch",
        &mut probes,
        |j| {
            let mut d = balanced("currency", 100, 2_000);
            d.lines[1].currency = "EUR".to_string();
            j.post(d)
        },
    );
    {
        let precision = Amount::parse_minor_units("12.34");
        probes.push((
            "I07 currency precision minor units only",
            precision.is_err() && precision.err().is_some_and(|e| e.code() == "CurrencyPrecision"),
            "12.34 cents refused; no float path".to_string(),
        ));
    }
    probe(
        "I08 negative Asset balance refused",
        "NegativeBalance",
        &mut probes,
        |j| {
            let mut d = balanced("negative", 100, 2_000);
            d.lines[0] = line("cash", Direction::Credit, 200);
            d.lines[1] = line("usage_expense", Direction::Debit, 200);
            j.post(d)
        },
    );
    probe(
        "I09 exposure limit refused",
        "ExposureLimitExceeded",
        &mut probes,
        |j| {
            j.set_controls("cash", Some(50), false, false)
                .expect("chart has cash");
            j.post(balanced("exposure", 100, 2_000))
        },
    );
    probe("I10 legal hold refused", "LegalHold", &mut probes, |j| {
        j.set_controls("cash", None, true, false)
            .expect("chart has cash");
        j.post(balanced("hold", 100, 2_000))
    });
    probe(
        "I11 four-eyes refused",
        "FourEyesRequired",
        &mut probes,
        |j| {
            j.set_controls("equity", None, false, true)
                .expect("chart has equity");
            let mut d = balanced("four-eyes", 100, 2_000);
            d.lines[1] = line("equity", Direction::Credit, 100);
            j.post(d)
        },
    );
    probe(
        "I12 reversal of unposted refused",
        "ReversalTargetNotPosted",
        &mut probes,
        |j| j.reverse(999, "rev-999", "platform:meter", None, 2_000),
    );
    probe(
        "I13 one reversal per entry refused",
        "AlreadyReversed",
        &mut probes,
        |j| {
            j.reverse(1, "rev-1", "platform:meter", None, 2_000)?;
            j.reverse(1, "rev-1-again", "platform:meter", None, 2_001)
        },
    );
    {
        let mut j = Journal::default_chart();
        let admitted = j.post(balanced("dup-key", 100, 1_000)).is_ok();
        let duplicate = j.post(balanced("dup-key", 100, 2_000));
        let ok = admitted
            && duplicate
                .as_ref()
                .is_err_and(|e| e.code() == "IdempotencyConflict");
        probes.push((
            "I14 idempotency duplicate refused",
            ok,
            match duplicate {
                Err(e) => format!("{}: {}", e.code(), e),
                Ok(_) => "NO REFUSAL".to_string(),
            },
        ));
    }
    probe(
        "I15 suspense must be cleared",
        "SuspenseNotCleared",
        &mut probes,
        |j| {
            let mut d = balanced("suspense", 100, 2_000);
            d.lines[0] = line("suspense", Direction::Debit, 100);
            j.post(d)
        },
    );
    probe(
        "I16 correction without reversal refused",
        "CorrectionWithoutReversal",
        &mut probes,
        |j| {
            j.correct(
                1,
                balanced("corr", 100, 2_000).lines,
                "corr-no-rev",
                "platform:meter",
                None,
                2_000,
            )
        },
    );

    // ---- compatibility view: ONE legacy single-entry row maps to a balanced journal entry -----
    {
        let legacy = draft_from_legacy_row(
            "led_compat_1",
            -250,
            "note-hash-blake3",
            10_500,
            "platform:meter",
        )
        .expect("legacy row maps to a balanced draft");
        let mut compat = Journal::default_chart();
        let posted = compat.post(legacy);
        probes.push((
            "B0 legacy single-entry row maps to balanced lines",
            posted.as_ref().is_ok_and(|e| e.lines.len() == 2)
                && compat.global_ledger_equation().is_ok(),
            format!(
                "lines={} legacy_row_unchanged=true",
                posted.map(|e| e.lines.len()).unwrap_or(0)
            ),
        ));
    }

    let invariant_probe_count = probes.len();

    // ---- real journal: balanced entries, reversal-only correction, one recorded refusal --------
    let mut journal = Journal::default_chart();
    let mut seqs: Vec<u64> = Vec::new();
    let mut op_seqs: Vec<u64> = Vec::new();
    for i in 1..=6u64 {
        let e = post_and_record(
            &engine,
            &contract,
            &mut journal,
            "team-a",
            balanced(&format!("entry-{i}"), 100i64 + i as i64, 10_000 + i),
        )
        .await
        .expect("balanced entry posts");
        seqs.push(e.seq);
    }
    let original_bytes = serde_json::to_vec(journal.entry(1).expect("original"))
        .expect("serialize original");
    let refusal = post_and_record(
        &engine,
        &contract,
        &mut journal,
        "team-a",
        {
            let mut d = balanced("recorded-unbalanced", 100, 20_000);
            d.lines[1] = line("revenue", Direction::Credit, 99);
            d
        },
    )
    .await;
    let refusal_ok = refusal.is_err_and(|e| e.code() == "UnbalancedEntry");
    probes.push((
        "B1 refused invariant is a signed oplog RECORD",
        refusal_ok && journal.writes_refused() == 1,
        format!("writes_attempted={} writes_refused={}", journal.writes_attempted(), journal.writes_refused()),
    ));
    let reversal_draft = {
        let target = journal.entry(1).expect("original").clone();
        let lines: Vec<JournalLine> = target
            .lines
            .iter()
            .map(|l| JournalLine {
                account: l.account.clone(),
                direction: l.direction.flip(),
                amount: l.amount,
                currency: l.currency.clone(),
                memo: l.memo.clone(),
            })
            .collect();
        JournalDraft {
            lines,
            currency: CURRENCY_USD.to_string(),
            idempotency_key: "reversal-of-1".to_string(),
            actor: "platform:meter".to_string(),
            second_actor: None,
            posted_ms: 20_001,
            memo: "reversal-of-1".to_string(),
            kind: hive_contract::journal::EntryKind::Reversal,
            reverses: Some(1),
        }
    };
    let reversal = post_and_record(
        &engine,
        &contract,
        &mut journal,
        "team-a",
        reversal_draft,
    )
    .await
    .expect("reversal posts");
    let correction = post_and_record(
        &engine,
        &contract,
        &mut journal,
        "team-a",
        {
            let mut d = balanced("correction-of-1", 105, 20_002);
            d.kind = hive_contract::journal::EntryKind::Correction;
            d.reverses = Some(1);
            d
        },
    )
    .await
    .expect("correction posts");
    let original_unchanged =
        serde_json::to_vec(journal.entry(1).expect("original")).expect("serialize original")
            == original_bytes;
    let second_reversal = journal.reverse(1, "second-reversal", "platform:meter", None, 20_003);
    probes.push((
        "B2 reversal-only correction leaves original byte-identical",
        original_unchanged
            && reversal.kind == hive_contract::journal::EntryKind::Reversal
            && correction.kind == hive_contract::journal::EntryKind::Correction
            && second_reversal.as_ref().is_err_and(|e| e.code() == "AlreadyReversed"),
        format!(
            "reversal_seq={} correction_seq={} second_reversal={:?}",
            reversal.seq,
            correction.seq,
            second_reversal.err().map(|e| e.code().to_string())
        ),
    ));

    // ---- systematic efficiencies: reconciliation, retention, group commit, read fence ----------
    let recon = journal.reconcile(20_010);
    let retention = journal.evaluate_retention(20_011, 0);
    let fence = journal.fence();
    let snapshot = journal.snapshot_at(&fence).expect("snapshot at fence");
    let point_lookup = journal.entry(3).is_some();
    let n = 64usize;
    let mut single = Journal::default_chart();
    let t0 = Instant::now();
    for i in 0..n {
        let _ = single.post(balanced(&format!("single-{i}"), 10 + i as i64, 30_000 + i as u64));
    }
    let single_us = t0.elapsed().as_micros();
    let mut batched = Journal::default_chart();
    let drafts: Vec<JournalDraft> = (0..n)
        .map(|i| balanced(&format!("batch-{i}"), 10 + i as i64, 30_000 + i as u64))
        .collect();
    let t1 = Instant::now();
    let outcomes = batched.post_batch(drafts);
    let batch_us = t1.elapsed().as_micros();
    probes.push((
        "E1 read fence + point lookup + immutable records",
        snapshot.entries.len() == journal.len()
            && point_lookup
            && journal.reconciliations().len() == 1
            && journal.retention_events().len() == 1
            && recon.record_hash == recon.compute_hash()
            && single.len() == n
            && outcomes.iter().all(|o| o.is_ok()),
        format!(
            "fence_seq={} snapshot={} recon={} retention={}",
            fence.sequence,
            snapshot.entries.len(),
            recon.entries_scanned,
            retention.entries_retained
        ),
    ));

    // Reconciliation and retention events are published through the SAME ONE seam.
    for (kind, payload) in [
        (JOURNAL_RECONCILIATION, reconciliation_payload("team-a", &recon)),
        (JOURNAL_RETENTION, retention_payload("team-a", &retention)),
    ] {
        let r = engine
            .submit(
                &contract,
                platform("team-a"),
                kind,
                payload,
                "team-a",
                20_012,
                Ok(()),
            )
            .await
            .expect("audit event is recordable");
        op_seqs.push(r.seq);
        assert!(r.committed && r.persist_error.is_none());
    }

    // ---- signed oplog replay / inclusion / fraud-proof coverage --------------------------------
    let store_dir = engine.store_dir(&contract.id).expect("store dir");
    let entries: Vec<LogEntry> = read_entries_from(&store_dir, 0, Some(&pk_bytes))
        .await
        .expect("read durable journal log");
    op_seqs.clear();
    op_seqs.extend(
        entries
            .iter()
            .filter(|e| e.op.kind == JOURNAL_ENTRY && e.ack.success)
            .map(|e| e.op.seq),
    );
    let head_index = entries
        .last()
        .map(|e| e.ack.index_hash.clone())
        .unwrap_or_default();
    let log = OpLog::restore(0, "", entries.clone()).expect("contract chain verifies");
    let replay_ok = hive_contract::verify(&contract, &log, &head_index).is_ok();
    let replayed = Journal::replay(&entries).expect("journal replays through the same gates");
    probes.push((
        "R1 contract replay + journal replay agree",
        replay_ok && replayed.entries() == journal.entries(),
        format!(
            "oplog_entries={} journal_entries={} head={}",
            entries.len(),
            replayed.len(),
            &head_index[..12.min(head_index.len())]
        ),
    ));
    let signed: Vec<SignedEntry> = read_signed_entries_from(&store_dir, 0, &pk_bytes)
        .await
        .expect("read signed entries");
    let signed_ok = signed.len() == entries.len() && signed.iter().all(|s| s.verify());
    let mut proofs_ok = signed_ok;
    for seq in &op_seqs {
        let p: Option<InclusionProof> = engine.inclusion_proof(&contract, *seq).await;
        proofs_ok &= p
            .as_ref()
            .is_some_and(|p| p.verify(&contract, Some(&pk)).is_ok());
        proofs_ok &= p
            .as_ref()
            .is_some_and(|p| p.verify(&contract, Some(&"0".repeat(64))).is_err());
    }
    probes.push((
        "R2 signed entries + pinned inclusion proofs",
        proofs_ok,
        format!("signed={} proofs={}", signed.len(), op_seqs.len()),
    ));

    // ---- tamper probes: rewritten / omitted / duplicated / reordered LINES and ENTRIES ---------
    let genuine: Vec<JournalEntry> = journal.entries().to_vec();
    let mut rewritten = genuine.clone();
    rewritten[0].lines[0].amount = Amount::new(9999).expect("non-zero");
    let mut omitted_line = genuine.clone();
    omitted_line[0].lines.remove(0);
    let mut duplicated_line = genuine.clone();
    duplicated_line[0].lines.push(genuine[0].lines[0].clone());
    let mut reordered_line = genuine.clone();
    reordered_line[0].lines.swap(0, 1);
    let mut omitted_entry = genuine.clone();
    omitted_entry.remove(2);
    let mut duplicated_entry = genuine.clone();
    duplicated_entry.insert(2, genuine[2].clone());
    let mut reordered_entry = genuine.clone();
    reordered_entry.swap(2, 3);
    let tamper_cases: [(&str, Vec<JournalEntry>); 7] = [
        ("rewritten line", rewritten),
        ("omitted line", omitted_line),
        ("duplicated line", duplicated_line),
        ("reordered lines", reordered_line),
        ("omitted entry", omitted_entry),
        ("duplicated entry", duplicated_entry),
        ("reordered entries", reordered_entry),
    ];
    let mut detected = 0usize;
    for (name, tampered) in &tamper_cases {
        if Journal::detect_tamper(&entries, tampered).is_err() {
            detected += 1;
        } else {
            probes.push((
                "R3 tamper detection",
                false,
                format!("{name} was NOT detected"),
            ));
        }
    }
    let mut dropped = entries.clone();
    dropped.remove(3);
    let mut duplicated = entries.clone();
    duplicated.insert(3, entries[3].clone());
    let mut reordered = entries.clone();
    reordered.swap(2, 3);
    let chain_tamper = OpLog::restore(0, "", dropped).is_err()
        && OpLog::restore(0, "", duplicated).is_err()
        && OpLog::restore(0, "", reordered.clone()).is_err()
        && Journal::replay(&reordered).is_err();
    probes.push((
        "R3 replay detects 7 line/entry tamper cases + chain tamper",
        detected == 7 && chain_tamper && Journal::detect_tamper(&entries, &genuine).is_ok(),
        format!("detected={detected}/7 chain_tamper={chain_tamper}"),
    ));

    // ---- sequence / chain / GLE corruption ------------------------------------------------------
    let mut corrupted_value = serde_json::to_value(&journal).expect("serialize journal");
    corrupted_value["entries"][0]["content_hash"] = serde_json::json!("0".repeat(64));
    let corrupted: Journal =
        serde_json::from_value(corrupted_value).expect("Deserialize bypasses private fields");
    let chain_break = corrupted.verify();
    let mut gled_value = serde_json::to_value(&journal).expect("serialize journal");
    gled_value["balances"]["cash"] = serde_json::json!(999_999);
    let gled: Journal =
        serde_json::from_value(gled_value).expect("Deserialize bypasses private fields");
    let gle = gled.global_ledger_equation();
    let sequence = Journal::replay(&{
        let mut e = entries.clone();
        e.retain(|x| x.op.kind != JOURNAL_ENTRY || x.op.payload.get("entry_seq").map(String::as_str) != Some("3"));
        e
    });
    probes.push((
        "R4 typed ChainBreak / GLE / SequenceViolation",
        chain_break.as_ref().is_err_and(|e| e.code() == "ChainBreak")
            && gle.as_ref().is_err_and(|e| e.code() == "GlobalLedgerEquation")
            && sequence.as_ref().is_err_and(|e| e.code() == "SequenceViolation"),
        format!(
            "chain={:?} gle={:?} sequence={:?}",
            chain_break.err().map(|e| e.code().to_string()),
            gle.err().map(|e| e.code().to_string()),
            sequence.err().map(|e| e.code().to_string())
        ),
    ));

    // ---- state.json round trip through serde ----------------------------------------------------
    // `--state` is OPTIONAL, like the other nine bins: with no file this probe is an informational
    // SKIP, not a failure. A file that WAS supplied and cannot be read, parsed or round-tripped is
    // a hard FAIL -- that is the silent-total-state-loss risk this probe exists to catch.
    let state_raw = if state_path.is_empty() {
        None
    } else {
        Some(std::fs::read_to_string(&state_path))
    };
    let (state_ok, state_detail) = match state_raw {
        None => (
            true,
            "SKIPPED: --state not supplied (informational; supply a real state.json copy for the hard gate)".to_string(),
        ),
        Some(Err(e)) => (false, format!("--state <path> could not be read: {e}")),
        Some(Ok(raw)) if raw.is_empty() => (false, "--state <path> is an empty file".to_string()),
        Some(Ok(raw)) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => {
                let back: serde_json::Value =
                    serde_json::from_str(&serde_json::to_string(&v).expect("serialize state"))
                        .expect("deserialize round trip");
                let keys = v.as_object().map(|m| m.len()).unwrap_or(0);
                let accounts = v
                    .get("billing")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                let rows = v
                    .get("billing_ledger")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                let required = [
                    "id", "tenant", "ts_ms", "kind", "amount_cents", "balance_after_cents", "note",
                ];
                let rows_intact = v
                    .get("billing_ledger")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| {
                        a.iter().all(|r| required.iter().all(|k| r.get(*k).is_some()))
                    })
                    .unwrap_or(true);
                (
                    v == back && keys > 0 && rows_intact,
                    format!("bytes={} keys={keys} accounts={accounts} ledger_rows={rows} required_keys_intact={rows_intact}", raw.len()),
                )
            }
            Err(e) => (false, format!("state.json deserialize failed: {e}")),
        },
    };
    probes.push(("S1 real state.json serde round trip", state_ok, state_detail));

    // ---- signed Merkle commitment over ANY entry range, independently verified ----------------
    let commitment = range_commitment(
        &journal,
        CONTRACT_ID,
        CONTRACT_VERSION,
        2,
        6,
        node.as_ref(),
        20_020,
    )
    .expect("range commitment");
    let verified = verify_range_commitment(&commitment, CONTRACT_ID, &pk);
    let wrong_key = verify_range_commitment(&commitment, CONTRACT_ID, &"0".repeat(64));
    let mut forged = commitment.clone();
    forged.entry_hashes[0] = "0".repeat(64);
    let forged_root = verify_range_commitment(&forged, CONTRACT_ID, &pk);
    probes.push((
        "C1 signed range commitment verifies independently",
        verified.is_ok() && wrong_key.is_err() && forged_root.is_err(),
        format!(
            "range={}..{} root={} verify={:?}",
            commitment.from_seq,
            commitment.to_seq,
            &commitment.checkpoint.checkpoint.merkle_root[..12],
            verified
        ),
    ));

    // ---- SOC 2 technical evidence: machine-derived, period-filtered, independently checked ----
    let evidence = hive_contract::evidence::generate_report(
        &journal,
        &entries,
        "witness-node-local",
        CONTRACT_ID,
        CONTRACT_VERSION,
        10_000,
        20_030,
        node.as_ref(),
        &pk,
    )
    .expect("signed evidence over a non-empty operating period");
    let evidence_check = hive_contract::evidence::verify_report(&evidence, &pk);
    let report_json = serde_json::to_string_pretty(&evidence).expect("serialize report");
    if !report_path.is_empty() {
        let _ = std::fs::write(&report_path, &report_json);
    }
    let boundary_ok = evidence.boundary.contains("TECHNICAL EVIDENCE SUPPORTING AN ASSESSMENT")
        && evidence.boundary.contains("NOT AN ATTESTATION")
        && evidence.boundary.contains("NOT A CERTIFICATION")
        && evidence.boundary.contains("NOT A COMPLIANCE CLAIM")
        && evidence.boundary.contains("independent CPA examination")
        && evidence.scope == "node-local / not fleet-wide"
        && !report_json.contains("COMPLIANT");
    probes.push((
        "SOC2 machine-derived evidence + independent verifier",
        evidence_check.is_ok() && boundary_ok && evidence.type2.entries_in_period > 0,
        format!(
            "controls={} period_entries={} status={:?} verify={:?}",
            evidence.controls.len(),
            evidence.type2.entries_in_period,
            evidence.overall_status,
            evidence_check
        ),
    ));

    // ---- report --------------------------------------------------------------------------------
    println!("== contract-double-entry ==");
    println!("contract            : {}", contract.id);
    println!("node public key     : {}...", &pk[..16.min(pk.len())]);
    println!("scope               : node-local oplog, NOT fleet-wide");
    println!("oplog entries       : {}", entries.len());
    println!("journal entries     : {}", journal.len());
    println!("invariants enforced : 16 typed errors + GLE + suspense");
    println!(
        "measurement         : single_posts={} single_us={single_us} group_posts={} group_us={batch_us} (numbers only, NO speedup claim)",
        n,
        batched.len()
    );
    for (name, ok, detail) in &probes {
        println!(
            "{:<62} {} {}",
            name,
            if *ok { "PASS" } else { "FAIL" },
            detail
        );
    }
    let failed = probes.iter().filter(|(_, ok, _)| !ok).count();
    if failed == 0 {
        println!(
            "RESULT PASS probes={} invariant_probes={} replay_ok={} signed={} proofs={} tamper_detected={}/7 state_round_trip={} commitment_verified={} evidence_verified={} single_us={single_us} group_us={batch_us}",
            probes.len(),
            invariant_probe_count,
            replay_ok,
            signed.len(),
            op_seqs.len(),
            detected,
            state_ok,
            verified.is_ok(),
            evidence_check.is_ok()
        );
        std::process::exit(0);
    }
    println!("RESULT FAIL failed={failed} of {}", probes.len());
    std::process::exit(1);
}
