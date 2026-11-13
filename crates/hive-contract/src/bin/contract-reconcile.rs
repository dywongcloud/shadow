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
    record_tenant, AuthPolicy, Caller, Contract, Effect, OpError, UNTAGGED_TENANT,
};
use hive_contract::contracts::managed_database::managed_db_contract;
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

/// The write plane a real node persists: canonical state's own count of the writes it served
/// against each managed database (`crates/hive-cloud/src/databases.rs`, `WritePlane`).
#[derive(serde::Deserialize, Default)]
struct RealWritePlane {
    #[serde(default)]
    writes: u64,
    #[serde(default)]
    last_bytes: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RealDatabaseData {
    #[serde(default)]
    write_plane: BTreeMap<String, RealWritePlane>,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    databases: Vec<RealDb>,
    #[serde(default)]
    database_tombstones: BTreeMap<String, u64>,
    /// What makes the write plane attestable at all. Absent in a synthetic snapshot and present in
    /// one copied off a node that has served writes, so P11 reports the difference rather than
    /// assuming it.
    #[serde(default)]
    database_data: RealDatabaseData,
}

/// Bounds on P11, so a snapshot taken off a busy node cannot turn a reconciliation into an
/// unbounded replay. Databases beyond the cap, or carrying more writes than the cap, are reported as
/// skipped rather than compared with a count the harness would have to invent.
const WRITE_PLANE_DBS: usize = 8;
const WRITE_PLANE_MAX: u64 = 256;

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
        owns_target: true, platform_attested: false,
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
    let mut dbs: Vec<CanonicalDb> = snap
        .databases
        .iter()
        .map(|d| CanonicalDb {
            id: d.id.clone(),
            team: d.team.clone(),
            project: d.project.clone(),
            status: d.status.clone(),
            created_ms: d.created_ms,
            write_count: None,
            last_bytes: None,
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
    // Status only at this point: no writes have been performed yet, so no write plane is recorded.
    let canonical_status = reconcile::project(&dbs, &snap.database_tombstones);
    println!("\n== P2 canonical projection ==");
    println!("projected keys   : {}", canonical_status.len());
    probe(&mut failures,
        canonical_status.len() == dbs.len(),
        "every live database is attestable",
        format!("{} keys for {} rows", canonical_status.len(), dbs.len()),
    );

    // ---- P3: run the real contract over the real databases ---------------------------------
    let contract = managed_db_contract();
    let engine = Engine::new(hive_contract::engine::DEFAULT_SHARDS);
    let mut writes_per_db: BTreeMap<String, u64> = BTreeMap::new();
    // A row with no tenant bound cannot emit a `TenantAuthorized` op at all -- the contract refuses
    // it by construction -- so it is not submitted rather than panicking the harness on the first
    // real snapshot that carries one. Reported as a count: an untagged row is a finding, not a
    // silent omission.
    let mut untagged_skipped = 0usize;
    for d in &snap.databases {
        let tenant = record_tenant(Some(&d.team));
        if tenant == UNTAGGED_TENANT {
            untagged_skipped += 1;
            continue;
        }
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
                Ok(()),
            )
            .await
            .expect("submit");
        assert!(r.committed, "db.create refused: {:?}", r.error);
        // Two writes per database. They used to produce keys canonical state could not attest and
        // the reconciler reported as contract-only: the write plane was outside coverage, so 500
        // unauthorized writes reconciled as in sync. P10 re-runs that attack.
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
                    Ok(()),
                )
                .await
                .expect("submit");
            assert!(w.committed, "db.write refused: {:?}", w.error);
            *writes_per_db.entry(d.id.clone()).or_default() += 1;
        }
    }
    // Canonical state records the write plane for the writes it actually performed -- that is what
    // a wired platform's own account looks like, and it is the only thing that makes the two sides
    // comparable instead of leaving every write unattested forever.
    for d in &mut dbs {
        d.write_count = writes_per_db.get(&d.id).copied();
        d.last_bytes = writes_per_db
            .get(&d.id)
            .copied()
            .map(|_| "1024".to_string());
    }
    if untagged_skipped > 0 {
        println!(
            "  untagged rows skipped: {untagged_skipped} (no bound tenant: the contract cannot emit a              tenant-authorized op for them)"
        );
    }
    let canonical = reconcile::project(&dbs, &snap.database_tombstones);
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
        rep.in_sync() && rep.contract_only.is_empty(),
        "in-contract mutations keep the log and canonical state in agreement",
        format!("{} divergences over {} attested keys", rep.divergences.len(), rep.compared),
    );
    probe(&mut failures,
        rep.compared == dbs.len() * 3,
        "the write plane is inside coverage and compared, not reported as contract-only",
        format!("{} compared keys over {} rows (status + writes + last_bytes)", rep.compared, dbs.len()),
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
        r5.count(DivergenceKind::StaleInContract) == 3 && !r5.in_sync(),
        "bypassed delete is detected, write plane included",
        format!(
            "stale_in_contract={} (dropped {}, status + writes + last_bytes)",
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
        created_ms: 0,
        write_count: Some(0),
        last_bytes: Some("0".to_string()),
    });
    let canonical_injected = reconcile::project(&injected, &snap.database_tombstones);
    let r6 = reconcile::reconcile(&canonical_injected, &replayed);
    println!("\n== P6 bypass: state created without the contract ==");
    for d in r6.divergences.iter().take(3) {
        println!("  {} {}", d.kind.label(), d.key);
    }
    probe(&mut failures,
        r6.count(DivergenceKind::MissingFromContract) == 3,
        "bypassed create is detected",
        format!("missing_from_contract={} (status + writes + last_bytes)", r6.count(DivergenceKind::MissingFromContract)),
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
        r7.count(DivergenceKind::MissingFromContract) == 3
            && r7.count(DivergenceKind::StaleInContract) == 3,
        "bypassed re-key is detected as stale + missing on every attestable key",
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
            Caller { tenant: tenant0.clone(), actor: "p8".into(), owns_target: false, platform_attested: false},
            "db.delete",
            payload(&[("db_id", &snap.databases[0].id), ("project", &snap.databases[0].project)]),
            &snap.databases[0].id,
            0,
            Ok(()),
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
            Caller { tenant: UNTAGGED_TENANT.to_string(), actor: "p8".into(), owns_target: true, platform_attested: false},
            "db.create",
            payload(&[("db_id", "db_x"), ("kind", "redis"), ("project", "p")]),
            "db_x",
            0,
            Ok(()),
        )
        .await
        .expect("submit");
    probe(&mut failures,
        !untagged.committed && untagged.error.as_deref() == Some(&OpError::UntaggedTenant.to_string()),
        "an untagged tenant is refused (fail-closed, never 'personal')",
        format!("{:?}", untagged.error),
    );
    let mut backdoor = Contract::new("hive.backdoor.v1");
    let declared = backdoor.declare(
        "db.write",
        AuthPolicy::OpenParticipation,
        Effect::StateWrite,
        &["db_id"],
        "db_id",
    );
    probe(&mut failures,
        declared.is_err(),
        "open participation may never be declared as a state write",
        format!("{:?}", declared.err().map(|e| e.to_string())),
    );
    let open = engine
        .submit(
            &contract,
            Caller { tenant: "anyone".into(), actor: "relay".into(), owns_target: false, platform_attested: false},
            "db.propose",
            payload(&[("db_id", &snap.databases[0].id), ("note", "observed")]),
            &snap.databases[0].id,
            0,
            Ok(()),
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
    // The map is a finding, not a constant: it grew from 6 to 10 when the three silent removal
    // paths plus workflow storage were located and wired, so a hard count is a maintenance trap --
    // a probe that fails every time coverage improves. What must hold is monotonicity (the map
    // never shrinks below the surface originally located) and closure: no path that removes or
    // creates a `databases` row may sit unwired again, because an unrecorded removal leaves the
    // index asserting a database canonical state no longer has.
    let unwired_mutations = reconcile::DECLARED_SCOPE
        .iter()
        .filter(|e| e.op_kind != "db.write" && !e.wired)
        .count();
    probe(&mut failures,
        declared_n >= 6 && unwired_mutations == 0,
        "the declared mutation surface is the one the map located, and every create/delete in it is wired",
        format!("{declared_n} declared, {wired_n} wired, {unwired_mutations} unwired create/delete paths"),
    );

    // ---- P10: the write plane is inside coverage ---------------------------------------------
    // The attack that exposed contract-write-plane-outside-canonical-coverage: 500 db.write ops
    // appended into a victim's namespace, none of them authorized by canonical state, reconciled as
    // in_sync=true with 0 divergences, because `is_attestable` admitted only /status and apply's
    // db.write emits only /writes and /last_bytes. Same attack, re-run.
    println!("\n== P10 unauthorized writes in a victim namespace ==");
    let victim_db = "db-victim";
    let mut forged = hive_contract::exec::Executor::new(contract.clone());
    // The victim database exists and is live: canonical state and the contract agree on that key,
    // which is exactly why the old reconciler saw nothing -- it attested only /status.
    forged
        .commit(
            hive_contract::oplog::Op {
                seq: 0,
                contract: contract.id.clone(),
                tenant: "victim".to_string(),
                actor: "platform:require_project".to_string(),
                target: victim_db.to_string(),
                kind: "db.create".to_string(),
                payload: payload(&[
                    ("db_id", victim_db),
                    ("kind", "sqlite"),
                    ("project", "proj-victim"),
                ]),
            },
            0,
            Ok(()),
        )
        .expect("commit");
    for i in 0..500u64 {
        // Straight onto the executor: the platform's gate never ran, so canonical state authorized
        // none of these. That is what an out-of-band write path looks like.
        forged
            .commit(
                hive_contract::oplog::Op {
                    seq: 0,
                    contract: contract.id.clone(),
                    tenant: "victim".to_string(),
                    actor: "out-of-band".to_string(),
                    target: victim_db.to_string(),
                    kind: "db.write".to_string(),
                    payload: payload(&[
                        ("bytes", "1048576"),
                        ("db_id", victim_db),
                        ("project", "proj-victim"),
                    ]),
                },
                i,
                Ok(()),
            )
            .expect("commit");
    }
    let forged_index = hive_contract::audit::replay(&contract, &forged.log).expect("replays clean");
    println!(
        "  contract index for the victim: writes={:?} last_bytes={:?}",
        forged_index.get(&reconcile::writes_key("victim", victim_db)),
        forged_index.get(&reconcile::last_bytes_key("victim", victim_db))
    );
    // Canonical state attests the row with the write plane it recorded: no writes.
    let victim_canonical = reconcile::project(
        &[CanonicalDb {
            id: victim_db.to_string(),
            team: "victim".to_string(),
            project: "proj-victim".to_string(),
            status: "ready".to_string(),
            created_ms: 0,
            write_count: Some(0),
            last_bytes: Some("0".to_string()),
        }],
        &BTreeMap::new(),
    );
    let r10 = reconcile::reconcile(&victim_canonical, &forged_index);
    for d in r10.divergences.iter().take(4) {
        println!(
            "  {} {} canonical={:?} contract={:?}",
            d.kind.label(),
            d.key,
            d.canonical,
            d.contract
        );
    }
    probe(&mut failures,
        !r10.in_sync() && r10.divergences.len() >= 1 && r10.compared >= 1,
        "500 unauthorized writes produce a divergence instead of in_sync=true",
        format!(
            "{} divergences over {} attested keys (was: in_sync=true divergences=0 contract_only=2)",
            r10.divergences.len(),
            r10.compared
        ),
    );
    probe(&mut failures,
        r10.count(DivergenceKind::ValueMismatch) == 2,
        "the write counter and the last-bytes value are both compared",
        format!(
            "value_mismatch={} missing={} stale={}",
            r10.count(DivergenceKind::ValueMismatch),
            r10.count(DivergenceKind::MissingFromContract),
            r10.count(DivergenceKind::StaleInContract)
        ),
    );
    // The control: the SAME 500 writes, with canonical state attesting the count it recorded. No
    // cry-wolf -- the reconciler only disagrees when the two accounts actually differ.
    let agreed_canonical = reconcile::project(
        &[CanonicalDb {
            id: victim_db.to_string(),
            team: "victim".to_string(),
            project: "proj-victim".to_string(),
            status: "ready".to_string(),
            created_ms: 0,
            write_count: Some(500),
            last_bytes: Some("1048576".to_string()),
        }],
        &BTreeMap::new(),
    );
    let r10b = reconcile::reconcile(&agreed_canonical, &forged_index);
    probe(&mut failures,
        r10b.in_sync() && r10b.compared == 3,
        "when canonical state records the same write plane, the two agree",
        format!(
            "{} divergences over {} attested keys",
            r10b.divergences.len(),
            r10b.compared
        ),
    );

    // ---- P11: the write plane a real node recorded is the write plane that reconciles ----------
    // Canonical state can only attest what it stores. When a node records no write plane, every
    // `/writes` and `/last_bytes` key the contract holds is unattested, and 500 unauthorized writes
    // once reconciled as in_sync on that absence. P11 takes the plane a real node persisted, replays
    // exactly those writes through the real contract, and reconciles -- so the claim being checked is
    // the platform's own account, not a number this harness invented.
    {
        let mut included = 0usize;
        let mut skipped = 0usize;
        let mut compared = 0usize;
        let mut diverged = 0usize;
        let contract11 = managed_db_contract();
        let engine11 = Engine::new(hive_contract::engine::DEFAULT_SHARDS);
        let mut dbs11: Vec<CanonicalDb> = Vec::new();
        for d in &snap.databases {
            let Some(wp) = snap.database_data.write_plane.get(&d.id) else {
                continue;
            };
            if wp.writes > WRITE_PLANE_MAX || included >= WRITE_PLANE_DBS {
                skipped += 1;
                continue;
            }
            let tenant = record_tenant(Some(&d.team));
            if tenant == UNTAGGED_TENANT {
                skipped += 1;
                continue;
            }
            engine11
                .submit(
                    &contract11,
                    admitted(&tenant),
                    "db.create",
                    payload(&[
                        ("db_id", &d.id),
                        ("kind", &d.kind),
                        ("project", &d.project),
                    ]),
                    &d.id,
                    d.created_ms,
                    Ok(()),
                )
                .await
                .expect("submit");
            for _ in 0..wp.writes {
                engine11
                    .submit(
                        &contract11,
                        admitted(&tenant),
                        "db.write",
                        payload(&[
                            (
                                "bytes",
                                wp.last_bytes.as_deref().unwrap_or("0"),
                            ),
                            ("db_id", &d.id),
                            ("project", &d.project),
                        ]),
                        &d.id,
                        d.created_ms,
                        Ok(()),
                    )
                    .await
                    .expect("submit");
            }
            dbs11.push(CanonicalDb {
                id: d.id.clone(),
                team: d.team.clone(),
                project: d.project.clone(),
                status: d.status.clone(),
                created_ms: d.created_ms,
                write_count: Some(wp.writes),
                last_bytes: wp.last_bytes.clone(),
            });
            included += 1;
        }
        if included > 0 {
            let contract_index = hive_contract::audit::replay(
                &contract11,
                &engine11.snapshot_log(&contract11.id).await.unwrap_or_default(),
            )
            .expect("replays clean");
            let r11 = reconcile::reconcile(
                &reconcile::project(&dbs11, &snap.database_tombstones),
                &contract_index,
            );
            compared = r11.compared;
            diverged = r11.divergences.len();
            for d in r11.divergences.iter().take(4) {
                println!(
                    "  {} {} canonical={:?} contract={:?}",
                    d.kind.label(),
                    d.key,
                    d.canonical,
                    d.contract
                );
            }
        }
        println!("\n== P11 write plane a real node recorded ==");
        println!("databases included : {included} (skipped {skipped})");
        println!("attested keys      : {compared}");
        println!("divergences        : {diverged}");
        // The claim under test is that canonical state CAN attest the write plane and that the two
        // accounts then agree. A snapshot with no write plane at all has nothing to attest, so it
        // reports the fact rather than failing a probe it was never in a position to run.
        if included > 0 {
            probe(
                &mut failures,
                diverged == 0 && compared == included * 3,
                "canonical state's own write plane reconciles against the contract index",
                format!("{diverged} divergences over {compared} attested keys for {included} databases"),
            );
        } else {
            println!(
                "NOTE no database in this snapshot carries a persisted write plane: the write \
                 plane is unattested here, and every /writes key the contract holds would                  reconcile as stale_in_contract"
            );
        }
    }

    if failures == 0 {
        println!("ALL PROBES PASSED");
    } else {
        println!("{failures} PROBE(S) FAILED");
        std::process::exit(1);
    }
}
