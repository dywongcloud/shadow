//! Adversarial witness for the contract's cryptographic bindings.
//!
//! A checkpoint that verifies is a claim that a stranger will act on, so the question this harness
//! asks is never "does the honest artefact verify" -- `contract-persist` already answers that -- but
//! "does a FORGED one fail". Every probe here builds an artefact an attacker could actually build
//! (a second signing identity, a copied signature, a file renamed on disk, an entry rewritten inside
//! a window a checkpoint already attested) and checks that the verification path refuses it.
//!
//! Not a test file and not a mock: it runs the real `Engine`, the real `Store`, the real signer and
//! the real adjudication against a real directory on a real filesystem, and each on-disk attack is
//! executed against a COPY of a store that was really built by submitting real ops.
//!
//! Two identities exist here on purpose. `node` is the identity the store is configured with;
//! `attacker` is a second, freshly minted Ed25519 keypair -- the one thing an attacker needs in order
//! to produce a checkpoint that is internally consistent and correctly signed by SOMEBODY.
//!
//! Usage: `contract-forge [--dir <path>] [--ops N] [--window N] [--retain N]`
//! Exit 0 only when every control holds and every forgery is refused.

use hive_contract::audit::{adjudicate, Dispute, Verdict};
use hive_contract::checkpoint::SignedCheckpoint;
use hive_contract::contract::{Caller, Contract};
use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::Engine;
use hive_contract::exec::index_hash;
use hive_contract::merkle;
use hive_contract::oplog::LogEntry;
use hive_contract::persist::{
    checkpoint_name, read_checkpoint, read_entries_from, read_latest_checkpoint, read_window,
    PersistConfig, DEFAULT_RETAIN_CHECKPOINTS,
};
use hive_contract::signing::{CheckpointSigner, KeyedSigner};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const OPS: usize = 136;
const WINDOW: usize = 32;
const RETAIN: usize = DEFAULT_RETAIN_CHECKPOINTS;

/// One probe's outcome. `refused` is what every forgery must do.
struct Probe {
    name: &'static str,
    what: String,
    /// What a correct implementation does: "refused" for a forgery, "accepted" for a genuine
    /// artefact.
    expect: &'static str,
    observed: String,
    ok: bool,
}

fn arg(name: &str, default: &str) -> String {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == format!("--{name}") {
            return it.next().unwrap_or_else(|| default.to_string());
        }
    }
    default.to_string()
}

fn payload(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create copy dir");
    for e in std::fs::read_dir(src).expect("read src") {
        let e = e.expect("entry");
        let to = dst.join(e.file_name());
        std::fs::copy(e.path(), &to).expect("copy file");
    }
}

fn checkpoint_ends(dir: &Path) -> Vec<u64> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = n.strip_prefix("cp-").and_then(|s| s.strip_suffix(".json")) {
                if let Ok(v) = rest.parse::<u64>() {
                    out.push(v);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// Reopen a store root cold, the way a restart would, and report the index it publishes.
async fn reopen(root: &Path, contract: &Contract, signer: Arc<dyn CheckpointSigner>) -> Result<String, String> {
    let engine = Engine::open(
        4,
        root.to_path_buf(),
        PersistConfig {
            checkpoint_every: WINDOW,
            retain_checkpoints: RETAIN,
            signer: Some(signer),
            sync: true,
        },
    );
    match engine.register(contract.clone()).await {
        Ok(()) => engine
            .index_hash(&contract.id)
            .await
            .ok_or_else(|| "registered but published no index".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tokio::main]
async fn main() {
    let dir_arg = arg("dir", "");
    let base = if dir_arg.is_empty() {
        PathBuf::from(format!("/private/tmp/hive-contract-forge-{}", std::process::id()))
    } else {
        PathBuf::from(dir_arg)
    };
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create run dir");

    let contract = managed_db_contract();
    let mut probes: Vec<Probe> = Vec::new();

    // ---- two identities -------------------------------------------------------------------------
    // `node` is the one the store is configured with. `attacker` is a second keypair: everything an
    // attacker needs to produce a self-consistent, correctly-signed checkpoint.
    let node_pkcs8 = KeyedSigner::generate_pkcs8().expect("generate node key");
    let attacker_pkcs8 = KeyedSigner::generate_pkcs8().expect("generate attacker key");
    let node: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("witness-node", &node_pkcs8).expect("load node key"));
    let attacker: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("attacker", &attacker_pkcs8).expect("load attacker key"));
    let node_pk_bytes = node.public_key();
    let node_pk = hex::encode(&node_pk_bytes);
    let attacker_pk = hex::encode(attacker.public_key());

    println!("== contract-forge ==");
    println!("ops / window / retain : {OPS} / {WINDOW} / {RETAIN}");
    println!("node public key       : {}...", &node_pk[..16]);
    println!("attacker public key   : {}...", &attacker_pk[..16]);
    println!("keys distinct         : {}", node_pk != attacker_pk);
    // What a stored checkpoint must claim to be adopted as this contract's history: signed by the
    // node's identity, for this contract. Everything else here is measured against that.
    let trust = hive_contract::checkpoint::TrustedCheckpoint {
        public_key_hex: &node_pk,
        contract_id: &contract.id,
        contract_version: contract.version,
    };

    // ---- build a real store ----------------------------------------------------------------------
    let live_root = base.join("live");
    let engine = Engine::open(
        4,
        live_root.clone(),
        PersistConfig {
            checkpoint_every: WINDOW,
            retain_checkpoints: RETAIN,
            signer: Some(node.clone()),
            sync: true,
        },
    );
    engine.register(contract.clone()).await.expect("register");
    let ts0 = now_ms();
    for i in 0..OPS {
        let db = format!("db_forge_{:03}", i % 8);
        let (kind, pl) = match i % 10 {
            0 => (
                "db.create",
                payload(&[("db_id", &db), ("kind", "sqlite"), ("project", "proj-00")]),
            ),
            9 => ("db.delete", payload(&[("db_id", &db), ("project", "proj-00")])),
            _ => (
                "db.write",
                payload(&[("bytes", &format!("{i}")), ("db_id", &db), ("project", "proj-00")]),
            ),
        };
        let r = engine
            .submit(
                &contract,
                Caller {
                    tenant: format!("team-{:02}", i % 3),
                    actor: "witness".to_string(),
                    owns_target: true,
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
    // Four windows close on their own at 32/64/96/128; the last 8 ops stay pending, so the store has
    // a real TAIL after the newest checkpoint. The dispute probes need it, and so does any claim
    // about replaying forward from a checkpoint.
    let store_dir = engine.store_dir(&contract.id).expect("store dir");
    let live_hash = engine.index_hash(&contract.id).await.expect("live index hash");
    println!("live index hash       : {}", &live_hash[..16]);
    println!(
        "checkpoints on disk   : {:?}",
        checkpoint_ends(&store_dir)
    );

    // ---- C4: an honest restart reproduces the live index ----------------------------------------
    let pristine = base.join("pristine");
    copy_dir(&store_dir, &pristine);
    let pristine_root = base.join("pristine-root");
    std::fs::create_dir_all(pristine_root.join(&contract.id)).expect("pristine root");
    copy_dir(&pristine, &pristine_root.join(&contract.id));
    let c4 = reopen(&pristine_root, &contract, node.clone()).await;
    let c4_ok = c4.as_deref() == Ok(live_hash.as_str());
    probes.push(Probe {
        name: "C4",
        what: "an honest restart republishes the live index hash".to_string(),
        expect: "accepted",
        observed: match &c4 {
            Ok(h) => format!("published {}", &h[..16]),
            Err(e) => format!("REFUSED: {e}"),
        },
        ok: c4_ok,
    });

    let genuine = read_latest_checkpoint(&store_dir, Some(&trust))
        .await
        .expect("latest checkpoint readable")
        .expect("a checkpoint exists");
    let cp = &genuine.checkpoint;

    // ---- C1: the genuine checkpoint verifies -----------------------------------------------------
    let c1 = genuine.verify(Some(&trust)).is_ok();
    probes.push(Probe {
        name: "C1",
        what: "the genuine signed checkpoint verifies".to_string(),
        expect: "accepted",
        observed: if c1 { "verifies".to_string() } else { "REFUSED".to_string() },
        ok: c1,
    });

    // ---- C2: a genuine inclusion proof verifies ---------------------------------------------------
    let win = read_window(&store_dir, cp.window_base, cp.window_end, WINDOW, Some(&node_pk_bytes))
        .await
        .unwrap_or_default();
    let hashes: Vec<String> = win.iter().map(|e| e.entry_hash.clone()).collect();
    let path = win.first().map(|_| merkle::merkle_prove(&hashes, 0).expect("prove"));
    let c2 = match (&win.first(), &path) {
        (Some(e), Some(p)) => genuine.verify_inclusion(e, p).is_ok(),
        _ => false,
    };
    probes.push(Probe {
        name: "C2",
        what: "a genuine inclusion path for a window entry verifies".to_string(),
        expect: "accepted",
        observed: if c2 {
            format!("accepted ({} siblings)", path.as_ref().map(|p| p.siblings.len()).unwrap_or(0))
        } else {
            "REFUSED".to_string()
        },
        ok: c2,
    });

    // ---- F5: the same path offered for a rewritten entry ------------------------------------------
    // The op's payload is edited but the entry_hash is COPIED from the real entry: without
    // re-deriving the hash from the op this travels as the original.
    if let (Some(e), Some(p)) = (win.first(), &path) {
        let mut f = e.clone();
        f.op
            .payload
            .insert("project".to_string(), "attacker-tenant".to_string());
        let acc = genuine.verify_inclusion(&f, p).is_ok();
        probes.push(Probe {
            name: "F5",
            what: "a rewritten entry under a copied entry_hash".to_string(),
            expect: "refused",
            observed: if acc { "ACCEPTED (forgery verified)".to_string() } else { "refused".to_string() },
            ok: !acc,
        });
    }

    // ---- F6: a path that claims a leaf count the window does not have ------------------------------
    // The window's size is signed in the checkpoint; a proof that names some other size is not a
    // proof about THIS window even if it recomputes.
    if let (Some(e), Some(p)) = (win.first(), &path) {
        let wrong = merkle::MerklePath {
            index: p.index,
            leaves: p.leaves + 1,
            leaf: p.leaf.clone(),
            siblings: p.siblings.clone(),
        };
        let acc = genuine.verify_inclusion(e, &wrong).is_ok();
        probes.push(Probe {
            name: "F6",
            what: format!("an inclusion path naming {} leaves for a window of {}", wrong.leaves, p.leaves),
            expect: "refused",
            observed: if acc { "ACCEPTED (forgery verified)".to_string() } else { "refused".to_string() },
            ok: !acc,
        });
    }

    // ---- F1: a checkpoint signed by an identity the store does not trust --------------------------
    // The attacker mints a keypair, forges the index, re-signs, and carries its own public key. Every
    // field is self-consistent; only the IDENTITY is wrong.
    let mut forged_cp = genuine.checkpoint.clone();
    forged_cp
        .index
        .insert("__untagged__/victim/status".to_string(), "live".to_string());
    forged_cp.index_hash = index_hash(&forged_cp.index);
    let attacker_signed = SignedCheckpoint::sign(forged_cp, attacker.as_ref(), now_ms()).expect("sign");
    let f1 = attacker_signed.verify(Some(&trust)).is_ok();
    probes.push(Probe {
        name: "F1",
        what: format!(
            "a checkpoint signed by a second identity (key {}...)",
            &attacker_signed.public_key_hex[..16]
        ),
        expect: "refused",
        observed: if f1 {
            "ACCEPTED (forgery verified)".to_string()
        } else {
            "refused".to_string()
        },
        ok: !f1,
    });

    // ---- F2: the same forgery adopted from disk on a restart --------------------------------------
    {
        let root = base.join("f2-root");
        std::fs::create_dir_all(root.join(&contract.id)).expect("f2 root");
        copy_dir(&store_dir, &root.join(&contract.id));
        std::fs::write(
            root.join(&contract.id).join(checkpoint_name(cp.window_end)),
            serde_json::to_vec_pretty(&attacker_signed).expect("serialize"),
        )
        .expect("write forged checkpoint");
        let out = reopen(&root, &contract, node.clone()).await;
        let adopted = out.is_ok();
        probes.push(Probe {
            name: "F2",
            what: "a restart adopts the attacker-signed checkpoint off disk".to_string(),
            expect: "refused",
            observed: match &out {
                Ok(h) => format!(
                    "ACCEPTED (forgery verified): node republished index {}",
                    &h[..16]
                ),
                Err(e) => format!("refused: {e}"),
            },
            ok: !adopted,
        });
    }

    // ---- F3: a checkpoint for a DIFFERENT contract, correctly signed by the node -------------------
    // Same key, same signatures, wrong contract: copied from another contract's directory.
    {
        let mut other = genuine.checkpoint.clone();
        other.contract_id = "hive.other-contract.v1".to_string();
        let signed = SignedCheckpoint::sign(other, node.as_ref(), now_ms()).expect("sign");
        let root = base.join("f3-root");
        std::fs::create_dir_all(root.join(&contract.id)).expect("f3 root");
        copy_dir(&store_dir, &root.join(&contract.id));
        std::fs::write(
            root.join(&contract.id).join(checkpoint_name(cp.window_end)),
            serde_json::to_vec_pretty(&signed).expect("serialize"),
        )
        .expect("write cross-contract checkpoint");
        let out = reopen(&root, &contract, node.clone()).await;
        probes.push(Probe {
            name: "F3",
            what: "a node-signed checkpoint for another contract, replayed here".to_string(),
            expect: "refused",
            observed: match &out {
                Ok(h) => format!("ACCEPTED (forgery verified): published {}", &h[..16]),
                Err(e) => format!("refused: {e}"),
            },
            ok: out.is_err(),
        });
    }

    // ---- F4: a checkpoint whose window_end is not the file that carries it --------------------------
    // Rollback by rename: an older, genuine, signed checkpoint placed at a later name.
    {
        let ends = checkpoint_ends(&store_dir);
        if let Some(oldest) = ends.first().copied() {
            let older = read_checkpoint(&store_dir, oldest, Some(&trust)).await.expect("read oldest");
            let later = cp.window_end + 1000;
            let root = base.join("f4-root");
            std::fs::create_dir_all(root.join(&contract.id)).expect("f4 root");
            copy_dir(&store_dir, &root.join(&contract.id));
            std::fs::write(
                root.join(&contract.id).join(checkpoint_name(later)),
                serde_json::to_vec_pretty(&older).expect("serialize"),
            )
            .expect("write renamed checkpoint");
            let out = reopen(&root, &contract, node.clone()).await;
            probes.push(Probe {
                name: "F4",
                what: format!("checkpoint {oldest} stored under the name cp-{later:012}"),
                expect: "refused",
                observed: match &out {
                    Ok(h) => format!("ACCEPTED (forgery verified): published {}", &h[..16]),
                    Err(e) => format!("refused: {e}"),
                },
                ok: out.is_err(),
            });
        }
    }

    // ---- F9: an entry rewritten inside a window a checkpoint already attests ------------------------
    // The retained window starts after the last checkpoint, so the entries INSIDE that checkpoint's
    // window are never replayed on a restart -- the only thing that can catch a rewrite there is the
    // signed Merkle root over them.
    {
        let root = base.join("f9-root");
        std::fs::create_dir_all(root.join(&contract.id)).expect("f9 root");
        copy_dir(&store_dir, &root.join(&contract.id));
        let dir = root.join(&contract.id);
        let seg = hive_contract::persist::segment_name(
            (cp.window_base / WINDOW as u64) * WINDOW as u64,
        );
        let text = std::fs::read_to_string(dir.join(&seg)).expect("read segment");
        let mut lines: Vec<String> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut l: hive_contract::persist::SegmentLine =
                serde_json::from_str(line).expect("parse segment line");
            if l.entry.op.seq >= cp.window_base && l.entry.op.seq < cp.window_end {
                l.entry
                    .op
                    .payload
                    .insert("bytes".to_string(), "999999999".to_string());
                // The attacker recomputes the hash so the file stays self-consistent. It cannot
                // re-sign the line -- that is the point -- so the original signature is left on it.
                l.entry.entry_hash = l.entry.expected_hash();
            }
            lines.push(serde_json::to_string(&l).expect("serialize segment line"));
        }
        std::fs::write(dir.join(&seg), lines.join("\n") + "\n").expect("rewrite segment");
        let out = reopen(&root, &contract, node.clone()).await;
        probes.push(Probe {
            name: "F9",
            what: format!(
                "one entry rewritten inside the attested window [{}, {})",
                cp.window_base, cp.window_end
            ),
            expect: "refused",
            observed: match &out {
                Ok(h) => format!("ACCEPTED (forgery verified): published {}", &h[..16]),
                Err(e) => format!("refused: {e}"),
            },
            ok: out.is_err(),
        });
    }

    // ---- disputes -----------------------------------------------------------------------------------
    let segment: Vec<LogEntry> = read_entries_from(&store_dir, cp.window_end, Some(&node_pk_bytes))
        .await
        .unwrap_or_default();
    let disputed_seq = segment.last().map(|e| e.op.seq).unwrap_or(cp.window_end);

    // C3: an honest segment settles as honest.
    let honest = adjudicate(
        &contract,
        &Dispute {
            checkpoint: genuine.clone(),
            segment: segment.clone(),
            disputed_seq,
        },
        Some(&node_pk),
    );
    probes.push(Probe {
        name: "C3",
        what: "a dispute over an honest ack settles as honest".to_string(),
        expect: "honest",
        observed: honest.label().to_string(),
        ok: matches!(honest, Verdict::Honest { .. }),
    });

    // F7: the same dispute with one ack rewritten settles as dishonest.
    let mut tampered = segment.clone();
    if let Some(last) = tampered.last_mut() {
        last.ack.index_hash = "0".repeat(64);
    }
    let dishonest = adjudicate(
        &contract,
        &Dispute {
            checkpoint: genuine.clone(),
            segment: tampered,
            disputed_seq,
        },
        Some(&node_pk),
    );
    probes.push(Probe {
        name: "F7",
        what: "a dispute over a rewritten ack".to_string(),
        expect: "dishonest",
        observed: dishonest.label().to_string(),
        ok: matches!(dishonest, Verdict::Dishonest { .. }),
    });

    // F8: a dispute built on an attacker-signed checkpoint. The accusation is fully self-consistent;
    // the only thing wrong with it is who signed the starting state.
    let forged_dispute = adjudicate(
        &contract,
        &Dispute {
            checkpoint: attacker_signed.clone(),
            segment: segment.clone(),
            disputed_seq,
        },
        Some(&node_pk),
    );
    let f8_ok = matches!(forged_dispute, Verdict::Inadmissible { .. });
    probes.push(Probe {
        name: "F8",
        what: "a dispute whose starting state was signed by the attacker".to_string(),
        expect: "inadmissible",
        observed: match &forged_dispute {
            Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
            other => format!("{} (forgery verified)", other.label()),
        },
        ok: f8_ok,
    });

    // F10: a segment that does not link to the checkpoint it is offered against.
    let mut unlinked = segment.clone();
    if let Some(first) = unlinked.first_mut() {
        first.prev_hash = "0".repeat(64);
    }
    let unlinked_v = adjudicate(
        &contract,
        &Dispute {
            checkpoint: genuine.clone(),
            segment: unlinked,
            disputed_seq,
        },
        Some(&node_pk),
    );
    probes.push(Probe {
        name: "F10",
        what: "a segment that does not link to the checkpoint head".to_string(),
        expect: "inadmissible",
        observed: match &unlinked_v {
            Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
            other => format!("{} (forgery verified)", other.label()),
        },
        ok: matches!(unlinked_v, Verdict::Inadmissible { .. }),
    });

    // ---- F12: an entry DELETED from a window a checkpoint already attests --------------------------
    // Every remaining line still carries a valid signature -- deletion is not a rewrite -- so the only
    // thing that can catch it is the signed Merkle root over the window. This is the probe that
    // exercises the root; F9 is caught earlier, by the signature.
    {
        let root = base.join("f12-root");
        std::fs::create_dir_all(root.join(&contract.id)).expect("f12 root");
        copy_dir(&store_dir, &root.join(&contract.id));
        let dir = root.join(&contract.id);
        let seg = hive_contract::persist::segment_name((cp.window_base / WINDOW as u64) * WINDOW as u64);
        let text = std::fs::read_to_string(dir.join(&seg)).expect("read window segment");
        let kept: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .filter(|(i, _)| *i != 3)
            .map(|(_, l)| l)
            .collect();
        std::fs::write(dir.join(&seg), kept.join("\n") + "\n").expect("rewrite window segment");
        let out = reopen(&root, &contract, node.clone()).await;
        probes.push(Probe {
            name: "F12",
            what: format!("one entry deleted from the attested window [{}, {})", cp.window_base, cp.window_end),
            expect: "refused",
            observed: match &out {
                Ok(h) => format!("ACCEPTED (forgery verified): published {}", &h[..16]),
                Err(e) => format!("refused: {e}"),
            },
            ok: out.is_err(),
        });
    }

    // ---- F11: a fabricated entry appended to the tail that no checkpoint has attested yet ----------
    // The chain is computable by anybody, so a hand-written line that links to the head, carries the
    // hash its own op produces and carries an ack replay agrees with looks exactly like history: it
    // lands inside the window the NEXT checkpoint will attest. The attacker here signs it with its
    // own key, which is the most a forger who has not stolen the node's can do.
    {
        let root = base.join("f11-root");
        std::fs::create_dir_all(root.join(&contract.id)).expect("f11 root");
        copy_dir(&store_dir, &root.join(&contract.id));
        let dir = root.join(&contract.id);
        let seg = hive_contract::persist::segment_name((cp.window_end / WINDOW as u64) * WINDOW as u64);
        let head = read_entries_from(&store_dir, cp.window_end, Some(&node_pk_bytes))
            .await
            .expect("read tail")
            .last()
            .map(|e| e.entry_hash.clone())
            .unwrap_or_else(|| cp.log_head.clone());
        // Deletes a namespace that has no keys, so `apply` produces no mutation and the ack the
        // forger has to write is the index hash it can already see.
        let forged_seq = OPS as u64;
        let mut forged = LogEntry {
            op: hive_contract::oplog::Op {
                seq: forged_seq,
                contract: contract.id.clone(),
                tenant: "attacker-tenant".to_string(),
                actor: "attacker".to_string(),
                target: "db_fabricated".to_string(),
                kind: "db.delete".to_string(),
                payload: BTreeMap::new(),
            },
            ack: hive_contract::oplog::Ack {
                seq: forged_seq,
                ts_ms: now_ms(),
                success: true,
                error: None,
                index_hash: live_hash.clone(),
                log_head: String::new(),
            },
            entry_hash: String::new(),
            prev_hash: head,
        };
        forged.entry_hash = forged.expected_hash();
        forged.ack.log_head = forged.entry_hash.clone();
        // Signed by the attacker: internally consistent, correctly signed, and not by the node.
        let sig = hex::encode(
            attacker
                .sign(&hive_contract::persist::entry_sig_preimage(
                    &forged.entry_hash,
                    &forged.ack,
                ))
                .expect("attacker signs"),
        );
        let mut text = std::fs::read_to_string(dir.join(&seg)).expect("read tail segment");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&serde_json::to_string(&hive_contract::persist::SegmentLine {
            entry: forged,
            sig,
        }).expect("serialize fabricated line"));
        text.push('\n');
        std::fs::write(dir.join(&seg), text).expect("append fabricated entry");
        let out = reopen(&root, &contract, node.clone()).await;
        probes.push(Probe {
            name: "F11",
            what: format!("a fabricated entry at seq {forged_seq}, signed by the attacker"),
            expect: "refused",
            observed: match &out {
                Ok(h) => format!("ACCEPTED (forgery verified): published {}", &h[..16]),
                Err(e) => format!("refused: {e}"),
            },
            ok: out.is_err(),
        });
    }

    // ---- fraud proofs: two conflicting acks for one op -----------------------------------------
    // Evidence is not a proof. The artefact below is the smallest thing that lets a STRANGER settle
    // "this executor published two different claims about one seq": one checkpoint, the run-up to
    // the disputed seq, and the two publications. It asks neither side anything and it replays one
    // window, never the whole log.
    let tail: Vec<LogEntry> = segment.clone();
    let prefix: Vec<LogEntry> = tail
        .iter()
        .take(tail.len().saturating_sub(1))
        .cloned()
        .collect();
    if let Some(last_entry) = tail.last().cloned() {
        let sign_by_node = |e: &LogEntry| {
            hive_contract::sign_entry(e, node.as_ref()).expect("node signs its own entry")
        };
        // C6: two IDENTICAL publications are not an accusation. Without this gate an honest
        // executor could be "convicted" by handing a verifier the same entry twice.
        let same_twice = hive_contract::adjudicate_equivocation(
            &contract,
            &hive_contract::Equivocation {
                checkpoint: genuine.clone(),
                prefix: prefix.clone(),
                a: sign_by_node(&last_entry),
                b: sign_by_node(&last_entry),
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "C6",
            what: format!("two identical publications for seq {}", last_entry.op.seq),
            expect: "inadmissible",
            observed: match &same_twice {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (an honest executor is convicted)", other.label()),
            },
            ok: matches!(same_twice, Verdict::Inadmissible { .. }),
        });

        // The two conflicting acks: the node's own entry, and the same op acked differently.
        let mut conflicting = last_entry.clone();
        conflicting.ack.index_hash = "0".repeat(64);
        conflicting.ack.success = false;
        conflicting.ack.error = Some("the executor's second story".to_string());

        // F13: the FORGED accusation. The accuser copies a real entry and its real signature and
        // edits the ack. Before the ack was bound into the signed preimage this VERIFIED, and any
        // observer of one signed line could convict an honest executor. Now the signature no
        // longer covers the artefact it is offered with.
        let copied_sig = sign_by_node(&last_entry).sig;
        let forged_accusation = hive_contract::SignedEntry {
            entry: conflicting.clone(),
            sig: copied_sig,
            public_key_hex: node_pk.clone(),
        };
        let f13 = hive_contract::adjudicate_equivocation(
            &contract,
            &hive_contract::Equivocation {
                checkpoint: genuine.clone(),
                prefix: prefix.clone(),
                a: sign_by_node(&last_entry),
                b: forged_accusation,
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "F13",
            what: format!(
                "an accusation built by editing the ack of a signed entry at seq {}",
                last_entry.op.seq
            ),
            expect: "inadmissible",
            observed: match &f13 {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (forged accusation verified)", other.label()),
            },
            ok: matches!(f13, Verdict::Inadmissible { .. }),
        });

        // F14: a GENUINE equivocation -- the node itself signed both publications.
        let f14 = hive_contract::adjudicate_fraud(
            &contract,
            &hive_contract::FraudProof::EquivocatedAcks(hive_contract::Equivocation {
                checkpoint: genuine.clone(),
                prefix: prefix.clone(),
                a: sign_by_node(&last_entry),
                b: sign_by_node(&conflicting),
            }),
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "F14",
            what: format!(
                "two conflicting acks the node itself signed for seq {}",
                last_entry.op.seq
            ),
            expect: "dishonest",
            observed: match &f14 {
                Verdict::Dishonest { seq, published, recomputed } => format!(
                    "dishonest at seq {seq}: published {}, the log supports {}",
                    &published[..published.len().min(16)],
                    &recomputed[..recomputed.len().min(16)]
                ),
                other => format!("{} (equivocation not proven)", other.label()),
            },
            ok: matches!(f14, Verdict::Dishonest { .. }),
        });

        // F15: both publications signed by an identity the verifier does not trust.
        let f15 = hive_contract::adjudicate_equivocation(
            &contract,
            &hive_contract::Equivocation {
                checkpoint: genuine.clone(),
                prefix: prefix.clone(),
                a: hive_contract::sign_entry(&last_entry, attacker.as_ref())
                    .expect("attacker signs"),
                b: hive_contract::sign_entry(&conflicting, attacker.as_ref())
                    .expect("attacker signs"),
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "F15",
            what: "an equivocation whose publications the attacker signed".to_string(),
            expect: "inadmissible",
            observed: match &f15 {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (forgery verified)", other.label()),
            },
            ok: matches!(f15, Verdict::Inadmissible { .. }),
        });

        // F16: two real entries at DIFFERENT seqs are not an equivocation.
        if let Some(earlier) = tail.iter().rev().nth(1).cloned() {
            let f16 = hive_contract::adjudicate_equivocation(
                &contract,
                &hive_contract::Equivocation {
                    checkpoint: genuine.clone(),
                    prefix: prefix.clone(),
                    a: sign_by_node(&last_entry),
                    b: sign_by_node(&earlier),
                },
                Some(&node_pk),
            );
            probes.push(Probe {
                name: "F16",
                what: format!(
                    "two entries offered as one equivocation (seq {} and {})",
                    last_entry.op.seq, earlier.op.seq
                ),
                expect: "inadmissible",
                observed: match &f16 {
                    Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                    other => format!("{} (forgery verified)", other.label()),
                },
                ok: matches!(f16, Verdict::Inadmissible { .. }),
            });
        }

        // F17: the run-up does not reach the position the two entries claim.
        let short_prefix: Vec<LogEntry> = prefix.iter().skip(1).cloned().collect();
        let f17 = hive_contract::adjudicate_equivocation(
            &contract,
            &hive_contract::Equivocation {
                checkpoint: genuine.clone(),
                prefix: short_prefix,
                a: sign_by_node(&last_entry),
                b: sign_by_node(&conflicting),
            },
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "F17",
            what: "an equivocation whose run-up is missing its first entry".to_string(),
            expect: "inadmissible",
            observed: match &f17 {
                Verdict::Inadmissible { reason } => format!("inadmissible: {reason}"),
                other => format!("{} (forgery verified)", other.label()),
            },
            ok: matches!(f17, Verdict::Inadmissible { .. }),
        });
    }

    // ---- F18: the other kind of fraud, through the same entry point ------------------------------
    // One published state the log does not support: the enum routes it to `audit::adjudicate`, so a
    // verifier that mishandles one kind cannot do so by calling the wrong function.
    {
        let mut tampered2 = segment.clone();
        if let Some(last) = tampered2.last_mut() {
            last.ack.index_hash = "0".repeat(64);
        }
        let f18 = hive_contract::adjudicate_fraud(
            &contract,
            &hive_contract::FraudProof::UnsupportedState(Dispute {
                checkpoint: genuine.clone(),
                segment: tampered2,
                disputed_seq,
            }),
            Some(&node_pk),
        );
        probes.push(Probe {
            name: "F18",
            what: format!("FraudProof::UnsupportedState over seq {disputed_seq}"),
            expect: "dishonest",
            observed: match &f18 {
                Verdict::Dishonest { seq, published, recomputed } => format!(
                    "dishonest at seq {seq}: published {}, the log supports {}",
                    &published[..published.len().min(16)],
                    &recomputed[..recomputed.len().min(16)]
                ),
                other => format!("{} (breach not proven)", other.label()),
            },
            ok: matches!(f18, Verdict::Dishonest { .. }),
        });
    }

    // ---- C5: the shutdown path leaves an empty tail, and must still reopen --------------------------
    // `checkpoint_now` is what a pre-roll or shutdown does: it attests the pending tail and compacts
    // the retained window to nothing. A restart then has NO entries to replay -- the one case where
    // the log's head has to come from the checkpoint alone.
    engine
        .checkpoint_now(&contract.id, ts0 + OPS as u64)
        .await
        .expect("force checkpoint");
    {
        let root = base.join("c5-root");
        std::fs::create_dir_all(root.join(&contract.id)).expect("c5 root");
        copy_dir(&store_dir, &root.join(&contract.id));
        let out = reopen(&root, &contract, node.clone()).await;
        let ok = out.as_deref() == Ok(live_hash.as_str());
        probes.push(Probe {
            name: "C5",
            what: "a restart after a forced checkpoint (empty retained tail)".to_string(),
            expect: "accepted",
            observed: match &out {
                Ok(h) => format!("published {}", &h[..16]),
                Err(e) => format!("REFUSED: {e}"),
            },
            ok,
        });
    }

    // ---- report -------------------------------------------------------------------------------------
    println!("\n== probes ==");
    let mut failed = 0usize;
    for p in &probes {
        let tag = if p.ok {
            "ok      "
        } else {
            "FAIL    "
        };
        println!("{tag} {} {}: {}", p.name, p.what, p.observed);
        println!("           expected {} -> {}", p.expect, if p.ok { "holds" } else { "VIOLATED" });
        if !p.ok {
            failed += 1;
        }
    }
    println!("\n== RESULT ==");
    println!("probes failed   : {failed}");
    if failed == 0 {
        println!("RESULT: ALL PROBES PASSED");
    } else {
        println!("RESULT: {failed} PROBE(S) FAILED");
        std::process::exit(1);
    }
}
