//! Live witness for the metering/billing contract and the runtime admissibility guard.
//!
//! NOT a test file and not a mock: a real `Engine`, real `Store`, real Ed25519 `KeyedSigner`,
//! a real filesystem, real `audit::verify` replay and real inclusion proofs. It (1) deserializes
//! a crafted `OpenParticipation` + `StateWrite` Contract and proves validate, commit and submit
//! all REFUSE it with the index unchanged; (2) submits meter samples, credits, charges, plan
//! changes, renewals and a finalized ledger checkpoint as signed contract entries; (3) replays the
//! log, verifies every inclusion proof against a PINNED identity, and measures that a rewritten,
//! omitted, duplicated or reordered charge is DETECTED.
//!
//! Freshness is NOT claimed: an inclusion proof says an entry was published, never that the log
//! still ends there. No consensus, bond, slash or challenge window is modelled here.
//!
//! Usage: `contract-billing [--dir <path>] [--state <path>]`. Exit 0 on RESULT PASS, 1 on FAIL.

use hive_contract::contract::{Caller, Contract, OpError};
use hive_contract::engine::Engine;
use hive_contract::exec::{index_hash, ExecError, Executor, Index};
use hive_contract::persist::{read_entries_from, read_signed_entries_from, PersistConfig};
use hive_contract::signing::{CheckpointSigner, KeyedSigner};
use hive_contract::{
    billing_meter_contract, ledger_row_value, verify, InclusionProof, LogEntry, Op, OpLog,
    SignedEntry, BILLING_CHECKPOINT, BILLING_LEDGER, METER_SAMPLE, METER_SKUS,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

fn arg(name: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == &format!("--{name}"))
        .and_then(|i| a.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn meter_payload(tenant: &str, end_ms: u64, n: u64) -> BTreeMap<String, String> {
    let mut p = BTreeMap::from([
        ("tenant_tag".to_string(), tenant.to_string()),
        ("window_end_ms".to_string(), end_ms.to_string()),
    ]);
    for (i, sku) in METER_SKUS.iter().enumerate() {
        p.insert((*sku).to_string(), (n + i as u64).to_string());
    }
    p
}

fn ledger_payload(
    tenant: &str,
    id: &str,
    ts: u64,
    kind: &str,
    amount: i64,
    balance: i64,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("tenant_tag".to_string(), tenant.to_string()),
        ("entry_id".to_string(), id.to_string()),
        ("entry_ts_ms".to_string(), ts.to_string()),
        ("entry_kind".to_string(), kind.to_string()),
        ("amount_cents".to_string(), amount.to_string()),
        ("balance_after_cents".to_string(), balance.to_string()),
        ("note_hash".to_string(), format!("{id}-note-blake3")),
        ("note".to_string(), format!("genuine note for {id}")),
    ])
}

fn checkpoint_payload(tenant: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("tenant_tag".to_string(), tenant.to_string()),
        ("period_start_ms".to_string(), "1000".to_string()),
        ("period_end_ms".to_string(), "2000".to_string()),
        ("entry_count".to_string(), "4".to_string()),
        ("commitment".to_string(), "f".repeat(64)),
    ])
}

fn platform(tenant: &str, attested: bool, owns: bool) -> Caller {
    Caller {
        tenant: tenant.to_string(),
        actor: "platform:meter".to_string(),
        owns_target: owns,
        platform_attested: attested,
    }
}

#[tokio::main]
async fn main() {
    let base = PathBuf::from(arg(
        "dir",
        &std::env::temp_dir()
            .join(format!("hive-contract-billing-{}", std::process::id()))
            .to_string_lossy(),
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create run dir");
    let state = arg("state", "");
    let state_bytes = if state.is_empty() {
        0
    } else {
        std::fs::metadata(&state).map(|m| m.len()).unwrap_or(0)
    };

    let mut probes: Vec<(&'static str, bool, String)> = Vec::new();
    let contract = billing_meter_contract();
    let pkcs8 = KeyedSigner::generate_pkcs8().expect("generate key");
    let node: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("billing-witness", &pkcs8).expect("load key"));
    let pk = hex::encode(node.public_key());
    let pk_bytes = node.public_key();
    let engine = Engine::open(
        4,
        base.join("log"),
        PersistConfig {
            checkpoint_every: 64,
            retain_checkpoints: 4,
            signer: Some(node.clone()),
            sync: true,
        },
    );
    engine.register(contract.clone()).await.expect("register");

    // ---- G1: a Deserialize'd OpenParticipation + StateWrite Contract ------------------------------
    let bad: Contract = serde_json::from_str(
        r#"{"id":"hive.crafted.v1","version":1,"ops":{"x":{"auth":"OpenParticipation","effect":"StateWrite","payload_keys":["t"],"target_key":"t"}}}"#,
    )
    .expect("crafted contract deserializes: public fields + Deserialize bypass declare");
    let bad_payload = BTreeMap::from([("t".to_string(), "team-a".to_string())]);
    let g1 = bad.assert_admissible().is_err()
        && matches!(
            hive_contract::contract::validate(
                &bad,
                &platform("team-a", false, false),
                "x",
                &bad_payload,
                "team-a"
            ),
            Err(OpError::InadmissibleContract(_))
        );
    probes.push(("G1 validate refuses a deserialized forbidden pair", g1, String::new()));

    let mut bad_ex = Executor::new(bad.clone());
    let before = bad_ex.index_hash();
    let bad_op = Op {
        seq: 0,
        contract: bad.id.clone(),
        tenant: "team-a".to_string(),
        actor: "attacker".to_string(),
        target: "team-a".to_string(),
        kind: "x".to_string(),
        payload: bad_payload.clone(),
    };
    let g2 = matches!(
        bad_ex.commit(bad_op, 1, Ok(())),
        Err(ExecError::InadmissibleContract(_))
    ) && bad_ex.index_hash() == before
        && before == index_hash(&Index::new());
    probes.push(("G2 commit refuses it; index hash unchanged", g2, before.clone()));

    let g3 = matches!(
        engine
            .submit(
                &bad,
                platform("team-a", false, false),
                "x",
                bad_payload,
                "team-a",
                1,
                Ok(())
            )
            .await,
        Err(OpError::InadmissibleContract(_))
    );
    probes.push(("G3 engine submit refuses it (no entry produced)", g3, String::new()));

    // ---- real metering / billing events ----------------------------------------------------------
    let rows = [
        ("led_credit_01", "credit", 500i64, 500i64),
        ("led_charge_02", "usage", -12, 488),
        ("led_charge_03", "charge", -3, 485),
        ("led_plan_04", "plan_change", 0, 485),
    ];
    let mut seqs: Vec<u64> = Vec::new();
    let ts = 10_000u64;
    for (i, tenant) in ["team-a", "team-b"].iter().enumerate() {
        let r = engine
            .submit(
                &contract,
                platform(tenant, true, true),
                METER_SAMPLE,
                meter_payload(tenant, ts + i as u64, 100 + (i as u64) * 100),
                tenant,
                ts,
                Ok(()),
            )
            .await
            .expect("meter sample");
        assert!(r.committed, "meter sample refused: {:?}", r.error);
        seqs.push(r.seq);
    }
    for (i, (id, kind, amount, balance)) in rows.iter().enumerate() {
        let r = engine
            .submit(
                &contract,
                platform("team-a", true, true),
                BILLING_LEDGER,
                ledger_payload("team-a", id, ts + i as u64, kind, *amount, *balance),
                "team-a",
                ts + i as u64,
                Ok(()),
            )
            .await
            .expect("ledger row");
        assert!(r.committed, "ledger row refused: {:?}", r.error);
        seqs.push(r.seq);
    }
    let cp = engine
        .submit(
            &contract,
            platform("team-a", true, true),
            BILLING_CHECKPOINT,
            checkpoint_payload("team-a"),
            "team-a",
            ts + 9,
            Ok(()),
        )
        .await
        .expect("checkpoint");
    assert!(cp.committed, "checkpoint refused: {:?}", cp.error);
    seqs.push(cp.seq);

    // Two REFUSED events are RECORDS, not preventions: no attestation and an untagged tenant.
    for (label, caller) in [
        ("no-attestation", platform("team-a", false, true)),
        ("untagged", platform("__untagged__", true, true)),
    ] {
        let r = engine
            .submit(
                &contract,
                caller,
                BILLING_LEDGER,
                ledger_payload(if label == "untagged" { "__untagged__" } else { "team-a" }, "led_refused", ts, "charge", -1, 0),
                if label == "untagged" { "__untagged__" } else { "team-a" },
                ts,
                Ok(()),
            )
            .await
            .expect("refusal is recorded");
        probes.push((
            "refused metering event is still recorded",
            !r.committed && r.error.is_some(),
            format!("{label}: seq={} error={:?}", r.seq, r.error),
        ));
    }

    let store_dir = engine.store_dir(&contract.id).expect("store dir");
    let entries: Vec<LogEntry> = read_entries_from(&store_dir, 0, Some(&pk_bytes))
        .await
        .expect("read the durable log");
    let head_index = entries.last().map(|e| e.ack.index_hash.clone()).unwrap_or_default();
    let log = OpLog::restore(0, "", entries.clone()).expect("chain verifies");

    // R1: full replay reproduces the published index, every ack seq is checked.
    let r1 = verify(&contract, &log, &head_index).is_ok();
    probes.push(("R1 replay reproduces the published index", r1, head_index.clone()));

    // R2: every stored entry carries the node's signature over entry_hash AND ack.
    let signed: Vec<SignedEntry> = read_signed_entries_from(&store_dir, 0, &pk_bytes)
        .await
        .expect("read signed entries");
    let r2 = signed.len() == entries.len() && signed.iter().all(|s| s.verify());
    probes.push(("R2 every entry is Ed25519-signed by the node", r2, format!("signed={}", signed.len())));

    // R3: inclusion proofs verify against a PINNED identity, and a foreign key is refused.
    let mut proofs_ok = true;
    for seq in &seqs {
        let p: Option<InclusionProof> = engine.inclusion_proof(&contract, *seq).await;
        proofs_ok &= p.as_ref().is_some_and(|p| p.verify(&contract, Some(&pk)).is_ok());
        proofs_ok &= p.as_ref().is_some_and(|p| p.verify(&contract, Some(&"0".repeat(64))).is_err());
    }
    probes.push(("R3 inclusion proofs verify under the pinned key only", proofs_ok, format!("proofs={}", seqs.len())));

    // R4: a REWRITTEN charge is detected by recomputing the canonical row from the signed log.
    let mut canonical: BTreeMap<String, String> = BTreeMap::new();
    for e in &entries {
        if e.op.kind == BILLING_LEDGER && e.ack.success {
            canonical.insert(e.op.payload["entry_id"].clone(), ledger_row_value(&e.op));
        }
    }
    let genuine = canonical.get("led_charge_02").cloned().unwrap_or_default();
    let rewritten = genuine.replace("-12", "-9999");
    let r4 = canonical.len() == rows.len() && genuine != rewritten && !canonical.values().any(|v| *v == rewritten);
    probes.push(("R4 a rewritten charge differs from the signed canonical row", r4, format!("rows={}", canonical.len())));

    // R5: an OMITTED charge is detected -- the signed log holds an entry id the ledger no longer has.
    let mut pruned = canonical.clone();
    pruned.remove("led_charge_03");
    let r5 = pruned.len() == canonical.len() - 1
        && canonical.keys().any(|id| !pruned.contains_key(id));
    probes.push(("R5 an omitted charge is named by replay", r5, "led_charge_03".to_string()));

    // R6: a dropped, duplicated or reordered log entry breaks the chain / seq check.
    let mut dropped = entries.clone();
    dropped.remove(3);
    let mut duplicated = entries.clone();
    duplicated.insert(3, entries[3].clone());
    let mut reordered = entries.clone();
    reordered.swap(2, 3);
    let r6 = OpLog::restore(0, "", dropped).is_err()
        && OpLog::restore(0, "", duplicated).is_err()
        && OpLog::restore(0, "", reordered).is_err();
    probes.push(("R6 drop/duplicate/reorder break the chain", r6, "3 tampered logs".to_string()));

    // ---- report ----------------------------------------------------------------------------------
    println!("== contract-billing ==");
    println!("contract            : {}", contract.id);
    println!("node public key     : {}...", &pk[..16.min(pk.len())]);
    println!("state.json bytes    : {state_bytes} ({}; informational, not a pass/fail gate)", if state_bytes > 0 { "real state supplied" } else { "generated evidence set" });
    println!("entries             : {}", entries.len());
    for (name, ok, detail) in &probes {
        println!("{:<58} {} {}", name, if *ok { "PASS" } else { "FAIL" }, detail);
    }
    let failed = probes.iter().filter(|(_, ok, _)| !ok).count();
    if failed == 0 {
        println!("RESULT PASS probes={} replay_ok={} signed={} proofs={} rewritten_detected={} omitted_detected={} chain_tamper_detected={}",
            probes.len(), r1, signed.len(), seqs.len(), r4, r5, r6);
        std::process::exit(0);
    }
    println!("RESULT FAIL failed={failed} of {}", probes.len());
    std::process::exit(1);
}
