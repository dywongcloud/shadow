//! Witness for the Execution-Transparency guarantees the crate did not have: the inclusion proof,
//! the append-only violation proof, the availability check, and a breach notice that carries the
//! executor's signature.
//!
//! Not a test file and not a mock: it runs the real `Engine`, the real `Store` and the real
//! adjudication against a real directory on a real filesystem, with a real Ed25519 keypair, and
//! every artefact it adjudicates is one the code under test produced.
//!
//! Each probe asks the question the whitepaper asks, not the question the code already answers:
//!   * T1/T2 -- can somebody who was NOT watching be shown that this entry was published (2.7, 3.4)?
//!   * T3/T4 -- can two divergent histories be proven from two signed artefacts alone (2.4, 2.6)?
//!   * T5    -- can "the executor will not produce its history" be proven rather than asserted (4.1)?
//!   * T6    -- does a breach notice carry the publisher's signature, so a monitor cannot convict an
//!              honest executor by writing an ack itself (2.6)?
//!
//! Usage: `contract-transparency [--dir <path>]`
//! Exit 0 only when every guarantee holds and every forgery is refused.

use hive_contract::appendonly::{
    adjudicate_fork, check_availability, Availability, ForkedHistory, Withheld,
};
use hive_contract::audit::{adjudicate, Dispute, Verdict};
use hive_contract::checkpoint::{Checkpoint, SignedCheckpoint, TrustedCheckpoint};
use hive_contract::contract::Caller;
use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::Engine;
use hive_contract::exec::{index_hash, Index};
use hive_contract::persist::{
    read_checkpoint, read_latest_checkpoint, read_signed_entries_from, PersistConfig,
};
use hive_contract::signing::{CheckpointSigner, KeyedSigner};
use hive_contract::{InclusionProof, SignedEntry};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Three whole windows, so the boundaries are known without probing for them.
const WINDOW: usize = 32;
const OPS: usize = 96;
/// A short open tail after the last window closed: entries with no checkpoint over them yet.
const TAIL: usize = 3;

struct Probe {
    name: &'static str,
    what: String,
    expect: &'static str,
    observed: String,
    ok: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn short(s: &str) -> String {
    format!("{}...", &s[..s.len().min(16)])
}

#[tokio::main]
async fn main() {
    let base = PathBuf::from(format!(
        "/private/tmp/hive-contract-transparency-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create run dir");

    let contract = managed_db_contract();
    let mut probes: Vec<Probe> = Vec::new();

    // `node` is the executor. `attacker` is a stranger: everything an accuser can mint without the
    // executor's key, which is the identity every forged artefact here is built with.
    let node_pkcs8 = KeyedSigner::generate_pkcs8().expect("generate node key");
    let attacker_pkcs8 = KeyedSigner::generate_pkcs8().expect("generate attacker key");
    let node: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("witness-node", &node_pkcs8).expect("load node key"));
    let attacker: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("attacker", &attacker_pkcs8).expect("load attacker key"));
    let node_pk_bytes = node.public_key();
    let node_pk = hex::encode(&node_pk_bytes);
    let attacker_pk = hex::encode(attacker.public_key());
    let trust = TrustedCheckpoint {
        public_key_hex: &node_pk,
        contract_id: &contract.id,
        contract_version: contract.version,
    };

    println!("== contract-transparency ==");
    println!("node public key     : {}", short(&node_pk));
    println!("attacker public key : {}", short(&attacker_pk));

    let root = base.join("live");
    let engine = Engine::open(
        4,
        root.clone(),
        PersistConfig {
            checkpoint_every: WINDOW,
            retain_checkpoints: 8,
            signer: Some(node.clone()),
            sync: true,
        },
    );
    engine.register(contract.clone()).await.expect("register");
    let store_dir = engine.store_dir(&contract.id).expect("store dir");

    let ts0 = now_ms();
    let mut last_receipt: Option<InclusionProof> = None;
    for i in 0..(OPS + TAIL) {
        let db = format!("db_tx_{:03}", i % 8);
        let payload = BTreeMap::from([
            ("db_id".to_string(), db.clone()),
            ("project".to_string(), "proj-00".to_string()),
            ("bytes".to_string(), i.to_string()),
        ]);
        let r = engine
            .submit(
                &contract,
                Caller {
                    tenant: "team-00".to_string(),
                    actor: "witness".to_string(),
                    owns_target: true, platform_attested: false,
                },
                "db.write",
                payload,
                &db,
                ts0 + i as u64,
                Ok(()),
            )
            .await
            .expect("submit");
        assert!(r.committed, "op {i} not committed: {:?}", r.error);
        last_receipt = r.proof.clone();
    }
    let _ = engine.flush().await;
    let live_hash = engine.index_hash(&contract.id).await.expect("index hash");

    // =========================================================================================
    // T1. THE PARTICIPANT'S RECEIPT (whitepaper 3.4 / 2.7).
    //
    // The submit path hands the caller a proof: the entry, the executor's signature over its hash
    // and its ack. A third party holding only that artefact accepts it -- or refuses it -- without
    // asking anybody.
    // =========================================================================================
    {
        let proof = last_receipt.clone().expect("submit produced a proof");
        let seq = proof.seq();
        let by_stranger = proof.verify(&contract, Some(&node_pk));
        let wrong_key = proof.verify(&contract, Some(&attacker_pk));
        let no_pin = proof.verify(&contract, None);
        let mut edited = proof.clone();
        edited.signed.entry.ack.index_hash = "0".repeat(64);
        let after_edit = edited.verify(&contract, Some(&node_pk));
        println!(
            "\nT1 receipt: seq {} / oplog root {} / index root {}",
            seq,
            short(proof.log_head_at()),
            short(proof.index_root())
        );
        probes.push(Probe {
            name: "T1",
            what: format!("the caller's own proof for seq {seq}, checked by a stranger holding the node's key"),
            expect: "accepted",
            observed: match &by_stranger {
                Ok(()) => "accepted".to_string(),
                Err(e) => format!("rejected: {e}"),
            },
            ok: by_stranger.is_ok(),
        });
        probes.push(Probe {
            name: "T1b",
            what: "the same proof checked against a key the verifier was told by an attacker".to_string(),
            expect: "rejected (untrusted signer)",
            observed: match &wrong_key {
                Ok(()) => "ACCEPTED (a forgery verifies)".to_string(),
                Err(e) => format!("rejected: {e}"),
            },
            ok: wrong_key.is_err(),
        });
        probes.push(Probe {
            name: "T1c",
            what: "the same proof with its ack edited, checked against the node's key".to_string(),
            expect: "rejected (signature no longer covers the ack)",
            observed: match &after_edit {
                Ok(()) => "ACCEPTED (an edited publication verifies)".to_string(),
                Err(e) => format!("rejected: {e}"),
            },
            ok: after_edit.is_err(),
        });
        probes.push(Probe {
            name: "T1d",
            what: "the same proof checked with no identity pinned (a stranger deciding for itself)".to_string(),
            expect: "accepted structurally",
            observed: match &no_pin {
                Ok(()) => "accepted structurally".to_string(),
                Err(e) => format!("rejected: {e}"),
            },
            ok: no_pin.is_ok(),
        });
    }

    // =========================================================================================
    // T2. THE WINDOW HALF (2.7: "a hash of the log's state at the length of the message").
    //
    // An entry inside an attested window can also be tied to the signed Merkle root over that
    // window. An entry in the open tail cannot -- no root exists over it yet -- but it still carries
    // the executor's signature, which is the inclusion proof the paper describes.
    // =========================================================================================
    let latest = read_latest_checkpoint(&store_dir, Some(&trust))
        .await
        .expect("readable")
        .expect("a checkpoint exists");
    let cp = &latest.checkpoint;
    let in_window = cp.window_base;
    let in_tail = OPS as u64;
    {
        let wproof = engine
            .inclusion_proof(&contract, in_window)
            .await
            .expect("a proof for an entry inside the attested window");
        let has_window = wproof.window.is_some();
        let verified = wproof.verify(&contract, Some(&node_pk));
        let siblings = wproof
            .window
            .as_ref()
            .map(|w| w.path.siblings.len())
            .unwrap_or(0);
        println!(
            "T2 window [{}, {}): seq {} -> {} sibling(s), root {}",
            cp.window_base,
            cp.window_end,
            in_window,
            siblings,
            short(&cp.merkle_root)
        );
        probes.push(Probe {
            name: "T2",
            what: format!(
                "a proof for seq {in_window} inside the attested window [{}, {})",
                cp.window_base, cp.window_end
            ),
            expect: "accepted, and tied to the signed window root",
            observed: match (&verified, has_window) {
                (Ok(()), true) => format!("accepted, path of {siblings} sibling(s) reaches the root"),
                (Ok(()), false) => "accepted, but no window was attached".to_string(),
                (Err(e), _) => format!("rejected: {e}"),
            },
            ok: verified.is_ok() && has_window,
        });
        // The same proof with its path moved to another position must not reach the same root.
        let mut moved = wproof.clone();
        if let Some(w) = moved.window.as_mut() {
            w.path.index = (w.path.index + 1) % w.path.leaves;
        }
        let moved_v = moved.verify(&contract, Some(&node_pk));
        probes.push(Probe {
            name: "T2b",
            what: "the same window proof with its path re-pointed at a different position".to_string(),
            expect: "rejected",
            observed: match &moved_v {
                Ok(()) => "ACCEPTED (a path for one entry proves another)".to_string(),
                Err(e) => format!("rejected: {e}"),
            },
            ok: moved_v.is_err(),
        });

        let tproof = engine
            .inclusion_proof(&contract, in_tail)
            .await
            .expect("a proof for a tail entry");
        let tail_v = tproof.verify(&contract, Some(&node_pk));
        probes.push(Probe {
            name: "T2c",
            what: format!("a proof for tail seq {in_tail}, which no checkpoint has attested yet"),
            expect: "accepted on the signature alone; no window",
            observed: match (&tail_v, tproof.window.is_some()) {
                (Ok(()), false) => "accepted on the signature alone, no window".to_string(),
                (Ok(()), true) => "accepted, but claims a window that cannot exist".to_string(),
                (Err(e), _) => format!("rejected: {e}"),
            },
            ok: tail_v.is_ok() && tproof.window.is_none(),
        });
    }

    // =========================================================================================
    // T3. TWO HISTORIES AT ONE LENGTH (2.4 / 2.6: the append-only violation).
    //
    // The executor signs a second checkpoint over the same window claiming a different root. Every
    // entry in both histories is well-formed and individually signed, so only a statement ABOUT A
    // LENGTH can catch it -- and two such statements are enough.
    // =========================================================================================
    {
        let twin = Checkpoint {
            merkle_root: "0".repeat(64),
            ..latest.checkpoint.clone()
        };
        let twin = SignedCheckpoint::sign(twin, node.as_ref(), now_ms()).expect("node signs");
        let v = adjudicate_fork(
            &contract,
            &ForkedHistory {
                a: latest.clone(),
                b: twin,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T3",
            what: format!(
                "two checkpoints the executor signed over one window [{}, {}) that disagree",
                cp.window_base, cp.window_end
            ),
            expect: "dishonest",
            observed: match &v {
                Verdict::Dishonest { seq, published, recomputed } => format!(
                    "dishonest at {seq}: {} vs {}",
                    short(published),
                    short(recomputed)
                ),
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                Verdict::Honest { .. } => "honest (a fork was not proven)".to_string(),
            },
            ok: matches!(v, Verdict::Dishonest { .. }),
        });

        // The same pair, consistent: no fork, and the verdict names the attested state.
        let same = adjudicate_fork(
            &contract,
            &ForkedHistory {
                a: latest.clone(),
                b: latest.clone(),
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T3b",
            what: "the same checkpoint offered twice".to_string(),
            expect: "honest",
            observed: match &same {
                Verdict::Honest { seq, index_hash } => {
                    format!("honest at {seq}, index {}", short(index_hash))
                }
                other => format!("{} (a consistent history was called a fork)", other.label()),
            },
            ok: matches!(same, Verdict::Honest { .. }),
        });

        // An attacker cannot accuse: its fork is correctly signed, by nobody the verifier trusts.
        let attacker_twin = Checkpoint {
            merkle_root: "0".repeat(64),
            ..latest.checkpoint.clone()
        };
        let attacker_twin =
            SignedCheckpoint::sign(attacker_twin, attacker.as_ref(), now_ms()).expect("attacker signs");
        let v = adjudicate_fork(
            &contract,
            &ForkedHistory {
                a: latest.clone(),
                b: attacker_twin,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T3c",
            what: "an attacker-signed fork adjudicated against the node's key".to_string(),
            expect: "inadmissible",
            observed: match &v {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (a stranger convicts the executor)", other.label()),
            },
            ok: matches!(v, Verdict::Inadmissible { .. }),
        });

        // And no pinned identity means no verdict that names a publisher.
        let v = adjudicate_fork(
            &contract,
            &ForkedHistory {
                a: latest.clone(),
                b: latest.clone(),
            },
            None,
        );
        probes.push(Probe {
            name: "T3d",
            what: "a fork adjudicated with no trusted identity supplied".to_string(),
            expect: "inadmissible",
            observed: match &v {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (a verdict with nobody to convict)", other.label()),
            },
            ok: matches!(v, Verdict::Inadmissible { .. }),
        });
    }

    // =========================================================================================
    // T4. TWO ADJACENT WINDOWS THAT DO NOT CHAIN (2.4).
    //
    // The first ends where the second begins, so the first's head IS the second's predecessor. If
    // they disagree, one of the two was rewritten -- decidable from the artefacts alone.
    // =========================================================================================
    {
        let first = Checkpoint {
            contract_id: contract.id.clone(),
            contract_version: contract.version,
            window_base: 0,
            window_end: WINDOW as u64,
            prev_head: String::new(),
            log_head: "a".repeat(64),
            merkle_root: "b".repeat(64),
            index_hash: index_hash(&Index::new()),
            index: Index::new(),
            ts_ms: now_ms(),
        };
        let second = Checkpoint {
            window_base: WINDOW as u64,
            window_end: 2 * WINDOW as u64,
            prev_head: "c".repeat(64),
            log_head: "d".repeat(64),
            merkle_root: "e".repeat(64),
            ..first.clone()
        };
        let a = SignedCheckpoint::sign(first, node.as_ref(), now_ms()).expect("node signs");
        let b = SignedCheckpoint::sign(second, node.as_ref(), now_ms()).expect("node signs");
        let v = adjudicate_fork(&contract, &ForkedHistory { a, b }, Some(&node_pk));
        probes.push(Probe {
            name: "T4",
            what: "two adjacent windows the executor signed that do not chain onto each other".to_string(),
            expect: "dishonest",
            observed: match &v {
                Verdict::Dishonest { seq, published, recomputed } => format!(
                    "dishonest at {seq}: {} vs {}",
                    short(published),
                    short(recomputed)
                ),
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                Verdict::Honest { .. } => "honest (a broken chain was not detected)".to_string(),
            },
            ok: matches!(v, Verdict::Dishonest { .. }),
        });
    }

    // =========================================================================================
    // T5. LOG AVAILABILITY (4.1).
    //
    // A signed checkpoint is a promise that the entries it covers exist. The check compares that
    // promise against what the executor actually serves, and names what is missing.
    // =========================================================================================
    {
        let log = engine
            .snapshot_log(&contract.id)
            .await
            .expect("a log exists");
        println!(
            "\nT5 served [{}, {}) / latest checkpoint [{}, {})",
            log.base(),
            log.next_seq(),
            cp.window_base,
            cp.window_end
            );

        // (a) The honest case: the log was compacted, and the checkpoint that licensed the cut is
        // produced. Compaction is legal; it is only legal against that attestation.
        let ok_case = check_availability(
            &contract,
            &Availability {
                checkpoint: latest.clone(),
                base_attestation: Some(latest.clone()),
            },
            &log,
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T5",
            what: "a compacted log whose cut is attested by the checkpoint that licensed it".to_string(),
            expect: "available",
            observed: match &ok_case {
                Ok(()) => "available".to_string(),
                Err(e) => format!("{e}"),
            },
            ok: ok_case.is_ok(),
        });

        // (b) The same log offered against an EARLIER checkpoint with nothing attesting the cut:
        // the entries between are gone, and nothing says they may be.
        let earlier_end = cp.window_base;
        let earlier = read_checkpoint(&store_dir, earlier_end, Some(&trust))
            .await
            .expect("an earlier checkpoint exists");
        let unattested = check_availability(
            &contract,
            &Availability {
                checkpoint: earlier.clone(),
                base_attestation: None,
            },
            &log,
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T5b",
            what: format!(
                "the same log offered against the earlier window ending at {earlier_end}, with no attestation for the cut"
            ),
            expect: "withheld (unattested base)",
            observed: match &unattested {
                Ok(()) => "available (history was dropped with nothing attesting it)".to_string(),
                Err(Withheld::UnattestedBase { base, attested_base }) => {
                    format!("withheld: log starts at {base}, window base is {attested_base}")
                }
                Err(e) => format!("{e}"),
            },
            ok: matches!(unattested, Err(Withheld::UnattestedBase { .. })),
        });

        // (c) A base attestation that describes some OTHER cut is a genuine, correctly signed
        // artefact that licenses nothing here.
        let mismatch = check_availability(
            &contract,
            &Availability {
                checkpoint: earlier.clone(),
                base_attestation: Some(earlier.clone()),
            },
            &log,
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T5c",
            what: "a base attestation for a window that is not this log's cut".to_string(),
            expect: "withheld (attestation mismatch)",
            observed: match &mismatch {
                Ok(()) => "available (a foreign attestation licensed the cut)".to_string(),
                Err(Withheld::BaseAttestationMismatch { .. }) => {
                    "withheld: the attestation is not of this cut".to_string()
                }
                Err(e) => format!("{e}"),
            },
            ok: matches!(mismatch, Err(Withheld::BaseAttestationMismatch { .. })),
        });

        // (d) The executor signed a window it does not serve. Compaction only removes from the
        // front, so anything missing at the tail is withheld, never reclaimed.
        let overshoot = Checkpoint {
            window_end: cp.window_end + 50,
            log_head: "f".repeat(64),
            ..latest.checkpoint.clone()
        };
        let overshoot =
            SignedCheckpoint::sign(overshoot, node.as_ref(), now_ms()).expect("node signs");
        let truncated = check_availability(
            &contract,
            &Availability {
                checkpoint: overshoot,
                base_attestation: Some(latest.clone()),
            },
            &log,
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T5d",
            what: format!(
                "a checkpoint the executor signed for [.., {}) against a log serving up to {}",
                cp.window_end + 50,
                log.next_seq()
            ),
            expect: "withheld (tail truncated)",
            observed: match &truncated {
                Ok(()) => "available (an attested window is not served)".to_string(),
                Err(Withheld::TailTruncated {
                    attested_end,
                    served_end,
                }) => format!("withheld: attests {attested_end}, serves {served_end}"),
                Err(e) => format!("{e}"),
            },
            ok: matches!(truncated, Err(Withheld::TailTruncated { .. })),
        });
    }

    // =========================================================================================
    // T6. THE BREACH NOTICE CARRIES THE PUBLISHER'S SIGNATURE (2.6).
    //
    // "the monitor can give notice of the breach by sharing the sequence number ... along with the
    // inclusion proofs (root hash and signature) of the relevant messages." A notice with no
    // signature is the accuser's word, and an adjudication that accepts one lets a monitor convict
    // an honest executor by writing any ack it likes.
    // =========================================================================================
    {
        let segment: Vec<SignedEntry> =
            read_signed_entries_from(&store_dir, cp.window_end, &node_pk_bytes)
                .await
                .expect("read signed tail");
        let disputed_seq = segment.last().map(|s| s.entry.op.seq).unwrap_or(cp.window_end);
        println!("\nT6 dispute segment  : {} entries, disputed seq {disputed_seq}", segment.len());

        let honest = adjudicate(
            &contract,
            &Dispute {
                checkpoint: latest.clone(),
                segment: segment.clone(),
                disputed_seq,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T6",
            what: format!("a dispute over the genuine publication at seq {disputed_seq}"),
            expect: "honest",
            observed: honest.label().to_string(),
            ok: matches!(honest, Verdict::Honest { .. }),
        });

        // The attack: the accuser rewrites the ack and leaves the executor's signature on it.
        let mut fabricated = segment.clone();
        if let Some(last) = fabricated.last_mut() {
            last.entry.ack.index_hash = "0".repeat(64);
        }
        let fabricated_v = adjudicate(
            &contract,
            &Dispute {
                checkpoint: latest.clone(),
                segment: fabricated,
                disputed_seq,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T6b",
            what: "the same dispute with the disputed ack rewritten by the accuser".to_string(),
            expect: "inadmissible",
            observed: match &fabricated_v {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!(
                    "{} (an honest executor is convicted by an artefact it never signed)",
                    other.label()
                ),
            },
            ok: matches!(fabricated_v, Verdict::Inadmissible { .. }),
        });

        // The genuine second publication: the same false ack, signed by the executor. Still convicts,
        // so refusing the forgery above cannot be confused with refusing everything.
        let mut published_false = segment.clone();
        if let Some(last) = published_false.last_mut() {
            last.entry.ack.index_hash = "0".repeat(64);
            *last = hive_contract::sign_entry(&last.entry, node.as_ref()).expect("node signs");
        }
        let false_v = adjudicate(
            &contract,
            &Dispute {
                checkpoint: latest.clone(),
                segment: published_false,
                disputed_seq,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T6c",
            what: "a dispute over a false ack the executor itself signed".to_string(),
            expect: "dishonest",
            observed: false_v.label().to_string(),
            ok: matches!(false_v, Verdict::Dishonest { .. }),
        });

        // A whole segment signed by a stranger is an accusation about nobody.
        let stranger: Vec<SignedEntry> = segment
            .iter()
            .map(|s| hive_contract::sign_entry(&s.entry, attacker.as_ref()).expect("attacker signs"))
            .collect();
        let stranger_v = adjudicate(
            &contract,
            &Dispute {
                checkpoint: latest.clone(),
                segment: stranger,
                disputed_seq,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "T6d",
            what: "a dispute whose whole segment was signed by the attacker".to_string(),
            expect: "inadmissible",
            observed: match &stranger_v {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (a stranger's segment convicts the executor)", other.label()),
            },
            ok: matches!(stranger_v, Verdict::Inadmissible { .. }),
        });
    }

    println!("\n-- probes --");
    let mut failed = 0usize;
    for p in &probes {
        println!(
            "{:>4} {:<58} expect {} / got {} [{}]",
            p.name,
            p.what,
            p.expect,
            p.observed,
            if p.ok { "ok" } else { "FAILED" }
        );
        if !p.ok {
            failed += 1;
        }
    }
    println!("\nprobes failed        : {failed} / {}", probes.len());
    println!("live index hash      : {}", short(&live_hash));
    println!("store                : {}", store_dir.display());
    if failed == 0 {
        println!("RESULT: ALL PROBES PASSED");
        std::process::exit(0);
    }
    println!("RESULT: BREACH -- {failed} guarantee(s) did not hold");
    std::process::exit(1);
}
