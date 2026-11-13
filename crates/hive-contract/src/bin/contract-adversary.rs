//! Adversarial witness for the FRAUD PROOF itself.
//!
//! `contract-forge` asks whether a forged ENTRY or CHECKPOINT is refused. This harness asks the
//! harder question the fraud proof exists to answer: can an artefact that CONVICTS AN HONEST
//! EXECUTOR be built and made to verify? A fraud proof that returns `Dishonest` for an artefact a
//! stranger authored is worse than no proof at all, because it is a machine for convicting the
//! innocent.
//!
//! Every probe here starts from ONE genuine, node-signed entry -- the whole thing an observer of a
//! real log would have -- and tries to turn it into a second, conflicting publication that still
//! carries the node's signature. It never mints a key the node does not have, never touches a
//! secret, and never weakens a gate: it only constructs artefacts and hands them to the real
//! adjudication path with the real public key.
//!
//! Not a test file and not a mock: real `Engine`, real `Store`, real `KeyedSigner`, real filesystem,
//! real `adjudicate` / `adjudicate_equivocation`.
//!
//! Usage: `contract-adversary [--dir <path>]`
//! Exit 0 when every fabrication is refused and every genuine proof still verifies; 1 otherwise.

use hive_contract::contract::Caller;
use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::Engine;
use hive_contract::exec::index_hash;
use hive_contract::persist::{
    read_entries_from, read_latest_checkpoint, read_signed_entries_from, PersistConfig,
};
use hive_contract::signing::{CheckpointSigner, KeyedSigner};
use hive_contract::{
    adjudicate, adjudicate_equivocation, apply, checkpoint, Ack, Checkpoint, Dispute, Equivocation,
    Index, LogEntry, Op, SignedCheckpoint, SignedEntry, Verdict,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const OPS: usize = 136;
const WINDOW: usize = 32;

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
        "/private/tmp/hive-contract-adversary-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create run dir");

    let contract = managed_db_contract();
    let mut probes: Vec<Probe> = Vec::new();

    // ---- identities -----------------------------------------------------------------------------
    // `node` signs the real log. `attacker` is a stranger's keypair: everything an accuser can mint
    // without the executor's key.
    let node_pkcs8 = KeyedSigner::generate_pkcs8().expect("generate node key");
    let attacker_pkcs8 = KeyedSigner::generate_pkcs8().expect("generate attacker key");
    let node: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("witness-node", &node_pkcs8).expect("load node key"));
    let attacker: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("attacker", &attacker_pkcs8).expect("load attacker key"));
    let node_pk_bytes = node.public_key();
    let node_pk = hex::encode(&node_pk_bytes);
    let trust = checkpoint::TrustedCheckpoint {
        public_key_hex: &node_pk,
        contract_id: &contract.id,
        contract_version: contract.version,
    };

    println!("== contract-adversary ==");
    println!("node public key     : {}", short(&node_pk));

    // ---- build a real log ------------------------------------------------------------------------
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
    let ts0 = now_ms();
    for i in 0..OPS {
        let db = format!("db_adv_{:03}", i % 8);
        let (kind, pl) = match i % 10 {
            0 => (
                "db.create",
                BTreeMap::from([
                    ("db_id".to_string(), db.clone()),
                    ("kind".to_string(), "sqlite".to_string()),
                    ("project".to_string(), "proj-00".to_string()),
                ]),
            ),
            9 => (
                "db.delete",
                BTreeMap::from([
                    ("db_id".to_string(), db.clone()),
                    ("project".to_string(), "proj-00".to_string()),
                ]),
            ),
            _ => (
                "db.write",
                BTreeMap::from([
                    ("bytes".to_string(), i.to_string()),
                    ("db_id".to_string(), db.clone()),
                    ("project".to_string(), "proj-00".to_string()),
                ]),
            ),
        };
        let r = engine
            .submit(
                &contract,
                Caller {
                    tenant: format!("team-{:02}", i % 3),
                    actor: "witness".to_string(),
                    owns_target: true, platform_attested: false,
                },
                kind,
                pl,
                &db,
                ts0 + i as u64,
                Ok(()),
            )
            .await
            .expect("submit");
        assert!(r.committed, "op {i} not committed: {:?}", r.error);
    }
    let store_dir = engine.store_dir(&contract.id).expect("store dir");
    let genuine = read_latest_checkpoint(&store_dir, Some(&trust))
        .await
        .expect("readable")
        .expect("a checkpoint exists");
    let cp = &genuine.checkpoint;
    let tail: Vec<LogEntry> = read_entries_from(&store_dir, cp.window_end, Some(&node_pk_bytes))
        .await
        .expect("read tail");
    let last = tail.last().cloned().expect("tail is non-empty");
    let seq = last.op.seq;
    let prefix: Vec<LogEntry> = tail.iter().take(tail.len() - 1).cloned().collect();
    let signed_last: SignedEntry =
        hive_contract::sign_entry(&last, node.as_ref()).expect("node signs its own entry");
    println!("checkpoint window   : [.., {})", cp.window_end);
    println!("disputed seq        : {} (kind {})", seq, last.op.kind);

    // =============================================================================================
    // A1. ACK MALLEABILITY: `error: None` and `error: Some("")` are the same canonical bytes.
    //
    // `Ack::canonical` writes `self.error.as_deref().unwrap_or_default()`, so a missing error and an
    // EMPTY error encode identically -- and therefore produce the SAME signature preimage. An
    // accuser who has seen one signed line can flip `error` from `None` to `Some("")`: the entry
    // hash is untouched (it is a function of prev+op), the signature still verifies, and `conflict`
    // sees the two acks as different because it compares the `Option`, not the encoding. That is a
    // second, conflicting publication carrying the honest executor's own signature.
    // =============================================================================================
    {
        let mut twin = last.clone();
        twin.ack.error = match twin.ack.error {
            None => Some(String::new()),
            Some(_) => None,
        };
        let preimage_same = hive_contract::persist::entry_sig_preimage(&last.entry_hash, &last.ack)
            == hive_contract::persist::entry_sig_preimage(&twin.entry_hash, &twin.ack);
        println!(
            "A1 ack preimage identical (None vs Some(\"\")): {}",
            preimage_same
        );
        let fabricated = SignedEntry {
            entry: twin,
            sig: signed_last.sig.clone(),
            public_key_hex: node_pk.clone(),
        };
        let v = adjudicate_equivocation(
            &contract,
            &Equivocation {
                checkpoint: genuine.clone(),
                prefix: prefix.clone(),
                a: signed_last.clone(),
                b: fabricated,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "A1",
            what: format!(
                "an accusation built by flipping ack.error None<->Some(\"\") on one signed entry at seq {seq}"
            ),
            expect: "inadmissible",
            observed: match &v {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                Verdict::Dishonest { published, recomputed, .. } => format!(
                    "DISHONEST (an honest executor is convicted): published {} vs {}",
                    short(published),
                    short(recomputed)
                ),
                Verdict::Honest { .. } => "honest".to_string(),
            },
            ok: matches!(v, Verdict::Inadmissible { .. }),
        });
    }

    // =============================================================================================
    // A2. PAYLOAD ENCODING COLLISION: `canonical_map` is not injective.
    //
    // Entries are `k=v` joined by \x1e, so {"a":"b","bytes":"1"} and {"a":"b\x1ebytes=1"} are
    // different maps with byte-identical canonical bytes -- hence the same `Op::canonical`, the same
    // entry hash and the same signature, while `apply` reads a DIFFERENT `bytes` value. An accuser
    // swaps the payload of one genuine entry for its colliding twin: nothing about the entry's
    // binding changes, but replay now computes another index, so `adjudicate` reports a divergence
    // the honest executor never produced.
    // =============================================================================================
    {
        let real_bytes = last.op.payload.get("bytes").cloned().unwrap_or_default();
        let mut colliding = BTreeMap::new();
        let mut glued = real_bytes.clone();
        for (k, v) in last.op.payload.iter() {
            if k == "bytes" {
                continue;
            }
            glued.push('\u{1e}');
            glued.push_str(k);
            glued.push('=');
            glued.push_str(v);
        }
        colliding.insert("bytes".to_string(), glued.clone());
        let same_canonical = {
            let a = last.op.clone();
            let mut b = last.op.clone();
            b.payload = colliding.clone();
            a.canonical() == b.canonical()
        };
        // Kept only to compute the collision for the report; the artefact below edits the monitor's
        // own signed copy in place.
        let _swapped = {
            let mut s = last.clone();
            s.op.payload = colliding.clone();
            s
        };
        // The monitor's own copy of the tail, carrying the signature each entry was published under.
        // The attack leaves that signature in place and swaps the payload: the accuser is asserting
        // that the executor published an entry it never did, and an adjudication that accepts an
        // unsigned -- or stale-signed -- segment cannot tell the difference.
        let mut segment: Vec<SignedEntry> =
            read_signed_entries_from(&store_dir, cp.window_end, &node_pk_bytes)
                .await
                .expect("read signed tail");
        if let Some(l) = segment.last_mut() {
            l.entry.op.payload = colliding.clone();
        }
        let v = adjudicate(
            &contract,
            &Dispute {
                checkpoint: genuine.clone(),
                segment,
                disputed_seq: seq,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "A2",
            what: format!(
                "one genuine entry's payload swapped for its canonical twin (same entry_hash, bytes {} -> {})",
                real_bytes,
                short(&glued)
            ),
            expect: "no breach provable from this artefact",
            observed: format!(
                "canonical bytes identical: {} / {}",
                same_canonical,
                match &v {
                    Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                    Verdict::Dishonest { published, recomputed, .. } => format!(
                        "DISHONEST (an honest executor is convicted): published {} vs recomputed {}",
                        short(published),
                        short(recomputed)
                    ),
                    Verdict::Honest { .. } => "honest".to_string(),
                }
            ),
            ok: !matches!(v, Verdict::Dishonest { .. }),
        });
    }

    // =============================================================================================
    // A3. NO IDENTITY PINNED: `adjudicate_fraud(.., None)` convicts on a stranger's artefact.
    //
    // `SignedCheckpoint::verify(None)` checks the signature under the key the ARTEFACT carries, so
    // an accuser mints a keypair, writes its own checkpoint and its own pair of conflicting entries,
    // and the adjudication -- which never names the publisher in its verdict -- returns Dishonest.
    // Nothing in the verdict lets the verifier see that the "executor" it just convicted is a key it
    // has never heard of.
    // =============================================================================================
    {
        let fake_cp = Checkpoint {
            contract_id: contract.id.clone(),
            contract_version: contract.version,
            window_base: 0,
            window_end: 0,
            prev_head: String::new(),
            log_head: String::new(),
            merkle_root: "0".repeat(64),
            index_hash: index_hash(&Index::new()),
            index: Index::new(),
            ts_ms: now_ms(),
        };
        let fake_signed = SignedCheckpoint::sign(fake_cp, attacker.as_ref(), now_ms())
            .expect("attacker signs its own checkpoint");
        let mk = |ok: bool| {
            let op = Op {
                seq: 0,
                contract: contract.id.clone(),
                tenant: "attacker-tenant".to_string(),
                actor: "attacker".to_string(),
                target: "db_fake".to_string(),
                kind: "db.create".to_string(),
                payload: BTreeMap::from([("db_id".to_string(), "db_fake".to_string())]),
            };
            let mut e = LogEntry {
                ack: Ack {
                    seq: 0,
                    ts_ms: now_ms(),
                    success: ok,
                    error: None,
                    index_hash: if ok { "a".repeat(64) } else { "b".repeat(64) },
                    log_head: String::new(),
                },
                op,
                entry_hash: String::new(),
                prev_hash: String::new(),
            };
            e.entry_hash = e.expected_hash();
            e.ack.log_head = e.entry_hash.clone();
            hive_contract::sign_entry(&e, attacker.as_ref()).expect("attacker signs")
        };
        let v = adjudicate_equivocation(
            &contract,
            &Equivocation {
                checkpoint: fake_signed,
                prefix: Vec::new(),
                a: mk(true),
                b: mk(false),
            },
            None,
        );
        probes.push(Probe {
            name: "A3",
            what: "a whole artefact (checkpoint + both publications) the attacker signed, adjudicated with no trusted key".to_string(),
            expect: "inadmissible",
            observed: match &v {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                Verdict::Dishonest { published, recomputed, .. } => format!(
                    "DISHONEST (a stranger's artefact convicts the executor): {} vs {}",
                    short(published),
                    short(recomputed)
                ),
                Verdict::Honest { .. } => "honest".to_string(),
            },
            ok: matches!(v, Verdict::Inadmissible { .. }),
        });
    }

    // =============================================================================================
    // A4. WINDOW ENDING ON THE DISPUTED SEQ DOUBLE-APPLIES THE OP.
    //
    // The prefix is `[window_end, disputed_seq)` and `replay_from` starts from the checkpoint's
    // index, which is the index AT `window_end`. When `window_end == disputed_seq` the prefix is
    // empty and the base index already contains the disputed op -- so `expected_index_hash` applies
    // it a second time. A checkpoint at the current head is the normal shape (the flush loop and the
    // shutdown checkpoint both close a window wherever the log stands), so this is reachable.
    // =============================================================================================
    {
        // The index at the disputed seq: the genuine checkpoint's attested index, plus the tail
        // before it. (The store compacts behind its retained checkpoints, so replaying from genesis
        // is not available here -- the checkpoint is what makes the state at `window_end` known.)
        let mut at_seq = cp.index.clone();
        for e in tail.iter().take(tail.len() - 1) {
            for m in apply(&at_seq, &e.op) {
                match m {
                    hive_contract::Mutation::Put { key, value } => {
                        at_seq.insert(key, value);
                    }
                    hive_contract::Mutation::Delete { key } => {
                        at_seq.remove(&key);
                    }
                }
            }
        }
        let idx_before = at_seq.clone();
        let before_hash = index_hash(&idx_before);
        for m in apply(&at_seq, &last.op) {
            match m {
                hive_contract::Mutation::Put { key, value } => {
                    at_seq.insert(key, value);
                }
                hive_contract::Mutation::Delete { key } => {
                    at_seq.remove(&key);
                }
            }
        }
        let honest_hash = index_hash(&at_seq);
        let cp_at_seq = Checkpoint {
            contract_id: contract.id.clone(),
            contract_version: contract.version,
            window_base: cp.window_end,
            window_end: seq,
            prev_head: cp.log_head.clone(),
            log_head: last.prev_hash.clone(),
            merkle_root: "0".repeat(64),
            index_hash: index_hash(&idx_before),
            index: idx_before,
            ts_ms: now_ms(),
        };
        let signed_at_seq = SignedCheckpoint::sign(cp_at_seq, node.as_ref(), now_ms())
            .expect("node signs a checkpoint at its own head");
        let mut conflicting = last.clone();
        conflicting.ack.index_hash = "0".repeat(64);
        conflicting.ack.success = false;
        conflicting.ack.error = Some("the executor's second story".to_string());
        let v = adjudicate_equivocation(
            &contract,
            &Equivocation {
                checkpoint: signed_at_seq,
                prefix: Vec::new(),
                a: hive_contract::sign_entry(&last, node.as_ref()).expect("sign"),
                b: hive_contract::sign_entry(&conflicting, node.as_ref()).expect("sign"),
            },
            Some(&node_pk),
        );
        // A window is half-open, so a checkpoint whose `window_end` is the disputed seq carries the
        // index BEFORE the disputed op. The verdict must name a hash the log actually supports: the
        // pre-op index (for the candidate that claims it failed) or the post-op index (for the one
        // that claims it succeeded).
        probes.push(Probe {
            name: "A4",
            what: format!("a genuine equivocation adjudicated from a checkpoint whose window_end == the disputed seq {seq}"),
            expect: "dishonest, naming a hash the log supports",
            observed: match &v {
                Verdict::Dishonest { recomputed, .. } => format!(
                    "dishonest, recomputed {} / log supports {} (pre-op) or {} (post-op)",
                    short(recomputed),
                    short(&before_hash),
                    short(&honest_hash)
                ),
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                Verdict::Honest { .. } => "honest (equivocation not proven)".to_string(),
            },
            ok: matches!(&v, Verdict::Dishonest { recomputed, .. }
                if recomputed == &honest_hash || recomputed == &before_hash),
        });
    }

    // ---- C1: the genuine equivocation still proves what it claims (the built work's own claim) ---
    {
        let mut conflicting = last.clone();
        conflicting.ack.index_hash = "0".repeat(64);
        conflicting.ack.success = false;
        conflicting.ack.error = Some("the executor's second story".to_string());
        let v = adjudicate_equivocation(
            &contract,
            &Equivocation {
                checkpoint: genuine.clone(),
                prefix: prefix.clone(),
                a: hive_contract::sign_entry(&last, node.as_ref()).expect("sign"),
                b: hive_contract::sign_entry(&conflicting, node.as_ref()).expect("sign"),
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "C1",
            what: format!("a genuine equivocation (the node itself signed both acks for seq {seq})"),
            expect: "dishonest",
            observed: match &v {
                Verdict::Dishonest { published, .. } => {
                    format!("dishonest, published {}", short(published))
                }
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                Verdict::Honest { .. } => "honest (a real breach was not proven)".to_string(),
            },
            ok: matches!(v, Verdict::Dishonest { .. }),
        });
    }

    println!("\n-- probes --");
    let mut failed = 0usize;
    let mut convicted = 0usize;
    for p in &probes {
        println!(
            "{:>4} {:<40} expect {} / got {} [{}]",
            p.name,
            p.what,
            p.expect,
            p.observed,
            if p.ok { "ok" } else { "FAILED" }
        );
        if !p.ok {
            failed += 1;
        }
        if p.observed.contains("DISHONEST") {
            convicted += 1;
        }
    }
    println!("\nprobes failed        : {failed} / {}", probes.len());
    println!("honest executor convicted by a fabricated artefact : {convicted}");
    if failed == 0 {
        println!("RESULT: ALL PROBES PASSED");
        std::process::exit(0);
    }
    println!("RESULT: BREACH -- the fraud proof convicts an honest executor");
    std::process::exit(1);
}
