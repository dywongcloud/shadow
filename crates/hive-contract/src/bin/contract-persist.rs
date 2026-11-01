//! Live witness harness for contract persistence: durability across a process restart, bounded
//! growth under checkpointing + compaction, and checkpoint-equivalent replay.
//!
//! Not a test file and not a mock. It runs the real `Engine`, the real `Store` and the real signer
//! against a real directory on a real filesystem, and it proves the properties by doing them:
//!
//!   * **Restart.** Phase 1 writes `ops` operations and prints the live index hash. It then spawns a
//!     SECOND PROCESS (`std::process::Command` on its own `current_exe`) which opens the same
//!     directory cold -- no shared memory, no inherited state -- and prints the index hash it
//!     rebuilt from disk. The two hashes are compared here. A serialization round-trip inside one
//!     process would prove nothing about a restart, so it is not what runs.
//!   * **Bounded growth.** The same op stream is written twice: once with checkpointing on
//!     (`--window`) and once into a control directory whose window never closes. Retained entries,
//!     file counts and directory bytes are reported for both.
//!   * **Checkpoint-equivalent replay.** Replaying from a signed checkpoint plus the tail must land
//!     on the same index hash as replaying the whole history. Measured against the OLDEST retained
//!     checkpoint and the newest one, never just the convenient one.
//!   * **Cryptographic binding.** A checkpoint is verified by a stranger (signature + its own index
//!     hash), an inclusion path is cut and verified against a checkpoint's Merkle root, and a forged
//!     variant of each is shown to fail. A dispute over one ack is adjudicated both ways.
//!
//! Usage: `contract-persist [--dir <path>] [--ops N] [--window N] [--retain N] [--state <file>]`
//!
//! The signing key is generated into the run directory (0600) on first use. This harness is the only
//! caller of `KeyedSigner::generate_pkcs8` in the tree: the library itself never mints a key.

use hive_contract::audit::{adjudicate, replay, replay_from, Dispute, Verdict};
use hive_contract::contract::{Caller, Contract};
use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::Engine;
use hive_contract::merkle;
use hive_contract::oplog::{LogEntry, OpLog};
use hive_contract::persist::{
    read_checkpoint, read_entries_from, read_latest_checkpoint, read_window, PersistConfig,
    DEFAULT_RETAIN_CHECKPOINTS,
};
use hive_contract::signing::{CheckpointSigner, KeyedSigner};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Set on the child so it knows to open cold instead of writing.
const PHASE_ENV: &str = "HIVE_CONTRACT_PERSIST_PHASE";
/// The line the parent parses out of the child's stdout.
const CHILD_MARKER: &str = "CHILD_INDEX_HASH=";

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

/// Real database ids when the operator has a production snapshot, synthetic ones otherwise. Either
/// way the code under witness is the real contract, the real engine and the real store.
fn targets(state: &str, count: usize) -> Vec<String> {
    if !state.is_empty() {
        if let Ok(raw) = std::fs::read_to_string(state) {
            #[derive(serde::Deserialize)]
            struct Db {
                #[serde(default)]
                id: String,
            }
            #[derive(serde::Deserialize)]
            struct Snap {
                #[serde(default)]
                databases: Vec<Db>,
            }
            if let Ok(s) = serde_json::from_str::<Snap>(&raw) {
                let mut ids: Vec<String> = s.databases.iter().map(|d| d.id.clone()).collect();
                ids.sort();
                if !ids.is_empty() {
                    return ids;
                }
            }
        }
    }
    (0..count).map(|i| format!("db_witness_{i:04}")).collect()
}

/// One deterministic op stream, replayed identically into every engine under test.
async fn run_ops(
    engine: &Engine,
    contract: &Contract,
    targets: &[String],
    ops: usize,
    ts0: u64,
) -> usize {
    let mut committed = 0usize;
    for i in 0..ops {
        let db = &targets[i % targets.len()];
        let tenant = format!("team-{:02}", i % 4);
        let project = format!("proj-{:02}", i % 3);
        let (kind, payload) = match i % 10 {
            0 => (
                "db.create",
                payload(&[("db_id", db), ("kind", "sqlite"), ("project", &project)]),
            ),
            9 => ("db.delete", payload(&[("db_id", db), ("project", &project)])),
            5 => (
                "db.propose",
                payload(&[("db_id", db), ("note", "relayed by any node")]),
            ),
            _ => (
                "db.write",
                payload(&[
                    ("bytes", &format!("{}", i * 7)),
                    ("db_id", db),
                    ("project", &project),
                ]),
            ),
        };
        let r = engine
            .submit(
                contract,
                Caller {
                    tenant,
                    actor: "witness".to_string(),
                    owns_target: true,
                },
                kind,
                payload,
                db,
                ts0 + i as u64,
                Ok(()),
            )
            .await;
        match r {
            Ok(s) if s.committed => committed += 1,
            Ok(s) => panic!("op {i} not committed: {:?} / {:?}", s.error, s.persist_error),
            Err(e) => panic!("op {i} refused: {e}"),
        }
    }
    committed
}

fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

fn count_files(dir: &std::path::Path, prefix: &str, suffix: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.starts_with(prefix) && n.ends_with(suffix)
                })
                .count()
        })
        .unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let dir_arg = arg("dir", "");
    let ops: usize = arg("ops", "2000").parse().unwrap_or(2000);
    let window: usize = arg("window", "64").parse().unwrap_or(64);
    let retain: usize = arg("retain", &DEFAULT_RETAIN_CHECKPOINTS.to_string())
        .parse()
        .unwrap_or(DEFAULT_RETAIN_CHECKPOINTS);
    let state = arg("state", "");

    let base = if dir_arg.is_empty() {
        PathBuf::from(format!("/private/tmp/hive-contract-persist-{}", std::process::id()))
    } else {
        PathBuf::from(dir_arg)
    };
    // The reopen half must NOT re-create the run directory: this harness wipes it on a fresh run, and
    // a child that wiped it would be "restarting" into an empty directory with nothing to rebuild.
    if std::env::var(PHASE_ENV).as_deref() == Ok("reopen") {
        reopen(&base.join("compacted"), window, retain).await;
        return;
    }
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create run dir");

    let mut failures = 0usize;
    let contract = managed_db_contract();

    // ---- the signing key -----------------------------------------------------------------------
    // Generated here, by the harness, into the run directory at 0600. The library has no path that
    // mints one: `KeyedSigner::from_hex_file` refuses an absent file, and `Store::open` refuses a
    // config with no signer at all.
    let key_path = base.join("integrity_signing.key");
    let pkcs8 = KeyedSigner::generate_pkcs8().expect("generate ed25519 key");
    std::fs::write(&key_path, hex::encode(&pkcs8)).expect("write key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600 key");
    }
    let signer: Arc<dyn CheckpointSigner> =
        Arc::new(KeyedSigner::from_pkcs8("witness-node", &pkcs8).expect("load generated key"));
    // What a stored checkpoint must claim to be adopted: signed by THIS identity, for THIS contract.
    // A stranger's artefact is not made acceptable by being internally consistent.
    let node_pk = signer.public_key();
    let node_pk_hex = hex::encode(&node_pk);
    let trust = hive_contract::checkpoint::TrustedCheckpoint {
        public_key_hex: &node_pk_hex,
        contract_id: &contract.id,
        contract_version: contract.version,
    };

    let targets = targets(&state, 12);
    let ts0 = now_ms();
    println!("== contract-persist ==");
    println!("ops              : {ops}");
    println!("window           : {window}");
    println!("retain           : {retain}");
    println!("targets          : {} ({})", targets.len(), if state.is_empty() { "synthetic" } else { "real snapshot" });

    // ---- P1: the same history, written twice ---------------------------------------------------
    // `control` never closes a window, so it retains the WHOLE log; `compacted` checkpoints and
    // compacts. Same ops, same order -> the index must be identical, and that is P2's claim.
    let control_dir = base.join("control");
    let compact_dir = base.join("compacted");
    let control = Engine::open(
        4,
        control_dir.clone(),
        PersistConfig {
            checkpoint_every: ops + 1,
            retain_checkpoints: retain,
            signer: Some(signer.clone()),
            sync: true,
        },
    );
    let compacted = Engine::open(
        4,
        compact_dir.clone(),
        PersistConfig {
            checkpoint_every: window,
            retain_checkpoints: retain,
            signer: Some(signer.clone()),
            sync: true,
        },
    );
    control
        .register(contract.clone())
        .await
        .expect("register control");
    compacted
        .register(contract.clone())
        .await
        .expect("register compacted");

    let control_store = control
        .store_dir(&contract.id)
        .expect("control store dir");
    let compact_store = compacted
        .store_dir(&contract.id)
        .expect("compacted store dir");

    let committed_control = run_ops(&control, &contract, &targets, ops, ts0).await;
    let committed_compact = run_ops(&compacted, &contract, &targets, ops, ts0).await;
    println!("\n== P1 written ==");
    println!("committed        : control={committed_control} compacted={committed_compact} of {ops}");

    // ---- P2: compaction does not change the truth ----------------------------------------------
    let control_log = control
        .snapshot_log(&contract.id)
        .await
        .expect("control log");
    let compact_log = compacted
        .snapshot_log(&contract.id)
        .await
        .expect("compacted log");
    let full_index = replay(&contract, &control_log).expect("control replay");
    let hash_full = hive_contract::exec::index_hash(&full_index);
    let hash_compact_live = compacted
        .index_hash(&contract.id)
        .await
        .expect("compacted index hash");
    // The compacted tail alone does NOT replay to the same place, and that is the point: `replay`
    // with no checkpoint starts from an EMPTY index, so the acks it meets are the ones a checkpoint
    // already accounts for. It diverges, and it must -- a bare tail is not a proof of anything.
    let bare_tail = match replay(&contract, &compact_log) {
        Ok(index) => format!("{} (replayed)", &hive_contract::exec::index_hash(&index)[..16]),
        Err(e) => format!("diverges as expected: {e}"),
    };
    println!("\n== P2 same history, two retention policies ==");
    println!("control   entries: {} (base {}) hash {}", control_log.len(), control_log.base(), &hash_full[..16]);
    println!("compacted entries: {} (base {}) hash {}", compact_log.len(), compact_log.base(), &hash_compact_live[..16]);
    let p2 = hash_full == hash_compact_live;
    println!("P2 identical index hash: {p2}");
    if !p2 {
        failures += 1;
    }
    // A bare tail starts from an EMPTY index, so it must NOT reproduce the full history: it is not a
    // proof of anything, and P3 is what supplies the missing half.
    println!("   bare tail replay: {bare_tail}");

    // ---- P3: replay from a checkpoint equals replay from the full log --------------------------
    let ends = checkpoint_ends(&compact_store);
    println!("\n== P3 replay from a checkpoint ==");
    println!("checkpoints on disk: {} ({}..{})", ends.len(), ends.first().copied().unwrap_or(0), ends.last().copied().unwrap_or(0));
    let mut p3 = true;
    for (n, end) in [ends.first().copied(), ends.last().copied()]
        .into_iter()
        .flatten()
        .enumerate()
    {
        // Resume from a checkpoint the way a third party would: read it, verify it, read the tail
        // from disk, replay. Nothing here touches the in-memory log.
        let read = futures_lite_block_on(read_checkpoint(&compact_store, end, Some(&trust)));
        let signed = match read {
            Ok(s) => s,
            Err(e) => {
                println!("  checkpoint {end}: UNREADABLE ({e})");
                p3 = false;
                continue;
            }
        };
        let tail = futures_lite_block_on(read_entries_from(&compact_store, end, Some(&node_pk))).unwrap_or_default();
        let log = match OpLog::restore(end, &signed.checkpoint.log_head, tail) {
            Ok(l) => l,
            Err(e) => {
                println!("  checkpoint {end}: tail does not chain ({})", e.stored);
                p3 = false;
                continue;
            }
        };
        match replay_from(&contract, &log, Some(&signed.checkpoint)) {
            Ok(index) => {
                let h = hive_contract::exec::index_hash(&index);
                let ok = h == hash_full;
                println!(
                    "  {}: checkpoint {end} + {} tail entries -> {} {}",
                    if n == 0 { "oldest" } else { "newest" },
                    log.len(),
                    &h[..16],
                    if ok { "MATCHES full replay" } else { "DIVERGES" }
                );
                if !ok {
                    p3 = false;
                }
            }
            Err(e) => {
                println!("  checkpoint {end}: replay refused ({e})");
                p3 = false;
            }
        }
    }
    println!("P3 checkpoint replay == full replay: {p3}");
    if !p3 {
        failures += 1;
    }

    // ---- P4: growth is bounded ------------------------------------------------------------------
    let control_bytes = dir_bytes(&control_store);
    let compact_bytes = dir_bytes(&compact_store);
    let compact_segments = count_files(&compact_store, "seg-", ".jsonl");
    let compact_cps = count_files(&compact_store, "cp-", ".json");
    let control_segments = count_files(&control_store, "seg-", ".jsonl");
    println!("\n== P4 bounded growth ==");
    println!("control   : {} entries retained, {} segment file(s), {} bytes", control_log.len(), control_segments, control_bytes);
    println!("compacted : {} entries retained, {} segment file(s), {} checkpoint(s), {} bytes", compact_log.len(), compact_segments, compact_cps, compact_bytes);
    let p4 = compact_log.len() <= window
        && compact_segments <= retain + 1
        && compact_cps <= retain
        && compact_bytes < control_bytes
        && control_log.len() == ops;
    println!(
        "P4 retained <= window ({} <= {}) and files <= retain ({} <= {}) and smaller than control: {p4}",
        compact_log.len(), window, compact_cps, retain
    );
    if !p4 {
        failures += 1;
    }

    // ---- P5: a stranger verifies a checkpoint, an inclusion path and a dispute ------------------
    println!("\n== P5 cryptographic binding ==");
    let latest = futures_lite_block_on(read_latest_checkpoint(&compact_store, Some(&trust)))
        .expect("latest checkpoint readable")
        .expect("a checkpoint exists");
    let p5_verify = latest.verify(Some(&trust)).is_ok();
    println!("  checkpoint verifies (signature + own index hash): {p5_verify}");
    if !p5_verify {
        failures += 1;
    }

    // Inclusion: cut a path for an entry inside the newest checkpoint's own window and check it.
    let cp = &latest.checkpoint;
    let win = futures_lite_block_on(read_window(&compact_store, cp.window_base, cp.window_end, window, Some(&node_pk)))
        .unwrap_or_default();
    let mut p5_inclusion = false;
    let mut p5_forged = false;
    if let Some(entry) = win.first() {
        let hashes: Vec<String> = win.iter().map(|e| e.entry_hash.clone()).collect();
        let path = merkle::merkle_prove(&hashes, 0).expect("prove first leaf");
        p5_inclusion = latest.verify_inclusion(entry, &path).is_ok();
        println!(
            "  inclusion path: {} sibling(s) for seq {} -> {}",
            path.siblings.len(),
            entry.op.seq,
            if p5_inclusion { "accepted" } else { "rejected" }
        );
        // Forged: the same path offered for a different entry must not verify.
        let mut forged = entry.clone();
        forged.op.payload.insert("project".to_string(), "forged".to_string());
        p5_forged = latest.verify_inclusion(&forged, &path).is_err();
        println!("  same path for a rewritten entry: {}", if p5_forged { "rejected" } else { "ACCEPTED (bad)" });
        // Forged: a path cut against a different leaf count must not verify either.
        let wrong_shape = merkle::MerklePath {
            index: path.index,
            leaves: path.leaves + 1,
            leaf: path.leaf.clone(),
            siblings: path.siblings.clone(),
        };
        let shape_ok = latest.verify_inclusion(entry, &wrong_shape).is_err();
        println!("  path claiming a different window size: {}", if shape_ok { "rejected" } else { "ACCEPTED (bad)" });
        if !shape_ok {
            failures += 1;
        }
    } else {
        println!("  inclusion path: no window entries to prove");
    }
    if !p5_inclusion || !p5_forged {
        failures += 1;
    }

    // Dispute: an honest segment settles as honest; the same segment with one ack rewritten settles
    // as dishonest. Both are decided from the artefact alone.
    // The segment is what follows the checkpoint, not what it covers: an accusation says "from this
    // attested state, these entries led to that ack".
    let segment: Vec<LogEntry> =
        futures_lite_block_on(read_entries_from(&compact_store, cp.window_end, Some(&node_pk))).unwrap_or_default();
    let disputed_seq = segment.last().map(|e| e.op.seq).unwrap_or(0);
    println!(
        "  dispute segment : {} entries from seq {} to {}",
        segment.len(),
        cp.window_end,
        disputed_seq
    );
    let honest = adjudicate(
        &contract,
        &Dispute {
            checkpoint: latest.clone(),
            segment: segment.clone(),
            disputed_seq,
        },
        Some(&node_pk_hex),
    );
    println!("  dispute over an honest ack: {} at seq {:?}", honest.label(), seq_of(&honest));
    let mut tampered = segment.clone();
    if let Some(last) = tampered.last_mut() {
        last.ack.index_hash = "0".repeat(64);
    }
    let dishonest = adjudicate(
        &contract,
        &Dispute {
            checkpoint: latest.clone(),
            segment: tampered,
            disputed_seq,
        },
        Some(&node_pk_hex),
    );
    println!("  dispute over a rewritten ack: {}", dishonest.label());
    let p5_dispute = matches!(honest, Verdict::Honest { .. })
        && matches!(dishonest, Verdict::Dishonest { .. });
    if !p5_dispute {
        failures += 1;
    }

    // A checkpoint whose stored index was edited must not verify: the signature covers index_hash,
    // so the edit is caught even though the signature itself is untouched. Done on a copy.
    let tamper_dir = base.join("tampered");
    std::fs::create_dir_all(&tamper_dir).expect("tamper dir");
    let cp_file = format!("cp-{:012}.json", cp.window_end);
    let _ = std::fs::copy(compact_store.join(&cp_file), tamper_dir.join(&cp_file));
    let mut forged_cp: hive_contract::checkpoint::SignedCheckpoint = {
        let raw = std::fs::read_to_string(tamper_dir.join(&cp_file)).expect("read copy");
        serde_json::from_str(&raw).expect("parse copy")
    };
    forged_cp
        .checkpoint
        .index
        .insert("__untagged__/victim/status".to_string(), "live".to_string());
    std::fs::write(
        tamper_dir.join(&cp_file),
        serde_json::to_vec_pretty(&forged_cp).expect("serialize"),
    )
    .expect("write forged");
    let tamper_caught = futures_lite_block_on(read_checkpoint(&tamper_dir, cp.window_end, Some(&trust))).is_err();
    println!("  checkpoint with an edited index: {}", if tamper_caught { "refused" } else { "ACCEPTED (bad)" });
    if !tamper_caught {
        failures += 1;
    }

    // ---- P6: a real process restart -------------------------------------------------------------
    // A second OS process, forked from this one's own image, opens the same directory cold. Nothing
    // is inherited except the path.
    println!("\n== P6 restart ==");
    let exe = std::env::current_exe().expect("current exe");
    let out = std::process::Command::new(exe)
        .env(PHASE_ENV, "reopen")
        .args([
            "--dir",
            &base.display().to_string(),
            "--ops",
            &ops.to_string(),
            "--window",
            &window.to_string(),
            "--retain",
            &retain.to_string(),
        ])
        .output()
        .expect("spawn reopen process");
    let child_out = String::from_utf8_lossy(&out.stdout).to_string();
    for line in child_out.lines() {
        println!("  | {line}");
    }
    if !out.status.success() {
        println!("  reopen process exited {:?}", out.status.code());
        println!("  stderr: {}", String::from_utf8_lossy(&out.stderr));
        failures += 1;
    }
    let child_hash = child_out
        .lines()
        .find_map(|l| l.trim().strip_prefix(CHILD_MARKER))
        .unwrap_or("")
        .to_string();
    let p6 = !child_hash.is_empty() && child_hash == hash_full;
    println!("  this process : {}", &hash_full[..16]);
    println!("  after restart: {}", if child_hash.is_empty() { "<none>" } else { &child_hash[..16] });
    println!("P6 restart replays to an identical index hash: {p6}");
    if !p6 {
        failures += 1;
    }

    println!("\n== RESULT ==");
    println!("probes failed   : {failures}");
    if failures == 0 {
        println!("RESULT: ALL PROBES PASSED");
    } else {
        println!("RESULT: {failures} PROBE(S) FAILED");
        std::process::exit(1);
    }
}

fn seq_of(v: &Verdict) -> Option<u64> {
    match v {
        Verdict::Honest { seq, .. } | Verdict::Dishonest { seq, .. } => Some(*seq),
        Verdict::Inadmissible { .. } => None,
    }
}

fn checkpoint_ends(dir: &std::path::Path) -> Vec<u64> {
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

/// The cold-open half: a separate process whose entire job is to rebuild from disk.
async fn reopen(dir: &std::path::Path, window: usize, retain: usize) {
    let contract = managed_db_contract();
    let key_path = dir
        .parent()
        .expect("run dir")
        .join("integrity_signing.key");
    let signer: Arc<dyn CheckpointSigner> = Arc::new(
        KeyedSigner::from_hex_file("witness-node", &key_path).expect("reopen: load signing key"),
    );
    let node_pk = signer.public_key();
    let node_pk_hex = hex::encode(&node_pk);
    let trust = hive_contract::checkpoint::TrustedCheckpoint {
        public_key_hex: &node_pk_hex,
        contract_id: &contract.id,
        contract_version: contract.version,
    };
    let hash = async {
        let engine = Engine::open(
            4,
            dir.to_path_buf(),
            PersistConfig {
                checkpoint_every: window,
                retain_checkpoints: retain,
                signer: Some(signer),
                sync: true,
            },
        );
        engine.register(contract.clone()).await.expect("reopen register");
        let store_dir = engine.store_dir(&contract.id).expect("reopen store dir");
        let log = engine.snapshot_log(&contract.id).await.expect("reopen log");
        println!("reopen log base  : {}", log.base());
        println!("reopen retained  : {} entries, next_seq {}", log.len(), log.next_seq());
        log.verify_chain().expect("reopen chain verifies");
        println!("reopen chain     : verifies from base_prev");
        let live = engine.index_hash(&contract.id).await.expect("reopen index hash");
        // And the same thing the long way: from the newest checkpoint plus the tail off disk.
        let latest = read_latest_checkpoint(&store_dir, Some(&trust))
            .await
            .expect("reopen: checkpoint readable")
            .expect("reopen: a checkpoint exists");
        let tail = read_entries_from(&store_dir, latest.checkpoint.window_end, Some(&node_pk))
            .await
            .unwrap_or_default();
        let rebuilt = OpLog::restore(
            latest.checkpoint.window_end,
            &latest.checkpoint.log_head,
            tail,
        )
        .expect("reopen: tail chains onto checkpoint");
        match replay_from(&contract, &rebuilt, Some(&latest.checkpoint)) {
            Ok(index) => {
                let h = hive_contract::exec::index_hash(&index);
                println!("reopen from cp   : {}", &h[..16]);
            }
            Err(e) => println!("reopen from cp   : REFUSED ({e})"),
        }
        live
    }
    .await;
    println!("{CHILD_MARKER}{hash}");
}

/// Run a read-only future from the harness's own async `main` without starting a second runtime.
/// These are disk reads between submissions, so they are parked off the async worker the same way
/// the platform parks bulk work -- never a nested `Runtime::new`.
fn futures_lite_block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}
