//! Live witness harness for canonical-state reconciliation.
//!
//! Not a test file and not a mock: it loads a REAL platform snapshot (`$HIVE_DATA/state.json` copied
//! off a fleet node), projects canonical state into contract-index shape, replays the contract log
//! the way a monitor would, and diffs the two. Then it BREAKS the invariant on purpose -- a delete
//! that skips the contract, a create that skips it, a re-key that skips it -- and shows the diff
//! catching each one. That is the falsification the design claims for itself.
//!
//! Usage: `contract-reconcile --state <state.json>`
//!
//! `connection` (passwords, connection strings) is never read and never printed.

use hive_contract::contract::{
    managed_db_contract, record_tenant, AuthPolicy, Caller, Contract, Effect, OpError,
    UNTAGGED_TENANT,
};
use hive_contract::engine::Engine;
use hive_contract::exec::Index;
use hive_contract::reconcile::{self, CanonicalDb, DivergenceKind};
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(serde::Deserialize)]
struct RealDb {
    #[serde(default)]
    id: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    team: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    created_ms: u64,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    databases: Vec<RealDb>,
    #[serde(default)]
    database_tombstones: BTreeMap<String, u64>,
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

/// The caller the platform's own gates produce: `admin::tenant` bound the tenant from the
/// cryptographically-bound claim, and `admin::require_project` (admin.rs:3368) returned "owns".
fn admitted(tenant: &str) -> Caller {
    Caller {
        tenant: tenant.to_string(),
        actor: "reconcile-harness".to_string(),
        owns_target: true,
    }
}

fn probe(failures: &mut usize, ok: bool, label: &str, detail: String) {
    if ok {
        println!("PASS {label}: {detail}");
    } else {
        println!("FAIL {label}: {detail}");
        *failures += 1;
    }
}

#[tokio::main]
async fn main() {
    let path = arg("state", "/tmp/vitra-real-state.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(2);
        }
    };
    println!("state            : {path} ({} bytes)", raw.len());
    let snap: Snapshot = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot parse {path}: {e}");
            std::process::exit(2);
        }
    };
    drop(raw);
    let mut failures = 0usize;

    // ---- P1: the real surface, unmodified -------------------------------------------------
    let dbs: Vec<CanonicalDb> = snap
        .databases
        .iter()
        .map(|d| CanonicalDb {
            id: d.id.clone(),
            team: d.team.clone(),
            project: d.project.clone(),
            status: d.status.clone(),
        })
        .collect();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    let mut tenants: BTreeSet<String> = BTreeSet::new();
    let mut empty_team = 0usize;
    for d in &snap.databases {
        *kinds.entry(d.kind.clone()).or_default() += 1;
        *statuses.entry(d.status.clone()).or_default() += 1;
        tenants.insert(record_tenant(Some(&d.team)));
        if record_tenant(Some(&d.team)) == UNTAGGED_TENANT {
            empty_team += 1;
        }
    }
    println!("\n== P1 real snapshot ==");
    println!("databases        : {}", dbs.len());
    println!("by kind          : {kinds:?}");
    println!("by status        : {statuses:?}");
    println!("distinct tenants : {}", tenants.len());
    println!("untagged rows    : {empty_team}");
    println!("tombstones       : {}", snap.database_tombstones.len());
    probe(&mut failures,!dbs.is_empty(), "real input loaded", format!("{} databases", dbs.len()));

    // ---- P2: canonical projection ---------------------------------------------------------
    let canonical = reconcile::project(&dbs, &snap.database_tombstones);
    println!("\n== P2 canonical projection ==");
    println!("projected keys   : {}", canonical.len());
    probe(&mut failures,
        canonical.len() == dbs.len(),
        "every live database is attestable",
        format!("{} keys for {} rows", canonical.len(), dbs.len()),
    );

    // ---- P3: run the real contract over the real databases ---------------------------------
    let contract = managed_db_contract();
    let engine = Engine::new(hive_contract::engine::DEFAULT_SHARDS);
    for d in &snap.databases {
        let tenant = record_tenant(Some(&d.team));
        let r = engine
            .submit(
                &contract,
                admitted(&tenant),
                "db.create",
                payload(&[
                    ("db_id", &d.id),
                    ("kind", &d.kind),
                    ("project", &d.project),
                ]),
                &d.id,
                d.created_ms,
            )
            .await
            .expect("submit");
        assert!(r.committed, "db.create refused: {:?}", r.error);
        // Two writes per database: these produce the contract-local keys canonical state cannot
        // attest, which is exactly the coverage boundary the reconciler has to report, not hide.
        for _ in 0..2 {
            let w = engine
                .submit(
                    &contract,
                    admitted(&tenant),
                    "db.write",
                    payload(&[
                        ("bytes", "1024"),
                        ("db_id", &d.id),
                        ("project", &d.project),
                    ]),
                    &d.id,
                    d.created_ms,
                )
                .await
                .expect("submit");
            assert!(w.committed, "db.write refused: {:?}", w.error);
        }
    }
    let log_len = engine.snapshot_log(&contract.id).await.unwrap().len();
    println!("\n== P3 contract execution over real rows ==");
    println!("ops committed    : {log_len} (1 create + 2 writes per database)");

    // ---- P4: the monitor's replay must agree with canonical state --------------------------
    let replayed: Index = match engine.audit(&contract).await {
        Ok(i) => i,
        Err(b) => {
            eprintln!("monitor replay reported a breach: {b:?}");
            std::process::exit(1);
        }
    };
    let rep = reconcile::reconcile(&canonical, &replayed);
    println!("\n== P4 reconcile: canonical vs monitor replay ==");
    println!("attested keys    : {}", rep.compared);
    println!("divergences      : {}", rep.divergences.len());
    println!("contract-only    : {} (outside canonical coverage)", rep.contract_only.len());
    probe(&mut failures,
        rep.in_sync(),
        "in-contract mutations keep the log and canonical state in agreement",
        format!("{} divergences over {} attested keys", rep.divergences.len(), rep.compared),
    );
    probe(&mut failures,
        rep.contract_only.len() == dbs.len() * 2,
        "coverage boundary reported, not hidden",
        format!("{} contract-only keys (writes + last_bytes)", rep.contract_only.len()),
    );

    // ---- P5: a delete that bypasses the contract -------------------------------------------
    // What a handler that skips `Engine::submit` does to canonical state. The oplog still claims
    // the row is live; the reconciler has to say so.
    let mut bypass = dbs.clone();
    let dropped = bypass.remove(0);
    let canonical_bypass = reconcile::project(&bypass, &snap.database_tombstones);
    let r5 = reconcile::reconcile(&canonical_bypass, &replayed);
    println!("\n== P5 bypass: state deleted without the contract ==");
    for d in r5.divergences.iter().take(3) {
        println!("  {} {}", d.kind.label(), d.key);
    }
    probe(&mut failures,
        r5.count(DivergenceKind::StaleInContract) == 1 && !r5.in_sync(),
        "bypassed delete is detected",
        format!(
            "stale_in_contract={} (dropped {})",
            r5.count(DivergenceKind::StaleInContract),
            dropped.id
        ),
    );

    // ---- P6: a create that bypasses the contract -------------------------------------------
    let mut injected = dbs.clone();
    injected.push(CanonicalDb {
        id: "db_outofband0000000000000000000000".to_string(),
        team: snap.databases[0].team.clone(),
        project: snap.databases[0].project.clone(),
        status: "ready".to_string(),
    });
    let canonical_injected = reconcile::project(&injected, &snap.database_tombstones);
    let r6 = reconcile::reconcile(&canonical_injected, &replayed);
    println!("\n== P6 bypass: state created without the contract ==");
    for d in r6.divergences.iter().take(3) {
        println!("  {} {}", d.kind.label(), d.key);
    }
    probe(&mut failures,
        r6.count(DivergenceKind::MissingFromContract) == 1,
        "bypassed create is detected",
        format!("missing_from_contract={}", r6.count(DivergenceKind::MissingFromContract)),
    );

    // ---- P7: a re-key that bypasses the contract -------------------------------------------
    let mut rekeyed = dbs.clone();
    let moved = rekeyed[1].clone();
    rekeyed[1].team = "tenant-moved-out-of-band".to_string();
    let canonical_rekey = reconcile::project(&rekeyed, &snap.database_tombstones);
    let r7 = reconcile::reconcile(&canonical_rekey, &replayed);
    println!("\n== P7 bypass: row re-keyed without the contract ==");
    for d in r7.divergences.iter().take(3) {
        println!("  {} {}", d.kind.label(), d.key);
    }
    probe(&mut failures,
        r7.count(DivergenceKind::MissingFromContract) == 1
            && r7.count(DivergenceKind::StaleInContract) == 1,
        "bypassed re-key is detected as one stale + one missing",
        format!(
            "stale={} missing={} (moved {})",
            r7.count(DivergenceKind::StaleInContract),
            r7.count(DivergenceKind::MissingFromContract),
            moved.id
        ),
    );

    // ---- P8: the gates the new module could have weakened, still in force ------------------
    println!("\n== P8 authorization is unchanged ==");
    let tenant0 = record_tenant(Some(&snap.databases[0].team));
    let no_owner = engine
        .submit(
            &contract,
            Caller { tenant: tenant0.clone(), actor: "p8".into(), owns_target: false },
            "db.delete",
            payload(&[("db_id", &snap.databases[0].id), ("project", &snap.databases[0].project)]),
            &snap.databases[0].id,
            0,
        )
        .await
        .expect("submit");
    probe(&mut failures,
        !no_owner.committed && no_owner.error.as_deref() == Some(&OpError::TenantMismatch.to_string()),
        "a caller that does not own the target is refused",
        format!("{:?}", no_owner.error),
    );
    let untagged = engine
        .submit(
            &contract,
            Caller { tenant: UNTAGGED_TENANT.to_string(), actor: "p8".into(), owns_target: true },
            "db.create",
            payload(&[("db_id", "db_x"), ("kind", "redis"), ("project", "p")]),
            "db_x",
            0,
        )
        .await
        .expect("submit");
    probe(&mut failures,
        !untagged.committed && untagged.error.as_deref() == Some(&OpError::UntaggedTenant.to_string()),
        "an untagged tenant is refused (fail-closed, never 'personal')",
        format!("{:?}", untagged.error),
    );
    let mut backdoor = Contract::new("hive.backdoor.v1");
    let declared = backdoor.declare("db.write", AuthPolicy::OpenParticipation, Effect::StateWrite, &["db_id"]);
    probe(&mut failures,
        declared.is_err(),
        "open participation may never be declared as a state write",
        format!("{:?}", declared.err().map(|e| e.to_string())),
    );
    let open = engine
        .submit(
            &contract,
            Caller { tenant: "anyone".into(), actor: "relay".into(), owns_target: false },
            "db.propose",
            payload(&[("db_id", &snap.databases[0].id), ("note", "observed")]),
            &snap.databases[0].id,
            0,
        )
        .await
        .expect("submit");
    let after = engine.index_hash(&contract.id).await.unwrap();
    probe(&mut failures,
        open.committed && after == hive_contract::index_hash(&replayed),
        "open participation is admitted and moves no state",
        format!("committed seq {} and index unchanged {}", open.seq, &after[..8]),
    );

    // ---- P9: how much of the surface is actually wired -------------------------------------
    let (declared_n, wired_n) = reconcile::scope_report();
    println!("\n== P9 contract scope coverage ==");
    for e in reconcile::DECLARED_SCOPE {
        println!(
            "  {:<10} {}:{}  {}",
            e.op_kind,
            e.file,
            e.line,
            if e.wired { "WIRED" } else { "not wired" }
        );
    }
    println!("declared={declared_n} wired={wired_n}");
    probe(&mut failures,
        declared_n == 6,
        "the declared mutation surface is the one the map located",
        format!("{declared_n} paths"),
    );

    if failures == 0 {
        println!("ALL PROBES PASSED");
    } else {
        println!("{failures} PROBE(S) FAILED");
        std::process::exit(1);
    }
}
