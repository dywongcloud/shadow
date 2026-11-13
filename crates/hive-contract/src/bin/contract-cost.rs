//! Live cost witness for contract durability: what one recorded mutation costs with and without a
//! durable store behind the engine.
//!
//! Durability is not free and this harness is the instrument that says what it costs. Everything it
//! reports is measured by running the real `Engine` against a real directory: no model, no estimate.
//!
//!   * **In-memory (`--mode fast`).** `Engine::new` -- the shape the platform ran before: an op is
//!     validated, committed and acked, and nothing touches the disk.
//!   * **Durable (`--mode slow`).** `Engine::open` -- the same op plus one append and one
//!     `sync_data` of a single JSON line, inside the contract's shard write lock.
//!
//! Both run the SAME op stream, sequentially, which is the shape a request path actually has: one
//! contract, one shard lock, one op at a time. Percentiles are reported rather than a mean alone,
//! because a request path is hurt by the tail, not by the average.
//!
//! The harness also measures what a restart costs: opening the store cold and rebuilding the index
//! from the newest verified checkpoint plus the tail. That is the price of the durability, paid once
//! per boot rather than per op.
//!
//! Usage: `contract-cost [--dir <path>] [--ops N] [--window N]`
//!
//! The signing key is generated into the run directory (0600) on first use; the library never mints
//! one itself.

use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::Engine;
use hive_contract::persist::PersistConfig;
use hive_contract::signing::KeyedSigner;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

fn arg(name: &str, default: &str) -> String {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == format!("--{name}") {
            return it.next().unwrap_or_else(|| default.to_string());
        }
    }
    default.to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pct(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) * p) / 100;
    sorted[i]
}

fn report(label: &str, samples: &[u128]) {
    let mut s = samples.to_vec();
    s.sort_unstable();
    let total: u128 = s.iter().sum();
    let mean = if s.is_empty() {
        0
    } else {
        total / s.len() as u128
    };
    println!(
        "COST {label} ops={} mean_us={} p50_us={} p95_us={} p99_us={} max_us={} total_ms={}",
        s.len(),
        mean,
        pct(&s, 50),
        pct(&s, 95),
        pct(&s, 99),
        pct(&s, 100),
        total / 1000,
    );
}

async fn bench(engine: &Engine, ops: usize, window: usize) -> Vec<u128> {
    let contract = managed_db_contract();
    let mut samples = Vec::with_capacity(ops);
    let t0 = now_ms();
    for i in 0..ops {
        let db = format!("db_cost_{:04}", i % 64);
        let tenant = format!("team-{:02}", i % 4);
        let project = format!("proj-{:02}", i % 3);
        let (kind, extra): (&str, Vec<(&str, &str)>) = match i % 4 {
            0 => ("db.create", vec![("kind", "sqlite")]),
            1 => ("db.write", vec![("bytes", "1024")]),
            2 => ("db.propose", vec![("note", "any node may append this")]),
            _ => ("db.delete", vec![]),
        };
        let mut payload = BTreeMap::new();
        payload.insert("db_id".to_string(), db.clone());
        payload.insert("project".to_string(), project);
        for (k, v) in extra {
            payload.insert(k.to_string(), v.to_string());
        }
        let started = std::time::Instant::now();
        let r = engine
            .submit(
                &contract,
                hive_contract::Caller {
                    tenant: tenant.clone(),
                    actor: "cost-harness".to_string(),
                    owns_target: true, platform_attested: false,
                },
                kind,
                payload,
                &db,
                t0 + i as u64,
                Ok(()),
            )
            .await;
        samples.push(started.elapsed().as_micros());
        match r {
            Ok(r) => {
                if let Some(e) = r.persist_error {
                    println!("COST persist_error at op {i}: {e}");
                }
            }
            Err(e) => println!("COST submit refused at op {i}: {e}"),
        }
    }
    // A window this short is what makes the checkpoint cost show up in the per-op numbers instead of
    // being amortised over `--window` ops; the default keeps it visible.
    let _ = window;
    samples
}

#[tokio::main]
async fn main() {
    let dir = PathBuf::from(arg("dir", "/tmp/hive-contract-cost"));
    let ops: usize = arg("ops", "1000").parse().unwrap_or(1000);
    let window: usize = arg("window", "256").parse().unwrap_or(256);
    let _ = std::fs::create_dir_all(&dir);

    let key_path = dir.join("signing.key");
    if !key_path.exists() {
        let doc = KeyedSigner::generate_pkcs8().expect("generate signing key");
        std::fs::write(&key_path, hex::encode(&doc)).expect("write signing key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
    }
    let signer = Arc::new(
        KeyedSigner::from_hex_file("cost-harness", &key_path).expect("load signing key"),
    );
    println!("COST dir={} ops={} window={}", dir.display(), ops, window);

    // ---- 1. in-memory: the shape the platform ran before -------------------------------------
    let fast = Engine::new(hive_contract::DEFAULT_SHARDS);
    report("in_memory", &bench(&fast, ops, window).await);

    // ---- 2. durable: one append + one sync_data per op ---------------------------------------
    let cfg = PersistConfig {
        checkpoint_every: window,
        retain_checkpoints: hive_contract::DEFAULT_RETAIN_CHECKPOINTS,
        signer: Some(signer.clone() as Arc<dyn hive_contract::signing::CheckpointSigner>),
        sync: true,
    };
    let slow = Engine::open(
        hive_contract::DEFAULT_SHARDS,
        dir.join("log"),
        cfg.clone(),
    );
    report("durable", &bench(&slow, ops, window).await);

    // ---- 2b. durable WITHOUT the per-entry fsync: the mode the platform runs ------------------
    // The entry is still written before `submit` returns and the segment is still synced before
    // every window closes, so a restart loses nothing; what is given up is the guarantee that one
    // entry survives a POWER LOSS the instant it is acked.
    let cfg_deferred = PersistConfig {
        sync: false,
        ..cfg.clone()
    };
    let deferred = Engine::open(
        hive_contract::DEFAULT_SHARDS,
        dir.join("log-deferred"),
        cfg_deferred.clone(),
    );
    report("durable_no_entry_sync", &bench(&deferred, ops, window).await);
    let started = std::time::Instant::now();
    let reopened_deferred = Engine::open(
        hive_contract::DEFAULT_SHARDS,
        dir.join("log-deferred"),
        cfg_deferred,
    );
    let contract = managed_db_contract();
    reopened_deferred
        .register(contract.clone())
        .await
        .expect("cold reopen of the deferred log rebuilds from the newest verified checkpoint");
    println!(
        "COST restart_deferred reopen_us={} index_match={}",
        started.elapsed().as_micros(),
        reopened_deferred.index_hash(&contract.id).await.unwrap_or_default()
            == deferred.index_hash(&contract.id).await.unwrap_or_default()
    );

    // ---- 3. restart: what the durability costs once per boot ---------------------------------
    let started = std::time::Instant::now();
    let reopened =
        Engine::open(hive_contract::DEFAULT_SHARDS, dir.join("log"), cfg.clone());
    let contract = managed_db_contract();
    reopened
        .register(contract.clone())
        .await
        .expect("cold reopen rebuilds from the newest verified checkpoint");
    let reopen_us = started.elapsed().as_micros();
    let reopened_hash = reopened.index_hash(&contract.id).await.unwrap_or_default();
    let live_hash = slow.index_hash(&contract.id).await.unwrap_or_default();
    println!(
        "COST restart reopen_us={} index_match={} index_hash={}",
        reopen_us,
        reopened_hash == live_hash,
        live_hash
    );
    println!(
        "RESULT {}",
        if reopened_hash == live_hash { "PASS" } else { "FAIL" }
    );
}
